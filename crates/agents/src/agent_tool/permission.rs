//! Permission a delegate runs under: deny-rules-first, the parent-bridged
//! `ParentPermission` that forwards questions to the parent session, `NoPrompt`,
//! and `ParentGrants`, which keeps a delegate's grants with its parent.

use super::*;
use base::interface::permission::{
    Basis, GrantError, GrantScope, PermissionGrants, PermissionOutcome, PermissionPolicy,
    PermissionRequest, Reason, Stage,
};

fn permitted(basis: Basis) -> PermissionOutcome {
    PermissionOutcome::Permit {
        basis,
        profile: Default::default(),
        obligations: Default::default(),
    }
}

/// A permission with a host's `deny` rules in front: what they refuse is
/// refused, anything else is decided as it would have been.
///
/// The rules sit in a deny-only gate, so its only verdicts of its own are
/// the denials; its mode never decides anything here.
pub(super) struct DenyRulesFirst {
    pub(super) rules: permissions::policy::Policy,
    pub(super) inner: Arc<dyn PermissionPolicy>,
}

#[async_trait]
impl PermissionPolicy for DenyRulesFirst {
    async fn decide(&self, req: &PermissionRequest<'_>) -> PermissionOutcome {
        match self.rules.decide(req).await {
            denied @ PermissionOutcome::Deny { .. } => denied,
            _ => self.inner.decide(req).await,
        }
    }
}

/// A sub-agent's policy: the parent's rules, plus the parent's channel to a
/// human.
///
/// Delegating the *decision* to the parent is what stops a session from
/// having its rules bypassed by spawning a sub-agent. But an `Ask` verdict
/// then has nowhere to go — a sub-agent has no host attached to it, and a
/// prompt on the sub-agent's own channel would stall until cancellation.
///
/// The parent does have a host. So forward the question to it: register the
/// prompt in the parent's own pending-permission registry and emit it on the
/// parent's event channel, which is the same pair `runtime::turn` uses for the
/// parent's own prompts — so `session.respondToPrompt` answers a sub-agent's
/// question with no new RPC and no new plumbing. The parent's input
/// demultiplexer intercepts `PermissionResponse` ahead of turn dispatch, so
/// this resolves even though the parent turn is blocked inside the `Agent`
/// tool call that spawned us.
pub(super) struct ParentPermission {
    pub(super) parent: Arc<dyn PermissionPolicy>,
    /// Where an "always allow" the parent's host gives a sub-agent's question
    /// is kept: with the parent, whose rules decide the sub-agent's calls.
    pub(super) parent_grants: Arc<dyn PermissionGrants>,
    /// The session `parent` belongs to; a sub-agent's call reaches it as a
    /// delegate's.
    pub(super) parent_session: Arc<base::context::SessionState>,
    pub(super) event_tx: runtime::agent::EventSender,
    pub(super) pending: runtime::agent::PendingPermissions,
    /// How long a question waits for its answer; `0` waits indefinitely.
    pub(super) timeout_secs: u64,
    pub(super) task: Option<Arc<base::context::RunningTask>>,
    /// The parent session's providers, for a check made before the
    /// sub-agent's own are bound.
    pub(super) parent_exec: base::interface::exec::ExecProviders,
}

/// A delegate's grants, kept by its parent: an answer the person gave lasts
/// as long there as it would have here. A turn's grant — a forked skill's
/// `allowed-tools` — is not passed up, and a delegate clears nothing of its
/// parent's.
pub(super) struct ParentGrants(pub(super) Arc<dyn PermissionGrants>);

#[async_trait]
impl PermissionGrants for ParentGrants {
    async fn grant(&self, grant: base::interface::permission::Grant) -> Result<(), GrantError> {
        match grant.scope {
            GrantScope::Turn => Err(GrantError::NotKept),
            _ => self.0.grant(grant).await,
        }
    }
    fn clear(&self, _: GrantScope) {}
}

