use super::*;

fn progress(tool_use_id: &str, progress: Option<f64>) -> AgentEvent {
    AgentEvent::ToolProgress {
        tool_use_id: tool_use_id.into(),
        name: "Bash".into(),
        message: "step 2".into(),
        progress,
        total: None,
        dropped: 7,
        turn_id: "inner-turn".into(),
    }
}

#[test]
fn a_tool_progress_frame_carries_the_call_and_what_was_merged_away() {
    let (frame, ask) = background_frame("s1", Some("t1"), &progress("toolu_1", Some(2.0)))
        .expect("tool progress is forwarded");
    assert!(ask.is_none());
    assert_eq!(frame.params["turn_id"], "t1");
    assert_eq!(
        frame.params["event"],
        serde_json::json!({
            "kind": "tool_progress", "tool_use_id": "toolu_1", "name": "Bash",
            "message": "step 2", "progress": 2.0, "dropped": 7
        })
    );
}

#[test]
fn a_sub_agents_tool_progress_is_wrapped_under_the_call_that_started_it() {
    let event = AgentEvent::SubagentProgress {
        agent_label: "general-purpose#1a2b3c4d".into(),
        agent_session_id: "child".into(),
        agent_type: Some("general-purpose".into()),
        parent_session_id: "s1".into(),
        parent_turn: 1,
        parent_tool_use_id: Some("toolu_agent".into()),
        task_id: None,
        event: Box::new(progress("toolu_inner", None)),
    };
    let (frame, _) = background_frame("s1", None, &event).expect("forwarded");
    let wrapped = &frame.params["event"];
    assert_eq!(wrapped["kind"], "subagent_progress");
    assert_eq!(wrapped["parent_tool_use_id"], "toolu_agent");
    let inner = &wrapped["event"];
    assert_eq!(inner["kind"], "tool_progress");
    assert_eq!(inner["tool_use_id"], "toolu_inner");
    assert_eq!(inner["dropped"], 7);
    assert!(inner.get("progress").is_none(), "absent, not null: {inner}");
}
