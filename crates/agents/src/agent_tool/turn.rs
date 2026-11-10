//! Driving one sub-agent turn: sidechain transcript markers, foreground and detached
//! turn runners, panic containment, and the persistent team-member loop.

use super::*;

/// Write the first `Meta` line for a freshly spawned sidechain (sub-agent /
/// team member) session, linking it back to its parent via
/// `parent_session_id` and marking it `session_kind: Sidechain` — this is
/// what makes it discoverable via `HistoryStore::child_sessions` and hidden
/// from `session.list` by default. See docs/ARCHITECTURE.md §3.3.
///
/// `project_root` is left `None` here: this crate has no per-session project
/// identity, so there is nothing correct to stamp.
///
/// Best-effort: a failure here does not fail the spawn. The sub-agent still
/// runs and its `User`/`Assistant` entries still persist via
/// `SessionManager::persist` — it just isn't linkable back to its parent.
pub(super) async fn write_sidechain_meta(
    store: &Arc<dyn HistoryStore>,
    session_id: &str,
    parent_session_id: Option<String>,
    scene_id: &str,
    cwd: &Path,
    settings: &Settings,
) {
    let Ok(sid) = base::session::SessionId::parse(session_id) else {
        return;
    };
    let meta = history::entry::LogEntry::Meta {
        cwd: cwd.display().to_string(),
        started_at: store.environment().now(),
        model: settings.model.model_name.clone(),
        permission_mode: serde_json::to_value(settings.permission_mode)
            .ok()
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_else(|| "default".to_string()),
        engine_version: env!("CARGO_PKG_VERSION").to_string(),
        agix_version: env!("CARGO_PKG_VERSION").to_string(),
        parent_session_id,
        scene: Some(scene_id.to_string()),
        project_root: None,
        session_kind: history::entry::SessionKind::Sidechain,
        schema_version: history::entry::CURRENT_META_SCHEMA_VERSION,
    };
    if let Err(e) = store.append(sid, meta).await {
        tracing::warn!(session_id, error = %e, "failed to write sidechain Meta entry");
    }
}

/// Marks a one-shot sub-agent session as having run its task to conclusion
/// (`SIDECHAIN_TERMINAL` in `docs/daemon_rpc_protocol.md`). Called only
/// through `mark_sidechain_terminal`
/// (from `run_sub_inner`/`run_sub_tagged`) once their turn has
/// returned — a session with no such marker was either never a sidechain, is
/// still running, was cancelled mid-flight, or was cut off from outside
/// (process exit, a future `agent.stop`, parent crash), all of which must
/// stay resumable. Best-effort, same rationale as `write_sidechain_meta`.
pub(super) async fn write_sidechain_terminal_marker(
    store: &Arc<dyn HistoryStore>,
    session_id: &str,
    state: history::entry::SessionEndState,
) {
    let Ok(sid) = base::session::SessionId::parse(session_id) else {
        return;
    };
    let entry = history::entry::LogEntry::SessionEnd { state };
    if let Err(e) = store.append(sid, entry).await {
        tracing::warn!(session_id, error = %e, "failed to write sidechain SessionEnd marker");
    }
}