/// The parent's policy as a sub-agent a program started sees it: nobody
/// watches a program run, so a call that would need the user's approval is
/// refused rather than asked about.
pub(super) struct NoPrompt(pub(super) Arc<dyn PermissionPolicy>);

pub(super) fn refuse_a_question(tool_name: &str, outcome: PermissionOutcome) -> PermissionOutcome {
    match outcome {
        PermissionOutcome::Prompt { message, .. } => PermissionOutcome::deny(
            Stage::Attendance,
            format!(
                "`{tool_name}` needs the user's approval, which a sub-agent started from a \
                 program cannot ask for. Leave this step out of the program and run it \
                 directly. Approval asked: {message}"
            ),
        ),
        other => other,
    }
}

#[async_trait]
impl PermissionPolicy for NoPrompt {
    async fn decide(&self, req: &PermissionRequest<'_>) -> PermissionOutcome {
        refuse_a_question(req.tool_name, self.0.decide(req).await)
    }
}

/// Holds a task as waiting on one question until dropped — answered, timed
/// out, or the sub-agent stopped while it waited.
pub(super) struct Waiting<'a> {
    pub(super) task: Option<&'a base::context::RunningTask>,
    pub(super) prompt_id: &'a str,
}

impl<'a> Waiting<'a> {
    pub(super) fn on(task: Option<&'a base::context::RunningTask>, prompt_id: &'a str) -> Self {
        if let Some(task) = task {
            task.begin_waiting(prompt_id);
        }
        Self { task, prompt_id }
    }
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        if let Some(task) = self.task {
            task.end_waiting(self.prompt_id);
        }
    }
}

/// What `mode` refuses on its own, for a parent policy that judges in its own
/// session's mode whatever `PermissionRequest::delegate` says. A rule-set
/// parent has already refused all of this, and more precisely.
pub(super) fn held_to_mode(
    outcome: PermissionOutcome,
    mode: base::permission::PermissionMode,
    tool: Option<&dyn base::tool::Tool>,
    input: &Value,
) -> PermissionOutcome {
    use base::permission::PermissionMode;
    match (mode, outcome) {
        (_, denied @ PermissionOutcome::Deny { .. }) => denied,
        (PermissionMode::Plan, _)
            if tool.is_some_and(|t| {
                let permission = t.spec().permission;
                !t.call_traits(input).read_only
                    && !permission.self_allow_overrides_mode
                    && !permission.leaves_plan_mode
            }) =>
        {
            PermissionOutcome::deny(Stage::PlanMode, "plan mode forbids non-readonly tools")
        }
        (PermissionMode::DontAsk, PermissionOutcome::Prompt { message, .. }) => {
            PermissionOutcome::deny(Stage::DontAsk, format!("dontAsk mode: {message}"))
        }
        (_, outcome) => outcome,
    }
}

