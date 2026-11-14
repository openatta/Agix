//! 会话帧投影的测试。golden 冻结每种 `AgentEvent` 样本、以及池自己合成的会话事件，
//! 在 turn 内和两轮之间各发出什么帧（`testdata/session_events.golden.json`，
//! `AGIX_UPDATE_GOLDEN=1` 时重写）。

use super::*;
use crate::session_pool::api::emitted_outside_turns;
use serde_json::{json, Value};
use std::sync::Arc;

const SID: &str = "s1";
const TURN: &str = "t1";

fn golden_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/session_events.golden.json")
}

fn agent_samples() -> Vec<(&'static str, Value)> {
    let usage = json!({"input_tokens": 10, "output_tokens": 5,
                       "cache_creation_input_tokens": 2, "cache_read_input_tokens": 1});
    let inner = |event: Value| {
        json!({"kind": "subagent_progress", "agent_label": "explore#1a2b3c4d",
               "agent_session_id": "child", "agent_type": "explore", "parent_session_id": SID,
               "parent_turn": 2, "parent_tool_use_id": "toolu_agent", "task_id": "a1",
               "event": event})
    };
    let text = json!({"kind": "text_delta", "text": "hello", "turn_id": "inner"});
    vec![
        ("text_delta", text.clone()),
        (
            "thinking_delta",
            json!({"kind": "thinking_delta", "text": "hmm", "turn_id": "inner"}),
        ),
        (
            "tool_use_start",
            json!({"kind": "tool_use_start", "id": "toolu_1", "name": "Bash", "turn_id": "inner"}),
        ),
        (
            "tool_args_delta",
            json!({"kind": "tool_args_delta", "id": "toolu_1", "partial_json": "{\"comm", "turn_id": "inner"}),
        ),
        (
            "tool_use",
            json!({"kind": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"command": "ls"}, "turn_id": "inner"}),
        ),
        (
            "tool_progress",
            json!({"kind": "tool_progress", "tool_use_id": "toolu_1", "name": "Bash", "message": "step 2", "progress": 2.0, "total": 10.0, "dropped": 3, "turn_id": "inner"}),
        ),
        (
            "tool_progress_fraction",
            json!({"kind": "tool_progress", "tool_use_id": "toolu_1", "name": "Bash", "message": "a quarter", "progress": 0.25, "dropped": 0, "turn_id": "inner"}),
        ),
        (
            "tool_progress_bare",
            json!({"kind": "tool_progress", "tool_use_id": "toolu_1", "name": "Bash", "message": "working", "dropped": 0, "turn_id": "inner"}),
        ),
        (
            "tool_result",
            json!({"kind": "tool_result", "id": "toolu_1", "name": "Bash", "content": "a\nb", "is_error": false, "turn_id": "inner"}),
        ),
        (
            "tool_result_unflagged",
            json!({"kind": "tool_result", "id": "toolu_1", "name": "Bash", "content": "a", "is_error": null, "turn_id": "inner"}),
        ),
        (
            "permission_prompt",
            json!({"kind": "permission_prompt", "prompt_id": "p1", "tool_name": "Bash", "message": "run ls?", "paths": ["/tmp/x"], "turn_id": "inner", "tool_use_id": "toolu_1", "task_id": "a1"}),
        ),
        (
            "permission_prompt_bare",
            json!({"kind": "permission_prompt", "prompt_id": "p2", "tool_name": "Write", "message": "write?", "paths": [], "turn_id": "inner"}),
        ),
        (
            "question_prompt",
            json!({"kind": "question_prompt", "prompt_id": "toolu_q", "question": "which?", "header": "Pick", "options": [{"key": "a", "label": "A"}], "turn_id": "inner", "tool_use_id": "toolu_q", "task_id": "a1"}),
        ),
        (
            "question_prompt_bare",
            json!({"kind": "question_prompt", "prompt_id": "q2", "question": "why?", "turn_id": "inner"}),
        ),
        (
            "prompt_resolved",
            json!({"kind": "prompt_resolved", "prompt_id": "p1", "resolution": "answered"}),
        ),
        (
            "prompt_withdrawn",
            json!({"kind": "prompt_resolved", "prompt_id": "q2", "resolution": "withdrawn"}),
        ),
        (
            "prompt_timed_out",
            json!({"kind": "prompt_resolved", "prompt_id": "p3", "resolution": "withdrawn", "reason": "timed_out"}),
        ),
        (
            "turn_start",
            json!({"kind": "turn_start", "turn_id": "inner"}),
        ),
        (
            "step_start",
            json!({"kind": "step_start", "step": 0, "turn_id": "inner"}),
        ),
        (
            "model_retry",
            json!({"kind": "model_retry", "step": 0, "attempt": 1, "delay_ms": 2000, "reason": "stream_interrupted: reset", "turn_id": "inner"}),
        ),
        (
            "step_end",
            json!({"kind": "step_end", "step": 0, "stop_reason": "tool_use", "usage": usage, "tool_calls": 1, "turn_id": "inner", "seq": 4}),
        ),
        (
            "turn_complete",
            json!({"kind": "turn_complete", "stop_reason": "end_turn", "api_calls": 2, "tool_calls": 1, "usage": usage, "turn_id": "inner", "message": "stopped by hook", "last_call_usage": usage, "context": {"tokens": 100, "compact_threshold": 800, "window": 1000}, "structured_output": {"ok": true}, "seq": 6, "truncated": true, "persisted": false}),
        ),
        (
            "turn_complete_bare",
            json!({"kind": "turn_complete", "stop_reason": "end_turn", "api_calls": 1, "tool_calls": 0, "usage": usage, "turn_id": "inner", "seq": 2}),
        ),
        (
            "user_message_delivered",
            json!({"kind": "user_message_delivered", "message_id": "m1", "content": "also this", "turn_id": "inner"}),
        ),
        (
            "user_message_refused",
            json!({"kind": "user_message_refused", "message_id": "m1", "content": "no", "message": "not now", "turn_id": "inner"}),
        ),
        (
            "user_message_refused_bare",
            json!({"kind": "user_message_refused", "message_id": "m1", "content": "no", "turn_id": "inner"}),
        ),
        (
            "user_messages_returned",
            json!({"kind": "user_messages_returned", "messages": [{"message_id": "m1", "content": "late"}], "turn_id": "inner"}),
        ),
        (
            "system_init",
            json!({"kind": "system_init", "scene": "code", "tools": [{"name": "Bash", "description": "run"}], "mcp_servers": []}),
        ),
        ("system", json!({"kind": "system", "message": "note"})),
        (
            "compact_action",
            json!({"kind": "compact_action", "strategy": "Snip", "messages_before": 40, "messages_after": 12, "turn_id": "inner", "dropped_rounds": 3, "dropped_messages": 28, "estimated_tokens_saved": 9000, "tokens_before": 12000, "tokens_after": 3000}),
        ),
        (
            "compact_action_bare",
            json!({"kind": "compact_action", "strategy": "Summary", "messages_before": 40, "messages_after": 2, "turn_id": "inner", "dropped_rounds": null, "dropped_messages": null, "estimated_tokens_saved": null}),
        ),
        (
            "session_changed",
            json!({"kind": "session_changed", "session_id": "s2"}),
        ),
        (
            "session_persisted",
            json!({"kind": "session_persisted", "session_id": SID}),
        ),
        (
            "skills_changed",
            json!({"kind": "skills_changed", "added": ["deploy"], "removed": ["old"], "turn_id": "inner"}),
        ),
        (
            "notification",
            json!({"kind": "notification", "message": "deploy ready", "status": "proactive", "turn_id": "inner"}),
        ),
        (
            "notification_bare",
            json!({"kind": "notification", "message": "look", "status": null, "turn_id": "inner"}),
        ),
        (
            "agent_spawned",
            json!({"kind": "agent_spawned", "agent_id": "a1", "parent_turn": 2, "turn_id": "inner"}),
        ),
        (
            "agent_completed",
            json!({"kind": "agent_completed", "agent_id": "a1", "outcome": "done", "turn_id": "inner"}),
        ),
        (
            "team_progress",
            json!({"kind": "team_progress", "team": "review", "team_id": "tm1", "stage": "read", "stage_index": 0, "stage_count": 2, "status": "completed", "members": ["r1", "r2"], "failed": ["r2"]}),
        ),
        (
            "error",
            json!({"kind": "error", "failure": {"kind": "rate_limited", "retryable": true, "retry_after_ms": 30000, "side_effects": true, "message": "try later"}, "turn_id": "inner"}),
        ),
        ("subagent_text_delta", inner(text)),
        (
            "subagent_tool_progress",
            inner(
                json!({"kind": "tool_progress", "tool_use_id": "toolu_in", "name": "Read", "message": "m", "progress": 42.0, "dropped": 1, "turn_id": "inner"}),
            ),
        ),
        (
            "subagent_permission_prompt",
            inner(
                json!({"kind": "permission_prompt", "prompt_id": "p3", "tool_name": "Bash", "message": "ok?", "paths": [], "turn_id": "inner"}),
            ),
        ),
        (
            "subagent_compact_action",
            inner(
                json!({"kind": "compact_action", "strategy": "Snip", "messages_before": 9, "messages_after": 3, "turn_id": "inner", "dropped_rounds": 1, "dropped_messages": 6, "estimated_tokens_saved": null}),
            ),
        ),
        (
            "subagent_system_init",
            inner(json!({"kind": "system_init", "scene": "code", "tools": [], "mcp_servers": []})),
        ),
        (
            "subagent_turn_complete",
            inner(
                json!({"kind": "turn_complete", "stop_reason": "end_turn", "api_calls": 1, "tool_calls": 0, "usage": usage, "turn_id": "inner", "seq": 2}),
            ),
        ),
        (
            "subagent_nested",
            inner(inner(
                json!({"kind": "text_delta", "text": "deep", "turn_id": "inner"}),
            )),
        ),
        (
            "subagent_bare",
            json!({"kind": "subagent_progress", "agent_label": "general#00", "agent_session_id": "child", "agent_type": null, "parent_session_id": "", "parent_turn": 0, "event": {"kind": "text_delta", "text": "x", "turn_id": "inner"}}),
        ),
    ]
}

