//! 池内通用的自由函数：scene 注册表、事件中继、LRU 驱逐、后台帧、空闲判定等。

use super::*;

/// The scenes a pool can serve: built-ins, then the host's, then the
/// declarative scenes under `user_scenes` (`<state root>/scenes/<id>/`), then
/// those `scene.register` added, then the plugins'. A declarative or
/// registered scene cannot take an id already there; one that does not load
/// is left out, named in the returned list, and the rest load.
pub(super) fn registry_of(
    host_scenes: &[Arc<dyn Scene>],
    plugin_scenes: &[Arc<dyn Scene>],
    user_scenes: &std::path::Path,
    registered: &[Arc<dyn Scene>],
) -> (scene::SceneRegistry, Vec<String>) {
    let mut registry = scene::SceneRegistry::new();
    let mut refused = Vec::new();
    registry.register_builtin();
    for s in host_scenes {
        registry.register(s.clone());
    }
    for e in registry.register_declarative_dir(user_scenes) {
        warn!(error = %e, "a declarative scene was not loaded");
        refused.push(e.to_string());
    }
    for s in registered {
        if registry.resolve(s.id()).is_some() {
            let e = format!(
                "scene `{}` (scene.register): the id is already registered",
                s.id()
            );
            warn!(error = %e, "a registered scene was not loaded");
            refused.push(e);
        } else {
            registry.register(s.clone());
        }
    }
    for s in plugin_scenes {
        registry.register(s.clone());
    }
    (registry, refused)
}

/// Where a connection the pool opens for itself writes: nowhere. Its turn's
/// frames still reach every real subscriber of the session.
/// A goal round as its own boxed future: `run_turn` starts rounds and rounds
/// run `run_turn`, so the `Send` of one cannot be inferred from the other.
pub(super) fn goal_round_task(
    pool: Arc<SessionPool>,
    sid: String,
    round: base::context::goal::GoalRound,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
    Box::pin(async move { pool.run_goal_round(sid, round).await })
}

/// Whether `live` is doing anything eviction would cut off: running a turn,
/// holding its turn slot (a turn about to start, a rebuild), or running
/// background tasks.
pub(super) fn is_busy(live: &LiveSession) -> bool {
    if live.current_turn.is_some() {
        return true;
    }
    let slot_free = live.event_rx.try_lock().is_ok_and(|slot| slot.is_some());
    if !slot_free {
        return true;
    }
    live.state.as_ref().is_some_and(|state| {
        state
            .list_running_tasks()
            .iter()
            .any(|(_, status)| matches!(status, base::context::RunningStatus::Running))
    })
}

/// Run `work` — disk reads and directory scans — on the blocking pool rather
/// than on a runtime worker.
pub(super) async fn off_the_runtime<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> T {
    match tokio::task::spawn_blocking(work).await {
        Ok(value) => value,
        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        Err(e) => panic!("blocking work did not finish: {e}"),
    }
}

/// The engine's events passed through to a receiver of their own, with a
/// notice each time one arrives — what lets the forwarder between turns sleep
/// until there is something to forward.
pub(super) fn relay_events(
    mut from_engine: EventReceiver,
) -> (EventReceiver, Arc<tokio::sync::Notify>) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let arrived = Arc::new(tokio::sync::Notify::new());
    let notify = arrived.clone();
    tokio::spawn(async move {
        while let Some(event) = from_engine.recv().await {
            if tx.send(event).is_err() {
                return;
            }
            notify.notify_one();
        }
    });
    (rx, arrived)
}

/// Take the least recently active idle session out of `sessions`, or `None`
/// when every session is busy — the pool then runs over its cap rather than
/// cut one off.
pub(super) fn evict_lru(
    sessions: &mut HashMap<String, LiveSession>,
) -> Option<(String, LiveSession)> {
    let sid = sessions
        .iter()
        .filter(|(_, live)| !is_busy(live))
        .min_by_key(|(_, live)| live.last_active)
        .map(|(k, _)| k.clone())?;
    sessions.remove_entry(&sid)
}