/// Run one turn against a sub-agent and collect its response text — the
/// shared core of `run_sub`/`run_sub_inner`/the persistent team-member loop.
/// Takes `event_rx` **borrowed**, not owned, specifically so a persistent
/// team member can call this repeatedly across many turns against the same
/// `Agent`/channel pair instead of a fresh one being built per message.
///
/// `AgentEvent::TurnComplete` is **not** guaranteed to be emitted on every
/// return path of `run_user_turn` (`crate::turn`) — several early returns
/// there (a plain model error, an already-cancelled token, the max-turns
/// guard) skip it, only the fully-successful tail sends it. Draining until
/// `TurnComplete` unconditionally deadlocks forever on those paths, because
/// the event channel's sender lives inside `agent`, which nothing drops
/// until this function's caller does — and the caller can't do that until
/// this function returns. `agent.run_turn(..)` resolving is itself the
/// authoritative "the turn is over" signal (a plain future, not gated on
/// any event); this races the drain against that future and, once it
/// resolves, only waits a short bounded grace window for the drain to
/// catch up on anything still buffered — never indefinitely on
/// `TurnComplete` itself. Confirmed reproducible even against unmodified
/// `runtime::turn` (a plain model error is enough to trigger it), not specific to
/// any one caller.
///
/// Returns the collected text alongside whether the turn ended because
/// `cancel` fired (`TurnOutcome::stop_reason == "cancelled"`) rather than
/// running to a real conclusion — callers that mark sidechain terminal state
/// need to tell the two apart (see `mark_sidechain_terminal`).
pub(super) async fn run_one_turn(
    agent: &mut Agent,
    event_rx: &mut EventReceiver,
    input_tx: &InputSender,
    prompt: String,
    cancel: tokio_util::sync::CancellationToken,
    // When set, every event this sub-agent emits is also mirrored onto the
    // parent session's channel wrapped in `AgentEvent::SubagentProgress`, so
    // the host can render sub-agent activity live. Purely additive: the
    // text return value is identical either way.
    tag: Option<SubagentTag>,
    task: Option<&base::context::RunningTask>,
) -> (Result<String, base::error::ToolError>, bool) {
    let turn_id = uuid::Uuid::new_v4().to_string();
    let _ = input_tx.send(InputMessage::User {
        content: prompt.clone(),
        attachments: vec![],
        turn_id: turn_id.clone(),
    });

    if let Some(tag) = &tag {
        tag.spawned();
    }

    let mut text = String::new();
    let run_turn_fut = agent.run_turn(prompt, turn_id, cancel);
    tokio::pin!(run_turn_fut);
    let outcome = loop {
        tokio::select! {
            biased;
            ev = event_rx.recv() => {
                // `ev` is `None` when the channel closed mid-turn (shouldn't
                // happen — `agent` outlives this loop — but don't spin on a
                // dead channel if it somehow does); nothing to do in that case.
                if let Some(ev) = ev {
                    if let AgentEvent::TextDelta { text: ref t, .. } = ev {
                        text.push_str(t);
                    }
                    note_on_task(task, &ev);
                    if let Some(tag) = &tag {
                        tag.forward(ev);
                    }
                }
            }
            outcome = &mut run_turn_fut => break outcome,
        }
    };

    // Grace window: `run_turn` resolving doesn't guarantee every already-
    // queued event has been *received* by us yet (channel delivery isn't
    // instantaneous) — drain whatever shows up in a short bounded window,
    // then stop regardless of whether more might theoretically still come.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(200);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, event_rx.recv()).await {
            Ok(Some(ev)) => {
                if let AgentEvent::TextDelta { text: ref t, .. } = ev {
                    text.push_str(t);
                }
                note_on_task(task, &ev);
                if let Some(tag) = &tag {
                    tag.forward(ev);
                }
            }
            Ok(None) | Err(_) => break,
        }
    }

    let cancelled = matches!(&outcome, Ok(o) if o.stop_reason == "cancelled");
    let result = match outcome {
        Ok(o) => Ok(with_stop_note(text, &o.stop_reason)),
        Err(runtime::turn::TurnError::Shutdown) => Ok(text),
        Err(e) => Err(base::error::ToolError::Execution(anyhow!("sub: {e}"))),
    };
    if let Some(tag) = &tag {
        tag.completed(match (&result, cancelled) {
            (_, true) => "cancelled",
            (Ok(_), _) => "completed",
            (Err(_), _) => "failed",
        });
    }
    (result, cancelled)
}

