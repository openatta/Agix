//! The one-shot modes: waited on, started in the background (the model's or a
//! host's), a team stage's member, a hook's judge. Each takes its slot, its
//! task lease and its permission by the same rules; only these differ.

use std::sync::Arc;

use base::context::{TaskLease, TaskType};
use base::interface::delegate::{
    AskRoute, DelegateEnd, DelegateError, DelegateOutcome, Delegated, StartedAgent,
};
use base::interface::permission::PermissionSet;
use tokio_util::sync::CancellationToken;

use super::Delegator;
use crate::agent_tool::{
    bg_task_id, conclude, task_notification, with_deny_rules, without_panics, OneShot, Spec,
};

/// Where the delegate's questions go, and the deny rules held in front of its
/// permission.
pub(super) struct Ask {
    pub(super) asks: AskRoute,
    pub(super) deny_rules: Vec<base::permission::PermissionRule>,
}

/// What a started delegate's task says about it.
pub(super) struct Started {
    pub(super) description: Option<String>,
    pub(super) tag: Option<String>,
    pub(super) notify_parent: bool,
}

impl Delegator {
    fn permission_for(
        &self,
        task: Option<Arc<base::context::RunningTask>>,
        ask: &Ask,
    ) -> PermissionSet {
        let can_ask = ask.asks == AskRoute::Host;
        with_deny_rules(
            self.permission_handler_asking(task, can_ask),
            &ask.deny_rules,
        )
    }

    fn lease(
        &self,
        task_id: String,
        task_type: TaskType,
        stop: &CancellationToken,
        detail: serde_json::Value,
    ) -> TaskLease {
        TaskLease::new(
            self.env.parent.state.clone(),
            task_id,
            task_type,
            stop.clone(),
            detail,
        )
    }

    fn finished(session_id: String, lease: TaskLease, text: String, end: DelegateEnd) -> Delegated {
        let structured = lease.task().structured_output.get().cloned();
        conclude(lease, &text, &end, false);
        Delegated::Finished(DelegateOutcome {
            text,
            end,
            structured,
            session_id,
        })
    }

    /// A program's delegate has nobody to ask, and a root session holds no
    /// slot: it waits for one instead of being refused.
    pub(super) async fn wait(
        &self,
        spec: Spec,
        prompt: String,
        description: Option<String>,
        stop: CancellationToken,
        ask: Ask,
    ) -> Result<Delegated, DelegateError> {
        let admission = match ask.asks {
            AskRoute::Refuse => self.limits.admit_waiting(&stop).await?,
            AskRoute::Host => self.limits.admit()?,
        };
        let tool_use_id = spec.called_from.as_ref().map(|c| c.tool_use_id.clone());
        let task_id = tool_use_id
            .clone()
            .filter(|id| !id.is_empty())
            .unwrap_or_else(bg_task_id);
        let detail = serde_json::json!({
            "agent_type": spec.agent_type,
            "description": description,
            "background": false,
            "tool_use_id": tool_use_id,
            "agent_session_id": spec.session_id,
        });
        let lease = self.lease(task_id, TaskType::Agent, &stop, detail);
        let task = lease.task().clone();
        let shot = OneShot {
            permission: self.permission_for(Some(task.clone()), &ask),
            spec,
            prompt,
            cancel: stop,
            task: Some(task),
            extra_tools: Vec::new(),
            background: false,
            stage: None,
        };
        let session_id = shot.spec.session_id.clone();
        let (text, end) = self.run_delegate(admission, shot).await;
        Ok(Self::finished(session_id, lease, text, end))
    }

