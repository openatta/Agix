//! Permission wrappers a delegate runs under: deny-rules-first, the parent-bridged
//! `ParentPermission` that forwards questions to the parent session, and `NoPrompt`.

use super::*;

/// A permission with a host's `deny` rules in front: what they refuse is
/// refused, anything else is decided as it would have been.
///
/// The rules sit in a deny-only gate, so its only verdicts of its own are
/// the denials; its mode never decides anything here.
pub(super) struct DenyRulesFirst {
    pub(super) rules: permissions::rule_set_permission::RuleSetPermission,
    pub(super) inner: Arc<dyn Permission>,
}

#[async_trait]
impl Permission for DenyRulesFirst {
    async fn check(
        &self,
        tool_name: &str,
        tool_input: &Value,
        cwd: &std::path::Path,
        session_id: &str,
    ) -> base::interface::permission::PermissionOutcome {
        use base::interface::permission::PermissionOutcome;
        match self
            .rules
            .check(tool_name, tool_input, cwd, session_id)
            .await
        {
            denied @ PermissionOutcome::Deny { .. } => denied,
            _ => {
                self.inner
                    .check(tool_name, tool_input, cwd, session_id)
                    .await
            }
        }
    }
    async fn check_for_delegate(
        &self,
        tool_name: &str,
        tool_input: &Value,
        cwd: &std::path::Path,
        session_id: &str,
        delegate: &Arc<base::context::SessionState>,
    ) -> base::interface::permission::PermissionOutcome {
        use base::interface::permission::PermissionOutcome;
        match self
            .rules
            .check(tool_name, tool_input, cwd, session_id)
            .await
        {
            denied @ PermissionOutcome::Deny { .. } => denied,
            _ => {
                self.inner
                    .check_for_delegate(tool_name, tool_input, cwd, session_id, delegate)
                    .await
            }
        }
    }
    fn bind_tool_registry(&self, tools: Arc<dyn base::tool::ToolRegistry>) {
        self.rules.bind_tool_registry(tools.clone());
        self.inner.bind_tool_registry(tools);
    }
    fn bind_session_state(&self, session: Arc<base::context::SessionState>) {
        self.inner.bind_session_state(session);
    }
    fn add_temporary_allow(&self, tool_name: &str, rule_content: Option<&str>) {
        self.inner.add_temporary_allow(tool_name, rule_content);
    }
    fn clear_temporary_allows(&self) {
        self.inner.clear_temporary_allows();
    }
    fn add_persistent_allow(&self, tool_name: &str, rule_content: Option<&str>) {
        self.inner.add_persistent_allow(tool_name, rule_content);
    }
}

/// A sub-agent's `Permission`: the parent's rules, plus the parent's channel
/// to a human.
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
///
/// With no channel wired (`event_tx`/`pending` are `None` — tests, embedders
/// that never call the setters), it refuses with an explanation rather than
/// blocking forever.
pub(super) struct ParentPermission {
    pub(super) inner: Arc<dyn Permission>,
    pub(super) event_tx: Option<runtime::agent::EventSender>,
    pub(super) pending: Option<runtime::agent::PendingPermissions>,
    /// How long a question waits for its answer; `0` waits indefinitely.
    pub(super) timeout_secs: u64,
    /// The sub-agent's own tools, for the input checks and the mode held here
    /// on top of the parent's verdict.
    pub(super) own_tools: std::sync::RwLock<Option<Arc<dyn base::tool::ToolRegistry>>>,
    pub(super) task: Option<Arc<base::context::RunningTask>>,
    /// The sub-agent's own session. When nobody attends it, a question is
    /// handed back for the turn to refuse, not sent up to wait for an answer.
    pub(super) session: std::sync::RwLock<Option<Arc<base::context::SessionState>>>,
}

/// The parent's permission as a sub-agent a program started sees it: nobody
/// watches a program run, so a call that would need the user's approval is
/// refused rather than asked about.
pub(super) struct NoPrompt(pub(super) Arc<dyn Permission>);