fn json_of<T: serde::Serialize>(frame: &T) -> Value {
    serde_json::to_value(frame).expect("a frame serializes")
}

fn frame(turn_id: Option<&str>, event: SessionEvent) -> SessionFrame {
    SessionFrame {
        session_id: Arc::from(SID),
        turn_id: turn_id.map(Arc::from),
        event,
    }
}

fn on_wire(frame: &SessionFrame) -> Value {
    SessionEventNotification::of(frame)
        .map(|n| json_of(&n))
        .unwrap_or(Value::Null)
}

fn in_turn(event: &AgentEvent) -> Value {
    on_wire(&frame(Some(TURN), SessionEvent::Agent(event.clone())))
}

fn between_turns(event: &AgentEvent) -> Value {
    if !emitted_outside_turns(event) {
        return Value::Null;
    }
    on_wire(&frame(None, SessionEvent::Agent(event.clone())))
}

fn daemon_frames() -> Vec<(&'static str, Value)> {
    let task = base::context::TaskInfo {
        task_id: "a1".into(),
        kind: base::context::TaskType::Agent,
        status: base::context::TaskStatus::Running,
        started_ms: 1,
        ended_ms: None,
        detail: json!({}),
        error: None,
        waiting_for: Vec::new(),
        usage: None,
        structured_output: None,
    };
    let task_event = |event: fn(base::context::TaskInfo) -> base::context::TaskEvent| {
        on_wire(&frame(None, SessionEvent::Task(event(task.clone()))))
    };
    let progress = base::context::TaskEvent::Progress {
        task_id: "a1".into(),
        appended: "out".into(),
        skipped_bytes: 0,
        from_byte: 0,
        next_byte: 3,
    };
    let goal = base::context::goal::GoalView {
        goal: base::context::goal::Goal {
            id: "g1".into(),
            revision: 1,
            objective: "ship".into(),
            phase: base::context::goal::GoalPhase::Active,
            max_rounds: 256,
            rounds_started: 0,
            rounds_since_armed: 0,
            blocked_reason: None,
        },
        armed: true,
    };
    vec![
        (
            "engine_stopped",
            on_wire(&frame(Some(TURN), SessionEvent::EngineStopped)),
        ),
        (
            "session_closed",
            on_wire(&frame(Some(TURN), SessionEvent::Closed)),
        ),
        (
            "task_started",
            task_event(|task| base::context::TaskEvent::Started { task }),
        ),
        (
            "task_updated",
            task_event(|task| base::context::TaskEvent::Updated { task }),
        ),
        (
            "task_finished",
            task_event(|task| base::context::TaskEvent::Finished { task }),
        ),
        (
            "task_progress",
            on_wire(&frame(None, SessionEvent::Task(progress))),
        ),
        (
            "goal_changed",
            on_wire(&frame(None, SessionEvent::GoalChanged(Some(goal)))),
        ),
        (
            "goal_cleared",
            on_wire(&frame(None, SessionEvent::GoalChanged(None))),
        ),
        (
            "extension_entry",
            on_wire(&frame(
                None,
                SessionEvent::ExtensionEntry(
                    json!({"ns": "host", "event": "note", "payload": {"a": 1}, "seq": 3}),
                ),
            )),
        ),
        (
            "subscription_dropped",
            json_of(&SessionEventNotification::subscription_dropped(SID)),
        ),
    ]
}

