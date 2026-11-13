//! 一次回合的入口与收尾：目标变化落盘、输入分发、用户提示拦截与钩子、排队消息投递、回合结束。

use super::attachments::describe_attachments;
use super::request::sync_mcp_adapters;
use super::token_budget::{find_token_budget_directive, strip_token_budget_directive};
use super::tool_exec::resolve_permission_response;
use super::{TurnError, TurnOutcome, ENDS_A_GOAL_ROUND_NORMALLY};
use crate::agent::{Agent, EngineCommand, InputMessage};
use base::event::AgentEvent;
use base::interface::model::{MessageRole, ModelContentBlock, ModelMessage, Usage};
use base::interface::prompt_middleware::{PromptDecision, PromptDelivery};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// A permission mode as hooks see it: the name settings files use.
pub(super) enum PromptScreen {
    Pass(String),
    Blocked(Option<String>),
}

pub(super) fn running_task_summary(task: &base::context::RunningTask) -> String {
    let kind = task.task_type.as_str();
    let detail = task.detail.lock().unwrap_or_else(|e| e.into_inner());
    match detail.get("description").and_then(|d| d.as_str()) {
        Some(description) if !description.is_empty() => format!("{kind}: {description}"),
        _ => kind.to_string(),
    }
}

pub(crate) fn hook_permission_mode(mode: base::permission::PermissionMode) -> String {
    serde_json::to_value(mode)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "default".into())
}

impl Agent {
    /// Write the goal changes the session log does not have yet.
    pub(crate) async fn record_goal_changes(&mut self) {
        let unrecorded = self.session_state.unrecorded_goal_changes().await;
        for goal in &unrecorded.changes {
            let payload = serde_json::json!({ "goal": goal });
            if let Err(e) = self
                .session
                .record_extension(
                    base::context::goal::GOAL_NS,
                    base::context::goal::GOAL_EVENT,
                    payload,
                )
                .await
            {
                tracing::warn!(error = %e, "failed to record a goal change");
            }
        }
    }

    /// Write the part of the session state that outlives the engine, when it
    /// changed since the log last had it.
    pub(crate) async fn record_session_state(&mut self) {
        let Some(state) = self.session_state.unrecorded_durable_state() else {
            return;
        };
        let Ok(payload) = serde_json::to_value(&state) else {
            return;
        };
        if let Err(e) = self
            .session
            .record_extension(
                base::context::SESSION_STATE_NS,
                base::context::SESSION_STATE_EVENT,
                payload,
            )
            .await
        {
            tracing::warn!(error = %e, "failed to record the session state");
        }
    }