pub(super) fn refuse_a_question(
    tool_name: &str,
    outcome: base::interface::permission::PermissionOutcome,
) -> base::interface::permission::PermissionOutcome {
    use base::interface::permission::PermissionOutcome;
    match outcome {
        PermissionOutcome::Prompt { message, .. } => PermissionOutcome::Deny {
            reason: format!(
                "`{tool_name}` needs the user's approval, which a sub-agent started from a \
                 program cannot ask for. Leave this step out of the program and run it \
                 directly. Approval asked: {message}"
            ),
        },
        other => other,
    }
}

#[async_trait]
impl Permission for NoPrompt {
    async fn check(
        &self,
        tool_name: &str,
        tool_input: &Value,
        cwd: &std::path::Path,
        session_id: &str,
    ) -> base::interface::permission::PermissionOutcome {
        refuse_a_question(
            tool_name,
            self.0.check(tool_name, tool_input, cwd, session_id).await,
        )
    }
    async fn check_for_delegate(
        &self,
        tool_name: &str,
        tool_input: &Value,
        cwd: &std::path::Path,
        session_id: &str,
        delegate: &Arc<base::context::SessionState>,
    ) -> base::interface::permission::PermissionOutcome {
        refuse_a_question(
            tool_name,
            self.0
                .check_for_delegate(tool_name, tool_input, cwd, session_id, delegate)
                .await,
        )
    }
    fn bind_tool_registry(&self, tools: Arc<dyn base::tool::ToolRegistry>) {
        self.0.bind_tool_registry(tools);
    }
    fn bind_session_state(&self, session: Arc<base::context::SessionState>) {
        self.0.bind_session_state(session);
    }
    fn add_temporary_allow(&self, tool_name: &str, rule_content: Option<&str>) {
        self.0.add_temporary_allow(tool_name, rule_content);
    }
    fn clear_temporary_allows(&self) {
        self.0.clear_temporary_allows();
    }
    fn add_persistent_allow(&self, tool_name: &str, rule_content: Option<&str>) {
        self.0.add_persistent_allow(tool_name, rule_content);
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

/// What `mode` refuses on its own, for a parent handler that can only judge
/// in its own session's mode (the default `check_for_delegate`). A rule-set
/// parent has already refused all of this, and more precisely.
pub(super) fn held_to_mode(
    outcome: base::interface::permission::PermissionOutcome,
    mode: base::permission::PermissionMode,
    tool: Option<&dyn base::tool::Tool>,
    input: &Value,
) -> base::interface::permission::PermissionOutcome {
    use base::interface::permission::PermissionOutcome;
    use base::permission::PermissionMode;
    match (mode, outcome) {
        (_, denied @ PermissionOutcome::Deny { .. }) => denied,
        (PermissionMode::Plan, _)
            if tool.is_some_and(|t| {
                !t.is_read_only(input) && !t.self_allow_overrides_mode() && !t.leaves_plan_mode()
            }) =>
        {
            PermissionOutcome::Deny {
                reason: "plan mode forbids non-readonly tools".into(),
            }
        }
        (PermissionMode::DontAsk, PermissionOutcome::Prompt { message, .. }) => {
            PermissionOutcome::Deny {
                reason: format!("dontAsk mode: {message}"),
            }
        }
        (_, outcome) => outcome,
    }
}

#[async_trait]
impl Permission for ParentPermission {
    async fn check(
        &self,
        tool_name: &str,
        tool_input: &Value,
        cwd: &std::path::Path,
        session_id: &str,
    ) -> base::interface::permission::PermissionOutcome {
        self.decide(tool_name, tool_input, cwd, session_id, None)
            .await
    }

    async fn check_for_delegate(
        &self,
        tool_name: &str,
        tool_input: &Value,
        cwd: &std::path::Path,
        session_id: &str,
        delegate: &Arc<base::context::SessionState>,
    ) -> base::interface::permission::PermissionOutcome {
        self.decide(tool_name, tool_input, cwd, session_id, Some(delegate))
            .await
    }

    fn bind_tool_registry(&self, tools: Arc<dyn base::tool::ToolRegistry>) {
        *self
            .own_tools
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(tools);
    }

    fn bind_session_state(&self, session: Arc<base::context::SessionState>) {
        *self
            .session
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(session);
    }
}

impl ParentPermission {
    /// The parent's verdict on a call of the sub-agent's — or, as `delegate`,
    /// of the sub-agent's own delegate — judged against the caller's session
    /// (its mode, what it has read), then asked of the host when it is a
    /// question.
    pub(super) async fn decide(
        &self,
        tool_name: &str,
        tool_input: &Value,
        cwd: &std::path::Path,
        session_id: &str,
        delegate: Option<&Arc<base::context::SessionState>>,
    ) -> base::interface::permission::PermissionOutcome {
        use base::interface::permission::PermissionOutcome;
        let own = self
            .own_tools
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .and_then(|tools| tools.get(tool_name));
        let session = self
            .session
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let judged = delegate.cloned().or_else(|| session.clone());
        if let (Some(tool), None) = (&own, delegate) {
            let exec = judged
                .as_ref()
                .and_then(|judged| judged.exec())
                .unwrap_or_else(platform::local_providers);
            let mut ctx = ToolContext::from_engine_ctx_with(
                cwd.to_path_buf(),
                tokio_util::sync::CancellationToken::new(),
                exec,
            );
            if let Some(judged) = &judged {
                ctx.session = judged.clone();
            }
            if let base::tool::ValidationResult::Err(reason, _) =
                tool.validate_input(tool_input, &ctx).await
            {
                return PermissionOutcome::Deny { reason };
            }
        }
        let outcome = match &judged {
            Some(judged) => {
                let mode = session.as_ref().map_or(judged.permission_mode(), |own| {
                    own.permission_mode().for_delegate(judged.permission_mode())
                });
                held_to_mode(
                    self.inner
                        .check_for_delegate(tool_name, tool_input, cwd, session_id, judged)
                        .await,
                    mode,
                    own.as_deref(),
                    tool_input,
                )
            }
            None => {
                self.inner
                    .check(tool_name, tool_input, cwd, session_id)
                    .await
            }
        };
        if session.as_ref().is_some_and(|s| s.unattended) {
            return outcome;
        }
        let PermissionOutcome::Prompt {
            prompt_id,
            message,
            paths,
            ..
        } = outcome
        else {
            return outcome;
        };

        let (Some(event_tx), Some(pending)) = (self.event_tx.as_ref(), self.pending.as_ref())
        else {
            return PermissionOutcome::Deny {
                reason: format!(
                    "`{tool_name}` needs interactive approval, and this sub-agent has no channel \
                     to ask on. Approve it in the parent session (or run this step there) and \
                     retry. Original prompt: {message}"
                ),
            };
        };

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
            .send(base::interface::event::AgentEvent::PermissionPrompt {
                prompt_id: prompt_id.clone(),
                tool_name: tool_name.to_string(),
                message: format!("[sub-agent {session_id}] {message}"),
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
                reason: format!("`{tool_name}`: the parent session closed before it could answer"),
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
                        reason: format!(
                            "`{tool_name}`: no answer to the permission prompt within {secs}s"
                        ),
                    };
                }
            },
        };
        match answer {
            Ok(runtime::agent::PermissionDecision::Permit) => PermissionOutcome::Permit,
            Ok(runtime::agent::PermissionDecision::PermitAlways { .. }) => {
                // Persist on the *parent's* handler, which is where the rule
                // engine actually lives, for what was asked — this command,
                // this path — as the parent's own answer is. A tool not in
                // the sub-agent's registry has no content to scope it by, and
                // is permitted this once.
                if let Some(tool) = &own {
                    self.inner.add_persistent_allow(
                        tool_name,
                        tool.permission_match_content(tool_input).as_deref(),
                    );
                }
                PermissionOutcome::Permit
            }
            Ok(runtime::agent::PermissionDecision::Deny { reason }) => {
                PermissionOutcome::Deny { reason }
            }
            // Sender dropped without an answer — fail closed, same as the
            // parent's own path does.
            Err(_) => PermissionOutcome::Deny {
                reason: format!("`{tool_name}`: permission request closed without an answer"),
            },
        }
    }
}

pub(super) struct AlwaysPermit;
#[async_trait]
impl Permission for AlwaysPermit {
    async fn check(
        &self,
        _tn: &str,
        _i: &Value,
        _c: &std::path::Path,
        _s: &str,
    ) -> base::interface::permission::PermissionOutcome {
        base::interface::permission::PermissionOutcome::Permit
    }
}