    /// The model's background delegate, or a host's: a task of the parent
    /// session, started at once.
    pub(super) fn start(
        &self,
        spec: Spec,
        prompt: String,
        stop: CancellationToken,
        ask: Ask,
        started: Started,
    ) -> Result<Delegated, DelegateError> {
        let me = self.me()?;
        let admission = self.limits.admit()?;
        let mut detail = serde_json::json!({
            "agent_type": spec.agent_type,
            "description": started.description,
            "background": true,
            "agent_session_id": spec.session_id,
        });
        if let Some(call) = &spec.called_from {
            detail["tool_use_id"] = call.tool_use_id.clone().into();
        }
        if let Some(tag) = &started.tag {
            detail["tag"] = tag.clone().into();
        }
        let mut lease = self.lease(bg_task_id(), TaskType::Agent, &stop, detail);
        let notify_parent = started.notify_parent;
        if notify_parent {
            lease = lease.notifying(|task| task_notification(task, &DelegateEnd::Cancelled));
        }
        let task = lease.task().clone();
        let agent = StartedAgent {
            task_id: task.task_id.clone(),
            agent_session_id: spec.session_id.clone(),
        };
        let shot = OneShot {
            permission: self.permission_for(Some(task.clone()), &ask),
            spec,
            prompt,
            cancel: stop,
            task: Some(task),
            extra_tools: Vec::new(),
            background: true,
            stage: None,
        };
        tokio::spawn(async move {
            let (text, end) =
                without_panics(async move { me.run_delegate(admission, shot).await }).await;
            conclude(lease, &text, &end, notify_parent);
        });
        Ok(Delegated::Started(agent))
    }

    /// A member of one team stage. It takes no slot: `team_stage_concurrency`
    /// bounds a stage instead.
    pub(super) async fn stage_member(
        &self,
        spec: Spec,
        prompt: String,
        stop: CancellationToken,
        ask: Ask,
        stage: String,
        extra_tools: Vec<Arc<dyn base::tool::Tool>>,
    ) -> Result<Delegated, DelegateError> {
        let admission = self.limits.admit_unslotted()?;
        let seat = spec.seat.as_ref();
        let detail = serde_json::json!({
            "team": seat.map(|s| s.team.clone()),
            "member": seat.map(|s| s.member.clone()),
            "agent_session_id": spec.session_id,
        });
        let lease = self.lease(bg_task_id(), TaskType::Teammate, &stop, detail);
        let task = lease.task().clone();
        let shot = OneShot {
            permission: self.permission_for(Some(task.clone()), &ask),
            spec,
            prompt,
            cancel: stop,
            task: Some(task),
            extra_tools,
            background: false,
            stage: Some(stage),
        };
        let session_id = shot.spec.session_id.clone();
        let (text, end) = self.run_delegate(admission, shot).await;
        Ok(Self::finished(session_id, lease, text, end))
    }

    /// The agent behind a `"type": "agent"` hook on an event of
    /// `judged_session`. It takes none of the session's slots: those are the
    /// model's to fill, and a guard the model can crowd out by holding them
    /// all is no guard — its hook would fail and, failing open, let the call
    /// through. What bounds hook agents instead is that one's own events
    /// start none (`HookAgents`).
    pub(super) async fn judge(
        &self,
        judged_session: &str,
        spec: Spec,
        prompt: String,
        stop: CancellationToken,
    ) -> Result<Delegated, DelegateError> {
        if self.limits.hook_agents.includes(judged_session) {
            return Err(DelegateError::Invalid(
                "an agent hook does not judge the agent running for another agent hook".into(),
            ));
        }
        let admission = self.limits.admit_unslotted()?;
        let _running = self.limits.hook_agents.enter(spec.session_id.clone());
        let session_id = spec.session_id.clone();
        let shot = OneShot {
            permission: self.permission_handler(),
            spec,
            prompt,
            cancel: stop,
            task: None,
            extra_tools: Vec::new(),
            background: false,
            stage: None,
        };
        let (text, end) = self.run_delegate(admission, shot).await;
        Ok(Delegated::Finished(DelegateOutcome {
            text,
            end,
            structured: None,
            session_id,
        }))
    }
}
