//! 特征化测试：每种 `AgentEvent` 样本、以及 daemon 自己合成的会话帧，按现在的
//! 路径（turn 内的帧、两轮之间转发的帧）序列化出的线上 JSON，冻结在
//! `testdata/session_events.golden.json`。`AGIX_UPDATE_GOLDEN=1` 时重写该文件。

use super::*;
use serde_json::{json, Value};

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
            "turn_start",
            json!({"kind": "turn_start", "turn_id": "inner"}),
        ),
        (
            "step_start",
            json!({"kind": "step_start", "step": 0, "turn_id": "inner"}),
        ),
        (
            "step_end",
            json!({"kind": "step_end", "step": 0, "stop_reason": "tool_use", "usage": usage, "tool_calls": 1, "turn_id": "inner", "seq": 4}),
        ),
        (
            "turn_complete",
            json!({"kind": "turn_complete", "stop_reason": "end_turn", "api_calls": 2, "tool_calls": 1, "usage": usage, "turn_id": "inner", "message": "stopped by hook", "last_call_usage": usage, "context": {"tokens": 100, "compact_threshold": 800, "window": 1000}, "structured_output": {"ok": true}, "seq": 6, "truncated": true}),
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
            json!({"kind": "error", "code": "overloaded", "message": "try later", "turn_id": "inner"}),
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

fn in_turn_old(event: &AgentEvent) -> Value {
    if let Some((frame, _)) = background_frame(SID, Some(TURN), event) {
        return json_of(&frame);
    }
    match turn_event_body(event) {
        Some(body) => json_of(&StreamFrame::event(SID, TURN, body)),
        None => Value::Null,
    }
}

fn between_turns_old(event: &AgentEvent) -> Value {
    background_frame(SID, None, event)
        .map(|(frame, _)| json_of(&frame))
        .unwrap_or(Value::Null)
}

fn daemon_frames_old() -> Vec<(&'static str, Value)> {
    let task = json!({"task_id": "a1", "kind": "agent", "status": "running",
                      "started_ms": 1, "ended_ms": null, "detail": {}});
    let task_event = |kind: &str| {
        let event = json!({"kind": kind, "task": task});
        json_of(&StreamFrame::task_event(SID, &event).expect("a task frame"))
    };
    let progress = json!({"kind": "task_progress", "task_id": "a1", "appended": "out",
                          "skipped_bytes": 0, "from_byte": 0, "next_byte": 3});
    vec![
        (
            "engine_stopped",
            json_of(&StreamFrame::event(
                SID,
                TURN,
                json!({
                    "kind":"error","code":"engine_stopped",
                    "message":"engine stopped before the turn completed"
                }),
            )),
        ),
        (
            "session_closed",
            json_of(&StreamFrame::event(
                SID,
                TURN,
                json!({
                    "kind": "error", "code": SESSION_CLOSED, "message": SESSION_CLOSED_MESSAGE,
                }),
            )),
        ),
        ("task_started", task_event("task_started")),
        ("task_updated", task_event("task_updated")),
        ("task_finished", task_event("task_finished")),
        (
            "task_progress",
            json_of(&StreamFrame::task_event(SID, &progress).expect("a task frame")),
        ),
        (
            "goal_changed",
            json_of(&StreamFrame::goal_changed(
                SID,
                json!({"id": "g1", "objective": "ship", "status": "active"}),
            )),
        ),
        (
            "goal_cleared",
            json_of(&StreamFrame::goal_changed(SID, Value::Null)),
        ),
        (
            "extension_entry",
            json_of(&StreamFrame::extension_entry(
                SID,
                json!({"ns": "host", "event": "note", "payload": {"a": 1}, "seq": 3}),
            )),
        ),
        (
            "subscription_dropped",
            json_of(&crate::rpc::StreamFrame::subscription_dropped(SID)),
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
                "in_turn": in_turn_old(&event),
                "between_turns": between_turns_old(&event),
            })
        })
        .collect();
    let daemon: Vec<Value> = daemon_frames_old()
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
