//! Driving one sub-agent turn: sidechain transcript markers, foreground and detached
//! turn runners, and panic containment.

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
pub(crate) async fn write_sidechain_meta(
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

/// Marks a sub-agent session as having run its task to conclusion
/// (`SIDECHAIN_TERMINAL` in `docs/daemon_rpc_protocol.md`), once its turn has
/// returned — a session with no such marker was either never a sidechain, is
/// still running, was cancelled mid-flight, or was cut off from outside
/// (process exit, a future `agent.stop`, parent crash), all of which must
/// stay resumable. Best-effort, same rationale as `write_sidechain_meta`.
pub(crate) async fn write_sidechain_terminal_marker(
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
/// shared core of every one-shot delegate and of the persistent member loop.
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
/// Returns the collected text and how the turn ended.
pub(crate) async fn run_one_turn(
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
) -> (String, DelegateEnd) {
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

    let ran = match outcome {
        Ok(o) if o.stop_reason == "cancelled" => {
            (with_stop_note(text, &o.stop_reason), DelegateEnd::Cancelled)
        }
        Ok(o) => {
            let end = match o.stop_reason.as_str() {
                "max_turns" | "host_ceiling" => DelegateEnd::CutShort("max_turns"),
                "max_structured_output_retries" => {
                    DelegateEnd::CutShort("max_structured_output_retries")
                }
                _ => DelegateEnd::Completed,
            };
            (with_stop_note(text, &o.stop_reason), end)
        }
        Err(runtime::turn::TurnError::Shutdown) => (text, DelegateEnd::Completed),
        Err(e) => failed(anyhow!("sub: {e}")),
    };
    if let Some(tag) = &tag {
        tag.completed(match &ran.1 {
            DelegateEnd::Cancelled => "cancelled",
            DelegateEnd::Failed(_) => "failed",
            DelegateEnd::Completed | DelegateEnd::CutShort(_) => "completed",
        });
    }
    ran
}

/// A run that failed with `e`, as the tool error the parent reads.
pub(crate) fn failed(e: anyhow::Error) -> (String, DelegateEnd) {
    (
        String::new(),
        DelegateEnd::Failed(base::error::ToolError::Execution(e).to_string()),
    )
}

/// A sub-agent's answer, with a line saying so when its turn was cut off by
/// a limit rather than finished — without it the parent reads a partial or
/// empty answer as the whole one.
pub(crate) fn with_stop_note(text: String, stop_reason: &str) -> String {
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
pub(crate) async fn run_one_turn_detached(
    session_id: &str,
    mut agent: Agent,
    mut event_rx: EventReceiver,
    input_tx: InputSender,
    prompt: String,
    cancel: tokio_util::sync::CancellationToken,
    tag: Option<SubagentTag>,
    task: Option<Arc<base::context::RunningTask>>,
) -> (String, DelegateEnd) {
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
    joined((&mut task.0).await)
}

fn joined(outcome: Result<(String, DelegateEnd), tokio::task::JoinError>) -> (String, DelegateEnd) {
    match outcome {
        Ok(outcome) => outcome,
        Err(e) if e.is_panic() => failed(anyhow!(
            "sub-agent panicked: {}",
            panic_message(e.into_panic())
        )),
        Err(_) => (String::new(), DelegateEnd::Cancelled),
    }
}

/// A background sub-agent's run, ending in the error its panic left rather
/// than unwinding past the bookkeeping that ends its task.
pub(crate) async fn without_panics(
    run: impl std::future::Future<Output = (String, DelegateEnd)> + Send + 'static,
) -> (String, DelegateEnd) {
    joined(tokio::spawn(run).await)
}

pub(crate) struct AbortOnDrop<T>(pub(crate) tokio::task::JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub(crate) fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
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
pub(crate) async fn mark_sidechain_terminal(
    store: Option<&Arc<dyn HistoryStore>>,
    session_id: &str,
    end: &DelegateEnd,
) {
    let Some(store) = store else { return };
    let state = match end {
        DelegateEnd::Cancelled => return,
        DelegateEnd::Failed(_) => history::entry::SessionEndState::Failed,
        DelegateEnd::Completed | DelegateEnd::CutShort(_) => {
            history::entry::SessionEndState::Completed
        }
    };
    write_sidechain_terminal_marker(store, session_id, state).await;
}