fn render() -> Value {
    let agent: Vec<Value> = agent_samples()
        .into_iter()
        .map(|(case, sample)| {
            let event: AgentEvent =
                serde_json::from_value(sample.clone()).unwrap_or_else(|e| panic!("{case}: {e}"));
            json!({
                "case": case,
                "event": sample,
                "in_turn": in_turn(&event),
                "between_turns": between_turns(&event),
            })
        })
        .collect();
    let daemon: Vec<Value> = daemon_frames()
        .into_iter()
        .map(|(case, frame)| json!({"case": case, "frame": frame}))
        .collect();
    json!({"agent": agent, "daemon": daemon})
}

#[test]
fn every_session_frame_keeps_its_wire_shape() {
    let rendered = render();
    if std::env::var_os("AGIX_UPDATE_GOLDEN").is_some() {
        let text = serde_json::to_string_pretty(&rendered).unwrap() + "\n";
        std::fs::write(golden_path(), text).unwrap();
        return;
    }
    let golden: Value =
        serde_json::from_str(&std::fs::read_to_string(golden_path()).unwrap()).unwrap();
    for section in ["agent", "daemon"] {
        let want = golden[section].as_array().unwrap();
        let got = rendered[section].as_array().unwrap();
        assert_eq!(want.len(), got.len(), "{section}: case count");
        for (want, got) in want.iter().zip(got) {
            assert_eq!(want, got, "{section}/{}", want["case"]);
        }
    }
}

