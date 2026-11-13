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
/// is an ask, forgetting the ask first when it is an ask's end.
pub(in crate::session_pool) fn publish_agent_event(
    hub: &SessionHub,
    session_id: &Arc<str>,
    turn_id: Option<&Arc<str>>,
    event: AgentEvent,
) {
    let ask = ask_of(&event);
    let resolved = match &event {
        AgentEvent::PromptResolved { prompt_id, .. } => Some(prompt_id.clone()),
        _ => None,
    };
    let frame = SessionFrame {
        session_id: session_id.clone(),
        turn_id: turn_id.cloned(),
        event: SessionEvent::Agent(event),
    };
    match (ask, resolved) {
        (Some((prompt_id, task_id, kind)), _) => {
            hub.publish_ask(Arc::new(frame), prompt_id, task_id, kind)
        }
        (None, Some(prompt_id)) => hub.publish_resolved(&frame, &prompt_id),
        (None, None) => hub.publish(&frame),
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

pub(in crate::session_pool) fn format_instant(t: Instant) -> String {
    let ago = Instant::now().duration_since(t);
    let secs_ago = ago.as_secs();
    let now = time::OffsetDateTime::now_utc();
    let abs = now - time::Duration::seconds(secs_ago as i64);
    abs.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "unknown".into())
}
