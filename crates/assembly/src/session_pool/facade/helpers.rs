//! 池内通用的自由函数：事件中继与发布、LRU 驱逐、空闲判定等。

use crate::session_pool::*;

/// A goal round as its own boxed future: `run_turn` starts rounds and rounds
/// run `run_turn`, so the `Send` of one cannot be inferred from the other.
pub(in crate::session_pool) fn goal_round_task(
    pool: Arc<SessionPool>,
    sid: String,
    round: base::context::goal::GoalRound,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
    Box::pin(async move { pool.run_goal_round(sid, round).await })
}

/// Whether `live` is doing anything eviction would cut off: running a turn,
/// being rebuilt, or running background tasks.
pub(in crate::session_pool) fn is_busy(live: &LiveSession) -> bool {
    !live.turn.is_idle()
        || live.state.as_ref().is_some_and(|state| {
            state
                .list_running_tasks()
                .iter()
                .any(|(_, status)| matches!(status, base::context::RunningStatus::Running))
        })
}

/// The engine's events passed through to a receiver of their own, with a
/// notice each time one arrives — what lets the forwarder between turns sleep
/// until there is something to forward.
pub(in crate::session_pool) fn relay_events(
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
pub(in crate::session_pool) fn evict_lru(
    sessions: &mut HashMap<String, LiveSession>,
) -> Option<(String, LiveSession)> {
    let sid = sessions
        .iter()
        .filter(|(_, live)| !is_busy(live))
        .min_by_key(|(_, live)| live.hub.last_active())
        .map(|(k, _)| k.clone())?;
    sessions.remove_entry(&sid)
}

/// The ask `event` raises — its `prompt_id`, the background task asking it
/// when one does, and what kind of answer it wants.
pub(in crate::session_pool) fn ask_of(
    event: &AgentEvent,
) -> Option<(String, Option<String>, AskKind)> {
    match event {
        AgentEvent::PermissionPrompt {
            prompt_id, task_id, ..
        } => Some((prompt_id.clone(), task_id.clone(), AskKind::Permission)),
        AgentEvent::QuestionPrompt {
            prompt_id, task_id, ..
        } => Some((prompt_id.clone(), task_id.clone(), AskKind::Question)),
        _ => None,
    }
}

/// Hand `event` to whoever watches the session, recording it first when it
/// is an ask.
pub(in crate::session_pool) fn publish_agent_event(
    hub: &SessionHub,
    session_id: &Arc<str>,
    turn_id: Option<&Arc<str>>,
    event: AgentEvent,
) {
    let ask = ask_of(&event);
    let frame = SessionFrame {
        session_id: session_id.clone(),
        turn_id: turn_id.cloned(),
        event: SessionEvent::Agent(event),
    };
    match ask {
        Some((prompt_id, task_id, kind)) => {
            hub.publish_ask(Arc::new(frame), prompt_id, task_id, kind)
        }
        None => hub.publish(&frame),
    }
}

/// A frame of `session_id` that belongs to no turn.
pub(in crate::session_pool) fn frame_between_turns(
    session_id: &str,
    event: SessionEvent,
) -> SessionFrame {
    SessionFrame {
        session_id: Arc::from(session_id),
        turn_id: None,
        event,
    }
}

pub(in crate::session_pool) fn reap_process_groups_later(live: &LiveSession) {
    if let Some(ledger) = live.process_groups.clone() {
        tokio::spawn(async move { ledger.reap(PROCESS_GROUP_GRACE).await });
    }
}

/// Session id 列表：`last_active` 距 `now` 超过 `timeout`、且手上没有活的那些。拆成纯函数
/// （不做任何驱逐动作，只挑出 id）——原逻辑内嵌在 `start_janitor` 真实跑
/// 300 秒一次的循环体里，没法在测试里等 5 分钟去验证；抽出来之后可以直接用
/// 人为构造的" last_active 已经很旧"的 session 测，不用真的睡觉。
pub(in crate::session_pool) fn idle_session_ids(
    sessions: &HashMap<String, LiveSession>,
    now: Instant,
    timeout: Duration,
) -> Vec<String> {
    sessions
        .iter()
        .filter(|(_, live)| {
            now.saturating_duration_since(live.hub.last_active()) > timeout && !is_busy(live)
        })
        .map(|(sid, _)| sid.clone())
        .collect()
}

pub(in crate::session_pool) fn format_instant(t: Instant) -> String {
    let ago = Instant::now().duration_since(t);
    let secs_ago = ago.as_secs();
    let now = time::OffsetDateTime::now_utc();
    let abs = now - time::Duration::seconds(secs_ago as i64);
    abs.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "unknown".into())
}
