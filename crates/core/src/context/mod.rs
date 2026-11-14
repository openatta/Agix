//! 会话的配置与状态，按读写性质拆开，而不是一个什么都装的上下文对象：
//! - **只读配置** → `Arc<EngineConfig>`
//! - **可变运行期** → `Arc<SessionState>`（内部按需 split lock）

pub mod config;
pub mod goal;
pub mod ledger;
pub mod session;
pub mod task;

pub use self::config::*;
pub use self::ledger::*;
pub use self::session::*;
pub use self::task::*;

#[cfg(test)]
mod wire_golden;

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn deepseek_infers_1m_window() {
        assert_eq!(infer_context_window_tokens("deepseek-v4-flash"), 1_000_000);
        assert_eq!(infer_context_window_tokens("ds-v4-pro"), 1_000_000);
    }

    #[test]
    fn codex_infers_400k_window() {
        assert_eq!(infer_context_window_tokens("gpt-5.2-codex"), 400_000);
    }

    #[test]
    fn thresholds_follow_effective_window() {
        let auto = default_auto_compact_threshold("deepseek-v4-flash", 65535);
        let blocking = default_blocking_limit("deepseek-v4-flash", 65535);
        assert!(blocking > auto);
        assert!(auto > 900_000);
    }

    fn task_events(rx: &mut tokio::sync::broadcast::Receiver<TaskEvent>) -> Vec<serde_json::Value> {
        std::iter::from_fn(|| rx.try_recv().ok())
            .map(|e| serde_json::to_value(e).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn a_stopped_task_finishes_once_its_work_winds_down() {
        let s = SessionState::for_tests(PathBuf::from("/tmp"));
        let mut rx = s.subscribe_tasks();
        let task = s.register_task(
            "cd-1",
            TaskType::Code,
            tokio_util::sync::CancellationToken::new(),
            serde_json::Value::Null,
        );
        task.append_output("round 1\n");
        assert!(s.cancel_running_task("cd-1"));
        assert!(task.cancel.is_cancelled());
        assert_eq!(task.info().status, TaskStatus::Stopped);
        assert!(!s.cancel_running_task("cd-1"), "already stopped");
        let kinds: Vec<_> = task_events(&mut rx)
            .iter()
            .map(|e| e["kind"].clone())
            .collect();
        assert_eq!(kinds, ["task_started"], "not finished while it winds down");

        task.append_output("wrapped up\n");
        s.finish_task("cd-1", RunningStatus::Completed);
        task.append_output("too late\n");
        let events = task_events(&mut rx);
        assert_eq!(events.len(), 2, "{events:?}");
        assert_eq!(events[0]["kind"], "task_progress");
        assert_eq!(events[0]["appended"], "round 1\nwrapped up\n");
        assert_eq!(events[1]["kind"], "task_finished");
        assert_eq!(events[1]["task"]["status"], "stopped");
        let read = task.output_from(0).unwrap();
        assert_eq!(read.next_byte, events[0]["next_byte"].as_u64().unwrap());
        assert!(read.closed);
    }

    #[test]
    fn a_task_stopped_outside_a_runtime_finishes_at_once() {
        let s = SessionState::for_tests(PathBuf::from("/tmp"));
        let mut rx = s.subscribe_tasks();
        let task = s.register_task(
            "ag-1",
            TaskType::Agent,
            tokio_util::sync::CancellationToken::new(),
            serde_json::Value::Null,
        );
        assert!(s.cancel_running_task("ag-1"));
        assert!(task.output_closed());
        let kinds: Vec<_> = task_events(&mut rx)
            .iter()
            .map(|e| e["kind"].clone())
            .collect();
        assert_eq!(kinds, ["task_started", "task_finished"]);
    }
}