#[test]
fn the_listed_kinds_are_exactly_the_ones_the_samples_produce() {
    let rendered = render();
    let mut produced: Vec<String> = Vec::new();
    let mut collect = |frame: &Value| {
        let mut event = &frame["params"]["event"];
        while !event.is_null() {
            produced.push(event["kind"].as_str().unwrap().to_string());
            event = &event["event"];
        }
    };
    for case in rendered["agent"].as_array().unwrap() {
        collect(&case["in_turn"]);
        collect(&case["between_turns"]);
    }
    for case in rendered["daemon"].as_array().unwrap() {
        collect(&case["frame"]);
    }
    produced.sort();
    produced.dedup();
    let mut listed: Vec<String> = SessionEventPayload::KINDS
        .iter()
        .map(|k| k.to_string())
        .collect();
    listed.sort();
    assert_eq!(produced, listed);
}

#[test]
fn a_sub_agents_event_is_projected_like_the_parents_and_dropped_when_the_parent_would_drop_it() {
    let sample = |inner: Value| -> AgentEvent {
        serde_json::from_value(json!({
            "kind": "subagent_progress", "agent_label": "explore#1", "agent_session_id": "child",
            "agent_type": null, "parent_session_id": SID, "parent_turn": 1, "event": inner,
        }))
        .unwrap()
    };
    let wrapped = in_turn(&sample(json!({
        "kind": "compact_action", "strategy": "Snip", "messages_before": 9, "messages_after": 3,
        "turn_id": "inner", "dropped_rounds": null, "dropped_messages": null,
        "estimated_tokens_saved": null,
    })));
    let inner = &wrapped["params"]["event"]["event"];
    assert_eq!(inner["kind"], "compact", "{wrapped}");
    assert!(inner.get("turn_id").is_none(), "{wrapped}");

    let dropped = in_turn(&sample(json!({"kind": "system", "message": "x"})));
    assert!(dropped.is_null(), "{dropped}");
}

#[test]
fn a_whole_progress_figure_is_an_integer_and_a_fractional_one_keeps_its_fraction() {
    let progress = |value: f64| -> AgentEvent {
        serde_json::from_value(json!({
            "kind": "tool_progress", "tool_use_id": "toolu_1", "name": "Bash", "message": "m",
            "progress": value, "dropped": 0, "turn_id": "inner",
        }))
        .unwrap()
    };
    let whole = in_turn(&progress(2.0));
    assert!(whole["params"]["event"]["progress"].is_u64(), "{whole}");
    let fraction = in_turn(&progress(0.25));
    assert_eq!(fraction["params"]["event"]["progress"].as_f64(), Some(0.25));
}
