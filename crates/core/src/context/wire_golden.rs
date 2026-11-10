//! 任务事件和目标视图的线上形状：`session.tasks`、`task_*`、`goal_changed`、
//! `session.goal` 都原样转发这里产生的值，客户端按这份形状解析。

use super::goal::{GoalAction, TurnOrigin};
use super::*;
use serde_json::{json, Value};
use std::path::PathBuf;

fn wire<T: serde::Serialize>(value: &T) -> Value {
    let mut value = serde_json::to_value(value).unwrap();
    scrub(&mut value);
    value
}

/// Clock readings and generated ids differ run to run; their presence and
/// type are what the shape promises.
fn scrub(value: &mut Value) {
    match value {
        Value::Object(fields) => {
            for (key, field) in fields.iter_mut() {
                match key.as_str() {
                    "started_ms" | "ended_ms" if field.is_i64() => *field = json!("<ms>"),
                    "id" if field.is_string() => *field = json!("<id>"),
                    _ => scrub(field),
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(scrub),
        _ => {}
    }
}

fn drain<T: Clone>(rx: &mut tokio::sync::broadcast::Receiver<T>) -> Vec<T> {
    std::iter::from_fn(|| rx.try_recv().ok()).collect()
}

fn zero_usage() -> Value {
    json!({
        "input_tokens": 0,
        "output_tokens": 0,
        "cache_creation_input_tokens": 0,
        "cache_read_input_tokens": 0,
    })
}

#[test]
fn a_sub_agent_task_s_events_keep_their_shape() {
    let s = SessionState::new(PathBuf::from("/tmp"));
    let mut rx = s.subscribe_tasks();
    let task = s.register_task(
        "ag-1",
        TaskType::Agent,
        tokio_util::sync::CancellationToken::new(),
        json!({"description": "look around", "subagent_type": "explore"}),
    );
    task.begin_waiting("p-1");
    task.add_usage(crate::interface::model::Usage {
        input_tokens: 3,
        output_tokens: 4,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: 1,
    });
    task.end_waiting("p-1");
    let _ = task.structured_output.set(json!({"answer": 42}));
    s.finish_task("ag-1", RunningStatus::Failed("boom".into()));

    let events: Vec<Value> = drain(&mut rx).iter().map(wire).collect();
    let detail = json!({"description": "look around", "subagent_type": "explore"});
    let used = json!({
        "input_tokens": 3,
        "output_tokens": 4,
        "cache_creation_input_tokens": 0,
        "cache_read_input_tokens": 1,
    });
    assert_eq!(
        events,
        [
            json!({"kind": "task_started", "task": {
                "task_id": "ag-1", "kind": "agent", "status": "running",
                "started_ms": "<ms>", "ended_ms": null, "detail": detail,
                "usage": zero_usage(),
            }}),
            json!({"kind": "task_updated", "task": {
                "task_id": "ag-1", "kind": "agent", "status": "waiting",
                "started_ms": "<ms>", "ended_ms": null, "detail": detail,
                "waiting_for": ["p-1"], "usage": zero_usage(),
            }}),
            json!({"kind": "task_updated", "task": {
                "task_id": "ag-1", "kind": "agent", "status": "waiting",
                "started_ms": "<ms>", "ended_ms": null, "detail": detail,
                "waiting_for": ["p-1"], "usage": used,
            }}),
            json!({"kind": "task_updated", "task": {
                "task_id": "ag-1", "kind": "agent", "status": "running",
                "started_ms": "<ms>", "ended_ms": null, "detail": detail,
                "usage": used,
            }}),
            json!({"kind": "task_finished", "task": {
                "task_id": "ag-1", "kind": "agent", "status": "failed",
                "started_ms": "<ms>", "ended_ms": "<ms>", "detail": detail,
                "error": "boom", "usage": used, "structured_output": {"answer": 42},
            }}),
        ]
    );
    assert_eq!(wire(&s.task_infos()), json!([events[4]["task"]]));
}

#[test]
fn a_streamed_task_s_progress_keeps_its_shape() {
    let (events, mut rx) = tokio::sync::broadcast::channel(64);
    let task = RunningTask::new("sh-1".into(), TaskType::Shell);
    task.report_to(events);
    task.announced
        .store(true, std::sync::atomic::Ordering::SeqCst);
    task.append_output("one\n");
    task.report_progress();
    task.append_output("two\n");
    task.close_output();

    let progress: Vec<Value> = drain(&mut rx).iter().map(wire).collect();
    assert_eq!(
        progress,
        [
            json!({"kind": "task_progress", "task_id": "sh-1", "skipped_bytes": 0,
                   "appended": "one\n", "from_byte": 0, "next_byte": 4}),
            json!({"kind": "task_progress", "task_id": "sh-1", "skipped_bytes": 0,
                   "appended": "two\n", "from_byte": 4, "next_byte": 8}),
        ]
    );
    assert_eq!(
        wire(&task.info()),
        json!({"task_id": "sh-1", "kind": "shell", "status": "running",
               "started_ms": "<ms>", "ended_ms": null, "detail": null}),
        "a shell command has no usage"
    );
}

#[test]
fn every_status_and_kind_keeps_its_name() {
    let s = SessionState::new(PathBuf::from("/tmp"));
    let cases = [
        (TaskType::Teammate, "tm-1", Some(RunningStatus::Completed)),
        (TaskType::CronJob, "cj-1", Some(RunningStatus::Cancelled)),
        (TaskType::Monitor, "mo-1", None),
        (TaskType::Code, "cd-1", None),
    ];
    for (task_type, id, status) in cases {
        let task = s.register_task(
            id,
            task_type,
            tokio_util::sync::CancellationToken::new(),
            Value::Null,
        );
        if let Some(status) = status {
            s.finish_task(id, status);
        }
        let _ = task;
    }
    let shapes: Vec<(Value, Value)> = s
        .task_infos()
        .iter()
        .map(|info| {
            let info = wire(info);
            (info["kind"].clone(), info["status"].clone())
        })
        .collect();
    let mut shapes: Vec<String> = shapes.iter().map(|(k, st)| format!("{k}/{st}")).collect();
    shapes.sort();
    assert_eq!(
        shapes,
        [
            r#""code"/"running""#,
            r#""cron"/"stopped""#,
            r#""monitor"/"running""#,
            r#""teammate"/"done""#,
        ]
    );
}

#[test]
fn the_goal_view_keeps_its_shape() {
    let s = SessionState::new(PathBuf::from("/tmp"));
    let mut rx = s.subscribe_goal();
    assert_eq!(wire(&s.goal_view()), Value::Null);

    let created = s
        .goal_create("ship it".into(), Some(5), TurnOrigin::Human)
        .unwrap();
    let active = json!({
        "id": "<id>", "revision": 1, "objective": "ship it", "phase": "active",
        "max_rounds": 5, "rounds_started": 0, "rounds_since_armed": 0, "armed": true,
    });
    assert_eq!(wire(&created), active);
    assert_eq!(wire(&s.goal_view()), active);

    let goal_id = serde_json::to_value(s.goal_view()).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let blocked = s
        .goal_update(
            &goal_id,
            1,
            GoalAction::Blocked {
                reason: "no key".into(),
            },
            TurnOrigin::Human,
        )
        .unwrap();
    let blocked_view = wire(&blocked);
    assert_eq!(blocked_view["phase"], "blocked");
    assert_eq!(blocked_view["blocked_reason"], "no key");
    assert_eq!(blocked_view["armed"], false);
    let mut keys: Vec<&String> = blocked_view.as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(
        keys,
        [
            "armed",
            "blocked_reason",
            "id",
            "max_rounds",
            "objective",
            "phase",
            "revision",
            "rounds_since_armed",
            "rounds_started",
        ]
    );

    s.goal_clear(TurnOrigin::Human).unwrap();
    let sent: Vec<Value> = drain(&mut rx).iter().map(wire).collect();
    assert_eq!(sent, [active, blocked_view, Value::Null]);
}
