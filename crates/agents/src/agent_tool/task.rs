//! Task plumbing for delegates: ids, the steer inbox, how a run's end reads
//! as a task status and as the notice its parent is posted.

use super::*;

pub(crate) fn bg_task_id() -> String {
    base::id::Id::new().to_string()
}

pub(crate) fn status_of(end: &DelegateEnd) -> base::context::RunningStatus {
    match end {
        DelegateEnd::Cancelled => base::context::RunningStatus::Cancelled,
        DelegateEnd::Failed(e) => base::context::RunningStatus::Failed(e.clone()),
        DelegateEnd::Completed | DelegateEnd::CutShort(_) => {
            base::context::RunningStatus::Completed
        }
    }
}

/// A sub-agent's [`base::context::TaskInbox`]: messages join its running turn
/// at the next step boundary, as a user's steering does.
#[derive(Debug)]
pub(crate) struct SteerInbox(pub(crate) runtime::steering::SteerHandle);

impl base::context::TaskInbox for SteerInbox {
    fn deliver(&self, message: String) -> Result<(), String> {
        match self.0.offer(base::id::Id::new().to_string(), message) {
            runtime::steering::SteerAck::Accepted => Ok(()),
            runtime::steering::SteerAck::NotRunning => {
                Err("the sub-agent is not in a turn that can take messages".into())
            }
        }
    }
}

pub(crate) fn open_inbox(task: Option<&base::context::RunningTask>, agent: &Agent) {
    if let Some(task) = task {
        let _ = task.inbox.set(Arc::new(SteerInbox(agent.steer_handle())));
    }
}

pub(crate) const NOTIFIED_RESULT_BYTES: usize = 4 * 1024;

/// What one of a sub-agent's events tells its task: a model call's cost, the
/// structured output it handed in.
pub(crate) fn note_on_task(task: Option<&base::context::RunningTask>, event: &AgentEvent) {
    let Some(task) = task else { return };
    match event {
        AgentEvent::StepEnd { usage, .. } => task.add_usage(*usage),
        AgentEvent::TurnComplete {
            structured_output: Some(output),
            ..
        } => {
            let _ = task.structured_output.set(output.clone());
        }
        _ => {}
    }
}

/// End `lease` as `end` says, with `text` as its output, telling the agent
/// that started it when `notify_parent`. A task stopped through `TaskStop` /
/// `session.stopTask` has ended already and gets no notice: whoever stopped
/// it knows. One stopped along with its turn does, so the model is not left
/// waiting for it.
pub(crate) fn conclude(
    lease: base::context::TaskLease,
    text: &str,
    end: &DelegateEnd,
    notify_parent: bool,
) {
    if !matches!(end, DelegateEnd::Failed(_)) {
        lease.task().append_output(text);
    }
    let notice = notify_parent.then(|| task_notification(lease.task(), end));
    lease.conclude(status_of(end), notice);
}

pub(crate) fn task_notification(task: &base::context::RunningTask, end: &DelegateEnd) -> String {
    let status = match end {
        DelegateEnd::Cancelled => "stopped",
        DelegateEnd::Failed(_) => "failed",
        DelegateEnd::Completed | DelegateEnd::CutShort(_) => "done",
    };
    let mut notice = format!(
        "<task-notification>\n<task-id>{}</task-id>\n<status>{status}</status>\n",
        task.task_id
    );
    let detail = task
        .detail
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    if let Some(d) = detail["description"].as_str() {
        notice.push_str(&format!("<description>{d}</description>\n"));
    }
    if let DelegateEnd::Failed(e) = end {
        notice.push_str(&format!("<error>{e}</error>\n"));
    }
    let tail = task.output_tail(NOTIFIED_RESULT_BYTES);
    if !tail.text.is_empty() {
        let more = if tail.from_byte > 0 {
            " (the start is cut; TaskOutput has what is kept)"
        } else {
            ""
        };
        notice.push_str(&format!("<result{more}>\n{}\n</result>\n", tail.text));
    }
    notice.push_str("</task-notification>");
    notice
}

pub(crate) fn bg_result(task_id: &str, status: &str) -> base::tool::ToolResult {
    base::tool::ToolResult::text(format!(
        "background task spawned (task_id: {task_id}, status: {status})"
    ))
}