    /// Process one input message — the core turn loop.
    /// Internal turn processing. Consumers should use [`Agent::run`] or [`Agent::run_turn`].
    pub(crate) async fn process_turn(
        &mut self,
        msg: InputMessage,
        cancel: CancellationToken,
    ) -> Result<TurnOutcome, TurnError> {
        match msg {
            InputMessage::User {
                content,
                attachments,
                turn_id,
            } => {
                self.session_state
                    .set_turn_origin(base::context::goal::TurnOrigin::from_turn_id(&turn_id));
                self.current_turn_id = turn_id;
                self.session.set_turn_id(Some(self.current_turn_id.clone()));
                let _ = self.channels.event_tx.send(AgentEvent::TurnStart {
                    turn_id: self.current_turn_id.clone(),
                });
                if let Some(sc) = crate::commands::parse_slash_command(&content) {
                    self.record_slash_command(&sc);
                    // `/mcp__<server>__<prompt>` → `prompts/get` on the owning
                    // server; the returned messages become this turn's
                    // content.
                    //
                    // Resolved off the registry when the prompt was registered
                    // there (the normal path — `Builder::build`), otherwise
                    // straight off the live `McpManager`. The fallback is what
                    // keeps this working for a registry built once and shared
                    // across sessions (`Builder::commands_override`, the
                    // daemon's path): that catalog predates this session's MCP
                    // connections, and it also lets prompts picked up by a
                    // later `refresh_prompts` be invocable immediately.
                    let mcp_command: Option<String> = match self.tooling.commands.resolve(&sc.name)
                    {
                        Some(crate::commands::Command::McpPrompt { command, .. }) => Some(command),
                        Some(_) => None,
                        None => self
                            .tooling
                            .mcp
                            .has_prompt_command(&sc.name)
                            .then(|| sc.name.clone()),
                    };
                    if let Some(command) = mcp_command {
                        // Argument mapping + validation and every failure mode
                        // (unreachable server, prompt handler error, bad
                        // arguments) are handled inside
                        // `invoke_prompt_command`, which always yields text —
                        // a broken MCP server must not fail the user's turn,
                        // it must produce something the model can read and
                        // explain to the user.
                        let invocation = self
                            .tooling
                            .mcp
                            .invoke_prompt_command(&command, &sc.args)
                            .await;
                        if invocation.is_error {
                            tracing::warn!(
                                command = %command,
                                "MCP prompt command failed; injecting the failure as text"
                            );
                        }
                        return self
                            .run_user_turn(
                                invocation.text,
                                attachments,
                                cancel,
                                history::entry::MessageOrigin::Command,
                            )
                            .await;
                    }

                    if let Some(cmd) = self.tooling.commands.resolve(&sc.name) {
                        match cmd {
                            crate::commands::Command::Prompt { entry } => {
                                let ctx = self.skill_invocation_context(cancel.clone());
                                let expansion = self
                                    .tooling
                                    .commands
                                    .run_prompt_command(
                                        &entry,
                                        &sc,
                                        &ctx,
                                        self.gate.permission.policy.as_ref(),
                                    )
                                    .await;
                                self.slash_skill_grants = expansion.grants;
                                return self
                                    .run_user_turn(
                                        expansion.text,
                                        attachments,
                                        cancel,
                                        history::entry::MessageOrigin::Command,
                                    )
                                    .await;
                            }
                            crate::commands::Command::McpPrompt { .. } => {
                                unreachable!("MCP prompt commands are dispatched above")
                            }
                            crate::commands::Command::Local { .. } => {
                                let result_text = match sc.name.as_str() {
                                    "help" => self.handle_help_command(),
                                    "skills" => self.handle_skills_command(),
                                    "clear" => {
                                        self.handle_clear_command().await;
                                        "Session cleared. All messages removed.".into()
                                    }
                                    "compact" => match self.compact_now(&cancel).await {
                                        Some(c) => format!(
                                            "Compacted ({}): {} → {} messages, ~{} → ~{} tokens.",
                                            c.strategy,
                                            c.messages_before,
                                            c.messages_after,
                                            c.tokens_before,
                                            c.tokens_after
                                        ),
                                        None if cancel.is_cancelled() => {
                                            "Compaction interrupted; nothing changed.".into()
                                        }
                                        None => "Nothing to compact.".into(),
                                    },
                                    "cost" => self.handle_cost_command(),
                                    _ => format!("Unknown local command: {}", sc.name),
                                };
                                let _ = self.channels.event_tx.send(AgentEvent::TextDelta {
                                    text: result_text,
                                    turn_id: self.current_turn_id.clone(),
                                });
                                self.last_had_tool_uses = false;
                                let stop_reason = if cancel.is_cancelled() {
                                    "cancelled"
                                } else {
                                    "command_executed"
                                };
                                return self
                                    .finish_turn(Ok(TurnOutcome::stopped(stop_reason)))
                                    .await;
                            }
                        }
                    }
                    // Unknown slash command — pass through to LLM as-is
                }

                let typed_by_a_person = crate::commands::parse_slash_command(&content).is_none()
                    && !matches!(
                        base::context::goal::TurnOrigin::from_turn_id(&self.current_turn_id),
                        base::context::goal::TurnOrigin::GoalRound { .. }
                    );
                let directive = typed_by_a_person
                    .then(|| find_token_budget_directive(&content))
                    .flatten();
                let processed_content = if let Some((target, range)) = directive {
                    self.token_budget.target = Some(target);
                    self.token_budget.unannounced = true;
                    self.token_budget.accumulated = 0;
                    self.token_budget.continuations = 0;
                    tracing::info!(target, "Token budget directive parsed — set output target");
                    strip_token_budget_directive(&content, range)
                } else {
                    content
                };

                // Not a slash command → normal flow
                self.run_user_turn(
                    processed_content,
                    attachments,
                    cancel,
                    history::entry::MessageOrigin::User,
                )
                .await
            }
            InputMessage::ToolResult {
                tool_use_id,
                content,
                is_error,
                ..
            } => {
                self.handle_tool_result(tool_use_id, content, is_error)
                    .await
            }
            InputMessage::PermissionResponse {
                prompt_id,
                decision,
            } => {
                // Normally unreachable via `Agent::run`: its input
                // demultiplexer intercepts `PermissionResponse` *before*
                // dispatching here, which is the whole point — see that
                // function's comment. Kept for direct `process_turn` callers
                // (tests, embedders driving the engine themselves), sharing
                // one implementation so the two paths can't diverge.
                resolve_permission_response(
                    &self.gate.pending_permissions,
                    &self.gate.permission_denial_count,
                    prompt_id,
                    decision,
                );
                Ok(TurnOutcome::default())
            }
            InputMessage::QuestionResponse { prompt_id, answer } => {
                crate::elicitation::resolve_question_response(
                    &self.gate.pending_questions,
                    &prompt_id,
                    answer,
                );
                Ok(TurnOutcome::default())
            }
            InputMessage::Steer { ack, .. } => {
                if let Some(ack) = ack {
                    let _ = ack.send(crate::steering::SteerAck::NotRunning);
                }
                Ok(TurnOutcome::default())
            }
            InputMessage::Context { request, reply } => {
                let _ = reply.send(self.handle_context(request).await);
                Ok(TurnOutcome::default())
            }
            InputMessage::System { kind, content } => {
                match kind {
                    EngineCommand::Shutdown => Err(TurnError::Shutdown),
                    EngineCommand::CompactNow => {
                        self.compact_now(&cancel).await;
                        Ok(TurnOutcome::default())
                    }
                    EngineCommand::SetSessionId => {
                        self.session.set_session_id(content);
                        let _ = self.channels.event_tx.send(AgentEvent::SessionChanged {
                            session_id: self.session.session_id.clone(),
                        });
                        Ok(TurnOutcome::default())
                    }
                    // `content` is the model name. Each turn re-reads
                    // `settings.model.model_name` into its `effective_model`,
                    // so swapping it here is what makes a host's `/model`
                    // change the model the next turn calls.
                    EngineCommand::UpdateModel => {
                        let mut settings = (*self.settings).clone();
                        settings.values.model.model_name = content.clone();
                        self.settings = Arc::new(settings);
                        let _ = self.channels.event_tx.send(AgentEvent::System {
                            message: format!("model set to {content}"),
                        });
                        Ok(TurnOutcome::default())
                    }
                    EngineCommand::RefreshMcp => {
                        self.tooling.mcp.refresh_tools().await;
                        sync_mcp_adapters(
                            self.tooling.tools.as_ref(),
                            self.tooling.mcp.as_ref(),
                            |t| self.tooling.tool_policy.permits(t),
                        );
                        Ok(TurnOutcome::default())
                    }
                    // `CancelTurn` is handled by the input demultiplexer, not
                    // here — see its doc comment on `EngineCommand`.
                    EngineCommand::CancelTurn { .. } => Ok(TurnOutcome::default()),
                }
            }
        }
    }

