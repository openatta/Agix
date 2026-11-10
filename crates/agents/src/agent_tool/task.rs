//! Background-task plumbing: ids, steer inbox, the `SubRun` result carrier, task
//! progress notes and completion notifications.

use super::*;

pub(super) fn bg_task_id() -> String {
    base::id::Id::new().to_string()
}

/// How a task that ran a sub-agent ended.
pub(super) fn finished(
    result: &Result<String, base::error::ToolError>,
    cancelled: bool,
) -> base::context::RunningStatus {
    match (result, cancelled) {
        (_, true) => base::context::RunningStatus::Cancelled,
        (Ok(_), false) => base::context::RunningStatus::Completed,
        (Err(e), false) => base::context::RunningStatus::Failed(e.to_string()),
    }
}

/// A sub-agent's [`base::context::TaskInbox`]: messages join its running turn
/// at the next step boundary, as a user's steering does.
#[derive(Debug)]
pub(super) struct SteerInbox(pub(super) runtime::steering::SteerHandle);

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

pub(super) fn open_inbox(task: Option<&base::context::RunningTask>, agent: &Agent) {
    if let Some(task) = task {
        let _ = task.inbox.set(Arc::new(SteerInbox(agent.steer_handle())));
    }
}

pub(super) const NOTIFIED_RESULT_BYTES: usize = 4 * 1024;

/// What a caller fixes about one sub-agent run besides its prompt and tools.
#[derive(Default)]
pub(crate) struct SubRun {
    /// The task the run's cost and structured output are reported on.
    pub(super) task: Option<Arc<base::context::RunningTask>>,
    /// The sidechain's id, when the caller has already handed it out.
    pub(super) session_id: Option<String>,
    /// Over the agent type's model.
    pub(super) model: Option<String>,
    pub(super) allowed_tools: Vec<String>,
    pub(super) disallowed_tools: Vec<String>,
    pub(super) deny_rules: Vec<base::permission::PermissionRule>,
    pub(super) output_schema: Option<Value>,
    /// A mode the caller asked for (a team's), held to the parent's.
    pub(super) permission_mode: Option<base::settings::PermissionMode>,
    /// A `TeamCreate` member, whose spawn and completion the team
    /// coordinator records with its stage.
    pub(super) team_member: bool,
    /// The team the run is a member of.
    pub(super) seat: Option<base::interface::agent_spawner::TeamSeat>,
}

impl SubRun {
    pub(super) fn for_task(task: &Arc<base::context::RunningTask>, session_id: &str) -> Self {
        Self {
            task: Some(task.clone()),
            session_id: Some(session_id.to_string()),
            ..Default::default()
        }
    }

    pub(super) fn permission(&self, base: PermissionSet) -> PermissionSet {
        if self.deny_rules.is_empty() {
            return base;
        }
        PermissionSet {
            policy: Arc::new(DenyRulesFirst {
                rules: permissions::rule_set_permission::RuleSetPermission::new(Arc::new(
                    permissions::gate::PermissionGate::deny_rules_only(
                        permissions::ruleset::RuleSet::new(self.deny_rules.clone()),
                    ),
                )),
                inner: base.policy,
            }),
            grants: base.grants,
        }
    }
}

/// What one of a sub-agent's events tells its task: a model call's cost, the
/// structured output it handed in.
pub(super) fn note_on_task(task: Option<&base::context::RunningTask>, event: &AgentEvent) {
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

/// Record how a background sub-agent's task ended and tell the agent that
/// started it. A task stopped through `TaskStop` / `session.stopTask` has
/// ended already and gets no notice: whoever stopped it knows. One stopped
/// along with its turn does, so the model is not left waiting for it.
pub(super) fn conclude_background_task(
    session: &base::context::SessionState,
    task: &base::context::RunningTask,
    result: &Result<String, base::error::ToolError>,
    cancelled: bool,
    notify_parent: bool,
) {
    if let Ok(text) = result {
        task.append_output(text);
    }
    // Posted before the task ends, so whoever wakes on its end and reads the
    // result (TaskWait, TaskOutput) finds the notice there to withdraw.
    if notify_parent && !task.has_ended() {
        session.post_notice(
            Some(task.task_id.clone()),
            task_notification(task, result, cancelled),
        );
    }
    session.finish_task(&task.task_id, finished(result, cancelled));
}

pub(super) fn task_notification(
    task: &base::context::RunningTask,
    result: &Result<String, base::error::ToolError>,
    cancelled: bool,
) -> String {
    let status = match (result, cancelled) {
        (_, true) => "stopped",
        (Ok(_), false) => "done",
        (Err(_), false) => "failed",
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
    if let Err(e) = result {
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

pub(super) fn bg_result(task_id: &str, status: &str) -> base::tool::ToolResult {
    base::tool::ToolResult {
        content: ToolResultContent::Text(format!(
            "background task spawned (task_id: {task_id}, status: {status})"
        )),
        is_error: false,
        structured_content: None,
        mcp_meta: None,
        new_messages: None,
    }
}