/// The frame for an event the engine sends whether or not a turn is running
/// — a question, a tool call's, a sub-agent's or a team's progress, changed
/// skills — with
/// the id of the question and of the background task asking it when it is
/// one; `None` for any other event.
///
/// A question is not terminal: the tool call that asked waits on
/// `session.respondToPrompt`. A sub-agent's activity arrives under its own
/// `kind`, so a client that does not know it ignores it and one that does
/// can show the sub-agent's tool calls under its `agent_label` instead of a
/// stalled `Agent` call.
pub(super) fn background_frame(
    sid: &str,
    turn_id: Option<&str>,
    event: &AgentEvent,
) -> Option<(StreamFrame, Option<Ask>)> {
    let (body, prompt_id) = match event {
        AgentEvent::PermissionPrompt {
            prompt_id,
            tool_name,
            message,
            paths,
            tool_use_id,
            task_id,
            ..
        } => {
            let mut body = serde_json::json!({
                "kind":"prompt","prompt_type":crate::rpc::PROMPT_TYPE_PERMISSION,"prompt_id":prompt_id,
                "tool_name":tool_name,"message":message,
                "paths":paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>()
            });
            if let Some(tool_use_id) = tool_use_id {
                body["tool_use_id"] = serde_json::json!(tool_use_id);
            }
            (body, Some((prompt_id.clone(), task_id.clone())))
        }
        AgentEvent::QuestionPrompt {
            prompt_id,
            question,
            header,
            options,
            tool_use_id,
            task_id,
            ..
        } => (
            serde_json::json!({
                "kind":"prompt","prompt_type":crate::rpc::PROMPT_TYPE_QUESTION,
                "prompt_id":prompt_id,"tool_use_id":tool_use_id,
                "question":question,"header":header,"options":options
            }),
            Some((prompt_id.clone(), task_id.clone())),
        ),
        AgentEvent::SubagentProgress {
            agent_label,
            agent_session_id,
            agent_type,
            parent_turn,
            parent_tool_use_id,
            task_id,
            event,
            ..
        } => (
            serde_json::json!({
                "kind":"subagent_progress","agent_label":agent_label,
                "agent_session_id":agent_session_id,"agent_type":agent_type,
                "parent_turn":parent_turn,"parent_tool_use_id":parent_tool_use_id,
                "task_id":task_id,
                "event":serde_json::to_value(&**event).unwrap_or(serde_json::Value::Null)
            }),
            None,
        ),
        AgentEvent::ToolProgress {
            tool_use_id,
            name,
            message,
            progress,
            total,
            dropped,
            turn_id: _,
        } => {
            let mut body = serde_json::json!({
                "kind":"tool_progress","tool_use_id":tool_use_id,"name":name,
                "message":message,"dropped":dropped
            });
            for (key, value) in [("progress", progress), ("total", total)] {
                if let Some(v) = value {
                    body[key] = serde_json::json!(v);
                }
            }
            (body, None)
        }
        AgentEvent::SkillsChanged { added, removed, .. } => (
            serde_json::json!({"kind":"skills_changed","added":added,"removed":removed}),
            None,
        ),
        AgentEvent::Notification {
            message, status, ..
        } => (
            serde_json::json!({"kind":"notification","message":message,"status":status}),
            None,
        ),
        AgentEvent::TeamProgress {
            team,
            team_id,
            stage,
            stage_index,
            stage_count,
            status,
            members,
            failed,
        } => (
            serde_json::json!({
                "kind":"team_progress","team":team,"team_id":team_id,"stage":stage,
                "stage_index":stage_index,"stage_count":stage_count,
                "status":status,"members":members,"failed":failed
            }),
            None,
        ),
        _ => return None,
    };
    Some((StreamFrame::event_in(sid, turn_id, body), prompt_id))
}

pub(super) fn reap_process_groups_later(live: &LiveSession) {
    if let Some(ledger) = live.process_groups.clone() {
        tokio::spawn(async move { ledger.reap(PROCESS_GROUP_GRACE).await });
    }
}

/// Session id 列表：`last_active` 距 `now` 超过 `timeout`、且手上没有活的那些。拆成纯函数
/// （不做任何驱逐动作，只挑出 id）——原逻辑内嵌在 `start_janitor` 真实跑
/// 300 秒一次的循环体里，没法在测试里等 5 分钟去验证；抽出来之后可以直接用
/// 人为构造的" last_active 已经很旧"的 session 测，不用真的睡觉。
pub(super) fn idle_session_ids(
    sessions: &HashMap<String, LiveSession>,
    now: Instant,
    timeout: Duration,
) -> Vec<String> {
    sessions
        .iter()
        .filter(|(_, live)| now.duration_since(live.last_active) > timeout && !is_busy(live))
        .map(|(sid, _)| sid.clone())
        .collect()
}

pub(super) fn format_instant(t: Instant) -> String {
    let ago = Instant::now().duration_since(t);
    let secs_ago = ago.as_secs();
    let now = time::OffsetDateTime::now_utc();
    let abs = now - time::Duration::seconds(secs_ago as i64);
    abs.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "unknown".into())
}