    /// Fire a notification-only lifecycle hook, if any are configured.
    ///
    /// Every lifecycle event shares the same three ambient fields (session id,
    /// cwd, permission mode) and discards the response — none of them has a
    /// defined recovery path for a hook that wants to veto, so none of them
    /// offers one. `build` adds whatever the specific event carries.
    ///
    /// The `has_hooks_for` guard matters: `HookRunner::run` is an async call
    /// that ends up spawning processes, and these sites are on the hot path of
    /// every turn and every tool call.
    pub(crate) async fn fire_lifecycle_hook(
        &self,
        event: hooks::HookEvent,
        build: impl FnOnce(hooks::HookInput) -> hooks::HookInput,
    ) {
        if !self.gate.hooks.has_hooks_for(event) {
            return;
        }
        let input = build(hooks::HookInput::lifecycle(
            format!("{event:?}"),
            self.session.session_id.clone(),
            self.settings.paths.working_dir().display().to_string(),
            hook_permission_mode(self.session_state.permission_mode()),
        ));
        let _ = self.gate.hooks.run(event, &input).await;
    }

    /// Run a full turn from a user message.
    ///
    /// The body leaves through a dozen `return`s; persisting and announcing
    /// the end live here so that none of them can skip either. A cancelled
    /// or failed turn persists too — the user's message is the one worth
    /// retrying.
    pub(super) async fn run_user_turn(
        &mut self,
        content: String,
        attachments: Vec<crate::agent::Attachment>,
        cancel: CancellationToken,
        origin: history::entry::MessageOrigin,
    ) -> Result<TurnOutcome, TurnError> {
        self.turn = super::TurnState::default();
        self.session_state.take_structured_output();
        crate::steering::lock(&self.channels.steering).open();
        let outcome = self
            .run_user_turn_inner(content, attachments, cancel, origin)
            .await;
        let undelivered = crate::steering::lock(&self.channels.steering).close();
        if !undelivered.is_empty() {
            let _ = self
                .channels
                .event_tx
                .send(AgentEvent::UserMessagesReturned {
                    messages: undelivered,
                    turn_id: self.current_turn_id.clone(),
                });
        }
        self.finish_turn(outcome).await
    }