/// A sub-agent's answer, with a line saying so when its turn was cut off by
/// a limit rather than finished — without it the parent reads a partial or
/// empty answer as the whole one.
pub(super) fn with_stop_note(text: String, stop_reason: &str) -> String {
    let note = match stop_reason {
        "max_turns" | "host_ceiling" => {
            "The sub-agent reached its limit of model calls (max_turns) before it finished"
        }
        "max_structured_output_retries" => {
            "The sub-agent stopped after too many attempts at its structured output"
        }
        _ => return text,
    };
    if text.trim().is_empty() {
        format!("[{note}; it gave no answer.]")
    } else {
        format!("{text}\n\n[{note}; the answer above may be incomplete.]")
    }
}

/// [`run_one_turn`] on a task of its own, so a foreground sub-agent is
/// scheduled like any other agent instead of being polled from inside the
/// parent's turn. The parent still waits for it; if the parent's tool call is
/// dropped instead, the guard aborts the sub-agent rather than leaving it
/// running unobserved.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_one_turn_detached(
    session_id: &str,
    mut agent: Agent,
    mut event_rx: EventReceiver,
    input_tx: InputSender,
    prompt: String,
    cancel: tokio_util::sync::CancellationToken,
    tag: Option<SubagentTag>,
    task: Option<Arc<base::context::RunningTask>>,
) -> (Result<String, base::error::ToolError>, bool) {
    use tracing::Instrument;
    let span = tracing::info_span!("subagent", session = %session_id);
    let mut task = AbortOnDrop(tokio::spawn(
        async move {
            let outcome = run_one_turn(
                &mut agent,
                &mut event_rx,
                &input_tx,
                prompt,
                cancel,
                tag,
                task.as_deref(),
            )
            .await;
            drop(input_tx);
            outcome
        }
        .instrument(span),
    ));
    match (&mut task.0).await {
        Ok(outcome) => outcome,
        Err(e) if e.is_panic() => (
            Err(base::error::ToolError::Execution(anyhow!(
                "sub-agent panicked: {}",
                panic_message(e.into_panic())
            ))),
            false,
        ),
        Err(_) => (
            Err(base::error::ToolError::Execution(anyhow!(
                "sub-agent task was aborted"
            ))),
            true,
        ),
    }
}

/// A background sub-agent's run, ending in the error its panic left rather
/// than unwinding past the bookkeeping that ends its task.
pub(super) async fn without_panics(
    run: impl std::future::Future<Output = (Result<String, base::error::ToolError>, bool)>
        + Send
        + 'static,
) -> (Result<String, base::error::ToolError>, bool) {
    match tokio::spawn(run).await {
        Ok(outcome) => outcome,
        Err(e) if e.is_panic() => (
            Err(base::error::ToolError::Execution(anyhow!(
                "sub-agent panicked: {}",
                panic_message(e.into_panic())
            ))),
            false,
        ),
        Err(_) => (
            Err(base::error::ToolError::Execution(anyhow!(
                "sub-agent task was aborted"
            ))),
            true,
        ),
    }
}

pub(super) struct AbortOnDrop<T>(pub(super) tokio::task::JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub(super) fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "(non-string panic payload)".into())
}

/// After a one-shot sidechain's turn has finished — successfully, with an
/// error, or because `cancel` fired — writes the matching `SessionEnd`
/// terminal marker.
///
/// A cancelled turn is deliberately left unmarked: cancellation means the
/// work was interrupted, not that the conversation reached a real endpoint,
/// so the sidechain must stay resumable exactly like one that's still
/// mid-flight.
pub(super) async fn mark_sidechain_terminal(
    store: Option<&Arc<dyn HistoryStore>>,
    session_id: &str,
    result: &Result<String, base::error::ToolError>,
    cancelled: bool,
) {
    let Some(store) = store else { return };
    if cancelled {
        return;
    }
    let state = if result.is_ok() {
        history::entry::SessionEndState::Completed
    } else {
        history::entry::SessionEndState::Failed
    };
    write_sidechain_terminal_marker(store, session_id, state).await;
}