#[async_trait]
impl PermissionPolicy for ParentPermission {
    /// The parent's verdict on a call of the sub-agent's — or, as
    /// `req.delegate`, of the sub-agent's own delegate — judged against the
    /// caller's session (its mode, what it has read), then asked of the host
    /// when it is a question.
    async fn decide(&self, req: &PermissionRequest<'_>) -> PermissionOutcome {
        let tool_name = req.tool_name;
        let judged = req.judged();
        if let (Some(tool), None) = (req.tool, req.delegate) {
            let exec = judged.exec().unwrap_or_else(|| self.parent_exec.clone());
            let mut ctx = ToolContext::from_engine_ctx_with(
                req.cwd.to_path_buf(),
                tokio_util::sync::CancellationToken::new(),
                exec,
            );
            ctx.session = judged.clone();
            if let base::tool::ValidationResult::Err(reason, _) =
                tool.validate_input(req.input, &ctx).await
            {
                return PermissionOutcome::deny(Stage::ToolRefusal, reason);
            }
        }
        let mode = req
            .session
            .permission_mode()
            .for_delegate(judged.permission_mode());
        let mut asked = req.for_delegate(judged);
        asked.session = &self.parent_session;
        let outcome = held_to_mode(
            self.parent.decide(&asked).await,
            mode,
            req.tool.map(|t| t.as_ref()),
            req.input,
        );
        if req.attendance == Some(base::interface::permission::Attendance::Unattended) {
            return outcome;
        }
        let PermissionOutcome::Prompt {
            prompt_id,
            message,
            paths,
            reason: asked,
            ..
        } = outcome
        else {
            return outcome;
        };

        let (event_tx, pending) = (&self.event_tx, &self.pending);
        let (tx, rx) = tokio::sync::oneshot::channel();
        pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(prompt_id.clone(), tx);
        let _waiting = Waiting::on(self.task.as_deref(), &prompt_id);
        // Prefixed so the host can tell a sub-agent's question apart from the
        // parent's own — the prompt itself is otherwise identical, which is
        // deliberate: it is answered through the identical path.
        if event_tx
            .send(base::event::AgentEvent::PermissionPrompt {
                prompt_id: prompt_id.clone(),
                tool_name: tool_name.to_string(),
                message: format!("[sub-agent {}] {message}", req.session_id),
                paths,
                turn_id: String::new(),
                tool_use_id: None,
                task_id: self.task.as_ref().map(|task| task.task_id.clone()),
            })
            .is_err()
        {
            // Parent channel gone (session tearing down): fail closed.
            pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&prompt_id);
            return PermissionOutcome::Deny {
                reason: Reason::new(
                    asked.stage,
                    format!(
                        "`{tool_name}`: the parent session closed before it could answer. \
                         Original prompt: {message}"
                    ),
                ),
            };
        }

        let answer = match self.timeout_secs {
            0 => rx.await,
            secs => match tokio::time::timeout(std::time::Duration::from_secs(secs), rx).await {
                Ok(answer) => answer,
                Err(_) => {
                    pending
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&prompt_id);
                    return PermissionOutcome::Deny {
                        reason: Reason::new(
                            asked.stage,
                            format!(
                                "`{tool_name}`: no answer to the permission prompt within {secs}s"
                            ),
                        ),
                    };
                }
            },
        };
        match answer {
            Ok(runtime::agent::PermissionDecision::Permit) => permitted(Basis::Person),
            Ok(runtime::agent::PermissionDecision::PermitAlways { .. }) => {
                // Kept by the parent, where the rule engine deciding this
                // sub-agent's calls lives, for what was asked — this command,
                // this path — as the parent's own answer is. A call with no
                // tool behind it has no content to scope it by, and is
                // permitted this once.
                if let Some(tool) = req.tool {
                    let grant = base::interface::permission::Grant::new(
                        tool_name,
                        tool.permission_match_content(req.input),
                        GrantScope::Session,
                    );
                    if let Err(e) = self.parent_grants.grant(grant).await {
                        tracing::debug!(tool_name, error = %e, "sub-agent permit-always not kept");
                    }
                }
                permitted(Basis::Person)
            }
            Ok(runtime::agent::PermissionDecision::Deny { reason }) => PermissionOutcome::Deny {
                reason: Reason::new(asked.stage, reason),
            },
            // Sender dropped without an answer — fail closed, same as the
            // parent's own path does.
            Err(_) => PermissionOutcome::Deny {
                reason: Reason::new(
                    asked.stage,
                    format!("`{tool_name}`: permission request closed without an answer"),
                ),
            },
        }
    }
}

#[cfg(test)]
pub(super) struct AlwaysPermit;
#[cfg(test)]
#[async_trait]
impl PermissionPolicy for AlwaysPermit {
    async fn decide(&self, _: &PermissionRequest<'_>) -> PermissionOutcome {
        permitted(Basis::Bypass)
    }
}