    /// Add the messages the user sent during this turn to the conversation.
    ///
    /// Called where the transcript ends on a complete step, so a delivered
    /// message never lands between a `tool_use` and its `tool_result`. At
    /// the turn's last boundary (`last`), finding nothing queued also closes
    /// the turn to further messages. Returns whether anything was delivered.
    ///
    /// Each message passes the same screening as one that opens a turn; a
    /// refused one is announced and left out, and the turn goes on.
    pub(super) async fn deliver_queued_user_messages(&mut self, last: bool) -> bool {
        let mut delivered = self.deliver_notices();
        // The turn goes on to answer the notices, so it must stay open to
        // messages too.
        let last = last && !delivered;
        loop {
            let queued = {
                let mut q = crate::steering::lock(&self.channels.steering);
                if last {
                    q.take_or_close()
                } else {
                    q.take()
                }
            };
            if queued.is_empty() {
                return delivered;
            }
            for m in queued {
                match self
                    .screen_user_prompt(m.content.clone(), &[], PromptDelivery::Steer)
                    .await
                {
                    PromptScreen::Pass(content) => {
                        self.session.push_message_from(
                            ModelMessage {
                                role: MessageRole::User,
                                content: vec![ModelContentBlock::Text {
                                    text: content.clone(),
                                }],
                            },
                            history::entry::MessageOrigin::Steer,
                        );
                        let _ = self
                            .channels
                            .event_tx
                            .send(AgentEvent::UserMessageDelivered {
                                message_id: m.message_id,
                                content,
                                turn_id: self.current_turn_id.clone(),
                            });
                        delivered = true;
                    }
                    PromptScreen::Blocked(message) => {
                        let _ = self.channels.event_tx.send(AgentEvent::UserMessageRefused {
                            message_id: m.message_id,
                            content: m.content,
                            message,
                            turn_id: self.current_turn_id.clone(),
                        });
                    }
                }
            }
            // At the last boundary a queue that only held refused messages is
            // still open; looking again is what closes it.
            if delivered || !last {
                return delivered;
            }
        }
    }

    /// [`base::context::SessionState::post_notice`]s, into the conversation.
    pub(super) fn deliver_notices(&mut self) -> bool {
        let notices = self.session_state.take_notices();
        for text in &notices {
            self.session.push_message_from(
                ModelMessage {
                    role: MessageRole::User,
                    content: vec![ModelContentBlock::Text { text: text.clone() }],
                },
                history::entry::MessageOrigin::Injected,
            );
        }
        !notices.is_empty()
    }

    /// `UserPromptSubmit` and then the prompt middlewares for one user
    /// message: what to put in the transcript, or why not to.
    pub(super) async fn screen_user_prompt(
        &self,
        content: String,
        attachments: &[crate::agent::Attachment],
        delivery: PromptDelivery,
    ) -> PromptScreen {
        let mut content = match self.run_prompt_hooks(content, delivery).await {
            PromptScreen::Pass(content) => content,
            blocked => return blocked,
        };
        let attachments = if self.prompt.prompt_middleware.is_empty() {
            Vec::new()
        } else {
            describe_attachments(attachments).await
        };
        for middleware in self.prompt.prompt_middleware.iter() {
            let call = base::interface::prompt_middleware::PromptCall {
                session_id: &self.session.session_id,
                turn_id: &self.current_turn_id,
                content: &content,
                delivery,
                permission_mode: self.session_state.permission_mode(),
            };
            match middleware
                .inspect_with_attachments(&call, &attachments)
                .await
            {
                PromptDecision::Continue => {}
                PromptDecision::Rewrite(rewritten) => content = rewritten,
                PromptDecision::Block { message } => {
                    tracing::info!(
                        delivery = delivery.as_str(),
                        reason = message.as_deref().unwrap_or("no reason given"),
                        "a prompt middleware blocked this message"
                    );
                    return PromptScreen::Blocked(message);
                }
            }
        }
        PromptScreen::Pass(content)
    }