pub(super) fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The persistent-member counterpart to `run_sub_inner` — instead of one
/// `run_one_turn` call and done, this loops for as long as messages keep
/// arriving on `message_rx`, reusing the same `Agent`/`event_rx`/`input_tx`
/// across every one of them (that reuse *is* the persistence — a fresh
/// `Agent` per message would have no memory of the previous ones). Exits
/// when `cancel` fires (`AgentTool::stop_team_member`/reclaim) or
/// `message_rx` closes (sender side — the map entry in
/// `Inner::team_members` — dropped, which shouldn't happen while this loop
/// is still registered there, but is handled instead of assumed).
///
/// Each message gets its own `RunningTask` (registered by the caller before
/// queuing it) — status/output are written there exactly like
/// `AgentTool::launch_bg`'s one-shot background tasks, so the model polls a
/// specific message's reply via the ordinary `TaskOutput`/`TaskStop`
/// mechanism, not a new one invented for this.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_persistent_member_loop(
    mut agent: Agent,
    mut event_rx: EventReceiver,
    input_tx: InputSender,
    mut message_rx: tokio::sync::mpsc::UnboundedReceiver<PersistentMessage>,
    cancel: tokio_util::sync::CancellationToken,
    registry: Option<Arc<team::registry::TeamRegistry>>,
    team_name: String,
    member_name: String,
    idle_since: Arc<std::sync::atomic::AtomicI64>,
    idle_seq: Arc<std::sync::atomic::AtomicU64>,
    idle_seq_counter: Arc<std::sync::atomic::AtomicU64>,
    session: Arc<base::context::SessionState>,
    stopped: Option<(std::sync::Weak<hooks::HookRunner>, SubagentPayload)>,
    spawned: Option<SpawnTelemetry>,
) {
    let mut last_failed = false;
    loop {
        let msg = tokio::select! {
            _ = cancel.cancelled() => break,
            m = message_rx.recv() => m,
        };
        let Some(PersistentMessage { prompt, task }) = msg else {
            break;
        };

        idle_since.store(i64::MAX, std::sync::atomic::Ordering::SeqCst);
        idle_seq.store(u64::MAX, std::sync::atomic::Ordering::SeqCst);
        if let Some(r) = &registry {
            r.update_member_lifecycle(
                &team_name,
                &member_name,
                team::coordinator::TeammateLifecycle::Active,
                None,
            );
        }

        open_inbox(Some(&task), &agent);
        let (result, cancelled) = run_one_turn(
            &mut agent,
            &mut event_rx,
            &input_tx,
            prompt,
            task.cancel.clone(),
            // No `SubagentTag`: this loop outlives any single parent turn (it
            // is a persistent team member, driven by `message_rx`, not by one
            // `Agent` tool call), so there is no parent turn to attribute its
            // events to. Persistent members surface through `TeamProgress`
            // and `TeamList` instead.
            None,
            Some(task.as_ref()),
        )
        .await;

        last_failed = result.is_err();
        conclude_background_task(&session, &task, &result, cancelled, true);

        let now = now_secs();
        idle_since.store(now, std::sync::atomic::Ordering::SeqCst);
        idle_seq.store(
            idle_seq_counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst),
            std::sync::atomic::Ordering::SeqCst,
        );
        if let Some(r) = &registry {
            r.update_member_lifecycle(
                &team_name,
                &member_name,
                team::coordinator::TeammateLifecycle::Idle,
                Some(now as u64),
            );
        }
    }
    if let Some(r) = &registry {
        r.update_member_lifecycle(
            &team_name,
            &member_name,
            team::coordinator::TeammateLifecycle::Shutdown,
            None,
        );
    }
    if let Some(spawned) = spawned {
        spawned.finish(last_failed);
    }
    if let Some((runner, payload)) = stopped {
        if let Some(runner) = runner.upgrade() {
            payload
                .stopped("shutdown")
                .fire(&runner, hooks::HookEvent::SubagentStop)
                .await;
        }
    }
}