    pub(super) async fn run_prompt_hooks(
        &self,
        content: String,
        delivery: PromptDelivery,
    ) -> PromptScreen {
        if !self
            .gate
            .hooks
            .has_hooks_for(hooks::HookEvent::UserPromptSubmit)
        {
            return PromptScreen::Pass(content);
        }
        let prompt_input = hooks::HookInput {
            hook_event_name: "UserPromptSubmit".into(),
            session_id: self.session.session_id.clone(),
            cwd: self.settings.paths.working_dir().display().to_string(),
            permission_mode: hook_permission_mode(self.session_state.permission_mode()),
            user_prompt: Some(content.clone()),
            delivery: Some(delivery.as_str().to_string()),
            ..Default::default()
        };
        let hook_result = self
            .gate
            .hooks
            .run(hooks::HookEvent::UserPromptSubmit, &prompt_input)
            .await;
        if let Some(response) = hook_result.blocked() {
            tracing::info!(
                delivery = delivery.as_str(),
                reason = response.message.as_deref().unwrap_or("no reason given"),
                "UserPromptSubmit hook blocked this message"
            );
            return PromptScreen::Blocked(response.message.clone());
        }
        match hook_result.updated_input().and_then(|v| v.as_str()) {
            Some(rewritten) => PromptScreen::Pass(rewritten.to_string()),
            None => PromptScreen::Pass(content),
        }
    }

    /// The one way a user turn ends: persist, then announce.
    ///
    /// The order is the contract a host builds on. A host treats
    /// `TurnComplete` as "the session is idle and its transcript is on disk"
    /// — the daemon hands the session to the next caller on it, and may
    /// rebuild it from that transcript straight away.
    pub(super) async fn finish_turn(
        &mut self,
        outcome: Result<TurnOutcome, TurnError>,
    ) -> Result<TurnOutcome, TurnError> {
        if let Err(e) = self.session.persist().await {
            tracing::warn!(error = %e, "failed to persist session");
        }
        let ended_normally = matches!(&outcome, Ok(o) if ENDS_A_GOAL_ROUND_NORMALLY.contains(&o.stop_reason.as_str()));
        if !ended_normally {
            self.session_state.goal_disarm();
        }
        self.record_goal_changes().await;
        self.record_session_state().await;
        if let Some(seat) = self.identity.team_seat.clone() {
            let stop_reason = match &outcome {
                Ok(o) => o.stop_reason.clone(),
                Err(_) => "error".into(),
            };
            if ended_normally {
                self.fire_lifecycle_hook(hooks::HookEvent::TaskCompleted, |i| {
                    i.with_team(&seat.team, &seat.member)
                        .with_turn_id(self.current_turn_id.clone())
                        .with_stop_reason(stop_reason.clone())
                })
                .await;
            }
            self.fire_lifecycle_hook(hooks::HookEvent::TeammateIdle, |i| {
                i.with_team(&seat.team, &seat.member)
                    .with_turn_id(self.current_turn_id.clone())
                    .with_stop_reason(stop_reason.clone())
            })
            .await;
        }
        crate::agent::lock_turn_cancel(&self.channels.current_turn_cancel)
            .ended(&self.current_turn_id);
        if let Ok(o) = &outcome {
            self.telemetry.spent.usage = self.telemetry.spent.usage.plus(o.total_usage);
            self.telemetry.spent.api_calls =
                self.telemetry.spent.api_calls.saturating_add(o.api_calls);
            self.telemetry.spent.tool_calls =
                self.telemetry.spent.tool_calls.saturating_add(o.tool_calls);
            let _ = self.channels.event_tx.send(AgentEvent::TurnComplete {
                stop_reason: o.stop_reason.clone(),
                api_calls: o.api_calls,
                tool_calls: o.tool_calls,
                usage: o.total_usage,
                turn_id: self.current_turn_id.clone(),
                message: o.message.clone(),
                last_call_usage: self.turn.last_call_usage.unwrap_or_default(),
                context: self.turn.last_call_usage.map(|u| self.context_usage(u)),
                structured_output: self.session_state.take_structured_output(),
                seq: self.session.messages.len() as u64,
                truncated: self.turn.truncated,
            });
        }
        outcome
    }

    pub(super) fn context_usage(&self, last_call: Usage) -> base::event::ContextUsage {
        base::event::ContextUsage {
            tokens: u64::from(last_call.input_tokens)
                + u64::from(last_call.cache_read_input_tokens)
                + u64::from(last_call.cache_creation_input_tokens),
            compact_threshold: self.context_budget().compact_threshold as u64,
            window: base::context::config::infer_context_window_tokens(
                &self.settings.model.model_name,
            ) as u64,
        }
    }
}
