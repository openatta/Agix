//! Turn loop — user message → streaming events → TurnOutcome.
//!
//! Core processing logic, over the agent's protocol-agnostic types.

use crate::agent::{Agent, EngineCommand, InputMessage};
use base::interface::event::AgentEvent;
use base::interface::memory::{DurableMemory, MemoryStore, MemoryType};
use base::interface::model::{
    MessageRole, ModelContentBlock, ModelMessage, ModelStream, ToolDef, Usage,
};
use base::interface::prompt::PromptBlock;
use base::interface::prompt_middleware::{PromptDecision, PromptDelivery};
use base::interface::scene::ScenePromptContext;
use base::tool::{ToolContext, ToolResultContent};
use std::borrow::Cow;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};

/// Empty hashset fallback when frozen context is not yet available.
static EMPTY: LazyLock<HashSet<String>> = LazyLock::new(HashSet::new);
use tokio_util::sync::CancellationToken;

/// Why a compaction runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompactTrigger {
    /// The context crossed the budget's threshold.
    Auto,
    /// Somebody asked: `/compact`, `EngineCommand::CompactNow`,
    /// [`Agent::compact_now`].
    Manual,
    /// The provider refused the request for its size.
    Recovery,
}

impl CompactTrigger {
    /// What `PreCompact` hooks are told. A size refusal is an automatic
    /// compaction as far as a hook is concerned; the two values are the ones
    /// hooks written for Claude Code match on.
    fn as_str(self) -> &'static str {
        match self {
            Self::Auto | Self::Recovery => "auto",
            Self::Manual => "manual",
        }
    }
}

/// Why a compaction left the conversation as it was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NotCompacted {
    /// A `PreCompact` hook declined it.
    Declined,
    /// Nothing could be dropped, or recovery would have cost what was.
    NothingToDrop,
    /// The compactor failed.
    Failed(String),
}

impl std::fmt::Display for NotCompacted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Declined => f.write_str("was declined by a PreCompact hook"),
            Self::NothingToDrop => f.write_str("saved nothing"),
            Self::Failed(e) => write!(f, "failed: {e}"),
        }
    }
}

/// What one compaction did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactSummary {
    pub strategy: String,
    pub messages_before: usize,
    pub messages_after: usize,
    pub tokens_before: usize,
    pub tokens_after: usize,
}

/// Turn endings that leave a goal armed. Anything else — a cancel, a hook
/// veto, an exhausted budget, an error — stops its automatic rounds.
const ENDS_A_GOAL_ROUND_NORMALLY: &[&str] = &[
    "end_turn",
    "stop_sequence",
    "max_tokens",
    "command_executed",
    "tool_result_received",
];

/// Turn endings after which an armed goal starts its next round.
pub const CONTINUES_A_GOAL: &[&str] = &["end_turn", "stop_sequence", "max_tokens"];

pub(crate) const SESSION_CONTEXT_OPENING: &str =
    "<system-reminder>\nAs you answer the user's questions, you can use the following context:\n";

/// Whether `messages` still hold the conversation's opening context message.
pub(crate) fn holds_session_context(messages: &[ModelMessage]) -> bool {
    messages.iter().any(|m| {
        m.role == MessageRole::User
            && m.content.iter().any(|block| {
                matches!(block, ModelContentBlock::Text { text } if text.starts_with(SESSION_CONTEXT_OPENING))
            })
    })
}

impl Agent {
    /// Write the goal changes the session log does not have yet.
    pub(crate) async fn record_goal_changes(&mut self) {
        for goal in self.session_state.take_unrecorded_goal_changes() {
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
                let _ = self.event_tx.send(AgentEvent::TurnStart {
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
                    let mcp_command: Option<String> = match self.commands.resolve(&sc.name) {
                        Some(crate::commands::Command::McpPrompt { command, .. }) => Some(command),
                        Some(_) => None,
                        None => self
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
                        let invocation = self.mcp.invoke_prompt_command(&command, &sc.args).await;
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

                    if let Some(cmd) = self.commands.resolve(&sc.name) {
                        match cmd {
                            crate::commands::Command::Prompt { entry } => {
                                let ctx = self.skill_invocation_context(cancel.clone());
                                let expansion = self
                                    .commands
                                    .run_prompt_command(&entry, &sc, &ctx, self.permission.as_ref())
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
                                    "compact" => match self.compact_now().await {
                                        Some(c) => format!(
                                            "Compacted ({}): {} → {} messages, ~{} → ~{} tokens.",
                                            c.strategy,
                                            c.messages_before,
                                            c.messages_after,
                                            c.tokens_before,
                                            c.tokens_after
                                        ),
                                        None => "Nothing to compact.".into(),
                                    },
                                    "cost" => self.handle_cost_command(),
                                    _ => format!("Unknown local command: {}", sc.name),
                                };
                                let _ = self.event_tx.send(AgentEvent::TextDelta {
                                    text: result_text,
                                    turn_id: self.current_turn_id.clone(),
                                });
                                self.last_had_tool_uses = false;
                                return self
                                    .finish_turn(Ok(TurnOutcome {
                                        stop_reason: "command_executed".into(),
                                        ..TurnOutcome::default()
                                    }))
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
                    self.output_token_target = Some(target);
                    self.output_token_target_unannounced = true;
                    self.accumulated_output_tokens = 0;
                    self.token_budget_continuation_count = 0;
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
                    &self.pending_permissions,
                    &self.permission_denial_count,
                    prompt_id,
                    decision,
                );
                Ok(TurnOutcome::default())
            }
            InputMessage::QuestionResponse { prompt_id, answer } => {
                crate::elicitation::resolve_question_response(
                    &self.pending_questions,
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
                        self.compact_now().await;
                        Ok(TurnOutcome::default())
                    }
                    EngineCommand::SetSessionId => {
                        self.session.set_session_id(content);
                        let _ = self.event_tx.send(AgentEvent::SessionChanged {
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
                        settings.model.model_name = content.clone();
                        self.settings = Arc::new(settings);
                        let _ = self.event_tx.send(AgentEvent::System {
                            message: format!("model set to {content}"),
                        });
                        Ok(TurnOutcome::default())
                    }
                    EngineCommand::RefreshMcp => {
                        self.mcp.refresh_tools().await;
                        sync_mcp_adapters(self.tools.as_ref(), self.mcp.as_ref(), |n| {
                            self.scene.permits_tool(n)
                        });
                        Ok(TurnOutcome::default())
                    }
                    // `CancelTurn` is handled by the input demultiplexer, not
                    // here — see its doc comment on `EngineCommand`.
                    EngineCommand::CancelTurn => Ok(TurnOutcome::default()),
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
        if !self.hooks.has_hooks_for(event) {
            return;
        }
        let input = build(hooks::HookInput::lifecycle(
            format!("{event:?}"),
            self.session.session_id.clone(),
            self.settings.paths.project_root().display().to_string(),
            hook_permission_mode(self.session_state.permission_mode()),
        ));
        let _ = self.hooks.run(event, &input).await;
    }

    /// Run a full turn from a user message.
    ///
    /// The body leaves through a dozen `return`s; persisting and announcing
    /// the end live here so that none of them can skip either. A cancelled
    /// or failed turn persists too — the user's message is the one worth
    /// retrying.
    async fn run_user_turn(
        &mut self,
        content: String,
        attachments: Vec<crate::agent::Attachment>,
        cancel: CancellationToken,
        origin: history::entry::MessageOrigin,
    ) -> Result<TurnOutcome, TurnError> {
        self.last_call_usage = None;
        self.turn_truncated = false;
        self.session_state.take_structured_output();
        crate::steering::lock(&self.steering).open();
        let outcome = self
            .run_user_turn_inner(content, attachments, cancel, origin)
            .await;
        let undelivered = crate::steering::lock(&self.steering).close();
        if !undelivered.is_empty() {
            let _ = self.event_tx.send(AgentEvent::UserMessagesReturned {
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
    async fn deliver_queued_user_messages(&mut self, last: bool) -> bool {
        let mut delivered = self.deliver_notices();
        // The turn goes on to answer the notices, so it must stay open to
        // messages too.
        let last = last && !delivered;
        loop {
            let queued = {
                let mut q = crate::steering::lock(&self.steering);
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
                        let _ = self.event_tx.send(AgentEvent::UserMessageDelivered {
                            message_id: m.message_id,
                            content,
                            turn_id: self.current_turn_id.clone(),
                        });
                        delivered = true;
                    }
                    PromptScreen::Blocked(message) => {
                        let _ = self.event_tx.send(AgentEvent::UserMessageRefused {
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
    fn deliver_notices(&mut self) -> bool {
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
    async fn screen_user_prompt(
        &self,
        content: String,
        attachments: &[crate::agent::Attachment],
        delivery: PromptDelivery,
    ) -> PromptScreen {
        let mut content = match self.run_prompt_hooks(content, delivery).await {
            PromptScreen::Pass(content) => content,
            blocked => return blocked,
        };
        let attachments = if self.prompt_middleware.is_empty() {
            Vec::new()
        } else {
            describe_attachments(attachments).await
        };
        for middleware in self.prompt_middleware.iter() {
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

    async fn run_prompt_hooks(&self, content: String, delivery: PromptDelivery) -> PromptScreen {
        if !self.hooks.has_hooks_for(hooks::HookEvent::UserPromptSubmit) {
            return PromptScreen::Pass(content);
        }
        let prompt_input = hooks::HookInput {
            hook_event_name: "UserPromptSubmit".into(),
            session_id: self.session.session_id.clone(),
            cwd: self.settings.paths.project_root().display().to_string(),
            permission_mode: hook_permission_mode(self.session_state.permission_mode()),
            user_prompt: Some(content.clone()),
            delivery: Some(delivery.as_str().to_string()),
            ..Default::default()
        };
        let hook_result = self
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
    async fn finish_turn(
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
        if let Some(seat) = self.team_seat.clone() {
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
        if let Ok(o) = &outcome {
            self.spent.usage = self.spent.usage.plus(o.total_usage);
            self.spent.api_calls = self.spent.api_calls.saturating_add(o.api_calls);
            self.spent.tool_calls = self.spent.tool_calls.saturating_add(o.tool_calls);
            let _ = self.event_tx.send(AgentEvent::TurnComplete {
                stop_reason: o.stop_reason.clone(),
                api_calls: o.api_calls,
                tool_calls: o.tool_calls,
                usage: o.total_usage,
                turn_id: self.current_turn_id.clone(),
                message: o.message.clone(),
                last_call_usage: self.last_call_usage.unwrap_or_default(),
                context: self.last_call_usage.map(|u| self.context_usage(u)),
                structured_output: self.session_state.take_structured_output(),
                seq: self.session.messages.len() as u64,
                truncated: self.turn_truncated,
            });
        }
        outcome
    }

    fn context_usage(&self, last_call: Usage) -> base::event::ContextUsage {
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

    async fn run_user_turn_inner(
        &mut self,
        mut content: String,
        attachments: Vec<crate::agent::Attachment>,
        cancel: CancellationToken,
        origin: history::entry::MessageOrigin,
    ) -> Result<TurnOutcome, TurnError> {
        // A script's budget is per turn, and this is where a turn begins.
        // Without this the counter only ever climbs, so the quota is a
        // per-session one wearing a per-turn name — and a script bound to a
        // point that fires per tool call goes quiet in the middle of a long
        // session with nothing to say why.
        for turn_start in self.turn_starts.iter() {
            turn_start();
        }

        // UserPromptSubmit: where a hook sees the message that opens a turn,
        // before any turn setup (skills rescan, FrozenContext,
        // CLAUDE.md injection, ...) has touched anything.
        //
        // Reuses the same `HookResponse` fields `PreToolUse`/`PostToolUse`
        // already use, same soft-fail philosophy (a hook that errors/times
        // out is treated as "ran and said nothing" — `blocked()`/
        // `updated_input()` just find nothing, turn proceeds unchanged; see
        // `HookRunner::run`, not special-cased per event):
        // - `decision: "block"` refuses to process this message at all —
        //   the turn ends immediately with `stopped_by_hook`, before a
        //   single model call, exactly like a `PreToolUse` block ends a
        //   tool call before it runs.
        // - `updated_input` (a JSON string) rewrites the message content
        //   that actually gets processed — the same field name
        //   `PreToolUse` uses for rewriting tool input, repurposed here for
        //   the one thing there is to rewrite: the prompt text itself.
        match self
            .screen_user_prompt(content, &attachments, PromptDelivery::Turn)
            .await
        {
            PromptScreen::Pass(screened) => content = screened,
            PromptScreen::Blocked(message) => {
                self.record_turn_complete("stopped_by_hook", 0, 0, std::time::Instant::now());
                return Ok(TurnOutcome {
                    stop_reason: "stopped_by_hook".into(),
                    message,
                    ..TurnOutcome::default()
                });
            }
        }

        // TurnStart — fires after UserPromptSubmit has had its chance to block
        // or rewrite, so a hook here sees the message that will actually be
        // processed. Notification only: the response is discarded.
        self.fire_lifecycle_hook(hooks::HookEvent::TurnStart, |input| {
            input.with_turn_id(self.current_turn_id.clone())
        })
        .await;

        // Same turn_no every `turn_complete` emitted below this point uses
        // (`self.session.turn_count` isn't incremented until the very end of
        // this function, see `increment_turn()`).
        let _ = self
            .telemetry_handle
            .record(telemetry::TelemetryEvent::turn_start(
                &self.session.session_id,
                self.session.turn_count,
                Some(self.current_turn_id.clone()),
                telemetry::TurnStartPayload {
                    resumed: false,
                    is_retry: false,
                },
            ));
        let _ = self
            .telemetry_handle
            .record(telemetry::TelemetryEvent::user_prompt_submit(
                &self.session.session_id,
                self.session.turn_count,
                Some(self.current_turn_id.clone()),
                telemetry::UserPromptSubmitPayload {
                    char_count: u32::try_from(content.chars().count()).unwrap_or(u32::MAX),
                    attachment_count: u32::try_from(attachments.len()).unwrap_or(u32::MAX),
                },
            ));

        // A skill's `allowed_tools` (see `SkillTool::call`) pre-approves
        // tools only for the rest of the turn it was invoked in — clear
        // whatever the *previous* turn injected before this new user
        // message starts a fresh one. No-op if nothing was injected, or on
        // `Permission` implementations without a backing rule engine.
        self.permission.clear_temporary_allows();
        for (tool, pattern) in std::mem::take(&mut self.slash_skill_grants) {
            self.permission
                .add_temporary_allow(&tool, pattern.as_deref());
        }

        let _timer = self.perf.start_timer("turn", "total");
        let mut api_calls: u32 = 0;
        let mut tool_calls: u32 = 0;
        let mut structured_output_calls: u32 = 0;

        let mut max_tokens_recovery: u32 = 0;
        let mut effective_max_tokens = self.settings.model.max_tokens;
        let mut effective_model = self.settings.model.model_name.clone();
        // What the turn has spent, kept once. The budget reads it through
        // `Spend` and a caller reads it whole; a second accumulator beside it
        // would let the two disagree about the cache.
        let mut turn_usage = Usage::default();
        let start = std::time::Instant::now();

        // The catalog may be shared with other sessions and swapped in place
        // by whoever watches the skill directories, so the diff is against
        // what this session was last shown, not against the catalog as it
        // stood a moment ago.
        let changed = self.skills.reload_changed();
        if changed > 0 {
            tracing::debug!(count = changed, "Skills reloaded from file changes");
        }
        let skills_now: std::collections::HashSet<String> =
            self.skills.list().iter().map(|s| s.name.clone()).collect();
        if let Some(skills_before) = self.announced_skills.replace(skills_now.clone()) {
            // Surface the diff as a system-reminder in the conversation itself,
            // not just a rebuilt static "## Available Skills" list — a model
            // mid-session tends to trust what it already said over a change
            // buried in a large prompt block it re-reads every turn without
            // necessarily re-attending to (observed directly: deepseek-v4-flash
            // kept asserting a just-deleted skill was still available even
            // with a "this list supersedes what you said earlier" disclaimer
            // right above the list; a pointed system-reminder about exactly
            // what changed is a much stronger, harder-to-ignore signal, the
            // same pattern already used for git-status/memory reminders).
            let mut removed: Vec<&String> = skills_before.difference(&skills_now).collect();
            let mut added: Vec<&String> = skills_now.difference(&skills_before).collect();
            removed.sort();
            added.sort();
            if !removed.is_empty() || !added.is_empty() {
                let mut note = String::from(
                    "<system-reminder>\nSkill availability just changed. This is \
                     authoritative — if you or the user said anything about these \
                     skills' availability earlier in this conversation, that is now \
                     outdated:\n",
                );
                for name in &removed {
                    note.push_str(&format!(
                        "- \"{name}\" was just removed — it no longer exists. Do not \
                         call it, and if asked whether it's available, say no.\n"
                    ));
                }
                for name in &added {
                    note.push_str(&format!(
                        "- \"{name}\" was just added — it's now available.\n"
                    ));
                }
                note.push_str("</system-reminder>");
                self.session.push_message(ModelMessage {
                    role: MessageRole::User,
                    content: vec![ModelContentBlock::Text { text: note }],
                });
                // The model is told above; the host is told here. Both need
                // it for the same reason — a skill list they were shown
                // earlier is now wrong — but a host cannot read the
                // conversation, and its slash-command view resolves through
                // the catalog that just changed underneath it.
                let _ = self.event_tx.send(AgentEvent::SkillsChanged {
                    added: added.iter().map(|n| (*n).clone()).collect(),
                    removed: removed.iter().map(|n| (*n).clone()).collect(),
                    turn_id: self.current_turn_id.clone(),
                });
            }
        }

        // Compute frozen context lazily on first turn. Includes git status,
        // branch, platform, etc.
        let collected_this_turn = self.frozen.is_none();
        let engine_git = engine_git_from(&self.config, &self.session.session_id);
        if collected_this_turn {
            let cwd = self.settings.paths.project_root();
            let paths = base::paths::ConfigPaths::from_settings(&self.settings.paths);
            self.frozen = Some(
                base::frozen::FrozenContext::collect(cwd, &paths, &*self.environment, &engine_git)
                    .await,
            );
        }

        // `FrozenContext` is a session snapshot on purpose — but the
        // reminder below re-injects `gitStatus` on *every* turn, and a stale
        // status is read by the model as current fact, not as a stale hint.
        // Commit between two turns and it keeps citing files that are no
        // longer dirty. So refresh precisely the fields that get re-shown
        // (see `refresh_git_status`); everything else stays frozen, which is
        // the point of the type. Skipped on the turn that just collected —
        // that snapshot is seconds old.
        if !collected_this_turn {
            if let Some(ref mut frozen) = self.frozen {
                frozen.refresh_git_status(&engine_git).await;
            }
        }

        if !self.session_context_injected {
            self.inject_session_context().await;
        }

        // Inject system-reminder: git status + memory summary, via the scene's
        // own `build_system_reminder` — each scene decides what belongs here.
        // Called once per
        // turn, and the git slice it reads was refreshed just above, so the
        // status shown is this turn's, not the session's opening snapshot.
        if let Some(ref frozen) = self.frozen {
            let ctx = base::interface::scene::ReminderContext {
                cwd: std::borrow::Cow::Owned(
                    self.settings
                        .paths
                        .project_root()
                        .to_string_lossy()
                        .into_owned(),
                ),
                git_status: frozen.git_status.as_deref().map(std::borrow::Cow::Borrowed),
                memory_summary: frozen
                    .memory_index
                    .as_deref()
                    .filter(|m| self.settings.memory_enabled && !m.is_empty())
                    .map(std::borrow::Cow::Borrowed),
            };
            let reminder = self.scene.build_system_reminder(&ctx);
            if !reminder.is_empty() {
                self.session.push_message(ModelMessage {
                    role: MessageRole::User,
                    content: vec![ModelContentBlock::Text { text: reminder }],
                });
            }
        }

        // Memory prefetch: fire LLM-based relevant memory selection as a
        // background task. The handle aborts it when dropped, so a recall that
        // misses its budget stops spending on the selection call.
        type Recall = (Vec<String>, Vec<base::interface::memory::DurableMemory>);
        let mut prefetch_handle: Option<tokio_util::task::AbortOnDropHandle<Recall>> =
            if !self.settings.memory_enabled {
                None
            } else {
                let store = self.memory_store.clone();
                let model = self.model.clone();
                let query = content.clone();
                let already_surfaced: HashSet<String> = self
                    .frozen
                    .as_ref()
                    .map(|f| f.already_surfaced.clone())
                    .unwrap_or_default();
                let recent_tools = self.tools.names();
                let model_name = self.settings.model.model_name.clone();
                let session_id = self.session.session_id.clone();
                let retriever = Arc::clone(&self.memory_retriever);
                let hooks = Arc::clone(&self.retrieval_hooks);
                Some(tokio_util::task::AbortOnDropHandle::new(tokio::spawn(
                    async move {
                        let memories = tokio::task::spawn_blocking(move || store.load_all())
                            .await
                            .unwrap_or_default();
                        // The retriever reads what was just loaded rather than
                        // the store, whose reads block.
                        let snapshot = base::interface::memory_contracts::InMemoryStorage::new(
                            memories.clone(),
                        );
                        let names = base::interface::memory_contracts::retrieve_with_hooks(
                            retriever.as_ref(),
                            &hooks,
                            &snapshot,
                            model.as_ref(),
                            base::interface::memory_contracts::RetrievalRequest {
                                query,
                                limit: 5,
                                already_surfaced,
                                recent_tools,
                                model_name,
                                session_id: Some(session_id),
                            },
                        )
                        .await;
                        (names, memories)
                    },
                )))
            };
        let prefetch_started_at: std::time::Instant = std::time::Instant::now();

        // Skill discovery prefetch: scan workspace for matching skills as a
        // background task. Discovery runs while the model streams and tools
        // execute; the result is consumed post-tool-execution alongside the
        // memory prefetch.
        let mut skill_prefetch: Option<
            tokio_util::task::AbortOnDropHandle<(Vec<base::frozen::SkillEntry>, Vec<String>)>,
        > = {
            let skills = self.skills.clone();
            let local_dir = self.settings.paths.local_data_dir.clone();
            let invoked_skills: Vec<String> = self.invoked_skills.clone();
            // Only scan when the previous turn produced tool calls that may
            // have written files that could reveal new skills — skip on
            // non-write turns. Runs on the first turn too (`last_had_tool_uses`
            // starts `true`).
            let should_run = self.last_had_tool_uses;
            if should_run {
                Some(tokio_util::task::AbortOnDropHandle::new(
                    tokio::task::spawn_blocking(move || {
                        let discovered = skills.discover_for_paths(&[local_dir]);
                        let new_names: Vec<String> = discovered
                            .iter()
                            .filter(|s| !invoked_skills.contains(&s.name))
                            .map(|s| s.name.clone())
                            .collect();
                        (discovered, new_names)
                    }),
                ))
            } else {
                None
            }
        };

        // Push user message. Recalled memories are injected right behind it,
        // below, so they are part of *this* turn's request.
        //
        // Attachments become additional blocks in this same message, after the
        // text, so the model reads the request and then what it refers to.
        // Images are only reachable this way — there is no other path by which
        // a user-supplied image enters the conversation.
        // `@server:scheme://path` references → MCP `resources/read`, inlined
        // into this same message so the model reads the request and the thing
        // it points at together. The matcher is deliberately narrow (see
        // `mcp::resources` for why: `@` is also emails, decorators, npm
        // scopes), and returns `None` — no work, no allocation — for the
        // overwhelmingly common message that contains no reference, or for
        // any session with no MCP servers connected at all.
        //
        // A reference that can't be resolved comes back as a visible error
        // block, never an error that fails the turn.
        let mut spans = vec![base::interface::model::InputSpan {
            source: base::interface::model::input_source::USER_PROMPT.into(),
            block: 0,
            range: Some((0, content.len())),
        }];
        if let Some(resources) = self.mcp.resolve_resource_refs(&content).await {
            let from = content.len();
            content.push_str(&resources);
            spans.push(base::interface::model::InputSpan {
                source: base::interface::model::input_source::MCP_RESOURCE.into(),
                block: 0,
                range: Some((from, content.len())),
            });
        }

        let mut user_blocks = vec![ModelContentBlock::Text { text: content }];
        user_blocks.extend(resolve_attachments(&attachments).await);
        for block in 1..user_blocks.len() {
            spans.push(base::interface::model::InputSpan {
                source: base::interface::model::input_source::ATTACHMENT.into(),
                block,
                range: None,
            });
        }
        let message = ModelMessage {
            role: MessageRole::User,
            content: user_blocks,
        };
        self.deliver_notices();
        self.pending_input = user_text_fingerprint(&message).map(|fp| (fp, spans));
        self.turn_started_at = Some(std::time::Instant::now());
        self.session.push_message_from(message, origin);
        if std::mem::take(&mut self.output_token_target_unannounced) {
            if let Some(target) = self.output_token_target {
                self.session.push_message(ModelMessage {
                    role: MessageRole::User,
                    content: vec![ModelContentBlock::Text {
                        text: format!(
                            "<system-reminder>\nThe user set an output token target of {target}. \
                             Your output token count is shown as you go and work continues \
                             automatically until the target is met. The target is a hard \
                             ceiling; let its size set how thorough to be.\n</system-reminder>"
                        ),
                    }],
                });
            }
        }

        // Collected before the first API call, not after tool execution, so a
        // turn the model answers without tools still sees its memories. The
        // wait is dead air before the user's first token, hence the short
        // latency budget. In the common case there is no wait at all —
        // `select_memories_from` returns without a model call when the
        // store holds no more candidates than the requested maximum.
        //
        // `already_surfaced` / `recall_count` are updated here and only here,
        // at the point the memories are actually placed in the transcript —
        // recording a memory as "shown to the model" when it was not
        // permanently suppresses a legitimate future surfacing of it.
        const MEMORY_RECALL_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);
        /// Per-entry cap on inlined memory content. Recall carries at most 5
        /// entries, so the whole injection stays bounded; the cap only exists
        /// so one oversized memory cannot dominate the turn.
        const MEMORY_RECALL_CONTENT_CAP: usize = 1024;
        if let Some(prefetch) = prefetch_handle.take() {
            let (prefetch_names, all) =
                match tokio::time::timeout(MEMORY_RECALL_BUDGET, prefetch).await {
                    Ok(Ok(recall)) => {
                        tracing::debug!(
                            count = recall.0.len(),
                            latency_ms = prefetch_started_at.elapsed().as_millis(),
                            "memory recall completed"
                        );
                        recall
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(error = %e, "memory recall task failed");
                        Default::default()
                    }
                    Err(_) => {
                        tracing::warn!(
                            budget_ms = MEMORY_RECALL_BUDGET.as_millis(),
                            "memory recall exceeded its latency budget"
                        );
                        Default::default()
                    }
                };
            let relevant: Vec<base::interface::memory::DurableMemory> = {
                let surfaced: &std::collections::HashSet<String> = self
                    .frozen
                    .as_ref()
                    .map(|f| &f.already_surfaced)
                    .unwrap_or(&EMPTY);
                prefetch_names
                    .iter()
                    .filter_map(|name| all.iter().find(|m| &m.name == name).cloned())
                    .filter(|m| !surfaced.contains(&m.name))
                    .take(5)
                    .collect()
            };
            if !relevant.is_empty() {
                // Mark injected memories as surfaced to avoid repeated injection.
                if let Some(ref mut frozen) = self.frozen {
                    for m in &relevant {
                        frozen.already_surfaced.insert(m.name.clone());
                    }
                }
                {
                    let store = self.memory_store.clone();
                    let names: Vec<String> = relevant.iter().map(|m| m.name.clone()).collect();
                    tokio::task::spawn_blocking(move || {
                        if let Err(e) = store.record_recalled(&names) {
                            tracing::debug!(error = %e, "recording a memory recall failed");
                        }
                    });
                }

                // Carry the memory body, not just its one-line description:
                // the description exists to decide relevance, and a model that
                // only gets the description has to spend a Read round trip to
                // learn anything. At ≤5 entries this is the cheaper option.
                let mut mem_text =
                    String::from("<system-reminder>\nRelevant memories for this query:\n");
                for m in &relevant {
                    mem_text.push_str(&format!("\n## {}\n{}\n", m.name, m.description));
                    let body = m.content.trim();
                    if !body.is_empty() {
                        let shown = truncate_at_char_boundary(body, MEMORY_RECALL_CONTENT_CAP);
                        mem_text.push_str(shown);
                        if shown.len() < body.len() {
                            mem_text.push_str(&format!(
                                "\n… (truncated — read {}.md for the full memory)",
                                m.name
                            ));
                        }
                        mem_text.push('\n');
                    }
                }
                mem_text
                    .push_str("\nUse these memories to inform your response.\n</system-reminder>");
                self.session.push_message(ModelMessage {
                    role: MessageRole::User,
                    content: vec![ModelContentBlock::Text { text: mem_text }],
                });
            }
        }

        let mut had_tool_uses_this_turn = false;
        let mut sent_back_by_stop_hook = false;

        loop {
            // Every step starts from a transcript that is on disk, so a
            // process that dies mid-turn loses at most the step in flight.
            // Here every tool_use already has its tool_result next to it.
            if let Err(e) = self.session.persist().await {
                tracing::warn!(error = %e, "failed to persist session");
            }
            self.record_goal_changes().await;
            if cancel.is_cancelled() {
                self.last_had_tool_uses = had_tool_uses_this_turn;
                self.record_turn_complete("cancelled", api_calls, tool_calls, start);
                return Ok(TurnOutcome {
                    stop_reason: "cancelled".into(),
                    api_calls,
                    tool_calls,
                    total_usage: turn_usage,
                    usage: Usage::default(),
                    message: None,
                });
            }
            if let base::interface::turn_policy::TurnStep::Stop { reason } = self
                .turn_policy
                .before_model_call(&base::interface::turn_policy::TurnProgress {
                    api_calls,
                    tool_calls,
                    structured_output_calls,
                    stop_reason: "",
                })
            {
                self.last_had_tool_uses = had_tool_uses_this_turn;
                self.record_turn_complete(&reason, api_calls, tool_calls, start);
                return Ok(TurnOutcome {
                    stop_reason: reason,
                    api_calls,
                    tool_calls,
                    total_usage: turn_usage,
                    usage: Usage::default(),
                    message: None,
                });
            }

            // 1. Compact if token budget exceeded
            self.compact_if_needed().await;

            // A ceiling compaction was unable to get under. Ending the turn is
            // the only honest answer left: the request that would go out next
            // is one the deployment said must never be sent.
            if let Some(cap) = self.context_budget().hard_cap {
                let carried = self.context_tokens();
                if carried > cap {
                    tracing::warn!(carried, cap, "context above the hard cap after compaction");
                    self.last_had_tool_uses = had_tool_uses_this_turn;
                    self.record_turn_complete("context_exceeded", api_calls, tool_calls, start);
                    return Ok(TurnOutcome {
                        stop_reason: "context_exceeded".into(),
                        api_calls,
                        tool_calls,
                        total_usage: turn_usage,
                        usage: Usage::default(),
                        message: None,
                    });
                }
            }

            // 2. Assemble the request
            let step = api_calls;
            api_calls += 1;
            let _ = self.event_tx.send(AgentEvent::StepStart {
                step,
                turn_id: self.current_turn_id.clone(),
            });
            let origin = Some(
                base::interface::model::CallOrigin::turn(
                    self.session.session_id.clone(),
                    self.session.turn_count,
                    step,
                )
                .with_lineage(
                    self.session.parent_session_id().map(str::to_string),
                    self.agent_type.clone(),
                ),
            );
            let request = self
                .prepare_request(&effective_model, effective_max_tokens, origin.clone())
                .await;
            // Kept back for the recovery paths, which retry the same tools and
            // (for an overload) the same conversation.
            let tool_defs = request.tool_defs.clone();
            let messages = request.messages.clone();
            // Counted before either is handed on: the call's own accounting
            // needs the size of what went out, and both get moved on the way.
            let sent_message_count = messages.len();
            let sent_tool_count = tool_defs.len();

            // 3. Call the model and 5. stream its reply, running tools as they
            //    arrive. 4. A call that fails before anything of it was kept
            //    goes to the recovery policy, once; the doing stays here,
            //    because compacting, rebuilding a request and sending it are
            //    things only this loop can sequence.
            let call_started = std::time::Instant::now();
            let tools = Arc::clone(&self.tools);
            let tools_for_safety = Arc::clone(&self.tools);
            let cwd = self.settings.paths.project_root();
            let local_settings_path = self.settings.paths.local_settings_file();
            let session_id = self.session.session_id.clone();
            let turn_no = self.session.turn_count;
            let th = self.telemetry_handle.clone();
            let tid = self.current_turn_id.clone();
            let cancel_for_exec = cancel.clone();
            let agent_depth_for_exec = self.agent_depth;
            let hooks_for_exec = Arc::clone(&self.hooks);
            let discontinued = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let discontinued_for_exec = Arc::clone(&discontinued);
            let events_for_exec = self.event_tx.clone();
            let config_for_exec = Arc::clone(&self.config);
            let permission_for_exec = Arc::clone(&self.permission);
            let session_state_for_exec = Arc::clone(&self.session_state);
            let elicitation_for_exec = Arc::clone(&self.elicitation);
            let tool_middleware_for_exec = Arc::clone(&self.tool_middleware);
            let result_transformers_for_exec = Arc::clone(&self.result_transformers);
            let tool_images: Arc<std::sync::Mutex<Vec<PendingToolImage>>> =
                Arc::new(std::sync::Mutex::new(Vec::new()));
            let tool_images_for_exec = Arc::clone(&tool_images);
            let execute_tool =
                move |tool_use_id: String, name: String, input: serde_json::Value| {
                    let exec_ctx = ToolExecCtx {
                        tool_use_id,
                        tools: Arc::clone(&tools),
                        cwd: cwd.clone(),
                        local_settings_path: local_settings_path.clone(),
                        session_id: session_id.clone(),
                        turn_no,
                        agent_depth: agent_depth_for_exec,
                        telemetry_handle: th.clone(),
                        turn_id: tid.clone(),
                        cancel: cancel_for_exec.clone(),
                        hooks: Arc::clone(&hooks_for_exec),
                        config: Arc::clone(&config_for_exec),
                        permission: Arc::clone(&permission_for_exec),
                        session_state: Arc::clone(&session_state_for_exec),
                        elicitation: Arc::clone(&elicitation_for_exec),
                        tool_middleware: Arc::clone(&tool_middleware_for_exec),
                        result_transformers: Arc::clone(&result_transformers_for_exec),
                        discontinued: Arc::clone(&discontinued_for_exec),
                        images: Arc::clone(&tool_images_for_exec),
                        from_code: false,
                        events: Some(events_for_exec.clone()),
                    };
                    async move { execute_tool_with_telemetry(&exec_ctx, &name, input).await }
                };
            let is_concurrency_safe = move |name: &str, input: &serde_json::Value| {
                tools_for_safety
                    .get(name)
                    .map(|t| t.is_concurrency_safe(input))
                    .unwrap_or(false)
            };
            let mut request = request;
            let mut recovered = false;
            let (stream_result, messages_before_step) = loop {
                let messages_before_step = self.session.messages.len();
                let attempt = match self.send(request, cancel.clone()).await {
                    Ok(stream) => {
                        crate::streaming::execute_stream(
                            stream,
                            &mut self.session,
                            &self.event_tx,
                            self.current_turn_id.clone(),
                            execute_tool.clone(),
                            is_concurrency_safe.clone(),
                            cancel.clone(),
                            &self.model_interceptors,
                            self.model.provenance(),
                        )
                        .await
                    }
                    Err(error) => Err(crate::streaming::StreamFailure::refused(error)),
                };
                let failure = match attempt {
                    Ok(result) => break (result, messages_before_step),
                    Err(failure) => failure,
                };
                self.record_api_error(&effective_model, &failure.error, call_started);
                if cancel.is_cancelled() {
                    break (
                        crate::streaming::StreamResult::cancelled(),
                        messages_before_step,
                    );
                }
                let recovery = if recovered || !failure.resendable {
                    base::interface::recovery_policy::Recovery::Fail
                } else {
                    self.classify_failure(&failure.error)
                };
                recovered = true;
                request = match recovery {
                    base::interface::recovery_policy::Recovery::Fail => {
                        return Err(TurnError::Model(format!(
                            "failed to stream model response: {}",
                            failure.error
                        )))
                    }
                    base::interface::recovery_policy::Recovery::RetryWith { model } => {
                        tracing::warn!(
                            from = %effective_model,
                            to = %model,
                            error = %failure.error,
                            "retrying against a different model"
                        );
                        effective_model = model;
                        // `fallback_model: None` — this retry *is* the
                        // fallback, so there is nothing further to fall back to.
                        self.prepare_retry(
                            &effective_model,
                            effective_max_tokens,
                            tool_defs.clone(),
                            messages.clone(),
                            None,
                            origin.clone(),
                        )
                        .await
                    }
                    base::interface::recovery_policy::Recovery::CompactAndRetry => {
                        self.compact_for_retry(&failure.error).await?;
                        // The retry keeps skills and MCP instructions: they
                        // cost a few KB, the history is what blew the budget,
                        // and the compaction freed far more.
                        self.prepare_retry(
                            &effective_model,
                            effective_max_tokens,
                            tool_defs.clone(),
                            pair_dangling_tool_uses(self.session.messages().to_vec()),
                            self.settings.model.fallback_model.clone(),
                            origin.clone(),
                        )
                        .await
                    }
                };
                let _ = self.event_tx.send(AgentEvent::StepStart {
                    step,
                    turn_id: self.current_turn_id.clone(),
                });
            };

            // Any images the round's tools returned ride out-of-band (see
            // `ToolExecCtx::images`) and get appended to the tool-result message
            // `execute_stream` just pushed — after every `tool_result` block, as
            // the API requires.
            let pending_images =
                std::mem::take(&mut *tool_images.lock().unwrap_or_else(|e| e.into_inner()));
            if !pending_images.is_empty() {
                attach_tool_result_images(&mut self.session.messages, pending_images);
            }

            let has_tool_uses = stream_result.has_tool_uses;
            had_tool_uses_this_turn = had_tool_uses_this_turn || has_tool_uses;
            tool_calls += stream_result.tool_calls;
            let stop_reason = stream_result.stop_reason;
            let usage = stream_result.usage;
            self.last_call_usage = Some(usage);
            self.turn_truncated |= stream_result.truncated;
            if let Err(e) = self.session.persist().await {
                tracing::warn!(error = %e, "failed to persist session");
            }
            let _ = self.event_tx.send(AgentEvent::StepEnd {
                step,
                stop_reason: stop_reason.clone(),
                usage,
                tool_calls: stream_result.tool_calls,
                turn_id: self.current_turn_id.clone(),
                seq: self.session.messages.len() as u64,
            });

            // One call, accounted for. Every other number the engine reports is
            // per turn, and a turn is several calls — so this is the event a
            // cost can be computed from, and the only one that says which model
            // the spending went to when a turn switched models partway.
            let _ = self
                .telemetry_handle
                .record(telemetry::TelemetryEvent::api_request(
                    &self.session.session_id,
                    self.session.turn_count,
                    Some(self.current_turn_id.clone()),
                    telemetry::ApiRequestPayload {
                        model: effective_model.clone(),
                        input_tokens: usage.input_tokens as u64,
                        output_tokens: usage.output_tokens as u64,
                        cache_creation_tokens: usage.cache_creation_input_tokens as u64,
                        cache_read_tokens: usage.cache_read_input_tokens as u64,
                        latency_ms: call_started.elapsed().as_millis() as u64,
                        stop_reason: stop_reason.clone(),
                        input_message_count: sent_message_count,
                        tool_count: sent_tool_count,
                        default_model: effective_model == self.settings.model.model_name,
                        // A turn's own call. The auxiliary ones are labelled
                        // by `telemetry::model::TelemetryModel`, which is
                        // where they are visible at all.
                        purpose: None,
                        truncated: stream_result.truncated,
                    },
                ));
            // Added here rather than beside the budget's own counter further
            // down: a turn can end between the two, and a call that happened
            // has to be counted whether or not the turn went on to check a
            // budget.
            turn_usage = turn_usage.plus(usage);
            if stream_result.cancelled {
                continue;
            }

            // `PostSampling`: one model response has been fully streamed and
            // is in the transcript. Purely observational: like the other lifecycle hooks, the
            // response is not modifiable from here (there is no defined
            // recovery path for a veto at this point).
            {
                let sr = stop_reason.clone();
                let out_tokens = usage.output_tokens;
                self.fire_lifecycle_hook(hooks::HookEvent::PostSampling, |i| {
                    i.with_reason(format!(
                        "model response complete (stop_reason={sr}, output_tokens={out_tokens})"
                    ))
                })
                .await;
            }

            // A git-mutating Bash call this round means the session-start
            // `FrozenContext` git snapshot (branch/status/log) is now stale
            // — refresh just that slice.
            if has_tool_uses && Self::included_git_mutating_bash_call(self.this_step()) {
                if let Some(ref mut frozen) = self.frozen {
                    frozen
                        .refresh_git(&engine_git_from(&self.config, &self.session.session_id))
                        .await;
                }
            }
            if has_tool_uses {
                self.notice_repeated_calls();
            }

            // A PostToolUse hook can request the turn end right here — every
            // tool_use emitted in this round already has its tool_result in
            // the session (execute_stream writes the reply together with all
            // of its results once the round is over), so there's no risk of
            // a dangling tool_use block without a matching result. Mirrors the Stop hook's
            // discontinue path below, just triggered from a different point.
            if discontinued.load(std::sync::atomic::Ordering::Relaxed) {
                tracing::info!("PostToolUse hook discontinued the turn");
                let tid = self.current_turn_id.clone();
                let _ = self
                    .telemetry_handle
                    .record(telemetry::TelemetryEvent::turn_complete(
                        &self.session.session_id,
                        self.session.turn_count,
                        Some(tid),
                        telemetry::TurnCompletePayload {
                            stop_reason: "stopped_by_hook".into(),
                            api_calls,
                            tool_calls,
                            permission_denials: self
                                .permission_denial_count
                                .load(std::sync::atomic::Ordering::Relaxed),
                            last_tool_name: None,
                            last_tool_was_error: false,
                            turn_duration_ms: start.elapsed().as_millis() as u64,
                        },
                    ));
                self.last_had_tool_uses = had_tool_uses_this_turn;
                return Ok(TurnOutcome {
                    stop_reason: "stopped_by_hook".into(),
                    api_calls,
                    tool_calls,
                    usage,
                    total_usage: turn_usage,
                    message: None,
                });
            }

            // Structured output retry limit.
            structured_output_calls += self
                .session
                .messages
                .get(messages_before_step..)
                .map_or(0, count_structured_output_calls);
            if let base::interface::turn_policy::TurnStep::Stop { reason } = self
                .turn_policy
                .after_model_call(&base::interface::turn_policy::TurnProgress {
                    api_calls,
                    tool_calls,
                    structured_output_calls,
                    stop_reason: &stop_reason,
                })
            {
                tracing::warn!(
                    structured_output_calls,
                    %reason,
                    "turn policy ended the turn"
                );
                self.last_had_tool_uses = had_tool_uses_this_turn;
                self.record_turn_complete(&reason, api_calls, tool_calls, start);
                return Ok(TurnOutcome {
                    stop_reason: reason,
                    api_calls,
                    tool_calls,
                    total_usage: turn_usage,
                    usage: Usage::default(),
                    message: None,
                });
            }

            // Cumulative token budget, checked against the real usage the
            // provider reported for this call — no per-model price table,
            // no estimate. At 90% inject a warning; at 100% abort.
            //
            // The engine hands over all four figures and does not decide which
            // of them count as spent; that is the policy's judgement, and the
            // telemetry below records the reading the engine's own policy uses.
            {
                let spend = base::interface::budget_policy::Spend::from(turn_usage);
                let total_tokens_used = spend.total_tokens();
                match self.budget_policy.on_usage(&spend) {
                    base::interface::budget_policy::Spending::WithinBudget => {}
                    base::interface::budget_policy::Spending::Warn { reminder, limit } => {
                        tracing::warn!(
                            total_tokens_used,
                            budget = limit,
                            "approaching token budget limit; injecting continue reminder"
                        );
                        let _ = self.telemetry_handle.record(
                            telemetry::TelemetryEvent::budget_enforced(
                                &self.session.session_id,
                                self.session.turn_count,
                                Some(self.current_turn_id.clone()),
                                telemetry::BudgetEnforcedPayload {
                                    action: telemetry::BudgetEnforcedAction::WarningInjected,
                                    total_tokens_used,
                                    budget: limit,
                                },
                            ),
                        );
                        self.session.push_message(ModelMessage {
                            role: MessageRole::User,
                            content: vec![ModelContentBlock::Text { text: reminder }],
                        });
                    }
                    base::interface::budget_policy::Spending::Exhausted { limit } => {
                        tracing::warn!(total_tokens_used, budget = limit, "token budget exceeded");
                        let _ = self.telemetry_handle.record(
                            telemetry::TelemetryEvent::budget_enforced(
                                &self.session.session_id,
                                self.session.turn_count,
                                Some(self.current_turn_id.clone()),
                                telemetry::BudgetEnforcedPayload {
                                    action: telemetry::BudgetEnforcedAction::TurnStopped,
                                    total_tokens_used,
                                    budget: limit,
                                },
                            ),
                        );
                        self.last_had_tool_uses = had_tool_uses_this_turn;
                        self.record_turn_complete("budget_exceeded", api_calls, tool_calls, start);
                        return Ok(TurnOutcome {
                            stop_reason: "budget_exceeded".into(),
                            api_calls,
                            tool_calls,
                            total_usage: turn_usage,
                            usage: Usage::default(),
                            message: None,
                        });
                    }
                }
            }

            // If tools were executed during streaming, continue to next API call.
            if has_tool_uses {
                // Collect async skill discovery prefetch (fired at turn start).
                if let Some(handle) = skill_prefetch.take() {
                    // Time-bounded wait: if discovery hasn't completed after
                    // tool execution, skip it this turn (it'll run next turn).
                    if let Ok(Ok((_discovered, new_names))) =
                        tokio::time::timeout(std::time::Duration::from_millis(500), handle).await
                    {
                        if !new_names.is_empty() {
                            let skills_text = format!(
                                "<system-reminder>\nSkills discovered in workspace: {}. Use /<skill-name> to invoke.\n</system-reminder>",
                                new_names.join(", ")
                            );
                            self.session.push_message(ModelMessage {
                                role: MessageRole::User,
                                content: vec![ModelContentBlock::Text { text: skills_text }],
                            });
                            for name in &new_names {
                                if !self.invoked_skills.contains(name) {
                                    self.invoked_skills.push(name.clone());
                                }
                            }
                        }
                    }
                }

                // Activate conditional skills whose `paths` patterns match
                // files accessed by Read/Write/Edit tool operations this turn.
                {
                    let file_paths = Self::extract_tool_file_paths(self.messages_this_turn());
                    if !file_paths.is_empty() {
                        let activated = self.skills.activate_for_paths(&file_paths);
                        if !activated.is_empty() {
                            let names: Vec<&str> =
                                activated.iter().map(|s| s.name.as_str()).collect();
                            let skills_text = format!(
                                "<system-reminder>\nConditional skills activated for \
                                 current context: {}. Use /<skill-name> to invoke.\n\
                                 </system-reminder>",
                                names.join(", "),
                            );
                            self.session.push_message(ModelMessage {
                                role: MessageRole::User,
                                content: vec![ModelContentBlock::Text { text: skills_text }],
                            });
                            for s in &activated {
                                if !self.invoked_skills.contains(&s.name) {
                                    self.invoked_skills.push(s.name.clone());
                                }
                            }
                        }
                    }
                }

                self.mcp.refresh_if_changed().await;
                sync_mcp_adapters(self.tools.as_ref(), self.mcp.as_ref(), |n| {
                    self.scene.permits_tool(n)
                });
                if !cancel.is_cancelled() {
                    self.deliver_queued_user_messages(false).await;
                }
                continue;
            }

            // 6. A reply that stopped early: escalate the output limit, or
            //    send a paused one back to be continued.
            if self.handle_early_stop(
                &stop_reason,
                &mut max_tokens_recovery,
                &mut effective_max_tokens,
            ) {
                continue;
            }

            // 7. Stop hooks: end the turn, send the model back, or let it stop.
            if self.hooks.has_hooks_for(hooks::HookEvent::Stop) {
                let stop_hook_input = hooks::HookInput {
                    hook_event_name: "Stop".into(),
                    session_id: self.session.session_id.to_string(),
                    cwd: self.settings.paths.project_root().display().to_string(),
                    permission_mode: hook_permission_mode(self.session_state.permission_mode()),
                    stop_hook_active: Some(sent_back_by_stop_hook),
                    ..Default::default()
                };
                let hook_result = self
                    .hooks
                    .run(hooks::HookEvent::Stop, &stop_hook_input)
                    .await;
                if hook_result.discontinued() {
                    tracing::info!("Stop hook discontinued the turn");
                    // `StopFailure` is the error-shaped sibling of `Stop`
                    // (same relationship `PostToolUseFailure` has to
                    // `PostToolUse`) and had no trigger. A turn cut short by
                    // a hook veto is the case it exists for: `Stop` fired,
                    // but the turn did not finish normally.
                    self.fire_lifecycle_hook(hooks::HookEvent::StopFailure, |i| {
                        i.with_reason("turn discontinued by a Stop hook")
                    })
                    .await;
                    let tid = self.current_turn_id.clone();
                    let _ = self
                        .telemetry_handle
                        .record(telemetry::TelemetryEvent::turn_complete(
                            &self.session.session_id,
                            self.session.turn_count,
                            Some(tid.clone()),
                            telemetry::TurnCompletePayload {
                                stop_reason: "stopped_by_hook".into(),
                                api_calls,
                                tool_calls,
                                permission_denials: self
                                    .permission_denial_count
                                    .load(std::sync::atomic::Ordering::Relaxed),
                                last_tool_name: None,
                                last_tool_was_error: false,
                                turn_duration_ms: start.elapsed().as_millis() as u64,
                            },
                        ));
                    self.last_had_tool_uses = had_tool_uses_this_turn;
                    return Ok(TurnOutcome {
                        stop_reason: "stopped_by_hook".into(),
                        api_calls,
                        tool_calls,
                        usage,
                        total_usage: turn_usage,
                        message: hook_result.discontinue_message().map(str::to_string),
                    });
                }
                if let Some(blocked) = hook_result.blocked() {
                    let reason = blocked
                        .message
                        .as_deref()
                        .filter(|m| !m.trim().is_empty())
                        .unwrap_or("a Stop hook asked for more work before this turn ends");
                    tracing::info!(reason, "Stop hook sent the model back to work");
                    self.session.push_message(ModelMessage {
                        role: MessageRole::User,
                        content: vec![ModelContentBlock::Text {
                            text: format!(
                                "<system-reminder>\nA Stop hook did not let this turn end yet:\n\
                                 {reason}\n</system-reminder>"
                            ),
                        }],
                    });
                    sent_back_by_stop_hook = true;
                    continue;
                }
            }

            // 8. Token budget continuation mode.
            // Continue while accumulated output < 90% of target AND not diminishing
            // (≥3 continuations, both this & previous delta < 500). No hard cap.
            if let Some(target) = self.output_token_target {
                self.accumulated_output_tokens = self
                    .accumulated_output_tokens
                    .saturating_add(usage.output_tokens as u64);

                let this_delta = usage.output_tokens as u64;
                if let base::interface::budget_policy::OutputTarget::KeepGoing { nudge } = self
                    .budget_policy
                    .on_output_target(&base::interface::budget_policy::OutputProgress {
                        accumulated: self.accumulated_output_tokens,
                        target,
                        continuations: self.token_budget_continuation_count,
                        this_delta,
                        last_delta: self.last_delta_tokens,
                    })
                {
                    self.token_budget_continuation_count += 1;
                    self.last_delta_tokens = this_delta;
                    self.session.push_message(ModelMessage {
                        role: MessageRole::User,
                        content: vec![ModelContentBlock::Text { text: nudge }],
                    });
                    tracing::info!(
                        accumulated = self.accumulated_output_tokens,
                        target,
                        continuation = self.token_budget_continuation_count,
                        "Token budget continuation — injecting nudge"
                    );
                    continue;
                }

                // Budget met (≥90%) or diminishing returns — clear budget state.
                let threshold = (target as f64 * 0.9) as u64;
                let budget_met = self.accumulated_output_tokens >= threshold;
                tracing::info!(
                    accumulated = self.accumulated_output_tokens,
                    target,
                    budget_met,
                    "Token budget session complete"
                );
                self.output_token_target = None;
                self.accumulated_output_tokens = 0;
                self.token_budget_continuation_count = 0;
                self.last_delta_tokens = 0;
            }

            // The model has answered; a message the user sent meanwhile is
            // answered in the same turn rather than left for one nobody
            // asked to start.
            if self.deliver_queued_user_messages(true).await {
                continue;
            }

            // No tools → turn complete
            self.session.increment_turn();
            let latency_ms = start.elapsed().as_millis() as f64;
            let tid = self.current_turn_id.clone();
            let _ = self
                .telemetry_handle
                .record(telemetry::TelemetryEvent::turn_complete(
                    &self.session.session_id,
                    turn_no,
                    Some(tid.clone()),
                    telemetry::TurnCompletePayload {
                        stop_reason: stop_reason.clone(),
                        api_calls,
                        tool_calls,
                        permission_denials: self
                            .permission_denial_count
                            .load(std::sync::atomic::Ordering::Relaxed),
                        last_tool_name: None,
                        last_tool_was_error: false,
                        turn_duration_ms: latency_ms as u64,
                    },
                ));
            // TurnComplete — the normal end of a turn. Distinct from the
            // `Stop` hook above, which fires whenever the model stops calling
            // tools and can *veto* the stop; this one is a notification that
            // the turn is over, carrying why.
            self.fire_lifecycle_hook(hooks::HookEvent::TurnComplete, |i| {
                i.with_turn_id(self.current_turn_id.clone())
                    .with_stop_reason(stop_reason.clone())
            })
            .await;
            // Auto-extract durable memories after turn completion.
            // Only extract if the model produced a complete response (not cancelled/max_turns).
            // Gated on `memory_enabled` — see the prefetch block above for why this matters
            // (the flag's whole point is opting out of the memory subsystem entirely, not just
            // the static prompt injection).
            if self.settings.memory_enabled {
                let session_messages = self.session.messages().to_vec();
                let store = self.memory_store.clone();
                // `task_models.memory` lets an app point this background
                // extraction call at a different vendor/model than the main
                // conversation — same TaskRouter mechanism already proven
                // for the "subagent" task type (see
                // `AgentTool::Inner::model_for_subagent`). No override
                // configured → falls back to the main model.
                let (model, model_name) = match &self.task_router {
                    Some(router) => (
                        self.accounted(router.model_for("memory")),
                        router
                            .model_name_for("memory")
                            .unwrap_or(&self.settings.model.model_name)
                            .to_string(),
                    ),
                    None => (self.model.clone(), self.settings.model.model_name.clone()),
                };
                let prompt_intro = self.scene.memory_extraction_prompt();
                let environment = self.environment.clone();
                let session_id = self.session.session_id.clone();
                tokio::spawn(async move {
                    extract_memories_after_turn(
                        store,
                        &session_messages,
                        model.as_ref(),
                        &model_name,
                        prompt_intro.as_deref(),
                        &*environment,
                        &session_id,
                    )
                    .await;
                });
            }

            // Remind the model to update its cross-session notes once they
            // have gone stale.
            if let Some(ref sm) = self.session.session_memory {
                let current_turn = self.session.turn_count;
                // If a Write/Edit this turn targeted the sidecar file itself,
                // the model just updated its notes — reset staleness instead
                // of nagging again next turn. Compares against the exact
                // path we tell the model below, so a literal match is enough
                // (no canonicalization needed).
                let updated_this_turn = Self::extract_tool_file_paths(self.messages_this_turn())
                    .iter()
                    .any(|p| p == sm.path());
                if updated_this_turn {
                    if let Err(e) = sm.mark_extraction_completed(current_turn).await {
                        tracing::warn!(error = %e, "failed to mark session_memory extraction completed");
                    }
                } else if sm.is_stale(current_turn) {
                    tracing::debug!(
                        last_update_turn = sm.last_update_turn(),
                        current_turn,
                        "session memory is stale; injecting update reminder"
                    );
                    self.session.push_message(ModelMessage {
                        role: MessageRole::User,
                        content: vec![ModelContentBlock::Text {
                            text: format!(
                                "\
<system-reminder>
Your session notes ({}) have not been updated in several turns.
Consider reviewing and updating them with any persistent facts, user preferences,
or project context that should survive across sessions.
</system-reminder>",
                                sm.path().display()
                            ),
                        }],
                    });
                }
            }

            self.last_had_tool_uses = had_tool_uses_this_turn;
            return Ok(TurnOutcome {
                stop_reason,
                api_calls,
                tool_calls,
                usage,
                total_usage: turn_usage,
                message: None,
            });
        }
    }

    async fn handle_tool_result(
        &mut self,
        tool_use_id: String,
        content: String,
        is_error: bool,
    ) -> Result<TurnOutcome, TurnError> {
        if !awaits_result(self.session.messages(), &tool_use_id) {
            return Err(TurnError::Internal(format!(
                "no tool call {tool_use_id} is waiting for a result"
            )));
        }
        self.session.push_message(ModelMessage {
            role: MessageRole::User,
            content: vec![ModelContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error: Some(is_error),
            }],
        });
        Ok(TurnOutcome {
            stop_reason: "tool_result_received".into(),
            api_calls: 0,
            tool_calls: 0,
            total_usage: Usage::default(),
            usage: Usage::default(),
            message: None,
        })
    }

    /// The messages pushed since this turn's user message, that message
    /// included; the whole transcript before any turn has started.
    fn messages_this_turn(&self) -> &[ModelMessage] {
        let messages = self.session.messages();
        let Some(started) = self.turn_started_at else {
            return messages;
        };
        let first = self
            .session
            .message_ages()
            .iter()
            .rposition(|(_, at)| *at < started)
            .map_or(0, |i| i + 1);
        &messages[first..]
    }

    /// The reply of the step that just ran and the results after it.
    fn this_step(&self) -> &[ModelMessage] {
        let messages = self.session.messages();
        let reply = messages
            .iter()
            .rposition(|m| m.role == MessageRole::Assistant)
            .unwrap_or(0);
        &messages[reply..]
    }

    /// The `file_path` of every `Read`/`Write`/`Edit` call in `messages`.
    fn extract_tool_file_paths(messages: &[ModelMessage]) -> Vec<PathBuf> {
        let tool_names = ["Read", "Write", "Edit"];
        let mut paths: Vec<PathBuf> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for msg in messages {
            for block in &msg.content {
                if let ModelContentBlock::ToolUse { name, input, .. } = block {
                    if !tool_names.contains(&name.as_str()) {
                        continue;
                    }
                    if let Some(fp) = input.get("file_path").and_then(|v| v.as_str()) {
                        let path = PathBuf::from(fp);
                        if seen.insert(path.clone()) {
                            paths.push(path);
                        }
                    }
                }
            }
        }
        paths
    }

    /// Counts the calls of the step that just ran — see [`crate::repeat_guard`].
    fn notice_repeated_calls(&mut self) {
        let Some(step) = self
            .session
            .messages()
            .iter()
            .rev()
            .find(|m| m.role == MessageRole::Assistant)
        else {
            return;
        };
        let turn = self.session.turn_count;
        let notices: Vec<String> = step
            .content
            .iter()
            .filter_map(|block| match block {
                ModelContentBlock::ToolUse { name, input, .. } => self
                    .repeat_guard
                    .record(turn, name, input)
                    .map(|count| (name.clone(), count)),
                _ => None,
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|(name, count)| {
                crate::repeat_guard::notice(&name, count, self.scene.interactive())
            })
            .collect();
        for text in notices {
            self.session.push_message(ModelMessage {
                role: MessageRole::User,
                content: vec![ModelContentBlock::Text { text }],
            });
        }
    }

    /// Whether any `Bash` call in `messages` looks like it mutated git state
    /// — used to decide whether to pay for a `FrozenContext::refresh_git()`
    /// round-trip (real subprocess I/O, not free) rather than trusting the
    /// session-start snapshot for the rest of a long-running session (see
    /// `crates/core/src/frozen/mod.rs`'s module-level "not refreshed during a
    /// session" caveat).
    ///
    /// Deliberately a heuristic, not a shell parser: scans for `git`
    /// followed within a few tokens by a known state-mutating subcommand
    /// name, tolerating global flags (`-C <dir>`, `--no-pager`, ...) between
    /// them. False positives (e.g. `git log --grep=commit`) just cost one
    /// extra (cheap, timeout-bounded) `refresh_git()` call; false negatives
    /// (a mutating command this list doesn't recognize, or one run through
    /// e.g. a wrapper script) just mean the "frozen until next session"
    /// staleness persists a bit longer — neither is a correctness hazard.
    fn included_git_mutating_bash_call(messages: &[ModelMessage]) -> bool {
        const GIT_MUTATING_SUBCOMMANDS: &[&str] = &[
            "checkout",
            "switch",
            "commit",
            "merge",
            "rebase",
            "reset",
            "revert",
            "cherry-pick",
            "stash",
            "pull",
            "add",
            "rm",
            "mv",
            "restore",
            "branch",
            "worktree",
            "init",
            "clone",
            "am",
            "apply",
        ];
        for msg in messages {
            for block in &msg.content {
                let ModelContentBlock::ToolUse { name, input, .. } = block else {
                    continue;
                };
                if name != "Bash" {
                    continue;
                }
                let Some(command) = input.get("command").and_then(|v| v.as_str()) else {
                    continue;
                };
                for (idx, _) in command.match_indices("git") {
                    // Require a word boundary before "git" so e.g. "digit"
                    // doesn't match.
                    if idx > 0 && command.as_bytes()[idx - 1].is_ascii_alphanumeric() {
                        continue;
                    }
                    let after: Vec<&str> = command[idx + 3..].split_whitespace().take(6).collect();
                    if after.iter().any(|t| GIT_MUTATING_SUBCOMMANDS.contains(t)) {
                        return true;
                    }
                }
            }
        }
        false
    }

    async fn build_tool_defs(&mut self) -> Vec<base::interface::model::ToolDef> {
        use std::collections::BTreeMap;
        /// The tool through which deferred schemas and `Tool::prompt()` usage
        /// guides are fetched. Named here because `build_tool_defs` must not
        /// advertise a guide when the scene has filtered the fetcher out.
        const TOOL_SEARCH_NAME: &str = "ToolSearch";
        let mut pool: BTreeMap<String, Arc<dyn base::tool::Tool>> = BTreeMap::new();
        for t in self.mcp.tools() {
            pool.insert(t.name().to_string(), t);
        }
        // MCP adapters also sit in `self.tools`, because dispatch looks them up
        // there; only a non-MCP entry can shadow one. Built-ins win.
        for t in self.tools.list() {
            if t.source().starts_with("mcp:") {
                continue;
            }
            let name = t.name().to_string();
            if pool.contains_key(&name) && self.mcp_shadow_warned.insert(name.clone()) {
                tracing::warn!(
                    tool = %name,
                    "built-in tool shadows an MCP tool of the same name; the MCP tool is not exposed to the model"
                );
            }
            pool.insert(name, t.clone());
        }
        let selected: Vec<Arc<dyn base::tool::Tool>> = pool
            .into_values()
            .filter(|t| self.scene.permits_tool(t.name()))
            .collect();

        // A guide is only worth pointing at if the model has a way to fetch
        // it — `ToolSearch` can be filtered out by a scene whitelist, and a
        // pointer to a tool that isn't there is worse than no pointer.
        let can_fetch_guides = selected.iter().any(|t| t.name() == TOOL_SEARCH_NAME);
        let prompt_ctx = base::tool::PromptContext {
            cwd: self.settings.paths.project_root(),
            model: self.settings.model.model_name.clone(),
            session_id: self.session.session_id.clone(),
            is_interactive: false,
            all_tool_names: selected.iter().map(|t| t.name().to_string()).collect(),
            allowed_agent_types: vec![],
            path_style: self.config.path_style,
        };

        let mut defs = Vec::with_capacity(selected.len());
        for t in selected {
            let mut description = t.description().to_string();
            // Surface the existence of `Tool::prompt()`, not its body: inlining
            // the guides would add ~85 KB to every request. A deferred tool
            // needs no pointer — fetching its schema returns the guide too.
            if can_fetch_guides
                && !t.is_deferred()
                && t.detailed_prompt(&prompt_ctx).await.is_some()
            {
                description.push_str(&format!(
                    "\n\nDetailed guide: ToolSearch{{\"query\":\"select:{}\"}}",
                    t.name()
                ));
            }
            defs.push(base::interface::model::ToolDef {
                name: t.name().to_string(),
                description,
                input_schema: base::tool::compact_schema(
                    t.input_schema_for(self.config.path_style),
                ),
                source: Some(t.source().into_owned()),
            });
        }
        defs
    }

    /// A model this session did not build, made to account for itself.
    ///
    /// `Builder::build` wraps the session's own model once, so everything
    /// that reaches the provider through it is counted. A task model comes
    /// from the router instead — pool-level, shared, built before any session
    /// existed — so a call routed to one is the single kind that would still
    /// arrive nowhere. Wrapping it here costs one allocation per auxiliary
    /// call and closes that.
    fn accounted(
        &self,
        model: Arc<dyn base::interface::model::Model>,
    ) -> Arc<dyn base::interface::model::Model> {
        Arc::new(telemetry::model::TelemetryModel::new(
            model,
            self.telemetry_handle.clone(),
            self.settings.model.model_name.clone(),
        ))
    }

    /// Build the summarizer used by compaction's FullCompact layer.
    ///
    /// Routed through the task-model router under the `"compact"` task type,
    /// so a `task_models.compact` entry can point summarization at a
    /// cheaper/faster model than the conversation. With no router configured
    /// it uses the conversation's own model and model name.
    fn compaction_summarizer(&self) -> Option<compaction::llm_summary::LlmSummarizer> {
        let (model, model_name) = match &self.task_router {
            Some(router) => (
                self.accounted(router.model_for("compact")),
                router
                    .model_name_for("compact")
                    .unwrap_or(&self.settings.model.model_name)
                    .to_string(),
            ),
            None => (self.model.clone(), self.settings.model.model_name.clone()),
        };
        if model_name.is_empty() {
            return None;
        }
        Some(
            compaction::llm_summary::LlmSummarizer::new(model, model_name)
                .for_session(self.session.session_id.clone()),
        )
    }

    /// Telemetry `turn_complete` for the paths that end a turn early and
    /// know nothing about its last tool. The `AgentEvent` is `finish_turn`'s.
    fn record_turn_complete(
        &self,
        stop_reason: &str,
        api_calls: u32,
        tool_calls: u32,
        start: std::time::Instant,
    ) {
        let tid = self.current_turn_id.clone();
        let _ = self
            .telemetry_handle
            .record(telemetry::TelemetryEvent::turn_complete(
                &self.session.session_id,
                self.session.turn_count,
                Some(tid.clone()),
                telemetry::TurnCompletePayload {
                    stop_reason: stop_reason.to_string(),
                    api_calls,
                    tool_calls,
                    permission_denials: self
                        .permission_denial_count
                        .load(std::sync::atomic::Ordering::Relaxed),
                    last_tool_name: None,
                    last_tool_was_error: false,
                    turn_duration_ms: start.elapsed().as_millis() as u64,
                },
            ));
    }

    fn record_api_error(
        &self,
        model: &str,
        error: &base::interface::model::ModelError,
        started: std::time::Instant,
    ) {
        use base::interface::model::ModelError;
        let (error_kind, http_status) = match error {
            ModelError::Cancelled | ModelError::NotRecorded(_) => return,
            ModelError::Api { status, .. } => ("api", Some(*status)),
            ModelError::Auth(_) => ("auth", None),
            ModelError::RateLimited => ("rate_limited", None),
            ModelError::Overloaded => ("overloaded", None),
            ModelError::Network(_) => ("network", None),
            ModelError::Internal(_) => ("internal", None),
            ModelError::Unsupported(_) => ("unsupported", None),
        };
        let _ = self
            .telemetry_handle
            .record(telemetry::TelemetryEvent::api_error(
                &self.session.session_id,
                self.session.turn_count,
                Some(self.current_turn_id.clone()),
                telemetry::ApiErrorPayload {
                    model: model.to_string(),
                    error_kind: error_kind.to_string(),
                    http_status,
                    error_message: error.to_string(),
                    latency_ms: started.elapsed().as_millis() as u64,
                },
            ));
    }

    fn record_slash_command(&self, command: &crate::commands::SlashCommand) {
        use crate::commands::Command;
        let kind = match self.commands.resolve(&command.name) {
            Some(Command::Local { .. }) => "local",
            Some(Command::Prompt { .. }) => "prompt",
            Some(Command::McpPrompt { .. }) => "mcp_prompt",
            None if self.mcp.has_prompt_command(&command.name) => "mcp_prompt",
            None => "unknown",
        };
        let _ = self
            .telemetry_handle
            .record(telemetry::TelemetryEvent::slash_command_used(
                &self.session.session_id,
                self.session.turn_count,
                Some(self.current_turn_id.clone()),
                telemetry::SlashCommandUsedPayload {
                    command: command.name.clone(),
                    kind: kind.to_string(),
                    has_arg: !command.args.trim().is_empty(),
                },
            ));
    }

    /// Compact session messages if token budget is exceeded.
    pub(crate) async fn compact_if_needed(&mut self) {
        // Every judgement below belongs to the compactor: what has gone stale,
        // whether to compact early, when to warn, and when the budget forces
        // it. The order they are asked in is the loop's.
        let compactor = self.compactor.clone();
        let cached_due = self.cached_mc.should_run();
        let cached_keep_recent = self.cached_mc.keep_recent();
        let ages = self.session.message_ages();
        let time_based = self.time_based_mc_config.clone();
        let aging_start = std::time::Instant::now();
        let aging_from = compaction::grouping::estimate_tokens(&self.session.messages);
        let aging = compactor.age(
            &mut self.session.messages,
            &compaction::compact::AgingInput {
                message_ages: &ages,
                time_based: &time_based,
                cached_due,
                cached_keep_recent,
            },
        );
        if aging.cleared() > 0 {
            self.tool_results_ever_cleared = true;
        }
        if aging.time_based.cleared > 0 {
            tracing::info!(
                cleared = aging.time_based.cleared,
                skipped = aging.time_based.skipped,
                "time-based micro-compact applied"
            );
            let messages = self.session.messages.len();
            self.report_compaction(
                &CompactSummary {
                    strategy: format!("{:?}", compaction::compact::CompactStrategy::MicroCompact),
                    messages_before: messages,
                    messages_after: messages,
                    tokens_before: aging_from,
                    tokens_after: compaction::grouping::estimate_tokens(&self.session.messages),
                },
                None,
                aging_start,
            );
        }
        if let Some(ids) = aging.cache_edits {
            tracing::info!(
                cleared = aging.cached_cleared,
                "cached micro-compact applied — pending cache_edits generated"
            );
            self.cached_mc.record_pass(ids);
        }

        // Enforce per-message tool result budget BEFORE compaction.
        let budget_modified = compactor.trim_tool_results(&mut self.session.messages);
        if budget_modified > 0 {
            self.tool_results_ever_cleared = true;
            tracing::debug!(modified = budget_modified, "tool result budget enforced");
        }

        let budget = self.context_budget();
        let context_tokens = self.context_tokens();
        let reactive_start = std::time::Instant::now();
        let reactive_from = compaction::grouping::estimate_tokens(&self.session.messages);
        match compactor.reactive(
            &mut self.session.messages,
            &compaction::compact::ReactiveInput {
                context_tokens,
                context_limit: budget.compact_threshold,
                keep_recent: budget.compact_keep_recent,
                enabled: self.settings.compaction.reactive,
                circuit_open: self.compaction_state.circuit_open,
            },
        ) {
            compaction::compact::ReactiveOutcome::Compacted => {
                self.tool_results_ever_cleared = true;
                self.compaction_state.record_success();
                let messages = self.session.messages.len();
                self.report_compaction(
                    &CompactSummary {
                        strategy: format!(
                            "{:?}",
                            compaction::compact::CompactStrategy::MicroCompact
                        ),
                        messages_before: messages,
                        messages_after: messages,
                        tokens_before: reactive_from,
                        tokens_after: compaction::grouping::estimate_tokens(&self.session.messages),
                    },
                    None,
                    reactive_start,
                );
                tracing::info!(context_tokens, "reactive micro-compact completed");
            }
            compaction::compact::ReactiveOutcome::NoEffect => {
                self.compaction_state.record_failure();
                tracing::warn!(
                    failures = self.compaction_state.consecutive_failures,
                    "reactive micro-compact had no effect"
                );
            }
            compaction::compact::ReactiveOutcome::NotTriggered => {}
        }

        let threshold = budget.compact_threshold;
        if let Some(reminder) = compactor.context_warning(&compaction::compact::BudgetState {
            context_tokens: self.context_tokens(),
            threshold,
            warned_already: self.compact_warning_issued,
        }) {
            self.session.push_message(ModelMessage {
                role: MessageRole::User,
                content: vec![ModelContentBlock::Text { text: reminder }],
            });
            self.compact_warning_issued = true;
        }
        // Asked again after the reminder, not before it: the reminder is
        // itself context, and a conversation one message short of the
        // threshold crosses it here rather than a turn later.
        if !compactor.should_compact(&compaction::compact::BudgetState {
            context_tokens: self.context_tokens(),
            threshold,
            warned_already: self.compact_warning_issued,
        }) {
            return;
        }
        // The threshold is on the whole request, so the conversation gets what
        // the fixed part of it leaves. Where that is nothing, no conversation
        // could fit, and the threshold bounds the conversation by itself.
        let room = match threshold.checked_sub(self.request_overhead_tokens) {
            Some(room) if room > 0 => room,
            _ if self.session.token_count() > threshold => threshold,
            _ => return,
        };
        let target = room.saturating_sub(self.recovery_reserve());
        let _ = self
            .run_compaction(target, budget.compact_keep_recent, CompactTrigger::Auto)
            .await;
    }

    /// One compaction, start to finish: the `PreCompact` hook, an LLM summary
    /// when one is wanted, the compactor's own cascade otherwise, recovery of
    /// what the summary cannot carry, the transcript marker, the event and
    /// `PostCompact`. Every trigger takes this path, so what a compaction has
    /// to put right afterwards is put right whichever one ran.
    ///
    /// `target` is what the conversation is cut to before recovery goes back
    /// in front of it. A compaction that leaves the conversation no smaller —
    /// nothing to drop, or recovery costing what was dropped — is not applied.
    pub(crate) async fn run_compaction(
        &mut self,
        target: usize,
        keep: usize,
        trigger: CompactTrigger,
    ) -> Result<CompactSummary, NotCompacted> {
        if !self.pre_compact_hooks_allow(target, trigger).await {
            return Err(NotCompacted::Declined);
        }
        let compactor = self.compactor.clone();
        let messages_before = self.session.messages.len();
        // The summarizer must see what is about to be destroyed, and recovery
        // re-attaches files from reads that are about to be dropped.
        let pre_compact = self.session.messages().to_vec();

        // Layer 0: FullCompact — LLM summary, before anything is dropped.
        //
        // Placed ahead of the DefaultCompactor cascade because every layer
        // in that cascade destroys information: Snip drops whole rounds,
        // MicroCompact blanks tool results, CollapseContext truncates each
        // text block to 200 bytes. A summary written after any of them
        // could only describe the wreckage. Summarize, then drop.
        //
        // Not unconditional: `should_summarize` requires that compaction is
        // genuinely about to discard a meaningful amount of history, so a
        // marginal compaction doesn't buy an API round-trip of latency on
        // the user's turn. Every failure mode (no summarizer configured,
        // strategy declines, model errors, timeout, empty response) falls
        // through to the non-LLM cascade below, so this can never fail a
        // turn — it can only fail to help.
        let compact_start = std::time::Instant::now();
        let llm_outcome = match self.compaction_summarizer() {
            Some(summarizer)
                if trigger == CompactTrigger::Manual
                    || compactor.wants_llm_summary(&pre_compact, target, keep) =>
            {
                match compaction::llm_summary::full_compact(
                    &summarizer,
                    pre_compact.clone(),
                    target,
                    keep,
                )
                .await
                {
                    Ok(ok) => {
                        tracing::info!("compaction: LLM summary produced (FullCompact)");
                        Some(ok)
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "compaction: LLM summary unavailable — falling back to the non-LLM cascade"
                        );
                        None
                    }
                }
            }
            _ => None,
        };

        let (mut compacted, result) = match llm_outcome {
            Some(ok) => ok,
            None => match compactor.compact(pre_compact.clone(), target, keep).await {
                Ok(ok) => ok,
                Err(e) => {
                    tracing::warn!(error = %e, "failed to compact messages, continuing with full context");
                    let tokens = self.context_tokens() as u64;
                    let _ =
                        self.telemetry_handle
                            .record(telemetry::TelemetryEvent::compact_action(
                                &self.session.session_id,
                                self.session.turn_count,
                                Some(self.current_turn_id.clone()),
                                telemetry::CompactActionPayload {
                                    strategy: "error".into(),
                                    before_tokens: tokens,
                                    after_tokens: tokens,
                                    before_message_count: messages_before,
                                    after_message_count: messages_before,
                                    success: false,
                                    latency_ms: compact_start.elapsed().as_millis() as u64,
                                },
                            ));
                    return Err(NotCompacted::Failed(e.to_string()));
                }
            },
        };
        if result.strategy == compaction::compact::CompactStrategy::NoOp {
            return Err(NotCompacted::NothingToDrop);
        }

        let recovery =
            self.post_compact_recovery(&pre_compact, &compacted, result.strategy.drops_history());
        compacted.splice(0..0, recovery);
        let tokens_before = compaction::grouping::estimate_tokens(&pre_compact);
        let tokens_after = compaction::grouping::estimate_tokens(&compacted);
        if tokens_after >= tokens_before {
            tracing::info!(
                strategy = ?result.strategy,
                tokens_before,
                tokens_after,
                "compaction would not have shrunk the conversation; left as it was"
            );
            return Err(NotCompacted::NothingToDrop);
        }
        if result.strategy == compaction::compact::CompactStrategy::MicroCompact {
            self.tool_results_ever_cleared = true;
        }
        let messages_after = compacted.len();
        if let Err(e) = self
            .session
            .replace_messages_after_compact(compacted, tokens_before as u64, tokens_after as u64)
            .await
        {
            tracing::warn!(
                error = %e,
                "the compaction marker is not on disk yet; the next persist writes it"
            );
        }
        let summary = CompactSummary {
            strategy: format!("{:?}", result.strategy),
            messages_before,
            messages_after,
            tokens_before,
            tokens_after,
        };
        self.report_compaction(&summary, result.projection.as_ref(), compact_start);
        // The warning can fire again once the budget fills up again.
        self.compact_warning_issued = false;

        // Two pieces of context are injected once per session, as
        // `<system-reminder>` user messages, and are therefore ordinary
        // compaction victims: the opening context message — session facts and
        // the project instructions — gated by `session_context_injected`, and
        // recalled memories, whose names go into `frozen.already_surfaced` so
        // they are not re-injected turn after turn. Neither is part of the
        // recovery above. Resetting both re-injects nothing here; it makes the
        // next turn's injection paths eligible again. A strategy that kept the
        // opening message keeps the flag.
        self.session_context_injected = holds_session_context(&self.session.messages);
        if let Some(ref mut frozen) = self.frozen {
            frozen.already_surfaced.clear();
        }

        tracing::info!(
            strategy = ?result.strategy,
            messages_before,
            messages_after,
            tokens_before,
            tokens_after,
            trigger = ?trigger,
            "compaction completed"
        );
        tracing::debug!(
            "{}",
            compaction::compact::format_context_analysis(&compaction::compact::analyze_context(
                &self.session.messages
            ))
        );

        if self
            .hooks
            .has_hooks_for(hooks::config::HookEvent::PostCompact)
        {
            let _ = self
                .hooks
                .run(
                    hooks::config::HookEvent::PostCompact,
                    &hooks::HookInput {
                        hook_event_name: "PostCompact".into(),
                        session_id: self.session.session_id.clone(),
                        cwd: self.settings.paths.project_root().display().to_string(),
                        permission_mode: hook_permission_mode(self.session_state.permission_mode()),
                        tool_input: Some(serde_json::json!({
                            "strategy": summary.strategy,
                            "messages_before": messages_before,
                            "messages_after": messages_after,
                            "tokens_before": tokens_before,
                            "tokens_after": tokens_after,
                        })),
                        tool_name: None,
                        tool_use_id: None,
                        tool_result: None,
                        is_error: None,
                        user_prompt: None,
                        ..Default::default()
                    },
                )
                .await;
        }
        Ok(summary)
    }

    /// Whether the `PreCompact` hooks let a compaction aimed at `target` run.
    async fn pre_compact_hooks_allow(&self, target: usize, trigger: CompactTrigger) -> bool {
        if !self
            .hooks
            .has_hooks_for(hooks::config::HookEvent::PreCompact)
        {
            return true;
        }
        let hook_result = self
            .hooks
            .run(
                hooks::config::HookEvent::PreCompact,
                &hooks::HookInput {
                    hook_event_name: "PreCompact".into(),
                    session_id: self.session.session_id.clone(),
                    cwd: self.settings.paths.project_root().display().to_string(),
                    permission_mode: hook_permission_mode(self.session_state.permission_mode()),
                    tool_input: Some(serde_json::json!({
                        "messages_before": self.session.messages.len(),
                        "token_count": self.context_tokens(),
                        "threshold": target,
                        "trigger": trigger.as_str(),
                    })),
                    tool_name: None,
                    tool_use_id: None,
                    tool_result: None,
                    is_error: None,
                    user_prompt: None,
                    ..Default::default()
                },
            )
            .await;
        if hook_result.discontinued() {
            tracing::info!("PreCompact hook discontinued — skipping compaction");
            return false;
        }
        if hook_result.blocked().is_some() {
            tracing::info!("PreCompact hook blocked — skipping compaction");
            return false;
        }
        true
    }

    /// What goes back in front of what a compaction `kept` of `pre_compact`:
    /// the files whose reads it dropped, the skills invoked, the deferred
    /// tools loaded, and — when it dropped turns — plan mode and the
    /// background tasks still running.
    fn post_compact_recovery(
        &self,
        pre_compact: &[ModelMessage],
        kept: &[ModelMessage],
        drops_history: bool,
    ) -> Vec<ModelMessage> {
        let still_there: HashSet<(String, String)> =
            compaction::compact::extract_recent_reads(kept)
                .into_iter()
                .collect();
        let recent_files = compaction::compact::extract_recent_reads(pre_compact)
            .into_iter()
            .filter(|read| !still_there.contains(read))
            .collect();
        // Each invoked skill's *current* content is resolved here because the
        // `compaction` crate does not depend on `skills`. A name that no
        // longer resolves still gets a record, so it is mentioned by name.
        let invoked_skills = self
            .invoked_skills
            .iter()
            .map(|name| compaction::compact::InvokedSkillRecord {
                name: name.clone(),
                content: self.skills.body(name),
                last_seq: self.skills.last_invoked_seq(name),
            })
            .collect();
        let mut activated_tools: Vec<String> =
            self.session_state.activated_tools().into_iter().collect();
        activated_tools.sort();
        compaction::compact::build_post_compact_recovery(&compaction::compact::PostCompactContext {
            recent_files,
            invoked_skills,
            in_plan_mode: drops_history
                && self.session_state.permission_mode() == base::permission::PermissionMode::Plan,
            activated_tools,
            running_tasks: if drops_history {
                self.session_state
                    .running_background_tasks()
                    .iter()
                    .map(|task| (task.task_id.clone(), running_task_summary(task)))
                    .collect()
            } else {
                Vec::new()
            },
        })
    }

    /// What any compaction puts back whatever it drops — skills, plan mode,
    /// tasks, loaded tools. Taken off the target, so that the recovery does
    /// not carry the conversation straight back over the threshold. Files are
    /// left out: one is re-attached only when its read was dropped, and the
    /// read cost at least as much.
    fn recovery_reserve(&self) -> usize {
        compaction::grouping::estimate_tokens(&self.post_compact_recovery(&[], &[], true))
    }

    /// The `compact` event and its telemetry.
    fn report_compaction(
        &self,
        summary: &CompactSummary,
        projection: Option<&compaction::compact::SnipProjection>,
        started: std::time::Instant,
    ) {
        let _ = self.event_tx.send(AgentEvent::CompactAction {
            strategy: summary.strategy.clone(),
            messages_before: summary.messages_before,
            messages_after: summary.messages_after,
            turn_id: self.current_turn_id.clone(),
            dropped_rounds: projection.map(|p| p.dropped_rounds),
            dropped_messages: projection.map(|p| p.dropped_messages),
            estimated_tokens_saved: projection.map(|p| p.estimated_tokens_saved),
            tokens_before: summary.tokens_before,
            tokens_after: summary.tokens_after,
        });
        let _ = self
            .telemetry_handle
            .record(telemetry::TelemetryEvent::compact_action(
                &self.session.session_id,
                self.session.turn_count,
                Some(self.current_turn_id.clone()),
                telemetry::CompactActionPayload {
                    strategy: summary.strategy.clone(),
                    before_tokens: summary.tokens_before as u64,
                    after_tokens: summary.tokens_after as u64,
                    before_message_count: summary.messages_before,
                    after_message_count: summary.messages_after,
                    success: true,
                    latency_ms: started.elapsed().as_millis() as u64,
                },
            ));
    }

    /// Build MCP server instructions text for system prompt injection.
    /// Per-server and total length are already bounded by
    /// `McpManager::server_instructions()` — see that method's doc comment.
    fn build_mcp_instructions(&self) -> String {
        let instructions = self.mcp.instructions();
        if instructions.is_empty() {
            return String::new();
        }
        let mut text = String::from(
            "# MCP Server Instructions\n\n\
             The following MCP servers have provided instructions for how to \
             use their tools and resources:\n\n",
        );
        for instr in &instructions {
            text.push_str(&format!("## {}\n{}\n\n", instr.name, instr.instructions));
        }
        text
    }

    /// Build skills text for system prompt injection.
    ///
    /// The listing always names every skill. Under budget pressure the
    /// *description* is what gives way — and it gives way whole-entry, not
    /// shortened everywhere at once: the least-invoked skills lose their
    /// description first (degrading to name-only), while the most-invoked
    /// skills keep their full (1536-char-capped) text — not every
    /// description shrunk proportionally regardless of use, which would mean
    /// a single heavily-used skill degraded exactly as much as one never
    /// invoked.
    fn build_skills_text(&self) -> String {
        let skills = self.skills.list();
        // Filter out skills with disable_model_invocation: true
        let llm_skills: Vec<_> = skills
            .iter()
            .filter(|s| !s.disable_model_invocation)
            .collect();
        if llm_skills.is_empty() {
            return String::new();
        }

        // Budget-aware skill listing: 1% of context window tokens * 4
        // chars/token, fallback 8000 chars.
        let context_window = self.context_budget().compact_threshold;
        let budget_chars = if context_window > 0 {
            ((context_window as f64 * 0.01) * 4.0) as usize
        } else {
            8000
        };
        const HEADER_CHARS: usize = 22;
        const PER_ENTRY_OVERHEAD: usize = 6;

        // Fixed per-entry ceiling on `description` + `when_to_use` combined
        // — applies regardless of listing budget (see `combined_skill_text_capped`).
        let combined: Vec<String> = llm_skills
            .iter()
            .map(|s| combined_skill_text_capped(&s.description, s.when_to_use.as_deref()))
            .collect();

        let available_budget = budget_chars.saturating_sub(HEADER_CHARS);
        let names: Vec<&str> = llm_skills.iter().map(|s| s.name.as_str()).collect();
        let counts: Vec<u32> = names
            .iter()
            .map(|n| self.skills.invocation_count(n))
            .collect();
        let entry_lens: Vec<usize> = combined.iter().map(|c| c.len()).collect();
        let keeps_description = select_skills_keeping_description(
            &names,
            &counts,
            &entry_lens,
            available_budget,
            PER_ENTRY_OVERHEAD,
        );

        let mut text = String::from(
            "## Available Skills\n\n\
             Current as of this turn; it overrides anything said earlier about \
             which skills exist.\n\n",
        );
        for (i, s) in llm_skills.iter().enumerate() {
            if !keeps_description[i] {
                text.push_str(&format!("- **{}**\n", s.name));
                continue;
            }
            let desc = &combined[i];
            if let Some(when) = &s.argument_hint {
                text.push_str(&format!("- **{}**: {} (args: {})\n", s.name, desc, when));
            } else {
                text.push_str(&format!("- **{}**: {}\n", s.name, desc));
            }
        }
        text
    }

    /// Build the instruction-file context injected once per session as a
    /// `<system-reminder>` (the `# claudeMd` block).
    ///
    /// `FrozenContext::collect` walks the directory tree upward and gathers
    /// every `AGENTS.md` it finds into `memory_blocks`
    /// (`collect_memory_files_with`), so a session rooted in a sub-package of
    /// a monorepo also sees the repository-root conventions.
    ///
    /// **Precedence: root-first, nearest-last, nearest wins.** Files are
    /// emitted farthest-ancestor first and the most specific file last
    /// (`collect_memory_files_with` already returns them in that order), so
    /// the closest file is what the model reads most recently, and the header
    /// states the rule explicitly rather than relying on position alone. The
    /// session's own `instruction_file` — which is usually the nearest
    /// `AGENTS.md` and therefore already in `memory_blocks` — is appended
    /// last if its content isn't already present. Deduplication is by trimmed
    /// content, not path, since `instruction_file` may be an alias or symlink
    /// of a file the walk already canonicalized under a different name.
    fn build_instruction_context(&self) -> String {
        let mut sections: Vec<(String, String)> = Vec::new();
        if let Some(ref frozen) = self.frozen {
            for entry in &frozen.memory_blocks {
                if entry.content.trim().is_empty() {
                    continue;
                }
                sections.push((entry.path.display().to_string(), entry.content.clone()));
            }
        }
        if let Some(ref own) = self.claude_md_content {
            let already = sections.iter().any(|(_, c)| c.trim() == own.trim());
            if !already && !own.trim().is_empty() {
                sections.push((
                    self.settings
                        .instruction_file
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "instruction file".to_string()),
                    own.clone(),
                ));
            }
        }

        match sections.len() {
            0 => String::new(),
            // A single file needs no precedence preamble and no path header —
            // keep the payload byte-identical to what it was before parent
            // files were merged in, which is the overwhelmingly common
            // non-monorepo case.
            1 => sections.pop().map(|(_, c)| c).unwrap_or_default(),
            _ => {
                let mut out = String::from(
                    "Instructions from multiple AGENTS.md files apply, listed from the \
                     outermost directory to the innermost. Where two files conflict, the \
                     later (more specific, closer to the working directory) one wins.\n",
                );
                for (path, content) in sections {
                    out.push_str(&format!("\n## {path}\n\n{}\n", content.trim_end()));
                }
                out
            }
        }
    }

    /// The conversation's opening `<system-reminder>`: the scene's session
    /// facts, the project instructions and today's date, as one user message.
    async fn inject_session_context(&mut self) {
        let model = self.settings.model.model_name.clone();
        let facts = {
            let ctx = build_prompt_context(
                &self.settings,
                self.frozen.as_ref(),
                None,
                None,
                &model,
                self.tool_results_ever_cleared,
                &*self.environment,
            );
            self.scene.build_session_context(&ctx)
        };
        let instructions = self.build_instruction_context();
        let today = chrono_now(&*self.environment);

        let mut text = String::from(SESSION_CONTEXT_OPENING);
        if !facts.is_empty() {
            text.push_str(&format!("# environment\n{facts}\n\n"));
        }
        if !instructions.is_empty() {
            text.push_str(&format!("# claudeMd\n{instructions}\n\n"));
        }
        text.push_str(&format!(
            "# currentDate\n\
             Today's date is {today}.\n\n\
             IMPORTANT: this context may or may not be relevant to your tasks. \
             You should not respond to this context unless it is highly relevant \
             to your task.\n\
             </system-reminder>"
        ));
        self.session.push_message(ModelMessage {
            role: MessageRole::User,
            content: vec![ModelContentBlock::Text { text }],
        });
        self.session_context_injected = true;

        if !instructions.is_empty() {
            let bytes = instructions.len();
            self.fire_lifecycle_hook(hooks::HookEvent::InstructionsLoaded, |i| {
                i.with_reason(format!("{bytes} bytes of project instructions injected"))
            })
            .await;
        }
        if self.agent_depth == 0 {
            let loaded: Vec<std::path::PathBuf> = self
                .frozen
                .iter()
                .flat_map(|f| f.memory_blocks.iter().map(|e| e.path.clone()))
                .collect();
            if let Err(e) = self.hooks.watch_files(&loaded) {
                tracing::warn!(error = %e, "could not watch the instruction files for FileChanged");
            }
        }
    }

    /// Assemble the system-prompt blocks for one API call.
    ///
    /// Shared by the normal path *and* both recovery paths (overloaded →
    /// fallback model, prompt-too-long → compact and retry), so a recovery
    /// call keeps the skill inventory and MCP guidance. Dropping them would
    /// not help a prompt-too-long: skills text is budgeted to ~1% of the
    /// compact threshold and MCP instructions are a few KB.
    async fn build_prompt_blocks(&self, effective_model: &str) -> Vec<PromptBlock> {
        let mcp_instructions = self.build_mcp_instructions();
        let mcp_ref: Option<&str> = if mcp_instructions.is_empty() {
            None
        } else {
            Some(&mcp_instructions)
        };
        let skills_text = self.build_skills_text();
        let skills_ref: Option<&str> = if skills_text.is_empty() {
            None
        } else {
            Some(&skills_text)
        };
        let mut seen_names = HashSet::new();
        let tool_names: String = self
            .tools
            .list()
            .iter()
            .map(|t| t.name().to_string())
            .chain(self.mcp.tools().into_iter().map(|t| t.name().to_string()))
            .filter(|n| seen_names.insert(n.clone()))
            .collect::<Vec<_>>()
            .join(",");
        let tools_ref: Option<Cow<'_, str>> = if tool_names.is_empty() {
            None
        } else {
            Some(Cow::Owned(tool_names))
        };
        let mut ctx = build_prompt_context(
            &self.settings,
            self.frozen.as_ref(),
            mcp_ref,
            skills_ref,
            effective_model,
            self.tool_results_ever_cleared,
            &*self.environment,
        );
        ctx.available_tools = tools_ref;
        let blocks =
            self.prompt_assembler
                .assemble(&base::interface::prompt_assembler::AssemblyRequest {
                    registry: self.prompt_registry.as_ref(),
                    scene: self.scene.as_ref(),
                    settings: &self.settings,
                    memory_store: &self.memory_store,
                    ctx: &ctx,
                    skills_text: skills_ref,
                    mcp_instructions: mcp_ref,
                });
        // Passes that have to await something — a script carrier, most of
        // all. Kept out of `assemble_prompt_with` so assembly stays a
        // synchronous function for every caller that registers none.
        base::interface::prompt_assembly::run_async_assembly_hooks(
            blocks,
            &self.prompt_registry.async_assembly_hooks(),
            &ctx,
        )
        .await
    }

    /// Locate this turn's user message in the request that is about to go out.
    ///
    /// Searches from the end because a turn's later steps append tool-result
    /// user messages after it. Returns `None` when compaction has folded the
    /// message away — an absent map says "not present", which is true, where a
    /// stale index would say something false.
    fn resolve_input_map(
        &self,
        messages: &[ModelMessage],
    ) -> Option<base::interface::model::InputMap> {
        resolve_input_map(self.pending_input.as_ref(), messages)
    }

    async fn build_prompt_for_turn(
        &mut self,
        effective_model: &str,
    ) -> (Vec<PromptBlock>, Vec<ToolDef>, Vec<ModelMessage>) {
        let prompt_blocks = self.build_prompt_blocks(effective_model).await;
        let tool_defs = self.build_tool_defs().await;
        // Remember what this request costs *before* any conversation content,
        // so the next turn's compaction check can include it — see
        // `request_overhead_tokens` and `context_tokens`.
        self.request_overhead_tokens = estimate_request_overhead(&prompt_blocks, &tool_defs);
        let messages = pair_dangling_tool_uses(self.session.messages().to_vec());
        (prompt_blocks, tool_defs, messages)
    }

    async fn handle_context(
        &mut self,
        request: crate::agent::ContextRequest,
    ) -> Result<serde_json::Value, String> {
        use crate::agent::ContextRequest;
        let count = self.session.messages().len();
        let unchanged = |total: Option<usize>| match total {
            Some(total) if total != count => Err(format!(
                "the context has {count} messages, not {total}: it changed since it was read"
            )),
            _ => Ok(()),
        };
        match request {
            ContextRequest::Inspect => Ok(self.inspect_context().await),
            ContextRequest::Drop { at, total } => {
                unchanged(total)?;
                for &i in &at {
                    match self.session.messages().get(i) {
                        None => return Err(format!("there is no message {i}; the context has {count}")),
                        Some(m) if m.role != base::interface::model::MessageRole::User => {
                            return Err(format!(
                                "message {i} is the model's; only user messages and tool results can be dropped"
                            ))
                        }
                        Some(_) => {}
                    }
                }
                for &i in &at {
                    self.session.drop_message(i)?;
                }
                Ok(serde_json::json!({"dropped": at}))
            }
            ContextRequest::Edit { at, text, total } => {
                unchanged(total)?;
                self.session.edit_message(at, text)?;
                Ok(serde_json::json!({"edited": at}))
            }
        }
    }

    /// What the next model call would carry if a step started now with no new
    /// input: the system blocks and who contributed each, the tool table, the
    /// messages, and roughly what each costs. Assembled the way a step
    /// assembles its request, minus what only a real call should do — it
    /// records no request overhead and spends no pending cache edits.
    ///
    /// A real next turn differs by what it adds: the user's message, any
    /// memories recalled for it, anything a hook injects on submit.
    async fn inspect_context(&mut self) -> serde_json::Value {
        let tokens_of = |text: &str| {
            compaction::grouping::estimate_tokens(&[ModelMessage {
                role: base::interface::model::MessageRole::User,
                content: vec![ModelContentBlock::Text {
                    text: text.to_string(),
                }],
            }])
        };
        let model = self.settings.model.model_name.clone();
        let blocks = self.build_prompt_blocks(&model).await;
        let tools = self.build_tool_defs().await;
        let messages = pair_dangling_tool_uses(self.session.messages().to_vec());

        let system: Vec<serde_json::Value> = blocks
            .iter()
            .map(|b| {
                serde_json::json!({
                    "name": b.name,
                    "role": b.role,
                    "origin": b.origin,
                    "tokens": tokens_of(&b.content),
                    "text": b.content,
                })
            })
            .collect();
        let tools: Vec<serde_json::Value> = tools
            .iter()
            .map(|t| {
                let schema = t.input_schema.to_string();
                serde_json::json!({
                    "name": t.name,
                    "source": t.source,
                    "schema_bytes": schema.len(),
                    "tokens": tokens_of(&t.description) + tokens_of(&schema),
                })
            })
            .collect();
        let message_tokens = compaction::grouping::estimate_tokens(&messages);
        let sum = |items: &[serde_json::Value]| {
            items
                .iter()
                .filter_map(|i| i["tokens"].as_u64())
                .sum::<u64>() as usize
        };
        let total = sum(&system) + sum(&tools) + message_tokens;
        let budget = self.context_budget();
        serde_json::json!({
            "basis": "next_step_without_new_input",
            "model": model,
            "max_tokens": self.settings.model.max_tokens,
            "system": system,
            "tools": tools,
            "messages": {"count": messages.len(), "tokens": message_tokens},
            "total_tokens": total,
            "compact_threshold": budget.compact_threshold,
            "hard_cap": budget.hard_cap,
        })
    }

    /// Assemble a step's request and account for what it costs before any
    /// conversation content (see `request_overhead_tokens`).
    ///
    /// This is also where the pending cache edits are consumed, which is why a
    /// retry after a failed call must go through [`Agent::prepare_retry`]
    /// instead: the edits belong to the call that already spent them.
    async fn prepare_request(
        &mut self,
        model: &str,
        max_tokens: u32,
        origin: Option<base::interface::model::CallOrigin>,
    ) -> ModelRequest {
        let (prompt_blocks, tool_defs, messages) = self.build_prompt_for_turn(model).await;
        let input_map = self.resolve_input_map(&messages);
        ModelRequest {
            prompt_blocks,
            tool_defs,
            messages,
            params: base::interface::model::StreamParams {
                model: model.to_string(),
                max_tokens,
                thinking_mode: self.settings.model.thinking_mode.clone(),
                effort: self.settings.model.effort,
                fallback_model: self.settings.model.fallback_model.clone(),
                cache_edits: self.cached_mc.consume_pending_edits(),
                origin,
                input_map,
                provider_search: None,
            },
        }
    }

    /// Reassemble after a recovery changed the model or the message list.
    ///
    /// The tool table and the messages come from the caller rather than being
    /// rebuilt: the overloaded path retries the *same* conversation against a
    /// different model, while the prompt-too-long path retries a conversation
    /// that compaction just shortened. Only the prompt blocks are rebuilt, so
    /// a fallback model still gets the skills inventory and MCP guidance the
    /// primary one got.
    ///
    /// Neither the overhead estimate nor the cache edits are redone — both
    /// belong to the call that failed.
    async fn prepare_retry(
        &self,
        model: &str,
        max_tokens: u32,
        tool_defs: Vec<ToolDef>,
        messages: Vec<ModelMessage>,
        fallback_model: Option<String>,
        origin: Option<base::interface::model::CallOrigin>,
    ) -> ModelRequest {
        let input_map = self.resolve_input_map(&messages);
        ModelRequest {
            prompt_blocks: self.build_prompt_blocks(model).await,
            tool_defs,
            messages,
            params: base::interface::model::StreamParams {
                model: model.to_string(),
                max_tokens,
                thinking_mode: self.settings.model.thinking_mode.clone(),
                effort: self.settings.model.effort,
                fallback_model,
                cache_edits: Vec::new(),
                origin,
                input_map,
                provider_search: None,
            },
        }
    }

    /// The turn's only call into the model.
    ///
    /// Routing all three paths — the normal one and the two recoveries —
    /// through here is what keeps them from diverging: a new field on
    /// `StreamParams` is filled once rather than in three places that have to
    /// be found first.
    async fn send(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> Result<ModelStream, base::interface::model::ModelError> {
        // The last point at which the engine's idea of the request and the
        // provider's can still be made to differ. Once per model call, never
        // per chunk — see `base::interface::model_interceptor`.
        let mut request = request;
        base::interface::model_interceptor::intercept_request(
            &self.model_interceptors,
            &mut request,
        );
        open_conversation_with_user_blocks(&mut request);
        self.model
            .stream(
                request.prompt_blocks,
                request.tool_defs,
                request.messages,
                request.params,
                cancel,
            )
            .await
    }

    /// Total tokens this session is currently sending per API call: the
    /// conversation plus the fixed per-request overhead.
    ///
    /// `SessionManager::token_count()` counts message content only. That is
    /// the right number for "how much of the context is conversation", and
    /// the wrong one for "are we near the context limit" — the assembled
    /// system prompt (scene blocks, skills inventory, MCP instructions) and
    /// the `tools` array (30+ tools, each with a full JSON schema) are sent
    /// on *every* call and are far from negligible here. Comparing only the
    /// message total against `compact_threshold` therefore under-counted the
    /// real context by a fixed, sizeable margin, and compaction fired later
    /// than it was configured to.
    ///
    /// Zero on the very first turn, before any prompt has been assembled —
    /// which is fine, a session's first turn is never near its budget.
    pub(crate) fn context_tokens(&self) -> usize {
        self.session.token_count() + self.request_overhead_tokens
    }

    /// What the scene asked for, after the deployment's budget policy has had
    /// its say.
    pub(crate) fn context_budget(&self) -> base::interface::budget_policy::ContextBudget {
        self.budget_policy
            .context_budget(&self.scene.token_budget())
    }

    /// Put a model error to the recovery policy.
    ///
    /// The classification is the engine's — which errors *are* an overload and
    /// which *are* a size refusal is a fact about the protocol, not an
    /// opinion. What to do about each is the policy's.
    fn classify_failure(
        &self,
        e: &base::interface::model::ModelError,
    ) -> base::interface::recovery_policy::Recovery {
        let rendered = e.to_string();
        self.recovery_policy
            .on_failure(&model_failure(e, &rendered))
    }

    /// Shorten the conversation after the provider refused it for size.
    ///
    /// The refusal is the measurement here, not the budget: the conversation
    /// that went out was too big whatever the estimate made of it, so the
    /// target is half of what was sent rather than the configured threshold,
    /// which it may well not have reached.
    pub(crate) async fn compact_for_retry(
        &mut self,
        refusal: &base::interface::model::ModelError,
    ) -> Result<(), TurnError> {
        tracing::warn!(error = %refusal, "refused for size; compacting, then retrying");
        let budget = self.context_budget();
        let sent = compaction::grouping::estimate_tokens(self.session.messages());
        let target = (sent / 2)
            .min(budget.compact_threshold)
            .saturating_sub(self.recovery_reserve());
        let keep = budget.compact_keep_recent.min(5);
        self.run_compaction(target, keep, CompactTrigger::Recovery)
            .await
            .map(|_| ())
            .map_err(|why| {
                TurnError::Model(format!(
                    "{refusal}; compacting the conversation to retry {why}"
                ))
            })
    }

    /// Ask the recovery policy what a stopped-early response calls for, and
    /// act on the answer.
    ///
    /// Returns true when the caller should run another round. The ladder
    /// itself — 64K first, 8K after, three attempts, the nudge — is
    /// `DefaultRecovery`'s; what stays here is applying the answer to the
    /// loop's own state, which is the part that has to happen in this order.
    /// A continuation adds nothing: the stopped reply is already the
    /// conversation's last message, which is what the provider continues from.
    fn handle_early_stop(
        &mut self,
        stop_reason: &str,
        max_tokens_recovery: &mut u32,
        effective_max_tokens: &mut u32,
    ) -> bool {
        use base::interface::recovery_policy::StopRecovery;
        let decision = self.recovery_policy.on_early_stop(
            stop_reason,
            &base::interface::recovery_policy::RecoveryAttempt {
                output_limit_escalations: *max_tokens_recovery,
                current_max_tokens: *effective_max_tokens,
            },
        );
        match decision {
            StopRecovery::Accept => false,
            StopRecovery::Continue => {
                tracing::info!(
                    stop_reason,
                    "the reply stopped early; sending it back to continue"
                );
                true
            }
            StopRecovery::RaiseOutputLimit { max_tokens, nudge } => {
                *max_tokens_recovery += 1;
                *effective_max_tokens = max_tokens;
                if let Some(nudge) = nudge {
                    self.session.push_message(ModelMessage {
                        role: MessageRole::User,
                        content: vec![ModelContentBlock::Text { text: nudge }],
                    });
                }
                tracing::info!(
                    recovery = *max_tokens_recovery,
                    max_tokens = *effective_max_tokens,
                    "output limit raised and the call retried"
                );
                true
            }
        }
    }
}

/// Tokens a request spends before any conversation content: the assembled
/// system prompt plus every tool definition's name, description and JSON
/// schema.
///
/// Both are re-sent on every API call, so they are part of the context
/// budget even though `SessionManager::token_count()` — which counts message
/// content only — cannot see them. Measured with the same tokenizer as
/// everything else by wrapping each piece of text in a `Text` block, so this
/// number is directly comparable to the message total rather than being a
/// second, differently-derived estimate (the exact mistake the token-counting
/// unification fixed elsewhere).
///
/// The per-tool JSON is serialized compactly; the API sends the schema as
/// structured JSON rather than a string, so this is an approximation of the
/// same shape, not of the exact bytes.
/// Locate this turn's user message in the request that is about to go out.
///
/// Searches from the end because a turn's later steps append tool-result user
/// messages after it. Returns `None` when compaction has folded the message
/// away — an absent map says "not present", which is true, where a stale index
/// would say something false.
fn resolve_input_map(
    pending: Option<&(u64, Vec<base::interface::model::InputSpan>)>,
    messages: &[ModelMessage],
) -> Option<base::interface::model::InputMap> {
    let (fingerprint, spans) = pending?;
    let user_message = messages.iter().rposition(|m| {
        m.role == MessageRole::User && user_text_fingerprint(m) == Some(*fingerprint)
    })?;
    Some(base::interface::model::InputMap {
        user_message,
        spans: spans.clone(),
    })
}

/// Hash of a message's leading text block, used to find that message again
/// after compaction has reshuffled the list. Only the first block: it is the
/// one holding the prompt, and hashing an attached image would mean hashing
/// megabytes of base64 to identify a message by its text.
fn user_text_fingerprint(message: &ModelMessage) -> Option<u64> {
    use std::hash::{Hash, Hasher};
    match message.content.first() {
        Some(ModelContentBlock::Text { text }) => {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            text.hash(&mut hasher);
            Some(hasher.finish())
        }
        _ => None,
    }
}

fn estimate_request_overhead(prompt_blocks: &[PromptBlock], tool_defs: &[ToolDef]) -> usize {
    let mut blocks: Vec<ModelContentBlock> = prompt_blocks
        .iter()
        .map(|b| ModelContentBlock::Text {
            text: b.content.clone(),
        })
        .collect();
    for t in tool_defs {
        blocks.push(ModelContentBlock::Text {
            text: format!(
                "{}{}{}",
                t.name,
                t.description,
                serde_json::to_string(&t.input_schema).unwrap_or_default()
            ),
        });
    }
    // Via `compaction::grouping`, which is itself a thin delegate to
    // `model::tokens` — the workspace's one real tokenizer. Going through it
    // keeps this number identical in kind to `SessionManager::token_count()`,
    // which is the whole point: the two get added together.
    compaction::grouping::estimate_tokens(&[ModelMessage {
        role: MessageRole::User,
        content: blocks,
    }])
}

/// Context bundle for tool execution — owned to survive async closures.
#[derive(Clone)]
pub(crate) struct ToolExecCtx {
    pub tool_use_id: String,
    pub tools: Arc<dyn base::tool::ToolRegistry>,
    pub cwd: std::path::PathBuf,
    /// Where a `PermitAlways { scope: Local }` answer persists its rule.
    /// Carried from `settings.paths` rather than derived from `cwd` here —
    /// see `PathSettings::local_settings_file` for why the exact location
    /// matters.
    pub local_settings_path: std::path::PathBuf,
    pub session_id: String,
    pub turn_no: u32,
    /// Forwarded into every `ToolContext` this turn builds — see
    /// `Agent::agent_depth`.
    pub agent_depth: u32,
    pub telemetry_handle: telemetry::TelemetryHandle,
    pub turn_id: String,
    pub cancel: tokio_util::sync::CancellationToken,
    pub hooks: Arc<hooks::HookRunner>,
    /// Set by a `PostToolUse` hook that returns `continue: false` — checked
    /// by `process_turn` after `execute_stream` returns to end the turn
    /// early, same outcome as the `Stop` hook's discontinue path. Shared
    /// (not per-call) so any tool in this round's batch can request it.
    pub discontinued: Arc<std::sync::atomic::AtomicBool>,
    /// Real `EngineConfig` derived from `Settings` — see `Agent.config`'s doc
    /// comment. Cloned into `ToolContext.config` instead of the
    /// `defaults_for("unknown")` placeholder.
    pub config: Arc<base::context::EngineConfig>,
    /// Real permission checker — see `Agent.permission`. Consulted once per
    /// tool call in `execute_tool_inner`, after the `PreToolUse` hook step.
    pub permission: Arc<dyn base::interface::permission::Permission>,
    /// The session's one shared `SessionState` — see `Agent.session_state`.
    /// Cloned into every `ToolContext` so state a tool records (read cache,
    /// file snapshots, todos, plan mode) is visible to the next tool call
    /// instead of being dropped with a per-call throwaway.
    pub session_state: Arc<base::context::SessionState>,
    /// How this session asks a person to authorize a call. The turn decides
    /// what to do with the answer — hooks, telemetry, the deadline — and this
    /// decides how the question reaches a human at all.
    pub elicitation: Arc<dyn base::interface::elicitation::Elicitation>,
    /// Rings around every tool call — see
    /// [`base::interface::tool_middleware`]. Empty in every session that
    /// registered none, which is the case dispatch short-circuits.
    pub tool_middleware: Arc<Vec<Arc<dyn base::interface::tool_middleware::ToolMiddleware>>>,
    /// The last hands on a tool's output — see
    /// [`base::interface::tool_result`].
    pub result_transformers: Arc<Vec<Arc<dyn base::interface::tool_result::ToolResultTransformer>>>,
    /// Images returned by tools this round, collected out-of-band.
    ///
    /// The tool-dispatch closure returns `(String, Option<Vec<Value>>)` — text
    /// and deferred `new_messages` — and `streaming::buffer_result` turns that
    /// String into the `ToolResult` block. There is no room in that channel for
    /// binary content, and widening it would mean changing the executor
    /// signature and every call site in `streaming.rs`. Instead each call
    /// deposits its images here (shared across the round's whole batch), and
    /// `attach_tool_result_images` appends them to the tool-result message once
    /// `execute_stream` returns. Order within the message follows tool
    /// completion order, which is fine — images trail all `tool_result` blocks
    /// either way, and the text of each result names its own image.
    pub images: Arc<std::sync::Mutex<Vec<PendingToolImage>>>,
    /// Set on the calls a `RunCode` program makes — see
    /// `ToolContext::from_code`.
    pub from_code: bool,
    /// The session's event stream, for what dispatch reports on a tool's
    /// behalf (a `PushNotification`). `None` where nobody is listening.
    pub events: Option<crate::agent::EventSender>,
}

/// A base64 image a tool returned, waiting to be attached to this round's
/// tool-result message. See [`ToolExecCtx::images`].
#[derive(Debug, Clone)]
pub(crate) struct PendingToolImage {
    pub media_type: String,
    pub data: String,
}

/// Render a `ToolResultContent` into the text the model sees, lifting out any
/// images so the caller can emit them as real `ModelContentBlock::Image`s.
///
/// **The bug this replaces:** the `Blocks` arm was `format!("{:?}", b)` — the
/// model literally received
/// `[ToolResultBlock { block_type: "image", text: None, source: Some(Object {..}) }]`.
/// Text blocks lost their content entirely (it was buried inside `text: Some(..)`
/// in Debug form), and images arrived as a Rust struct dump. Every MCP server
/// returning anything other than one plain string was affected.
///
/// `Blocks` is not an exotic shape — `mcp::adapter::into_tool_result` produces it
/// for any structured MCP result — so this is the normal path for a large class
/// of tools, not an edge case.
fn render_tool_result_content(
    content: base::tool::ToolResultContent,
) -> (String, Vec<PendingToolImage>) {
    match content {
        ToolResultContent::Text(t) => (t, Vec::new()),
        ToolResultContent::Blocks(blocks) => {
            let mut parts: Vec<String> = Vec::new();
            let mut images: Vec<PendingToolImage> = Vec::new();
            for block in &blocks {
                if let Some((media_type, data)) = block.as_image() {
                    images.push(PendingToolImage {
                        media_type: media_type.to_string(),
                        data: data.to_string(),
                    });
                    continue;
                }
                // Any non-image block contributes whatever text it carries.
                // An unknown `block_type` with text is still worth showing;
                // one with neither text nor a usable image source has nothing
                // to contribute and is dropped rather than Debug-printed.
                if let Some(text) = block.text.as_ref() {
                    if !text.is_empty() {
                        parts.push(text.clone());
                    }
                }
            }
            let mut text = parts.join("\n");
            if text.is_empty() && !images.is_empty() {
                // A tool_result whose content is the empty string is not useful
                // to the model (and some providers reject it) — name what the
                // accompanying Image blocks are instead.
                text = if images.len() == 1 {
                    "[image]".to_string()
                } else {
                    format!("[{} images]", images.len())
                };
            }
            (text, images)
        }
    }
}

/// Attach `images` to the tool-result message `execute_stream` just pushed.
///
/// **Shape, and why it is API-valid:** the images become
/// `ModelContentBlock::Image` blocks appended *after* every `ToolResult` block in
/// the same user message. `model::adapter::to_content_block` already maps that
/// variant to `base::message::ContentBlock::Image { source: ImageSource::Base64 }`,
/// so no new wire mapping is involved, and a user message's content array is a
/// heterogeneous list that accepts `image` blocks alongside `tool_result` ones.
/// Appending (never inserting) is the load-bearing detail: the API requires a
/// user turn's `tool_result` blocks to come first, so images must trail them.
///
/// This is deliberately *not* done by rewriting `ModelContentBlock::ToolResult`
/// to hold structured content. That variant has ~32 construction sites and ~116
/// match sites across 17 files; widening it to carry images would be a
/// workspace-wide refactor to express something the sibling-block form already
/// expresses correctly.
fn attach_tool_result_images(messages: &mut [ModelMessage], images: Vec<PendingToolImage>) {
    let target = messages.iter_mut().rev().find(|m| {
        m.role == MessageRole::User
            && m.content
                .iter()
                .any(|b| matches!(b, ModelContentBlock::ToolResult { .. }))
    });
    let Some(msg) = target else {
        // No tool-result message to hang them on (the round produced no tool
        // results at all). Dropping is correct: an orphan image block with no
        // surrounding context is noise, and we must not fabricate a user turn.
        tracing::debug!(
            count = images.len(),
            "tool returned images but no tool_result message was pushed; dropping"
        );
        return;
    };
    for image in images {
        msg.content.push(ModelContentBlock::Image {
            media_type: image.media_type,
            data: image.data,
        });
    }
}

/// Deliver a host's answer to the `execute_tool_inner` call awaiting it.
///
/// A missing entry means the prompt already resolved (timed out, or the
/// session was cancelled) or this is a stale/duplicate answer — either way
/// there is nothing left to wake, so it is dropped rather than treated as an
/// error.
pub(crate) fn resolve_permission_response(
    pending: &Arc<
        std::sync::Mutex<
            std::collections::HashMap<
                String,
                tokio::sync::oneshot::Sender<crate::agent::PermissionDecision>,
            >,
        >,
    >,
    denial_count: &Arc<std::sync::atomic::AtomicU32>,
    prompt_id: String,
    decision: crate::agent::PermissionDecision,
) {
    if let crate::agent::PermissionDecision::Deny { .. } = decision {
        denial_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    if let Some(sender) = pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&prompt_id)
    {
        let _ = sender.send(decision);
    }
}

/// `Agent::fire_lifecycle_hook`'s counterpart for the free functions in the
/// tool-dispatch path, which hold a `ToolExecCtx` rather than `&self`.
///
/// Same contract: notification only, response discarded, guarded on
/// `has_hooks_for` because this sits on the per-tool-call hot path.
async fn fire_ctx_lifecycle_hook(
    ctx: &ToolExecCtx,
    event: hooks::HookEvent,
    build: impl FnOnce(hooks::HookInput) -> hooks::HookInput,
) {
    if !ctx.hooks.has_hooks_for(event) {
        return;
    }
    let input = build(
        hooks::HookInput::lifecycle(
            format!("{event:?}"),
            ctx.session_id.clone(),
            ctx.cwd.display().to_string(),
            hook_permission_mode(ctx.session_state.permission_mode()),
        )
        .with_turn_id(ctx.turn_id.clone()),
    );
    let _ = ctx.hooks.run(event, &input).await;
}

/// The head of a failed call's text, as much of it as a telemetry line keeps.
fn telemetry_error_text(text: &str) -> String {
    const KEPT_CHARS: usize = 512;
    match text.char_indices().nth(KEPT_CHARS) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_string(),
    }
}

fn record_permission_decision(
    ctx: &ToolExecCtx,
    tool: &Arc<dyn base::tool::Tool>,
    name: &str,
    input: &serde_json::Value,
    checked: &base::interface::permission::PermissionOutcome,
    started: std::time::Instant,
) {
    use base::interface::permission::PermissionOutcome;
    let outcome = match checked {
        PermissionOutcome::Permit => telemetry::PermissionDecisionOutcome::AutoAllow,
        PermissionOutcome::Deny { .. } => telemetry::PermissionDecisionOutcome::Deny,
        PermissionOutcome::Prompt { .. } => telemetry::PermissionDecisionOutcome::Ask,
    };
    let _ = ctx
        .telemetry_handle
        .record(telemetry::TelemetryEvent::permission_decision(
            &ctx.session_id,
            ctx.turn_no,
            Some(ctx.turn_id.clone()),
            telemetry::PermissionDecisionPayload {
                tool_name: name.to_string(),
                outcome,
                permission_mode: hook_permission_mode(ctx.session_state.permission_mode()),
                destructive: tool.is_destructive(input),
                latency_ms: started.elapsed().as_millis() as u64,
            },
        ));
}

/// A `PushNotification` reaches the host as a `notification` frame and the
/// `Notification` hook, which is the same hook a pending permission prompt
/// rings.
async fn notify_host(ctx: &ToolExecCtx, input: &serde_json::Value) {
    let Some(message) = input.get("message").and_then(|m| m.as_str()).map(str::trim) else {
        return;
    };
    let status = input
        .get("status")
        .and_then(|s| s.as_str())
        .map(str::to_string);
    if let Some(events) = &ctx.events {
        let _ = events.send(AgentEvent::Notification {
            message: message.to_string(),
            status,
            turn_id: ctx.turn_id.clone(),
        });
    }
    fire_ctx_lifecycle_hook(ctx, hooks::HookEvent::Notification, |i| {
        i.with_tool("PushNotification", input.clone())
            .with_reason(message)
    })
    .await;
}

/// Execute a single tool and record telemetry. Free function for streaming executor.
pub(crate) async fn execute_tool_with_telemetry(
    ctx: &ToolExecCtx,
    name: &str,
    input: serde_json::Value,
) -> base::interface::tool_middleware::ToolOutcome {
    // No wrappers means no wrapping: the call goes straight through, with no
    // context clone and no future inside a future. A seam nobody uses should
    // cost nothing on the hot path.
    if ctx.tool_middleware.is_empty() {
        return execute_tool_inner(ctx, name, input).await;
    }
    let chain = Arc::clone(&ctx.tool_middleware);
    let call = base::interface::tool_middleware::ToolCall {
        name: name.to_string(),
        input: input.clone(),
    };
    base::interface::tool_middleware::dispatch_through(
        &chain,
        call,
        ctx.cancel.clone(),
        move |cancel| {
            // Each pass through gets the signal the wrappers settled on. The
            // rest of the context is the same one dispatch always had, which
            // is what keeps a retry a retry rather than a differently
            // configured call.
            let mut ctx = ctx.clone();
            ctx.cancel = cancel;
            let name = name.to_string();
            let input = input.clone();
            async move { execute_tool_inner(&ctx, &name, input).await }
        },
    )
    .await
}

/// Lower `EngineConfig`'s sandbox policy into the cross-crate view a tool
/// receives.
///
/// `deny_read`'s `None` means "use the built-in credential deny defaults",
/// and `SandboxSettings` has no way to say that — it carries a plain `Vec`,
/// with the same empty-means-defaults convention that
/// `tools::bash::to_sandbox_policy` reads on the other side. So `None` and
/// `Some(vec![])` both flatten to an empty vec here, and the distinction is
/// preserved where it is actually decided (`EngineConfig::from_settings`).
fn sandbox_settings_from(config: &base::context::EngineConfig) -> base::tool::SandboxSettings {
    base::tool::SandboxSettings {
        allow_read: config.sandbox_policy.allow_read.clone(),
        deny_read: config.sandbox_policy.deny_read.clone().unwrap_or_default(),
        allowed_domains: config.sandbox_policy.allowed_domains.clone(),
        network_mode: config.sandbox_policy.network_mode,
        deny_write: config.sandbox_policy.deny_write.clone(),
        allow_write: config.sandbox_policy.allow_write.clone(),
        state_root: config.sandbox_policy.state_root.clone(),
        require_enforcement: config.sandbox_policy.require_enforcement,
        unattended_requires_network_policy: config
            .sandbox_policy
            .unattended_requires_network_policy,
    }
}

/// How the engine starts its own git for this session: in the sandbox
/// [`exec_providers_from`] gives its tool calls, whenever it confines them.
pub(crate) fn engine_git_from(
    config: &base::context::EngineConfig,
    session_id: &str,
) -> base::frozen::git::EngineGit {
    tools::bash::engine_git_for(
        &sandbox_settings_from(config),
        Arc::new(base::interface::exec::local::PlatformSandbox),
        config.dangerously_disable_sandbox,
        session_id,
    )
}

/// The execution providers a tool call reaches the machine through.
///
/// Only the egress is configured from `EngineConfig`; the other three are
/// this machine either way. `sandbox.network_mode` is a statement about where
/// the *model* may reach, which is as true of a `WebFetch` as it is of a
/// `curl` inside a sandboxed command.
fn exec_providers_from(
    config: &base::context::EngineConfig,
) -> base::interface::exec::ExecProviders {
    let env = config.subprocess_env.clone();
    match &config.process_groups {
        Some(ledger) => {
            base::interface::exec::ExecProviders::local_with_ledger(env, ledger.clone())
        }
        None => base::interface::exec::ExecProviders::local_with(env),
    }
    .with_network(Arc::new(
        base::interface::exec::local::LocalNetwork::for_agent_policy(
            config.sandbox_policy.network_mode,
            config.sandbox_policy.allowed_domains.clone(),
        ),
    ))
}

/// Sub-calls a tool makes (`RunCode`), through the same dispatch as the
/// model's own calls. Each runs under its own tool-use id derived from the
/// caller's, cannot ask a person anything, and leaves the round's batch alone:
/// a hook ending the turn or an image a sub-call returned are the caller's to
/// report, not the round's.
struct SubCallDispatcher {
    ctx: ToolExecCtx,
    next: std::sync::atomic::AtomicU64,
}

impl SubCallDispatcher {
    fn new(ctx: &ToolExecCtx) -> Self {
        let mut ctx = ctx.clone();
        ctx.elicitation = Arc::new(NoAsking);
        ctx.discontinued = Arc::new(std::sync::atomic::AtomicBool::new(false));
        ctx.images = Arc::default();
        ctx.from_code = true;
        Self {
            ctx,
            next: std::sync::atomic::AtomicU64::new(1),
        }
    }

    fn callable_tool(&self, name: &str) -> Option<Arc<dyn base::tool::Tool>> {
        self.ctx.tools.get(name).filter(|t| t.callable_from_code())
    }
}

#[async_trait::async_trait]
impl base::interface::tool_dispatch::ToolDispatch for SubCallDispatcher {
    fn callable(&self) -> Vec<base::interface::tool_dispatch::CallableTool> {
        let mut tools: Vec<_> = self
            .ctx
            .tools
            .list()
            .into_iter()
            .filter(|t| t.callable_from_code())
            .map(|t| base::interface::tool_dispatch::CallableTool {
                name: t.name().to_string(),
                description: t
                    .short_description()
                    .unwrap_or_else(|| t.description().to_string()),
                input_schema: base::tool::compact_schema(t.full_input_schema()),
                result: t.code_result().map(str::to_string),
            })
            .collect();
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        tools
    }

    fn concurrency_safe(&self, name: &str, input: &serde_json::Value) -> bool {
        self.callable_tool(name)
            .is_some_and(|t| t.is_concurrency_safe(input))
    }

    async fn call(
        &self,
        name: &str,
        input: serde_json::Value,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<base::interface::tool_dispatch::SubCallAnswer, String> {
        if self.callable_tool(name).is_none() {
            return Err(format!("{name} cannot be called from code in this session"));
        }
        let n = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut ctx = self.ctx.clone();
        ctx.tool_use_id = format!("{}.{n}", self.ctx.tool_use_id);
        ctx.cancel = cancel;
        execute_tool_with_telemetry(&ctx, name, input)
            .await
            .map(|a| base::interface::tool_dispatch::SubCallAnswer {
                text: a.text,
                structured: a.structured,
                reported_failure: a.reported_failure,
            })
    }
}

/// The approval a sub-call would need cannot be asked for from inside a
/// program; the model has to make that call itself.
struct NoAsking;

#[async_trait::async_trait]
impl base::interface::elicitation::Elicitation for NoAsking {
    async fn ask(
        &self,
        _: base::interface::elicitation::ElicitRequest,
    ) -> base::interface::elicitation::ElicitOutcome {
        base::interface::elicitation::ElicitOutcome::declined(
            "this call needs the user's approval, which a program cannot ask for — \
             make this call directly instead of from code",
        )
    }
}

/// The context a call to `tool` runs with: the same for validating its input
/// as for running it.
fn tool_context_for(ctx: &ToolExecCtx, tool: &Arc<dyn base::tool::Tool>) -> ToolContext {
    let additional_writable_dirs = ctx.session_state.additional_writable_dirs.clone();
    ToolContext {
        cwd: ctx.cwd.clone(),
        session_id: ctx.session_id.clone(),
        turn_no: ctx.turn_no,
        sandbox: sandbox_settings_from(&ctx.config),
        cancel: ctx.cancel.clone(),
        additional_writable_dirs,
        snapshot_file: None,
        effects: None,
        running_tasks: Some(ctx.session_state.clone()),
        dangerously_disable_sandbox: ctx.config.dangerously_disable_sandbox,
        max_file_read_bytes: ctx.config.file_limits.max_file_read_bytes as usize,
        // The *live* mode, not the session's opening value — plan mode is
        // entered and left mid-session.
        permission_mode: ctx.session_state.permission_mode(),
        config: ctx.config.clone(),
        session: ctx.session_state.clone(),
        tool_use_id: ctx.tool_use_id.clone(),
        agent: None,
        parent_messages: None,
        agent_depth: ctx.agent_depth,
        events_tx: None,
        elicitation: Some(Arc::clone(&ctx.elicitation)),
        exec: exec_providers_from(&ctx.config),
        dispatch: tool.needs_dispatch().then(|| {
            Arc::new(SubCallDispatcher::new(ctx))
                as Arc<dyn base::interface::tool_dispatch::ToolDispatch>
        }),
        from_code: ctx.from_code,
    }
}

impl Agent {
    /// What a skill's `` !`cmd` `` runs with when a `/name` command expands
    /// it outside any tool call: the context a `Bash` call this turn would get.
    fn skill_invocation_context(&self, cancel: CancellationToken) -> ToolContext {
        ToolContext {
            cwd: self.settings.paths.project_root(),
            session_id: self.session.session_id.clone(),
            turn_no: self.session.turn_count,
            sandbox: sandbox_settings_from(&self.config),
            cancel,
            additional_writable_dirs: self.session_state.additional_writable_dirs.clone(),
            snapshot_file: None,
            effects: None,
            running_tasks: Some(self.session_state.clone()),
            dangerously_disable_sandbox: self.config.dangerously_disable_sandbox,
            max_file_read_bytes: self.config.file_limits.max_file_read_bytes as usize,
            permission_mode: self.session_state.permission_mode(),
            config: self.config.clone(),
            session: self.session_state.clone(),
            tool_use_id: String::new(),
            agent: None,
            parent_messages: None,
            agent_depth: self.agent_depth,
            events_tx: None,
            elicitation: Some(Arc::clone(&self.elicitation)),
            exec: exec_providers_from(&self.config),
            dispatch: None,
            from_code: false,
        }
    }
}

/// Under `panic = "unwind"` a tool that panics fails its own call instead of
/// unwinding through the round; under `panic = "abort"` (the `release`
/// profile) the process ends before this ever runs.
async fn run_catching_panics(
    call: impl std::future::Future<Output = Result<base::tool::ToolResult, base::error::ToolError>>,
) -> Result<base::tool::ToolResult, base::error::ToolError> {
    use futures::FutureExt as _;
    match std::panic::AssertUnwindSafe(call).catch_unwind().await {
        Ok(result) => result,
        Err(payload) => Err(base::error::ToolError::exec(format!(
            "tool panicked: {}",
            panic_message(payload.as_ref())
        ))),
    }
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("(non-string panic payload)")
}

async fn execute_tool_inner(
    ctx: &ToolExecCtx,
    name: &str,
    input: serde_json::Value,
) -> base::interface::tool_middleware::ToolOutcome {
    let tool = ctx
        .tools
        .get(name)
        .ok_or_else(|| format!("Tool not found: {name}"))?;

    // PreToolUse: can block the call outright, rewrite its input, or answer
    // the permission question in advance.
    let mut input = input;
    let mut hook_approved = false;
    if ctx.hooks.has_hooks_for(hooks::HookEvent::PreToolUse) {
        let pre_input = hooks::HookInput {
            hook_event_name: "PreToolUse".into(),
            session_id: ctx.session_id.clone(),
            cwd: ctx.cwd.display().to_string(),
            permission_mode: hook_permission_mode(ctx.session_state.permission_mode()),
            tool_name: Some(name.to_string()),
            tool_input: Some(input.clone()),
            tool_use_id: Some(ctx.tool_use_id.clone()),
            tool_result: None,
            is_error: None,
            user_prompt: None,
            ..Default::default()
        };
        let hook_result = ctx
            .hooks
            .run(hooks::HookEvent::PreToolUse, &pre_input)
            .await;
        if let Some(response) = hook_result.blocked() {
            return Err(format!(
                "Blocked by PreToolUse hook: {}",
                response.message.as_deref().unwrap_or("no reason given")
            ));
        }
        if let Some(updated) = hook_result.updated_input() {
            input = updated.clone();
        }
        hook_approved = hook_result.approved().is_some();
    }

    // Permission check. `Permit` proceeds unchanged; `Deny` blocks the call outright; `Prompt`
    // asks the host and genuinely blocks until it answers — matching
    // conventional confirmation-prompt UX (a CLI `y/n`, `sudo`, ...), not a
    // race against an arbitrary timeout. The one thing that can still
    // interrupt the wait is the *session* going away: `ctx.cancel` is the
    // same token `daemon::SessionPool::shutdown_session` fires on
    // `session.close`/client-disconnect, threaded down through
    // `Agent::run(cancel)` → `process_turn` → `run_user_turn`. If nobody explicitly approved the call before the
    // session tore down, the safe default is to *not* run it.
    // Whatever permission handler the host installed, a call whose input the
    // tool rejects never reaches it.
    if let base::tool::ValidationResult::Err(message, _) = tool
        .validate_input(&input, &tool_context_for(ctx, &tool))
        .await
    {
        return Err(message);
    }

    use base::interface::permission::PermissionOutcome;
    let check_started = std::time::Instant::now();
    let checked = ctx
        .permission
        .check(name, &input, &ctx.cwd, &ctx.session_id)
        .await;
    record_permission_decision(ctx, &tool, name, &input, &checked, check_started);
    let checked = match checked {
        PermissionOutcome::Prompt { .. } if hook_approved => PermissionOutcome::Permit,
        other => other,
    };
    let user_approved = matches!(checked, PermissionOutcome::Prompt { .. });
    match checked {
        PermissionOutcome::Permit => {}
        PermissionOutcome::Deny { reason } => {
            fire_ctx_lifecycle_hook(ctx, hooks::HookEvent::PermissionDenied, |i| {
                i.with_tool(name, input.clone()).with_reason(reason.clone())
            })
            .await;
            return Err(format!("Denied by permission: {reason}"));
        }
        PermissionOutcome::Prompt { message, .. } if ctx.session_state.unattended => {
            let reason = format!(
                "{message} — and nobody is attending this session to approve it. Work within \
                 what is allowed; if the task needs this, report it: the instance's settings \
                 grant it (`sandbox.additional_directories`, `sandbox.read_directories`, or a \
                 `permission_rules` allow rule)."
            );
            fire_ctx_lifecycle_hook(ctx, hooks::HookEvent::PermissionDenied, |i| {
                i.with_tool(name, input.clone()).with_reason(reason.clone())
            })
            .await;
            return Err(format!("Denied by permission: {reason}"));
        }
        PermissionOutcome::Prompt {
            prompt_id,
            message,
            paths,
        } => {
            let permission_wait_start = std::time::Instant::now();
            fire_ctx_lifecycle_hook(ctx, hooks::HookEvent::PermissionRequested, |i| {
                i.with_tool(name, input.clone())
                    .with_reason(message.clone())
            })
            .await;
            // `Notification` is "fired when user attention is required
            // (typically a permission prompt is pending)". A host that wants to
            // ring a bell, raise a desktop notification or ping a channel
            // when the agent is blocked on a human hooks it here.
            fire_ctx_lifecycle_hook(ctx, hooks::HookEvent::Notification, |i| {
                i.with_tool(name, input.clone())
                    .with_reason(format!("waiting for a permission decision on `{name}`"))
            })
            .await;
            let ask = ctx
                .elicitation
                .ask(base::interface::elicitation::ElicitRequest {
                    id: prompt_id.clone(),
                    kind: base::interface::elicitation::ElicitKind::Authorization {
                        tool_name: name.to_string(),
                        paths,
                        tool_use_id: Some(ctx.tool_use_id.clone()),
                    },
                    message,
                    options: crate::elicitation::ChannelElicitation::authorization_options(),
                });
            tokio::pin!(ask);
            // `0` means wait indefinitely; `tokio::select!` needs a future
            // either way, so an unbounded wait becomes one that never
            // resolves rather than a branch that isn't there.
            let prompt_timeout = ctx.config.permission_prompt_timeout_secs;
            let timeout_fut = async move {
                if prompt_timeout == 0 {
                    std::future::pending::<()>().await
                } else {
                    tokio::time::sleep(std::time::Duration::from_secs(prompt_timeout)).await
                }
            };
            tokio::pin!(timeout_fut);
            tokio::select! {
                _ = &mut timeout_fut => {
                    // Fail closed. An unanswered prompt is not consent, and a
                    // host that never answers must not be able to hold a tool
                    // call open forever. Dropping `ask` here is what discards
                    // a late answer for this id — see `ChannelElicitation`'s
                    // registration guard.
                    fire_ctx_lifecycle_hook(ctx, hooks::HookEvent::PermissionDenied, |i| {
                        i.with_tool(name, input.clone())
                            .with_reason("permission prompt timed out")
                    })
                    .await;
                    return Err(format!(
                        "Denied by permission: no answer to the permission prompt within {prompt_timeout}s"
                    ));
                }
                outcome = &mut ask => {
                    match outcome.answer_as::<crate::agent::PermissionDecision>() {
                        Some(crate::agent::PermissionDecision::Permit) => {}
                        Some(crate::agent::PermissionDecision::PermitAlways { scope }) => {
                            // Same content derivation the rule engine itself
                            // uses to match a tool call against a rule (see
                            // `PermissionGate::check` step 3 — `RuleHit`
                            // evaluation) — this is what makes the persisted
                            // pattern actually match future calls the same
                            // way the one the user just answered did.
                            let match_content = tool.permission_match_content(&input);
                            ctx.permission
                                .add_persistent_allow(name, match_content.as_deref());
                            if let crate::agent::PersistScope::Local = scope {
                                let local_settings_path = &ctx.local_settings_path;
                                match permissions::settings_patch::build_rule_string(
                                    name,
                                    match_content.as_deref(),
                                ) {
                                    Some(rule_string) => {
                                        if let Err(e) = permissions::settings_patch::append_permission_rule(
                                            local_settings_path,
                                            permissions::settings_patch::AppendTarget::Allow,
                                            &rule_string,
                                        ) {
                                            tracing::warn!(
                                                path = %local_settings_path.display(),
                                                error = %e,
                                                "failed to persist permit-always rule to settings.local.json; \
                                                 in-memory allow for this session still applies"
                                            );
                                        }
                                    }
                                    None => {
                                        tracing::warn!(
                                            tool_name = %name,
                                            "permit-always with scope=local but no rule content could be \
                                             derived — nothing written to settings.local.json; \
                                             in-memory allow for this session still applies"
                                        );
                                    }
                                }
                            }
                        }
                        Some(crate::agent::PermissionDecision::Deny { reason }) => {
                            // Denied by the *host's* answer to the prompt, as
                            // opposed to the rule-engine `Deny` above. Same
                            // event either way — a hook watching for denials
                            // cares that the call was refused, not by whom.
                            fire_ctx_lifecycle_hook(
                                ctx,
                                hooks::HookEvent::PermissionDenied,
                                |i| i.with_tool(name, input.clone()).with_reason(reason.clone()),
                            )
                            .await;
                            return Err(format!("Denied by permission: {reason}"));
                        }
                        // Declined, or answered with something that is not a
                        // decision. Either way nobody authorized this call, and
                        // the reason is the decliner's own words — a host that
                        // cannot ask says so, and the model is told what it was
                        // told rather than a phrase invented here.
                        None => {
                            let reason = outcome
                                .decline_reason()
                                .unwrap_or("the answer to the permission prompt was not a decision")
                                .to_string();
                            fire_ctx_lifecycle_hook(
                                ctx,
                                hooks::HookEvent::PermissionDenied,
                                |i| i.with_tool(name, input.clone()).with_reason(reason.clone()),
                            )
                            .await;
                            return Err(reason);
                        }
                    }
                }
                _ = ctx.cancel.cancelled() => {
                    let _ = ctx.telemetry_handle.record(telemetry::TelemetryEvent::tool_cancelled(
                        &ctx.session_id,
                        ctx.turn_no,
                        Some(ctx.turn_id.clone()),
                        telemetry::ToolCancelledPayload {
                            tool_name: name.to_string(),
                            reason: "session_closing_during_permission_wait".into(),
                            elapsed_ms: permission_wait_start.elapsed().as_millis() as u64,
                        },
                    ));
                    return Err("permission request cancelled: session closing".into());
                }
            }
        }
    }

    let tool_ctx = tool_context_for(ctx, &tool);
    let input_for_post_hook = input.clone();
    let input_json_size = serde_json::to_string(&input)
        .map(|s| s.len() as u64)
        .unwrap_or(0);
    let _ = ctx
        .telemetry_handle
        .record(telemetry::TelemetryEvent::tool_start(
            &ctx.session_id,
            ctx.turn_no,
            Some(ctx.turn_id.clone()),
            telemetry::ToolStartPayload {
                tool_name: name.to_string(),
                tool_use_id: ctx.tool_use_id.clone(),
                input_json_size,
                destructive: tool.is_destructive(&input),
                deferred: tool.is_deferred(),
            },
        ));
    let tool_start = std::time::Instant::now();
    let result =
        run_catching_panics(tool.call(input, tool_ctx, base::tool::ProgressSender::noop(""))).await;
    let latency_ms = tool_start.elapsed().as_millis() as u64;

    let mut outcome = match result {
        Ok(r) => {
            let (text, images) = render_tool_result_content(r.content);
            if !images.is_empty() {
                ctx.images
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .extend(images);
            }
            Ok(base::interface::tool_middleware::ToolAnswer {
                text,
                new_messages: r.new_messages,
                reported_failure: r.is_error,
                structured: r.structured_content,
            })
        }
        Err(e) => Err(e.to_string()),
    };
    // Both ways a call can go wrong: the engine could not run it, or it ran
    // and called its own result a failure. Reading only `is_err` reported a
    // shell command that exited non-zero as a success.
    let (is_error, error_message, result_content_size) = match &outcome {
        Ok(answer) if answer.reported_failure => (
            true,
            Some(telemetry_error_text(&answer.text)),
            answer.text.len() as u64,
        ),
        Ok(answer) => (false, None, answer.text.len() as u64),
        Err(message) => (true, Some(telemetry_error_text(message)), 0),
    };
    let _ = ctx
        .telemetry_handle
        .record(telemetry::TelemetryEvent::tool_execution(
            &ctx.session_id,
            ctx.turn_no,
            Some(ctx.turn_id.clone()),
            telemetry::ToolExecutionPayload {
                tool_name: name.to_string(),
                tool_use_id: ctx.tool_use_id.clone(),
                outcome: if is_error {
                    telemetry::ToolOutcome::Failed
                } else {
                    telemetry::ToolOutcome::Succeeded
                },
                is_error,
                error_message,
                latency_ms,
                input_json_size,
                result_content_size,
                user_approved,
            },
        ));

    // PostToolUse: the tool already ran, but this can still act two ways.
    //
    // 1. `continue: false` requests the whole turn end. That's a turn-level
    //    decision this function can't act on directly (it only reports this
    //    one call's outcome) — surfaced via `ctx.discontinued`, a shared
    //    flag `process_turn` checks once `execute_stream` returns.
    //
    // 2. `decision: "block"` (the same field `PreToolUse` uses, and the same
    //    `HookRunResult::blocked()` accessor — `blocked()` isn't tied to a
    //    specific event, it just checks for that decision) denies *this*
    //    call's result: `outcome` gets overwritten with an `Err`, which
    //    becomes this tool_use's tool_result content (an error/denial
    //    message instead of whatever the tool actually returned). Turning it
    //    into an `Err` also — for free, not via new plumbing — triggers
    //    `execute_stream`'s "any tool error cancels all siblings in the
    //    same concurrent batch" mechanism (`streaming.rs`'s `batch_abort`),
    //    so in-flight/not-yet-started sibling calls in this batch get
    //    cancelled too.
    //
    // PostToolUseFailure — the error-only sibling of PostToolUse, so a hook
    // that only cares about failures doesn't have to subscribe to every tool
    // call and filter on `is_error`. Fires before PostToolUse, which still
    // sees the failure too (with `is_error: true`) and can still rewrite it.
    if let Err(ref e) = outcome {
        let err = e.clone();
        let inp = input_for_post_hook.clone();
        fire_ctx_lifecycle_hook(ctx, hooks::HookEvent::PostToolUseFailure, |i| {
            i.with_tool(name, inp).with_reason(err)
        })
        .await;
    }

    if ctx.hooks.has_hooks_for(hooks::HookEvent::PostToolUse) {
        let (result_json, is_error_flag) = match &outcome {
            Ok(a) => (
                serde_json::Value::String(a.text.clone()),
                a.reported_failure,
            ),
            Err(e) => (serde_json::Value::String(e.clone()), true),
        };
        let post_input = hooks::HookInput {
            hook_event_name: "PostToolUse".into(),
            session_id: ctx.session_id.clone(),
            cwd: ctx.cwd.display().to_string(),
            permission_mode: hook_permission_mode(ctx.session_state.permission_mode()),
            tool_name: Some(name.to_string()),
            tool_input: Some(input_for_post_hook.clone()),
            tool_use_id: Some(ctx.tool_use_id.clone()),
            tool_result: Some(result_json),
            is_error: Some(is_error_flag),
            user_prompt: None,
            ..Default::default()
        };
        let hook_result = ctx
            .hooks
            .run(hooks::HookEvent::PostToolUse, &post_input)
            .await;
        if let Some(response) = hook_result.blocked() {
            outcome = Err(format!(
                "Denied by PostToolUse hook: {}",
                response.message.as_deref().unwrap_or("no reason given")
            ));
        }
        if hook_result.discontinued() {
            ctx.discontinued
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    // Tool-shaped lifecycle events. `WorktreeCreate` and `WorktreeRemove`
    // describe things a *tool* does, but tools have no access to the `HookRunner` — `ToolContext` deliberately
    // carries no hooks handle, and threading one through every tool for this
    // would be a large change for a small feature. Dispatch is the natural
    // single site: it knows the tool name, the input, and whether the call
    // succeeded, which is exactly what these events carry.
    //
    // Fired only on success — a `WorktreeCreate` hook should not run for a
    // worktree that was never created.
    if outcome.is_ok() {
        let lifecycle_event = match name {
            "EnterWorktree" => Some(hooks::HookEvent::WorktreeCreate),
            "ExitWorktree" => Some(hooks::HookEvent::WorktreeRemove),
            _ => None,
        };
        if let Some(event) = lifecycle_event {
            // `input_for_post_hook` is the pre-dispatch clone the PostToolUse
            // block above already keeps for exactly this reason — `input`
            // itself was moved into `Tool::call`.
            let inp = input_for_post_hook.clone();
            fire_ctx_lifecycle_hook(ctx, event, |i| i.with_tool(name, inp)).await;
        }
        if name == "PushNotification" && !outcome.as_ref().is_ok_and(|a| a.reported_failure) {
            notify_host(ctx, &input_for_post_hook).await;
        }
    }

    apply_result_transformers(ctx, name, &input_for_post_hook, outcome)
}

/// Run the registered transformers over this call's outcome.
///
/// Deliberately the last thing that touches it — after every hook, after the
/// lifecycle events, immediately before the caller gets it. That ordering is
/// what makes a redacting transformer a guarantee: nothing downstream can put
/// back what it removed.
///
/// The images a tool returned were deposited in `ctx.images` on the way
/// through, so a transformer that drops one has to reach in there rather than
/// hand back a value. They are pulled out, offered, and put back — the
/// side-channel exists because the dispatch signature has no room for binary
/// content, not because images are supposed to be untouchable.
fn apply_result_transformers(
    ctx: &ToolExecCtx,
    name: &str,
    input: &serde_json::Value,
    outcome: base::interface::tool_middleware::ToolOutcome,
) -> base::interface::tool_middleware::ToolOutcome {
    if ctx.result_transformers.is_empty() {
        return outcome;
    }
    let call = base::interface::tool_middleware::ToolCall {
        name: name.to_string(),
        input: input.clone(),
    };
    // A transformer that turns a plain answer into a failure gets the
    // engine-level treatment that has always meant: the call is an `Err`,
    // which cancels its concurrent siblings. One handed a result the tool
    // itself already called a failure hands back the same shape, because
    // nothing about it changed.
    let was_reported_failure = matches!(&outcome, Ok(a) if a.reported_failure);
    let (text, is_error, new_messages, structured) = match outcome {
        Ok(a) => (a.text, a.reported_failure, a.new_messages, a.structured),
        Err(e) => (e, true, None, None),
    };
    let original = (text.clone(), is_error);
    let images = std::mem::take(&mut *ctx.images.lock().unwrap_or_else(|e| e.into_inner()));
    let mut draft = base::interface::tool_result::ToolResultDraft {
        text,
        images: images
            .into_iter()
            .map(|i| base::interface::tool_result::ResultImage {
                media_type: i.media_type,
                data: i.data,
            })
            .collect(),
        is_error,
    };
    base::interface::tool_result::apply(&ctx.result_transformers, &call, &mut draft);
    // The structured form is the same answer as the text; once a transformer
    // has rewritten the text (a redaction, say), handing a program the
    // untouched structure would undo it.
    let structured =
        structured.filter(|_| (&draft.text, draft.is_error) == (&original.0, original.1));

    *ctx.images.lock().unwrap_or_else(|e| e.into_inner()) = draft
        .images
        .into_iter()
        .map(|i| PendingToolImage {
            media_type: i.media_type,
            data: i.data,
        })
        .collect();

    if draft.is_error && !was_reported_failure {
        Err(draft.text)
    } else {
        Ok(base::interface::tool_middleware::ToolAnswer {
            text: draft.text,
            new_messages,
            reported_failure: draft.is_error,
            structured,
        })
    }
}

/// Make the dispatch registry's MCP entries match `mcp`'s current adapter
/// list: new tools are added, re-listed ones take the fresh instance, and ones
/// a server stopped listing are withdrawn so they can no longer be called.
/// Adapters the scene does not permit are never registered — see
/// `Scene::permits_tool`.
fn sync_mcp_adapters(
    tools: &dyn base::tool::ToolRegistry,
    mcp: &dyn base::interface::mcp_servers::McpServers,
    permits: impl Fn(&str) -> bool,
) {
    let is_mcp = |t: &Arc<dyn base::tool::Tool>| t.source().starts_with("mcp:");
    let current: Vec<_> = mcp
        .tools()
        .into_iter()
        .filter(|a| permits(a.name()))
        .collect();
    let live: HashSet<&str> = current.iter().map(|a| a.name()).collect();
    let registered = tools.all();
    let builtin_names: HashSet<&str> = registered
        .iter()
        .filter(|t| !is_mcp(t))
        .map(|t| t.name())
        .collect();
    for stale in registered
        .iter()
        .filter(|t| is_mcp(t) && !live.contains(t.name()))
    {
        // `remove` withdraws every registration under the name, a builtin's too.
        if !builtin_names.contains(stale.name()) {
            tools.remove(stale.name());
        }
    }
    for adapter in &current {
        if builtin_names.contains(adapter.name()) {
            continue;
        }
        match tools.get(adapter.name()) {
            Some(existing) if Arc::ptr_eq(&existing, adapter) => {}
            _ => {
                tools.replace(adapter.clone());
            }
        }
    }
}

/// Moves the request's `User` prompt blocks into one `<system-reminder>`
/// message at the head of the conversation (see `BlockRole`).
fn open_conversation_with_user_blocks(request: &mut ModelRequest) {
    let (opening, system): (Vec<PromptBlock>, Vec<PromptBlock>) =
        std::mem::take(&mut request.prompt_blocks)
            .into_iter()
            .partition(|b| b.role == base::interface::prompt::BlockRole::User);
    request.prompt_blocks = system;
    if opening.is_empty() {
        return;
    }
    let text = opening
        .iter()
        .map(|b| b.content.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    request.messages.insert(
        0,
        ModelMessage {
            role: MessageRole::User,
            content: vec![ModelContentBlock::Text {
                text: format!("<system-reminder>\n{text}\n</system-reminder>"),
            }],
        },
    );
    if let Some(map) = request.params.input_map.as_mut() {
        map.user_message += 1;
    }
}

fn build_prompt_context<'a>(
    settings: &'a base::interface::settings::Settings,
    frozen: Option<&'a base::frozen::FrozenContext>,
    mcp_instructions: Option<&'a str>,
    skills_text: Option<&'a str>,
    effective_model: &str,
    tool_results_ever_cleared: bool,
    env: &dyn base::interface::environment::Environment,
) -> ScenePromptContext<'a> {
    let (is_git, git_branch, is_worktree, git_status, memory_index, output_style, shell, home_dir) =
        if let Some(f) = frozen {
            (
                f.is_git,
                f.git_branch.clone(),
                f.is_worktree,
                f.git_status.clone(),
                f.memory_index.clone().filter(|_| settings.memory_enabled),
                f.output_style.as_ref().map(|os| os.content.clone()),
                f.shell.clone(),
                f.home_dir.clone(),
            )
        } else {
            (false, None, false, None, None, None, None, None)
        };
    ScenePromptContext {
        cwd: Cow::Owned(settings.paths.project_root().display().to_string()),
        os: Cow::Borrowed(std::env::consts::OS),
        // `FrozenContext::collect` always sets `shell` (falls back to
        // `/bin/bash` itself when `$SHELL` is unset), so this fallback only
        // matters before the first turn's collection has happened.
        shell: Cow::Owned(shell.unwrap_or_else(|| "/bin/bash".to_string())),
        // From the environment snapshot, same as `shell` — a fact about the
        // machine, collected once where a test can replace it.
        home_dir: Cow::Owned(home_dir.unwrap_or_else(|| "/home/user".to_string())),
        date: Cow::Owned(chrono_now(env)),
        // The model actually being called this turn, not the configured
        // default — after an overloaded-fallback switch these diverge, and
        // the prompt should describe reality (knowledge cutoff, model
        // recommendations, etc. downstream all key off this).
        model_name: Cow::Owned(effective_model.to_string()),
        skills_text: skills_text.map(|s| Cow::Owned(s.to_string())),
        mcp_instructions: mcp_instructions.map(|s| Cow::Owned(s.to_string())),
        session_memory: memory_index.map(Cow::Owned),
        is_git,
        git_branch: git_branch.map(Cow::Owned),
        git_status: git_status.map(Cow::Owned),
        is_worktree,
        language: settings.language.clone().map(Cow::Owned),
        // `local_data_dir` is already the `.agix/` dir (flat, no scope
        // segment) — no need to prepend another `.agix/`.
        scratchpad_dir: settings
            .paths
            .local_data_dir
            .join("scratchpad")
            .to_str()
            .map(|s| Cow::Owned(s.to_string())),
        output_style_content: output_style.map(Cow::Owned),
        available_tools: None, // populated by caller if needed
        tool_results_ever_cleared,
    }
}

fn chrono_now(env: &dyn base::interface::environment::Environment) -> String {
    let now = env.now();
    format!(
        "{:04}-{:02}-{:02}",
        now.year(),
        u8::from(now.month()),
        now.day()
    )
}

/// Fixed per-entry ceiling on a skill's `description` + `when_to_use`,
/// applied before (and independent of) `build_skills_text`'s listing-wide
/// budget division — a two-layer cap: this 1536-char ceiling always
/// applies, the listing budget then further shrinks entries only when there
/// are enough skills to need it. `when_to_use` is appended (space-separated)
/// to `description`, not shown separately.
const SKILL_COMBINED_TEXT_CAP: usize = 1536;

fn combined_skill_text_capped(description: &str, when_to_use: Option<&str>) -> String {
    let combined = match when_to_use {
        Some(w) if !w.is_empty() => format!("{description} {w}"),
        _ => description.to_string(),
    };
    if combined.len() <= SKILL_COMBINED_TEXT_CAP {
        combined
    } else {
        truncate_at_char_boundary(&combined, SKILL_COMBINED_TEXT_CAP).to_string()
    }
}

/// Decide which skills keep their full description in the listing versus
/// degrading to name-only, under a shared character budget — pulled out of
/// `Agent::build_skills_text` as a free function so the priority/greedy-fill
/// logic is testable without constructing a real `Agent`. `names`, `counts`,
/// and `entry_lens` must be the same length and index-aligned (one entry
/// per skill, in the order they'll be printed); the returned `Vec<bool>` is
/// aligned the same way. Ties (equal invocation count — the common case:
/// everything at `0` in a fresh session) break on name for determinism,
/// independent of whatever order the caller's data happened to arrive in.
fn select_skills_keeping_description(
    names: &[&str],
    counts: &[u32],
    entry_lens: &[usize],
    available_budget: usize,
    per_entry_overhead: usize,
) -> Vec<bool> {
    let mut priority_order: Vec<usize> = (0..names.len()).collect();
    priority_order.sort_by(|&a, &b| {
        counts[b]
            .cmp(&counts[a])
            .then_with(|| names[a].cmp(names[b]))
    });
    let mut keeps_description = vec![false; names.len()];
    let mut spent = 0usize;
    for idx in priority_order {
        let cost = entry_lens[idx] + per_entry_overhead;
        if spent + cost > available_budget {
            break;
        }
        spent += cost;
        keeps_description[idx] = true;
    }
    keeps_description
}

/// Media type for an image path, by extension. `None` for anything the
/// Anthropic API does not accept as an image, which is the signal to fall back
/// to reading the file as text.
fn image_media_type(path: &std::path::Path) -> Option<&'static str> {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => Some("image/png"),
        Some("jpg") | Some("jpeg") => Some("image/jpeg"),
        Some("gif") => Some("image/gif"),
        Some("webp") => Some("image/webp"),
        _ => None,
    }
}

const MAX_ATTACHMENT_IMAGE_BYTES: u64 = base::tool::MAX_IMAGE_BYTES;

/// What a prompt middleware is shown of the attachments.
async fn describe_attachments(
    attachments: &[crate::agent::Attachment],
) -> Vec<base::interface::prompt_middleware::PromptAttachment<'_>> {
    use crate::agent::Attachment;
    use base::interface::prompt_middleware::PromptAttachment;

    let mut described = Vec::with_capacity(attachments.len());
    for att in attachments {
        described.push(match att {
            Attachment::File { path } => {
                let size = tokio::fs::metadata(path).await.ok().map(|m| m.len());
                PromptAttachment::file(path, image_media_type(std::path::Path::new(path)), size)
            }
            Attachment::Text { path, content } => PromptAttachment::text(path, content),
            Attachment::Image { media_type, data } => {
                PromptAttachment::image(media_type, base64_decoded_len(data))
            }
        });
    }
    described
}

fn base64_decoded_len(data: &str) -> u64 {
    let padding = data.bytes().rev().take_while(|b| *b == b'=').count().min(2);
    ((data.len() / 4) * 3).saturating_sub(padding) as u64
}

/// Turn attachments into content blocks for the user message.
///
/// Failures degrade to an explanatory text block rather than propagating: an
/// unreadable attachment should not fail the user's whole turn, and telling
/// the model "this attachment could not be read" is more useful than silently
/// dropping it (the user will refer to it, and the model needs to be able to
/// say why it cannot see it).
async fn resolve_attachments(attachments: &[crate::agent::Attachment]) -> Vec<ModelContentBlock> {
    use crate::agent::Attachment;
    use base64::Engine as _;

    let mut blocks = Vec::new();
    for att in attachments {
        match att {
            Attachment::Image { media_type, data } => blocks.push(ModelContentBlock::Image {
                media_type: media_type.clone(),
                data: data.clone(),
            }),
            Attachment::Text { path, content } => blocks.push(ModelContentBlock::Text {
                text: format!("<attachment path=\"{path}\">\n{content}\n</attachment>"),
            }),
            Attachment::File { path } => {
                let p = std::path::Path::new(path);
                match image_media_type(p) {
                    Some(media_type) => {
                        let size = tokio::fs::metadata(p).await.map(|m| m.len()).unwrap_or(0);
                        if size > MAX_ATTACHMENT_IMAGE_BYTES {
                            blocks.push(ModelContentBlock::Text {
                                text: format!(
                                    "<attachment path=\"{path}\" error=\"image too large: \
                                     {size} bytes, cap is {MAX_ATTACHMENT_IMAGE_BYTES}\" />"
                                ),
                            });
                            continue;
                        }
                        match tokio::fs::read(p).await {
                            Ok(bytes) => blocks.push(ModelContentBlock::Image {
                                media_type: media_type.to_string(),
                                data: base64::engine::general_purpose::STANDARD.encode(&bytes),
                            }),
                            Err(e) => blocks.push(ModelContentBlock::Text {
                                text: format!("<attachment path=\"{path}\" error=\"{e}\" />"),
                            }),
                        }
                    }
                    None => match tokio::fs::read_to_string(p).await {
                        Ok(text) => blocks.push(ModelContentBlock::Text {
                            text: format!("<attachment path=\"{path}\">\n{text}\n</attachment>"),
                        }),
                        Err(e) => blocks.push(ModelContentBlock::Text {
                            text: format!("<attachment path=\"{path}\" error=\"{e}\" />"),
                        }),
                    },
                }
            }
        }
    }
    blocks
}

/// Truncate `s` to at most `max_bytes` bytes without splitting a multi-byte
/// UTF-8 character. A module-local alias for `base::text`'s, which all
/// truncation sites share.
fn truncate_at_char_boundary(s: &str, max_bytes: usize) -> &str {
    base::text::truncate_at_char_boundary(s, max_bytes)
}

/// Count StructuredOutput tool uses in the current session messages.
fn count_structured_output_calls(messages: &[base::interface::model::ModelMessage]) -> u32 {
    messages
        .iter()
        .filter(|m| {
            m.role == base::interface::model::MessageRole::Assistant
                && m.content.iter().any(|b| {
                    matches!(b, base::interface::model::ModelContentBlock::ToolUse { name, .. } if name == "StructuredOutput")
                })
        })
        .count() as u32
}

const MAX_TOKEN_TARGET: u64 = 2_000_000_000;

/// Words that may introduce a natural-language budget, taken out with it.
const BUDGET_VERBS: &[&str] = &["spend", "use", "set", "budget"];

/// An output-token target the user set in their message, and the bytes of the
/// message that say it: `+500k` opening the message, or `500k tokens` /
/// `2M output tokens` anywhere in it, with a `spend` / `use` / `set` /
/// `budget` just before. The multiplier is required, so a bare number next to
/// a word (`use 4 spaces`) is never a budget.
fn find_token_budget_directive(input: &str) -> Option<(u64, std::ops::Range<usize>)> {
    let bytes = input.as_bytes();
    let first = input.len() - input.trim_start().len();
    if bytes.get(first) == Some(&b'+') {
        if let Some((target, end)) = suffixed_number(bytes, first + 1) {
            if at_word_end(bytes, end) {
                return Some((target, first..end));
            }
        }
    }
    (0..bytes.len())
        .filter(|&i| bytes[i].is_ascii_digit() && (i == 0 || bytes[i - 1].is_ascii_whitespace()))
        .find_map(|i| {
            let (target, after) = suffixed_number(bytes, i)?;
            let end = tokens_word(bytes, after)?;
            Some((target, budget_verb_before(bytes, i)..end))
        })
}

/// `500k` / `2M` / `1B` starting at `at`: the count and where it ends.
fn suffixed_number(bytes: &[u8], at: usize) -> Option<(u64, usize)> {
    let digits = bytes[at.min(bytes.len())..]
        .iter()
        .take_while(|b| b.is_ascii_digit())
        .count();
    if digits == 0 {
        return None;
    }
    let multiplier = match bytes.get(at + digits)?.to_ascii_lowercase() {
        b'k' => 1_000,
        b'm' => 1_000_000,
        b'b' => 1_000_000_000,
        _ => return None,
    };
    let n: u64 = std::str::from_utf8(&bytes[at..at + digits])
        .ok()?
        .parse()
        .ok()?;
    let target = n.checked_mul(multiplier)?;
    (1..=MAX_TOKEN_TARGET)
        .contains(&target)
        .then_some((target, at + digits + 1))
}

/// ` tokens` / ` output tokens` starting at `at`: where it ends.
fn tokens_word(bytes: &[u8], at: usize) -> Option<usize> {
    let mut i = skip_spaces(bytes, at)?;
    if let Some(after) = word_at(bytes, i, "output") {
        i = skip_spaces(bytes, after)?;
    }
    let mut end = word_at(bytes, i, "token")?;
    if bytes
        .get(end)
        .is_some_and(|b| b.eq_ignore_ascii_case(&b's'))
    {
        end += 1;
    }
    at_word_end(bytes, end).then_some(end)
}

fn skip_spaces(bytes: &[u8], at: usize) -> Option<usize> {
    let n = bytes[at..].iter().take_while(|b| **b == b' ').count();
    (n > 0).then_some(at + n)
}

fn word_at(bytes: &[u8], at: usize, word: &str) -> Option<usize> {
    let end = at + word.len();
    bytes
        .get(at..end)
        .is_some_and(|w| w.eq_ignore_ascii_case(word.as_bytes()))
        .then_some(end)
}

fn at_word_end(bytes: &[u8], at: usize) -> bool {
    !bytes.get(at).is_some_and(u8::is_ascii_alphanumeric)
}

/// Where a budget verb right before `at` starts, or `at` when there is none.
fn budget_verb_before(bytes: &[u8], at: usize) -> usize {
    let mut end = at;
    while end > 0 && bytes[end - 1] == b' ' {
        end -= 1;
    }
    BUDGET_VERBS
        .iter()
        .find_map(|verb| {
            let start = end.checked_sub(verb.len())?;
            let word_start = start == 0 || bytes[start - 1].is_ascii_whitespace();
            (word_start && bytes[start..end].eq_ignore_ascii_case(verb.as_bytes())).then_some(start)
        })
        .unwrap_or(at)
}

/// The message without the directive at `range`.
fn strip_token_budget_directive(input: &str, range: std::ops::Range<usize>) -> String {
    let (head, tail) = (&input[..range.start], &input[range.end..]);
    let (before, after) = (head.trim(), tail.trim());
    if before.is_empty() || after.is_empty() {
        return format!("{before}{after}");
    }
    let gap = |s: &str| s.contains('\n');
    let sep = if gap(&head[head.trim_end().len()..])
        || gap(&tail[..tail.len() - tail.trim_start().len()])
    {
        "\n"
    } else {
        " "
    };
    format!("{before}{sep}{after}")
}

/// One assembled call to the model.
///
/// The normal path and the two recovery paths differ only in how these four
/// fields are produced, so they build one of these and hand it to
/// [`Agent::send`] rather than each spelling out a `StreamParams` of its own.
/// A turn's model call, assembled.
///
/// The same type interceptors see — one shape rather than a private struct
/// and a public mirror of it that have to be kept agreeing.
type ModelRequest = base::interface::model_interceptor::ModelRequestView;

/// A permission mode as hooks see it: the name settings files use.
enum PromptScreen {
    Pass(String),
    Blocked(Option<String>),
}

fn running_task_summary(task: &base::context::RunningTask) -> String {
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

/// Which of the recovery policy's failures a model error is. `rendered` is
/// the error's own text, which `Other` carries.
fn model_failure<'a>(
    e: &'a base::interface::model::ModelError,
    rendered: &'a str,
) -> base::interface::recovery_policy::ModelFailure<'a> {
    use base::interface::model::ModelError;
    use base::interface::recovery_policy::ModelFailure;
    match e {
        ModelError::Overloaded => ModelFailure::Overloaded,
        ModelError::Api {
            status: 413,
            message,
        } => ModelFailure::ContextTooLong { message },
        ModelError::Api { status, message }
            if (400..500).contains(status) && says_context_too_long(message) =>
        {
            ModelFailure::ContextTooLong { message }
        }
        ModelError::Internal(message) if says_context_too_long(message) => {
            ModelFailure::ContextTooLong { message }
        }
        _ => ModelFailure::Other { message: rendered },
    }
}

/// Whether a refusal's text says the request was too large for the model.
///
/// No provider gives this a status of its own — Anthropic, OpenAI and the
/// relays in front of them all answer 400 with a type that covers every bad
/// request — so the wording is the signal.
fn says_context_too_long(message: &str) -> bool {
    const WORDINGS: &[&str] = &[
        "prompt is too long",
        "prompt too long",
        "input is too long",
        "context_length_exceeded",
        "maximum context length",
        "context length exceeded",
        "exceeds the context window",
        "exceeds the maximum number of tokens",
        "too many tokens",
        "request_too_large",
    ];
    let message = message.to_ascii_lowercase();
    WORDINGS.iter().any(|w| message.contains(w))
}

/// Whether `id` is a `tool_use` of the last reply that nothing after it has
/// answered yet.
fn awaits_result(messages: &[ModelMessage], id: &str) -> bool {
    let Some(last_reply) = messages
        .iter()
        .rposition(|m| m.role == MessageRole::Assistant)
    else {
        return false;
    };
    let used = messages[last_reply]
        .content
        .iter()
        .any(|b| matches!(b, ModelContentBlock::ToolUse { id: used, .. } if used == id));
    let answered = messages[last_reply + 1..]
        .iter()
        .flat_map(|m| &m.content)
        .any(
            |b| matches!(b, ModelContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == id),
        );
    used && !answered
}

/// Gives every `tool_use` a `tool_result` in the message right after it, and
/// drops every `tool_result` that answers no `tool_use` there.
///
/// The engine never writes an unpaired one, but a transcript written by a
/// process that died mid-step can hold one, and the API rejects every
/// request after it — a session that can never answer again. Results a host
/// injected after the engine's own arrive as later user messages; they are
/// gathered into the first. Applied to the outgoing request only; the
/// transcript keeps what actually happened.
fn pair_dangling_tool_uses(messages: Vec<ModelMessage>) -> Vec<ModelMessage> {
    let mut out: Vec<ModelMessage> = Vec::with_capacity(messages.len() + 1);
    let mut messages = messages.into_iter().peekable();
    while let Some(message) = messages.next() {
        if message.role != MessageRole::Assistant {
            push_unless_empty(&mut out, without_tool_results(message));
            continue;
        }
        let used: Vec<String> = message
            .content
            .iter()
            .filter_map(|b| match b {
                ModelContentBlock::ToolUse { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect();
        out.push(message);

        let mut answers: std::collections::HashMap<String, ModelContentBlock> =
            std::collections::HashMap::new();
        let mut replies: Vec<ModelMessage> = Vec::new();
        while let Some(reply) = messages.next_if(|m| m.role == MessageRole::User) {
            let (results, rest): (Vec<_>, Vec<_>) = reply
                .content
                .into_iter()
                .partition(|b| matches!(b, ModelContentBlock::ToolResult { .. }));
            for result in results {
                if let ModelContentBlock::ToolResult { tool_use_id, .. } = &result {
                    if used.contains(tool_use_id) && !answers.contains_key(tool_use_id) {
                        answers.insert(tool_use_id.clone(), result);
                    }
                }
            }
            replies.push(ModelMessage {
                role: MessageRole::User,
                content: rest,
            });
        }
        if used.is_empty() {
            for reply in replies {
                push_unless_empty(&mut out, reply);
            }
            continue;
        }
        let mut paired: Vec<ModelContentBlock> = used
            .iter()
            .map(|id| {
                answers
                    .remove(id)
                    .unwrap_or_else(|| ModelContentBlock::ToolResult {
                        tool_use_id: id.clone(),
                        content: "The tool call did not complete.".into(),
                        is_error: Some(true),
                    })
            })
            .collect();
        let mut replies = replies.into_iter();
        if let Some(first) = replies.next() {
            paired.extend(first.content);
        }
        out.push(ModelMessage {
            role: MessageRole::User,
            content: paired,
        });
        for reply in replies {
            push_unless_empty(&mut out, reply);
        }
    }
    out
}

fn without_tool_results(mut message: ModelMessage) -> ModelMessage {
    message
        .content
        .retain(|b| !matches!(b, ModelContentBlock::ToolResult { .. }));
    message
}

fn push_unless_empty(out: &mut Vec<ModelMessage>, message: ModelMessage) {
    if !message.content.is_empty() {
        out.push(message);
    }
}

#[derive(Debug, Clone, Default)]
pub struct TurnOutcome {
    pub stop_reason: String,
    pub api_calls: u32,
    pub tool_calls: u32,
    /// What the **last** model call of this turn reported, and zero on the
    /// paths that end a turn without one.
    ///
    /// Kept as it is because hosts read it. It is not what its name suggests
    /// and never was — see [`Self::total_usage`], which is. The two will be
    /// reconciled at a version boundary rather than by changing what this one
    /// means underneath somebody.
    pub usage: Usage,
    /// What every model call in this turn reported, added up.
    ///
    /// The number to budget on: a turn is several calls, and one of them is a
    /// fraction of the answer. A host that capped spending on `usage` was
    /// capping on whatever the last round trip happened to cost.
    pub total_usage: Usage,
    /// Why the turn was stopped, when whatever stopped it said so.
    pub message: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum TurnError {
    #[error("model error: {0}")]
    Model(String),
    #[error("shutdown")]
    Shutdown,
    #[error("internal: {0}")]
    Internal(String),
}

#[cfg(test)]
mod sandbox_settings_tests {
    use super::*;
    use base::context::config::NetworkModeConfig;
    use std::path::PathBuf;

    /// The `settings.json` sandbox knobs reach `BashTool` through the tool
    /// context.
    #[test]
    fn tool_context_sandbox_is_taken_from_the_engine_config() {
        let mut config = base::context::EngineConfig::defaults_for("test-model");
        config.sandbox_policy = base::context::config::SandboxPolicyConfig {
            allow_read: vec![PathBuf::from("/tmp/ok")],
            deny_read: Some(vec![PathBuf::from("/tmp/secret")]),
            network_mode: NetworkModeConfig::DenyAll,
            allowed_domains: vec!["api.example.com".into()],
            state_root: None,
            require_enforcement: false,
            unattended_requires_network_policy: true,
            deny_write: Vec::new(),
            allow_write: Vec::new(),
        };

        let s = sandbox_settings_from(&config);

        assert_eq!(s.allow_read, [PathBuf::from("/tmp/ok")]);
        assert_eq!(s.deny_read, [PathBuf::from("/tmp/secret")]);
        assert_eq!(s.network_mode, NetworkModeConfig::DenyAll);
        assert_eq!(s.allowed_domains, ["api.example.com"]);
        assert!(s.unattended_requires_network_policy);
    }

    /// The same knob, reaching the egress a tool's own HTTP goes out through
    /// and not only the sandbox wrapped around a subprocess.
    #[test]
    fn tool_context_egress_is_taken_from_the_engine_config() {
        use base::interface::exec::Origin;

        let mut config = base::context::EngineConfig::defaults_for("test-model");
        config.sandbox_policy.network_mode = NetworkModeConfig::Allowlist;
        config.sandbox_policy.allowed_domains = vec!["api.example.com".into()];

        let exec = exec_providers_from(&config);

        assert!(exec.network.permits("api.example.com", Origin::Agent));
        assert!(
            !exec.network.permits("evil.test", Origin::Agent),
            "a configured allowlist has to reach the tools' egress"
        );
        assert!(
            exec.network.permits("evil.test", Origin::Operator),
            "and stop at what the model chose, not at what the deployment did"
        );
    }

    /// `None` ("use built-in deny defaults") flattens to an empty vec, which
    /// is the same empty-means-defaults convention `tools::bash` reads on the
    /// other side — not a request to deny nothing.
    #[test]
    fn absent_deny_read_flattens_to_empty() {
        let config = base::context::EngineConfig::defaults_for("test-model");
        assert!(config.sandbox_policy.deny_read.is_none());
        assert!(sandbox_settings_from(&config).deny_read.is_empty());
    }
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;
    use base::interface::settings::ThinkingMode;

    #[test]
    fn user_blocks_open_the_conversation_and_the_input_map_follows() {
        let text = |t: &str| ModelMessage {
            role: MessageRole::User,
            content: vec![ModelContentBlock::Text { text: t.into() }],
        };
        let mut request = ModelRequest {
            prompt_blocks: vec![
                PromptBlock::system("rules"),
                PromptBlock::user("skills: a, b").named(base::prompt::names::SKILLS_CATALOG),
            ],
            tool_defs: vec![],
            messages: vec![text("context"), text("hello")],
            params: base::interface::model::StreamParams {
                model: "m".into(),
                max_tokens: 1,
                thinking_mode: ThinkingMode::Off,
                effort: None,
                fallback_model: None,
                cache_edits: vec![],
                origin: None,
                input_map: Some(base::interface::model::InputMap {
                    user_message: 1,
                    spans: vec![],
                }),
                provider_search: None,
            },
        };
        open_conversation_with_user_blocks(&mut request);

        assert_eq!(request.prompt_blocks.len(), 1);
        assert_eq!(request.prompt_blocks[0].content, "rules");
        assert_eq!(request.messages.len(), 3);
        assert!(matches!(
            &request.messages[0].content[0],
            ModelContentBlock::Text { text } if text == "<system-reminder>\nskills: a, b\n</system-reminder>"
        ));
        assert_eq!(request.params.input_map.unwrap().user_message, 2);
    }

    /// The session-memory staleness reset in `run_user_turn` recognizes a self-update by comparing `extract_tool_file_paths`
    /// against `sm.path()` — this pins down that a `Write` to the exact
    /// sidecar path is detected, and an unrelated `Write` isn't mistaken
    /// for one.
    #[test]
    fn extract_tool_file_paths_finds_write_to_exact_path() {
        let sidecar = PathBuf::from("/tmp/sessions/abc/session_memory.md");
        let msg = ModelMessage {
            role: MessageRole::Assistant,
            content: vec![ModelContentBlock::ToolUse {
                id: "1".into(),
                name: "Write".into(),
                input: serde_json::json!({"file_path": sidecar.to_string_lossy(), "content": "x"}),
            }],
        };
        let paths = Agent::extract_tool_file_paths(&[msg]);
        assert!(paths.contains(&sidecar));
    }

    #[test]
    fn extract_tool_file_paths_does_not_match_unrelated_write() {
        let sidecar = PathBuf::from("/tmp/sessions/abc/session_memory.md");
        let msg = ModelMessage {
            role: MessageRole::Assistant,
            content: vec![ModelContentBlock::ToolUse {
                id: "1".into(),
                name: "Write".into(),
                input: serde_json::json!({"file_path": "/tmp/some/other/file.rs", "content": "x"}),
            }],
        };
        let paths = Agent::extract_tool_file_paths(&[msg]);
        assert!(!paths.contains(&sidecar));
    }

    #[tokio::test]
    async fn clearing_old_tool_results_by_age_is_reported_as_a_compaction() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut agent, mut event_rx, _input_tx) = crate::agent::Builder::new()
            .scene(Arc::new(scene::code::CodeScene::default()))
            .model(Arc::new(FixedUsageModel {
                usage: Default::default(),
            }))
            .settings(Arc::new(recall_test_settings(tmp.path())))
            .skip_warmup(true)
            .build()
            .expect("build should succeed");
        agent.time_based_mc_config = compaction::time_based_mc::TimeBasedMcConfig {
            max_age: Some(std::time::Duration::from_millis(1)),
            min_messages: 0,
        };
        agent.session.push_message(ModelMessage {
            role: MessageRole::Assistant,
            content: vec![ModelContentBlock::ToolUse {
                id: "r1".into(),
                name: "Read".into(),
                input: serde_json::json!({"file_path": "/x"}),
            }],
        });
        agent.session.push_message(ModelMessage {
            role: MessageRole::User,
            content: vec![ModelContentBlock::ToolResult {
                tool_use_id: "r1".into(),
                content: "old contents ".repeat(200),
                is_error: None,
            }],
        });
        std::thread::sleep(std::time::Duration::from_millis(5));

        agent.compact_if_needed().await;

        let mut frames = Vec::new();
        while let Ok(event) = event_rx.try_recv() {
            if let AgentEvent::CompactAction {
                strategy,
                tokens_before,
                tokens_after,
                ..
            } = event
            {
                frames.push((strategy, tokens_before, tokens_after));
            }
        }
        assert_eq!(frames.len(), 1, "{frames:?}");
        assert_eq!(frames[0].0, "MicroCompact");
        assert!(frames[0].2 < frames[0].1, "{frames:?}");
    }

    #[tokio::test]
    async fn this_turn_starts_at_its_user_message_however_long_the_transcript() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut agent, _event_rx, _input_tx) = crate::agent::Builder::new()
            .scene(Arc::new(scene::code::CodeScene::default()))
            .model(Arc::new(FixedUsageModel {
                usage: Default::default(),
            }))
            .settings(Arc::new(recall_test_settings(tmp.path())))
            .skip_warmup(true)
            .build()
            .expect("build should succeed");
        let write = |path: &str| ModelMessage {
            role: MessageRole::Assistant,
            content: vec![ModelContentBlock::ToolUse {
                id: path.into(),
                name: "Write".into(),
                input: serde_json::json!({"file_path": path, "content": "x"}),
            }],
        };
        for i in 0..60 {
            agent.session.push_message(write(&format!("/old/{i}")));
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
        agent.turn_started_at = Some(std::time::Instant::now());
        agent.session.push_message(write("/new"));

        assert_eq!(
            Agent::extract_tool_file_paths(agent.messages_this_turn()),
            [PathBuf::from("/new")]
        );
    }

    /// `included_git_mutating_bash_call` gates whether `FrozenContext`
    /// pays for a `refresh_git()` round-trip — this pins its heuristic down
    /// against the cases it's meant to catch and the ones it must not
    /// false-positive on.
    fn bash_msg(command: &str) -> ModelMessage {
        ModelMessage {
            role: MessageRole::Assistant,
            content: vec![ModelContentBlock::ToolUse {
                id: "1".into(),
                name: "Bash".into(),
                input: serde_json::json!({"command": command}),
            }],
        }
    }

    #[test]
    fn git_mutating_bash_detects_plain_checkout() {
        assert!(Agent::included_git_mutating_bash_call(&[bash_msg(
            "git checkout -b feature-x"
        )]));
    }

    #[test]
    fn git_mutating_bash_detects_commit_with_global_flags() {
        assert!(Agent::included_git_mutating_bash_call(&[bash_msg(
            "git -C /repo --no-pager commit -m 'msg'"
        )]));
    }

    #[test]
    fn git_mutating_bash_detects_within_a_chained_command() {
        assert!(Agent::included_git_mutating_bash_call(&[bash_msg(
            "cd /repo && git add -A && git commit -m wip"
        )]));
    }

    #[test]
    fn git_mutating_bash_ignores_read_only_git_status() {
        assert!(!Agent::included_git_mutating_bash_call(&[bash_msg(
            "git status --short"
        )]));
    }

    #[test]
    fn git_mutating_bash_ignores_read_only_git_log() {
        assert!(!Agent::included_git_mutating_bash_call(&[bash_msg(
            "git log --oneline -n 5"
        )]));
    }

    #[test]
    fn git_mutating_bash_ignores_non_bash_tools() {
        let msg = ModelMessage {
            role: MessageRole::Assistant,
            content: vec![ModelContentBlock::ToolUse {
                id: "1".into(),
                name: "Read".into(),
                input: serde_json::json!({"file_path": "git commit history.md"}),
            }],
        };
        assert!(!Agent::included_git_mutating_bash_call(&[msg]));
    }

    #[test]
    fn git_mutating_bash_ignores_unrelated_commands() {
        assert!(!Agent::included_git_mutating_bash_call(&[bash_msg(
            "ls -la && cat README.md"
        )]));
    }

    /// A probe tool that records the input it was actually called with.
    struct ProbeTool {
        called: std::sync::Arc<std::sync::Mutex<Option<serde_json::Value>>>,
    }
    #[async_trait::async_trait]
    impl base::tool::Tool for ProbeTool {
        fn name(&self) -> &str {
            "Probe"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn call(
            &self,
            input: serde_json::Value,
            _ctx: base::tool::ToolContext,
            _progress: base::tool::ProgressSender,
        ) -> Result<base::tool::ToolResult, base::error::ToolError> {
            *self.called.lock().unwrap() = Some(input);
            Ok(base::tool::ToolResult::text("probe-ran"))
        }
    }

    /// Rejects every input, and records whether it ran anyway.
    struct PickyTool {
        ran: Arc<std::sync::atomic::AtomicBool>,
    }
    #[async_trait::async_trait]
    impl base::tool::Tool for PickyTool {
        fn name(&self) -> &str {
            "Picky"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn validate_input(
            &self,
            _input: &serde_json::Value,
            _ctx: &base::tool::ToolContext,
        ) -> base::tool::ValidationResult {
            base::tool::ValidationResult::err("picky refuses this input", 1)
        }
        async fn call(
            &self,
            _input: serde_json::Value,
            _ctx: base::tool::ToolContext,
            _progress: base::tool::ProgressSender,
        ) -> Result<base::tool::ToolResult, base::error::ToolError> {
            self.ran.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(base::tool::ToolResult::text("picky-ran"))
        }
    }

    /// Always errors, so the failure-only lifecycle events have something to
    /// fire on.
    struct FailingTool;
    #[async_trait::async_trait]
    impl base::tool::Tool for FailingTool {
        fn name(&self) -> &str {
            "Failing"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn call(
            &self,
            _input: serde_json::Value,
            _ctx: base::tool::ToolContext,
            _progress: base::tool::ProgressSender,
        ) -> Result<base::tool::ToolResult, base::error::ToolError> {
            Err(base::error::ToolError::exec("probe failed on purpose"))
        }
    }

    struct DenyingPermission;
    #[async_trait::async_trait]
    impl base::interface::permission::Permission for DenyingPermission {
        async fn check(
            &self,
            _tool_name: &str,
            _tool_input: &serde_json::Value,
            _cwd: &std::path::Path,
            _session_id: &str,
        ) -> base::interface::permission::PermissionOutcome {
            base::interface::permission::PermissionOutcome::Deny {
                reason: "denied by test rule engine".into(),
            }
        }
    }

    struct AllowAllTestPermission;
    #[async_trait::async_trait]
    impl base::interface::permission::Permission for AllowAllTestPermission {
        async fn check(
            &self,
            _tool_name: &str,
            _tool_input: &serde_json::Value,
            _cwd: &std::path::Path,
            _session_id: &str,
        ) -> base::interface::permission::PermissionOutcome {
            base::interface::permission::PermissionOutcome::Permit
        }
    }

    fn test_exec_ctx(
        tools: Arc<base::tool::InMemoryToolRegistry>,
        hooks: Arc<hooks::HookRunner>,
    ) -> ToolExecCtx {
        test_exec_ctx_with_permission(tools, hooks, Arc::new(AllowAllTestPermission))
    }

    fn test_exec_ctx_with_permission(
        tools: Arc<base::tool::InMemoryToolRegistry>,
        hooks: Arc<hooks::HookRunner>,
        permission: Arc<dyn base::interface::permission::Permission>,
    ) -> ToolExecCtx {
        ToolExecCtx {
            tool_use_id: "toolu_test".into(),
            tools,
            cwd: std::env::temp_dir(),
            local_settings_path: std::env::temp_dir()
                .join(".agix")
                .join(base::interface::settings::SETTINGS_LOCAL_FILE),
            session_id: "test-session".into(),
            turn_no: 1,
            agent_depth: 0,
            telemetry_handle: telemetry::TelemetryHandle::noop(),
            turn_id: "test-turn".into(),
            cancel: tokio_util::sync::CancellationToken::new(),
            hooks,
            discontinued: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            config: Arc::new(base::context::EngineConfig::defaults_for("test")),
            permission,
            session_state: Arc::new(base::context::SessionState::new(std::env::temp_dir())),
            elicitation: Arc::new(base::interface::elicitation::DeclineAll),
            tool_middleware: Arc::new(Vec::new()),
            result_transformers: Arc::new(Vec::new()),
            images: Arc::new(std::sync::Mutex::new(Vec::new())),
            from_code: false,
            events: None,
        }
    }

    #[tokio::test]
    async fn tool_start_fires_before_tool_execution_completes() {
        let called = std::sync::Arc::new(std::sync::Mutex::new(None));
        let tools = Arc::new(base::tool::InMemoryToolRegistry::new());
        tools.register(std::sync::Arc::new(ProbeTool {
            called: called.clone(),
        }));
        let hooks_runner = Arc::new(hooks::HookRunner::new(std::collections::HashMap::new()));

        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let mut ctx = test_exec_ctx(tools, hooks_runner);
        ctx.telemetry_handle = telemetry::TelemetryHandle::new(tx);

        let result = execute_tool_with_telemetry(&ctx, "Probe", serde_json::json!({"x": 1})).await;
        assert!(result.is_ok());
        drop(ctx);

        let decided = rx
            .recv()
            .await
            .expect("permission_decision should have been recorded");
        assert_eq!(decided.kind(), "permission_decision");
        let first = rx
            .recv()
            .await
            .expect("tool_start should have been recorded");
        assert_eq!(first.kind(), "tool_start");
        let second = rx
            .recv()
            .await
            .expect("tool_execution should have been recorded");
        assert_eq!(second.kind(), "tool_execution");
    }

    async fn recorded_tool_execution(
        ctx: &mut ToolExecCtx,
        tool: &str,
        input: serde_json::Value,
    ) -> (
        base::interface::tool_middleware::ToolOutcome,
        serde_json::Value,
    ) {
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        ctx.telemetry_handle = telemetry::TelemetryHandle::new(tx);
        let result = execute_tool_with_telemetry(ctx, tool, input).await;
        ctx.telemetry_handle = telemetry::TelemetryHandle::noop();
        let mut execution = None;
        while let Some(event) = rx.recv().await {
            if event.kind() == "tool_execution" {
                execution = Some(serde_json::to_value(event).unwrap());
            }
        }
        (
            result,
            execution.expect("tool_execution should have been recorded"),
        )
    }

    #[tokio::test]
    async fn tool_execution_reports_the_sizes_and_the_failure_it_saw() {
        let tools = Arc::new(base::tool::InMemoryToolRegistry::new());
        tools.register(Arc::new(ProbeTool {
            called: Default::default(),
        }));
        tools.register(Arc::new(FailingTool));
        let hooks_runner = Arc::new(hooks::HookRunner::new(std::collections::HashMap::new()));
        let mut ctx = test_exec_ctx(tools, hooks_runner);

        let input = serde_json::json!({"x": 1});
        let (result, ran) = recorded_tool_execution(&mut ctx, "Probe", input.clone()).await;
        assert!(result.is_ok());
        assert_eq!(ran["input_json_size"], input.to_string().len());
        assert_eq!(ran["result_content_size"], "probe-ran".len());
        assert_eq!(ran["is_error"], false);
        assert!(ran.get("error_message").is_none(), "{ran}");
        assert_eq!(
            ran["user_approved"], false,
            "the rule engine allowed it; nobody was asked"
        );

        let (result, failed) =
            recorded_tool_execution(&mut ctx, "Failing", serde_json::json!({})).await;
        assert!(result.is_err());
        assert_eq!(failed["is_error"], true);
        assert_eq!(failed["outcome"], "failed");
        let message = failed["error_message"].as_str().unwrap();
        assert!(message.contains("probe failed on purpose"), "{failed}");
    }

    #[tokio::test]
    async fn tool_execution_says_a_user_approved_the_call_they_were_asked_about() {
        let tools = Arc::new(base::tool::InMemoryToolRegistry::new());
        tools.register(Arc::new(ProbeTool {
            called: Default::default(),
        }));
        let hooks_runner = Arc::new(hooks::HookRunner::new(std::collections::HashMap::new()));
        let mut ctx = test_exec_ctx_with_permission(
            tools,
            hooks_runner,
            Arc::new(PromptTestPermission { prompt_id: "asked" }),
        );
        let pending: Arc<
            std::sync::Mutex<
                std::collections::HashMap<
                    String,
                    tokio::sync::oneshot::Sender<crate::agent::PermissionDecision>,
                >,
            >,
        > = Default::default();
        let (events, _events_rx) = tokio::sync::mpsc::unbounded_channel();
        ctx.elicitation = Arc::new(crate::elicitation::ChannelElicitation::new(
            crate::event_bus::EventBus::new(events),
            pending.clone(),
        ));
        let answer = tokio::spawn(async move {
            loop {
                if let Some(tx) = pending.lock().unwrap().remove("asked") {
                    let _ = tx.send(crate::agent::PermissionDecision::Permit);
                    return;
                }
                tokio::task::yield_now().await;
            }
        });

        let (result, ran) = recorded_tool_execution(&mut ctx, "Probe", serde_json::json!({})).await;
        answer.await.unwrap();
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(ran["user_approved"], true);
    }

    #[test]
    fn a_long_failure_text_is_cut_on_a_character_boundary() {
        let kept = telemetry_error_text(&"é".repeat(2000));
        assert_eq!(kept.chars().count(), 513);
        assert!(kept.ends_with('…'));
        assert_eq!(telemetry_error_text("short"), "short");
    }

    #[tokio::test]
    async fn a_pre_tool_use_approve_answers_the_permission_question_but_not_a_denial() {
        let called = std::sync::Arc::new(std::sync::Mutex::new(None));
        let tools = Arc::new(base::tool::InMemoryToolRegistry::new());
        tools.register(Arc::new(ProbeTool {
            called: called.clone(),
        }));
        let approving = || {
            let mut settings: hooks::config::HooksSettings = std::collections::HashMap::new();
            settings.insert(
                hooks::HookEvent::PreToolUse,
                vec![hooks::config::HookConfig::Command {
                    command: r#"echo '{"decision":"approve"}'"#.into(),
                    shell: None,
                    timeout: None,
                    if_pattern: None,
                    only_on_error: None,
                    once: None,
                    async_rewake: None,
                    on_error: hooks::OnError::Allow,
                }],
            );
            Arc::new(hooks::HookRunner::new(settings))
        };

        let unanswered = test_exec_ctx_with_permission(
            tools.clone(),
            Arc::new(hooks::HookRunner::new(std::collections::HashMap::new())),
            Arc::new(PromptTestPermission { prompt_id: "p" }),
        );
        let declined =
            execute_tool_with_telemetry(&unanswered, "Probe", serde_json::json!({})).await;
        assert!(declined.is_err(), "nobody answered, so it does not run");
        assert!(called.lock().unwrap().is_none());

        let mut approved = test_exec_ctx_with_permission(
            tools.clone(),
            approving(),
            Arc::new(PromptTestPermission { prompt_id: "p" }),
        );
        let (result, ran) =
            recorded_tool_execution(&mut approved, "Probe", serde_json::json!({})).await;
        assert!(result.is_ok(), "{result:?}");
        assert!(
            called.lock().unwrap().is_some(),
            "the hook's approval let it run"
        );
        assert_eq!(ran["user_approved"], false, "a hook is not a user");

        *called.lock().unwrap() = None;
        let denied =
            test_exec_ctx_with_permission(tools, approving(), Arc::new(DenyAllTestPermission));
        let refused = execute_tool_with_telemetry(&denied, "Probe", serde_json::json!({})).await;
        assert!(
            refused.is_err(),
            "approve answers a question, it does not lift a rule"
        );
        assert!(called.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn input_the_tool_rejects_never_runs_whatever_the_permission_handler() {
        let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let tools = Arc::new(base::tool::InMemoryToolRegistry::new());
        tools.register(Arc::new(PickyTool { ran: ran.clone() }));
        let hooks_runner = Arc::new(hooks::HookRunner::new(std::collections::HashMap::new()));
        let ctx = test_exec_ctx(tools, hooks_runner);

        let result = execute_tool_with_telemetry(&ctx, "Picky", serde_json::json!({})).await;
        let error = result.expect_err("an allow-all permission does not let invalid input through");
        assert!(error.contains("picky refuses this input"), "{error}");
        assert!(!ran.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn pre_tool_use_hook_blocks_call() {
        let called = std::sync::Arc::new(std::sync::Mutex::new(None));
        let tools = Arc::new(base::tool::InMemoryToolRegistry::new());
        tools.register(std::sync::Arc::new(ProbeTool {
            called: called.clone(),
        }));

        let mut settings: hooks::config::HooksSettings = std::collections::HashMap::new();
        settings.insert(
            hooks::HookEvent::PreToolUse,
            vec![hooks::config::HookConfig::Command {
                command: r#"echo '{"decision":"block","message":"nope"}'"#.into(),
                shell: None,
                timeout: None,
                if_pattern: None,
                only_on_error: None,
                once: None,
                async_rewake: None,
                on_error: hooks::OnError::Allow,
            }],
        );
        let hooks_runner = Arc::new(hooks::HookRunner::new(settings));
        let ctx = test_exec_ctx(tools, hooks_runner);

        let result = execute_tool_with_telemetry(&ctx, "Probe", serde_json::json!({"x": 1})).await;
        assert!(result.is_err(), "expected the tool call to be blocked");
        assert!(result.unwrap_err().contains("nope"));
        assert!(
            called.lock().unwrap().is_none(),
            "tool must not have actually run"
        );
    }

    #[tokio::test]
    async fn pre_tool_use_hook_rewrites_input() {
        let called = std::sync::Arc::new(std::sync::Mutex::new(None));
        let tools = Arc::new(base::tool::InMemoryToolRegistry::new());
        tools.register(std::sync::Arc::new(ProbeTool {
            called: called.clone(),
        }));

        let mut settings: hooks::config::HooksSettings = std::collections::HashMap::new();
        settings.insert(
            hooks::HookEvent::PreToolUse,
            vec![hooks::config::HookConfig::Command {
                command: r#"echo '{"updated_input":{"x":99}}'"#.into(),
                shell: None,
                timeout: None,
                if_pattern: None,
                only_on_error: None,
                once: None,
                async_rewake: None,
                on_error: hooks::OnError::Allow,
            }],
        );
        let hooks_runner = Arc::new(hooks::HookRunner::new(settings));
        let ctx = test_exec_ctx(tools, hooks_runner);

        let result = execute_tool_with_telemetry(&ctx, "Probe", serde_json::json!({"x": 1})).await;
        assert!(
            result.is_ok(),
            "expected the (rewritten) call to succeed: {result:?}"
        );
        assert_eq!(*called.lock().unwrap(), Some(serde_json::json!({"x": 99})));
    }

    struct FakePushNotification;
    #[async_trait::async_trait]
    impl base::tool::Tool for FakePushNotification {
        fn name(&self) -> &str {
            "PushNotification"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn call(
            &self,
            _input: serde_json::Value,
            _ctx: base::tool::ToolContext,
            _progress: base::tool::ProgressSender,
        ) -> Result<base::tool::ToolResult, base::error::ToolError> {
            Ok(base::tool::ToolResult::text("Notification sent"))
        }
    }

    #[tokio::test]
    async fn a_push_notification_reaches_the_event_stream_and_the_notification_hook() {
        let tools = Arc::new(base::tool::InMemoryToolRegistry::new());
        tools.register(Arc::new(FakePushNotification));
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("hook.json");
        let mut settings: hooks::config::HooksSettings = std::collections::HashMap::new();
        settings.insert(
            hooks::HookEvent::Notification,
            vec![hooks::config::HookConfig::Command {
                command: format!("cat > '{}'", log.display()),
                shell: None,
                timeout: None,
                if_pattern: None,
                only_on_error: None,
                once: None,
                async_rewake: None,
                on_error: hooks::OnError::Allow,
            }],
        );
        let mut ctx = test_exec_ctx(tools, Arc::new(hooks::HookRunner::new(settings)));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        ctx.events = Some(crate::event_bus::EventBus::new(tx));

        let result = execute_tool_with_telemetry(
            &ctx,
            "PushNotification",
            serde_json::json!({"message": " build done ", "status": "proactive"}),
        )
        .await;
        assert!(result.is_ok(), "{result:?}");

        let mut notified = None;
        while let Ok(event) = rx.try_recv() {
            if let AgentEvent::Notification {
                message, status, ..
            } = event
            {
                notified = Some((message, status));
            }
        }
        assert_eq!(
            notified,
            Some(("build done".to_string(), Some("proactive".to_string())))
        );
        let hooked: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&log).unwrap()).unwrap();
        assert_eq!(hooked["hook_event_name"], "Notification");
        assert_eq!(hooked["reason"], "build done");
    }

    #[tokio::test]
    async fn post_tool_use_hook_fires_after_execution() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("post-hook-ran");
        let tools = Arc::new(base::tool::InMemoryToolRegistry::new());
        tools.register(std::sync::Arc::new(ProbeTool {
            called: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }));

        let mut settings: hooks::config::HooksSettings = std::collections::HashMap::new();
        settings.insert(
            hooks::HookEvent::PostToolUse,
            vec![hooks::config::HookConfig::Command {
                command: format!("touch {}", marker.display()),
                shell: None,
                timeout: None,
                if_pattern: None,
                only_on_error: None,
                once: None,
                async_rewake: None,
                on_error: hooks::OnError::Allow,
            }],
        );
        let hooks_runner = Arc::new(hooks::HookRunner::new(settings));
        let ctx = test_exec_ctx(tools, hooks_runner);

        let result = execute_tool_with_telemetry(&ctx, "Probe", serde_json::json!({})).await;
        assert!(result.is_ok());
        assert!(
            marker.exists(),
            "PostToolUse hook should have run and created the marker file"
        );
    }

    /// Builds a `ToolExecCtx` whose runner touches `marker` when `event` fires.
    fn ctx_with_marker_hook(
        event: hooks::HookEvent,
        marker: &std::path::Path,
        tools: Arc<base::tool::InMemoryToolRegistry>,
    ) -> ToolExecCtx {
        let mut settings: hooks::config::HooksSettings = std::collections::HashMap::new();
        settings.insert(
            event,
            vec![hooks::config::HookConfig::Command {
                command: format!("touch {}", marker.display()),
                shell: None,
                timeout: None,
                if_pattern: None,
                only_on_error: None,
                once: None,
                async_rewake: None,
                on_error: hooks::OnError::Allow,
            }],
        );
        test_exec_ctx(tools, Arc::new(hooks::HookRunner::new(settings)))
    }

    /// `PostToolUseFailure` is the error-only sibling of `PostToolUse`, so a
    /// hook that only cares about failures does not have to subscribe to
    /// every tool call and filter on `is_error`.
    #[tokio::test]
    async fn post_tool_use_failure_fires_only_when_the_tool_errors() {
        let dir = tempfile::tempdir().unwrap();
        let tools = Arc::new(base::tool::InMemoryToolRegistry::new());
        tools.register(std::sync::Arc::new(FailingTool));
        tools.register(std::sync::Arc::new(ProbeTool {
            called: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }));

        // Failing tool → fires.
        let on_error = dir.path().join("on-error");
        let ctx = ctx_with_marker_hook(
            hooks::HookEvent::PostToolUseFailure,
            &on_error,
            tools.clone(),
        );
        let _ = execute_tool_with_telemetry(&ctx, "Failing", serde_json::json!({})).await;
        assert!(
            on_error.exists(),
            "PostToolUseFailure should fire when the tool returns an error"
        );

        // Succeeding tool → does not fire.
        let on_success = dir.path().join("on-success");
        let ctx = ctx_with_marker_hook(hooks::HookEvent::PostToolUseFailure, &on_success, tools);
        let r = execute_tool_with_telemetry(&ctx, "Probe", serde_json::json!({})).await;
        assert!(r.is_ok());
        assert!(
            !on_success.exists(),
            "PostToolUseFailure must not fire for a successful call"
        );
    }

    /// `PermissionDenied` fires for a rule-engine `Deny` (this test) and for a
    /// host answering a prompt with a denial (the other branch in
    /// `execute_tool_inner`).
    #[tokio::test]
    async fn permission_denied_hook_fires_on_a_rule_engine_deny() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("denied");
        let tools = Arc::new(base::tool::InMemoryToolRegistry::new());
        tools.register(std::sync::Arc::new(ProbeTool {
            called: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }));

        let mut ctx = ctx_with_marker_hook(hooks::HookEvent::PermissionDenied, &marker, tools);
        ctx.permission = Arc::new(DenyingPermission);

        let result = execute_tool_with_telemetry(&ctx, "Probe", serde_json::json!({})).await;
        assert!(result.is_err(), "the call must still be denied");
        assert!(
            marker.exists(),
            "PermissionDenied should have fired for the refused call"
        );
    }

    /// A lifecycle hook must not be able to veto — every firing site discards
    /// the response. A `PostToolUseFailure` hook returning `continue: false`
    /// (which *would* stop the turn from `PostToolUse`) changes nothing.
    #[tokio::test]
    async fn lifecycle_hooks_cannot_discontinue_the_turn() {
        let tools = Arc::new(base::tool::InMemoryToolRegistry::new());
        tools.register(std::sync::Arc::new(FailingTool));

        let mut settings: hooks::config::HooksSettings = std::collections::HashMap::new();
        settings.insert(
            hooks::HookEvent::PostToolUseFailure,
            vec![hooks::config::HookConfig::Command {
                command: r#"echo '{"continue":false}'"#.into(),
                shell: None,
                timeout: None,
                if_pattern: None,
                only_on_error: None,
                once: None,
                async_rewake: None,
                on_error: hooks::OnError::Allow,
            }],
        );
        let ctx = test_exec_ctx(tools, Arc::new(hooks::HookRunner::new(settings)));

        let _ = execute_tool_with_telemetry(&ctx, "Failing", serde_json::json!({})).await;
        assert!(
            !ctx.discontinued.load(std::sync::atomic::Ordering::SeqCst),
            "a lifecycle hook must not be able to end the turn"
        );
    }

    #[tokio::test]
    async fn post_tool_use_hook_discontinue_sets_the_shared_flag() {
        let tools = Arc::new(base::tool::InMemoryToolRegistry::new());
        tools.register(std::sync::Arc::new(ProbeTool {
            called: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }));

        let mut settings: hooks::config::HooksSettings = std::collections::HashMap::new();
        settings.insert(
            hooks::HookEvent::PostToolUse,
            vec![hooks::config::HookConfig::Command {
                command: r#"echo '{"continue":false}'"#.into(),
                shell: None,
                timeout: None,
                if_pattern: None,
                only_on_error: None,
                once: None,
                async_rewake: None,
                on_error: hooks::OnError::Allow,
            }],
        );
        let hooks_runner = Arc::new(hooks::HookRunner::new(settings));
        let ctx = test_exec_ctx(tools, hooks_runner);

        assert!(!ctx.discontinued.load(std::sync::atomic::Ordering::Relaxed));
        let result = execute_tool_with_telemetry(&ctx, "Probe", serde_json::json!({})).await;
        assert!(
            result.is_ok(),
            "the tool call itself still succeeds: {result:?}"
        );
        assert!(
            ctx.discontinued.load(std::sync::atomic::Ordering::Relaxed),
            "PostToolUse discontinue should have set the shared flag"
        );
    }

    /// A `PostToolUse` hook returning `decision: "block"` denies *this call's*
    /// result specifically (`execute_tool_inner`'s return value becomes an
    /// `Err`, which is what ends up as the tool_result the model sees),
    /// without ending the turn unless the hook *also* sets `continue: false`.
    #[tokio::test]
    async fn post_tool_use_hook_block_denies_only_this_result() {
        let tools = Arc::new(base::tool::InMemoryToolRegistry::new());
        tools.register(std::sync::Arc::new(ProbeTool {
            called: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }));

        let mut settings: hooks::config::HooksSettings = std::collections::HashMap::new();
        settings.insert(
            hooks::HookEvent::PostToolUse,
            vec![hooks::config::HookConfig::Command {
                command: r#"echo '{"decision":"block","message":"looked risky"}'"#.into(),
                shell: None,
                timeout: None,
                if_pattern: None,
                only_on_error: None,
                once: None,
                async_rewake: None,
                on_error: hooks::OnError::Allow,
            }],
        );
        let hooks_runner = Arc::new(hooks::HookRunner::new(settings));
        let ctx = test_exec_ctx(tools, hooks_runner);

        let result = execute_tool_with_telemetry(&ctx, "Probe", serde_json::json!({})).await;
        let err = result.expect_err("blocked PostToolUse result should surface as an Err");
        assert!(
            err.contains("looked risky"),
            "denial message should carry the hook's reason: {err}"
        );
        assert!(
            !ctx.discontinued.load(std::sync::atomic::Ordering::Relaxed),
            "a bare decision:block (no continue:false) must not end the whole turn, \
             only deny this one call's result"
        );
    }

    struct DenyAllTestPermission;
    #[async_trait::async_trait]
    impl base::interface::permission::Permission for DenyAllTestPermission {
        async fn check(
            &self,
            _tool_name: &str,
            _tool_input: &serde_json::Value,
            _cwd: &std::path::Path,
            _session_id: &str,
        ) -> base::interface::permission::PermissionOutcome {
            base::interface::permission::PermissionOutcome::Deny {
                reason: "denied by test".into(),
            }
        }
    }

    /// Always returns the same fixed `prompt_id` so a test can reach into
    /// `ctx.pending_permissions` and answer it from outside `execute_tool_inner`.
    struct PromptTestPermission {
        prompt_id: &'static str,
    }
    #[async_trait::async_trait]
    impl base::interface::permission::Permission for PromptTestPermission {
        async fn check(
            &self,
            _tool_name: &str,
            _tool_input: &serde_json::Value,
            _cwd: &std::path::Path,
            _session_id: &str,
        ) -> base::interface::permission::PermissionOutcome {
            base::interface::permission::PermissionOutcome::Prompt {
                prompt_id: self.prompt_id.into(),
                message: "allow?".into(),
                paths: vec![],
            }
        }
    }

    #[tokio::test]
    async fn permission_deny_blocks_the_call() {
        let called = std::sync::Arc::new(std::sync::Mutex::new(None));
        let tools = Arc::new(base::tool::InMemoryToolRegistry::new());
        tools.register(std::sync::Arc::new(ProbeTool {
            called: called.clone(),
        }));
        let hooks_runner = Arc::new(hooks::HookRunner::new(std::collections::HashMap::new()));
        let ctx =
            test_exec_ctx_with_permission(tools, hooks_runner, Arc::new(DenyAllTestPermission));

        let result = execute_tool_with_telemetry(&ctx, "Probe", serde_json::json!({})).await;
        assert!(result.is_err(), "expected the tool call to be denied");
        assert!(result.unwrap_err().contains("denied by test"));
        assert!(
            called.lock().unwrap().is_none(),
            "tool must not have actually run"
        );
    }

    #[tokio::test]
    async fn permission_prompt_with_no_response_blocks_until_cancelled() {
        // Conventional confirmation-prompt UX: no arbitrary timeout, the
        // call genuinely waits for an answer. The only thing that should
        // unblock it without one is the session's own cancellation token
        // (`session.close` / client disconnect on the daemon side) — and
        // when that happens, the call must NOT proceed (nobody approved
        // it).
        let called = std::sync::Arc::new(std::sync::Mutex::new(None));
        let tools = Arc::new(base::tool::InMemoryToolRegistry::new());
        tools.register(std::sync::Arc::new(ProbeTool {
            called: called.clone(),
        }));
        let hooks_runner = Arc::new(hooks::HookRunner::new(std::collections::HashMap::new()));
        let mut ctx = test_exec_ctx_with_permission(
            tools,
            hooks_runner,
            Arc::new(PromptTestPermission {
                prompt_id: "unanswered",
            }),
        );
        // The real asker, so the registration this test is about actually
        // exists to be cleaned up.
        let pending: Arc<
            std::sync::Mutex<
                std::collections::HashMap<
                    String,
                    tokio::sync::oneshot::Sender<crate::agent::PermissionDecision>,
                >,
            >,
        > = Default::default();
        let (events, _events_rx) = tokio::sync::mpsc::unbounded_channel();
        ctx.elicitation = Arc::new(crate::elicitation::ChannelElicitation::new(
            crate::event_bus::EventBus::new(events),
            pending.clone(),
        ));
        let (telemetry_tx, mut telemetry_rx) = tokio::sync::mpsc::channel(16);
        ctx.telemetry_handle = telemetry::TelemetryHandle::new(telemetry_tx);

        let call = execute_tool_with_telemetry(&ctx, "Probe", serde_json::json!({"x": 1}));
        tokio::pin!(call);

        // Give it a real chance to actually be waiting, not just scheduled —
        // if it resolved here, that would mean it did NOT block.
        tokio::select! {
            _ = &mut call => panic!("must not resolve before an answer or cancellation"),
            _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
        }
        assert!(
            called.lock().unwrap().is_none(),
            "tool must not have run while still waiting for an answer"
        );

        ctx.cancel.cancel();
        let result = call.await;
        assert!(result.is_err(), "cancellation must not fall back to Permit");
        assert!(result.unwrap_err().contains("cancelled"));
        assert!(called.lock().unwrap().is_none());
        assert!(
            pending.lock().unwrap().is_empty(),
            "the cancelled registration should have been cleaned up"
        );

        let asked = telemetry_rx
            .recv()
            .await
            .expect("permission_decision should have been recorded");
        assert_eq!(asked.kind(), "permission_decision");
        let event = telemetry_rx
            .recv()
            .await
            .expect("tool_cancelled should have been recorded");
        assert_eq!(event.kind(), "tool_cancelled");
    }

    #[tokio::test]
    async fn permission_prompt_answered_deny_blocks_the_call() {
        let called = std::sync::Arc::new(std::sync::Mutex::new(None));
        let tools = Arc::new(base::tool::InMemoryToolRegistry::new());
        tools.register(std::sync::Arc::new(ProbeTool {
            called: called.clone(),
        }));
        let hooks_runner = Arc::new(hooks::HookRunner::new(std::collections::HashMap::new()));
        let mut ctx = test_exec_ctx_with_permission(
            tools,
            hooks_runner,
            Arc::new(PromptTestPermission {
                prompt_id: "answered-deny",
            }),
        );
        ctx.elicitation = base::interface::elicitation::FixedElicitation::new(
            base::interface::elicitation::ElicitOutcome::answered(
                &crate::agent::PermissionDecision::Deny {
                    reason: "answered no".into(),
                },
            ),
        );

        let result = execute_tool_with_telemetry(&ctx, "Probe", serde_json::json!({})).await;
        assert!(result.is_err(), "expected the real Deny response to win");
        assert!(result.unwrap_err().contains("answered no"));
        assert!(called.lock().unwrap().is_none());
    }

    /// Like `ProbeTool`, but overrides `permission_match_content` so the
    /// derived rule pattern is non-empty — needed to exercise the
    /// `PermitAlways` disk-write path, since `settings_patch::build_rule_string`
    /// returns `None` (nothing to write) for empty/absent content.
    struct ContentProbeTool {
        called: std::sync::Arc<std::sync::Mutex<Option<serde_json::Value>>>,
    }
    #[async_trait::async_trait]
    impl base::tool::Tool for ContentProbeTool {
        fn name(&self) -> &str {
            "Probe"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        fn permission_match_content(&self, _input: &serde_json::Value) -> Option<String> {
            Some("git status".into())
        }
        async fn call(
            &self,
            input: serde_json::Value,
            _ctx: base::tool::ToolContext,
            _progress: base::tool::ProgressSender,
        ) -> Result<base::tool::ToolResult, base::error::ToolError> {
            *self.called.lock().unwrap() = Some(input);
            Ok(base::tool::ToolResult::text("probe-ran"))
        }
    }

    type RecordedAllows = std::sync::Arc<std::sync::Mutex<Vec<(String, Option<String>)>>>;

    /// Records every `add_persistent_allow` call it receives, and always
    /// prompts on `check` (fixed `prompt_id`) so a test can drive the
    /// `InputMessage::PermissionResponse` -> `PermitAlways` path the same
    /// way a real interactive "always allow" answer would.
    struct PromptThenRecordPersistentAllow {
        prompt_id: &'static str,
        recorded: RecordedAllows,
    }
    #[async_trait::async_trait]
    impl base::interface::permission::Permission for PromptThenRecordPersistentAllow {
        async fn check(
            &self,
            _tool_name: &str,
            _tool_input: &serde_json::Value,
            _cwd: &std::path::Path,
            _session_id: &str,
        ) -> base::interface::permission::PermissionOutcome {
            base::interface::permission::PermissionOutcome::Prompt {
                prompt_id: self.prompt_id.into(),
                message: "allow?".into(),
                paths: vec![],
            }
        }
        fn add_persistent_allow(&self, tool_name: &str, rule_content: Option<&str>) {
            self.recorded
                .lock()
                .unwrap()
                .push((tool_name.to_string(), rule_content.map(|s| s.to_string())));
        }
    }

    #[tokio::test]
    async fn permission_prompt_answered_permit_always_local_persists_rule_to_disk() {
        // End-to-end: a `PermitAlways{scope: Local}` response must (1) call
        // `Permission::add_persistent_allow` so the in-memory rule engine
        // allows the rest of this session immediately, using the same
        // `tool.permission_match_content` derivation the rule engine itself
        // uses to match calls, and (2) write the equivalent rule to
        // `<project_root>/.agix/settings.local.json` (NOT `settings.json` —
        // that's the shared/committed project tier), in the shape
        // `Settings::load` reads back — and (3) still let the pending tool
        // call proceed, same as a plain `Permit` would.
        let dir = tempfile::tempdir().unwrap();
        let agix_dir = dir.path().join(".agix");
        let called = std::sync::Arc::new(std::sync::Mutex::new(None));
        let tools = Arc::new(base::tool::InMemoryToolRegistry::new());
        tools.register(std::sync::Arc::new(ContentProbeTool {
            called: called.clone(),
        }));
        let hooks_runner = Arc::new(hooks::HookRunner::new(std::collections::HashMap::new()));
        let recorded = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let permission = Arc::new(PromptThenRecordPersistentAllow {
            prompt_id: "permit-always-local",
            recorded: recorded.clone(),
        });

        let ctx = ToolExecCtx {
            tool_use_id: "toolu_test".into(),
            tools,
            cwd: dir.path().to_path_buf(),
            local_settings_path: agix_dir.join(base::interface::settings::SETTINGS_LOCAL_FILE),
            session_id: "test-session".into(),
            turn_no: 1,
            agent_depth: 0,
            telemetry_handle: telemetry::TelemetryHandle::noop(),
            turn_id: "test-turn".into(),
            cancel: tokio_util::sync::CancellationToken::new(),
            hooks: hooks_runner,
            discontinued: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            config: Arc::new(base::context::EngineConfig::defaults_for("test")),
            permission,
            session_state: Arc::new(base::context::SessionState::new(dir.path().to_path_buf())),
            tool_middleware: Arc::new(Vec::new()),
            result_transformers: Arc::new(Vec::new()),
            elicitation: base::interface::elicitation::FixedElicitation::new(
                base::interface::elicitation::ElicitOutcome::answered(
                    &crate::agent::PermissionDecision::PermitAlways {
                        scope: crate::agent::PersistScope::Local,
                    },
                ),
            ),
            images: Arc::new(std::sync::Mutex::new(Vec::new())),
            from_code: false,
            events: None,
        };

        let result = execute_tool_with_telemetry(&ctx, "Probe", serde_json::json!({})).await;

        assert!(
            result.is_ok(),
            "PermitAlways must let the pending tool call proceed: {result:?}"
        );
        assert!(
            called.lock().unwrap().is_some(),
            "the tool must actually have run"
        );
        assert_eq!(
            recorded.lock().unwrap().as_slice(),
            &[("Probe".to_string(), Some("git status".to_string()))],
            "add_persistent_allow must be called with the tool's derived match content"
        );

        let settings_local_path = agix_dir.join(base::interface::settings::SETTINGS_LOCAL_FILE);
        assert!(
            settings_local_path.exists(),
            "expected {} to exist — anywhere else is outside the Bash sandbox's \
             deny-file-write protection for this file",
            settings_local_path.display()
        );

        // The rule has to survive a real load, not just be present as JSON:
        // a shape `Settings` doesn't deserialize would leave the user
        // re-answering the same prompt every restart.
        let empty = tempfile::tempdir().unwrap();
        let loaded = base::interface::settings::Settings::load(
            empty.path().to_path_buf(),
            empty.path().to_path_buf(),
            agix_dir.clone(),
            "code",
            "m",
        );
        let rules = permissions::rule::rules_from_all_tiers(&loaded);
        assert_eq!(
            rules.len(),
            1,
            "expected exactly one loaded rule: {rules:?}"
        );
        assert_eq!(rules[0].tool_name, "Probe");
        assert_eq!(rules[0].rule_content.as_deref(), Some("git status"));
        assert_eq!(rules[0].behavior, base::permission::RuleBehavior::Allow);
        assert_eq!(
            rules[0].source,
            base::permission::RuleSource::LocalSettings,
            "must carry the local tier's source so its priority outranks ProjectSettings"
        );

        // Not written to the shared/committed settings.json tier.
        assert!(
            !agix_dir
                .join(base::interface::settings::SETTINGS_FILE)
                .exists(),
            "must not touch the shared/committed settings.json tier"
        );
    }

    #[test]
    fn turn_outcome_default() {
        let outcome = TurnOutcome::default();
        assert_eq!(outcome.api_calls, 0);
    }

    fn budget_target(input: &str) -> Option<u64> {
        find_token_budget_directive(input).map(|(target, _)| target)
    }

    fn strip_budget(input: &str) -> String {
        match find_token_budget_directive(input) {
            Some((_, range)) => strip_token_budget_directive(input, range),
            None => input.to_string(),
        }
    }

    #[test]
    fn a_number_without_a_multiplier_or_the_word_tokens_is_not_a_budget() {
        for text in [
            "please use 4 spaces",
            "use 1 banana",
            "+1",
            "+12 more reviewers",
            "set 3 tokens aside",
            "use 500k",
            "spend 2M tokensmith",
        ] {
            assert_eq!(budget_target(text), None, "{text}");
            assert_eq!(strip_budget(text), text, "{text}");
        }
    }

    #[test]
    fn a_budget_after_multibyte_text_is_found_and_stripped_without_panicking() {
        assert_eq!(budget_target("İ use é 500k tokens"), Some(500_000));
        assert_eq!(strip_budget("İ use é 500k tokens"), "İ use é");
        assert_eq!(
            strip_budget("重构这个模块 USE 2M Output Tokens 然后提交"),
            "重构这个模块 然后提交"
        );
    }

    #[test]
    fn stripping_keeps_the_rest_of_the_message_and_its_line_breaks() {
        assert_eq!(
            strip_budget("Fix the parser.\nspend 1m tokens\nThen run the tests."),
            "Fix the parser.\nThen run the tests."
        );
        assert_eq!(strip_budget("  +2M  "), "");
    }

    #[test]
    fn a_redacting_transformer_also_takes_the_structured_result_away() {
        let mut ctx = test_exec_ctx(
            Arc::new(base::tool::InMemoryToolRegistry::new()),
            Arc::new(hooks::HookRunner::new(std::collections::HashMap::new())),
        );
        ctx.result_transformers =
            Arc::new(vec![
                Arc::new(base::interface::tool_result::RedactLiterals::new([
                    "hunter2",
                ])) as Arc<dyn base::interface::tool_result::ToolResultTransformer>,
            ]);
        let answer = |text: &str| {
            Ok(base::interface::tool_middleware::ToolAnswer {
                text: text.into(),
                new_messages: None,
                reported_failure: false,
                structured: Some(serde_json::json!({"password": text})),
            })
        };

        let redacted = apply_result_transformers(
            &ctx,
            "Read",
            &serde_json::json!({}),
            answer("password: hunter2"),
        )
        .unwrap();
        assert!(!redacted.text.contains("hunter2"), "{}", redacted.text);
        assert_eq!(redacted.structured, None);

        let untouched =
            apply_result_transformers(&ctx, "Read", &serde_json::json!({}), answer("nothing here"))
                .unwrap();
        assert_eq!(
            untouched.structured,
            Some(serde_json::json!({"password": "nothing here"}))
        );
    }

    #[test]
    fn parse_shorthand_500k() {
        assert_eq!(budget_target("+500k do this task"), Some(500_000));
    }

    #[test]
    fn parse_shorthand_2m() {
        assert_eq!(budget_target("+2M"), Some(2_000_000));
    }

    #[test]
    fn parse_shorthand_1b() {
        assert_eq!(budget_target("+1B"), Some(1_000_000_000));
    }

    #[test]
    fn parse_spend_natural_language() {
        assert_eq!(
            budget_target("spend 2M tokens on refactoring"),
            Some(2_000_000)
        );
    }

    #[test]
    fn parse_use_natural_language() {
        assert_eq!(budget_target("use 500k output tokens"), Some(500_000));
    }

    #[test]
    fn select_skills_keeping_description_prefers_most_invoked_when_budget_is_tight() {
        // Three skills, each description costs 50+overhead; budget only
        // fits one. "b" has the highest count, so it alone should keep its
        // description; "a" and "c" degrade to name-only despite being
        // alphabetically first/earlier in the list.
        let names = ["a", "b", "c"];
        let counts = [1u32, 5, 2];
        let lens = [50usize, 50, 50];
        let keeps = select_skills_keeping_description(&names, &counts, &lens, 60, 5);
        assert_eq!(keeps, vec![false, true, false]);
    }

    #[test]
    fn select_skills_keeping_description_keeps_everyone_when_budget_is_ample() {
        let names = ["a", "b"];
        let counts = [0u32, 0];
        let lens = [50usize, 50];
        let keeps = select_skills_keeping_description(&names, &counts, &lens, 10_000, 5);
        assert_eq!(keeps, vec![true, true]);
    }

    #[test]
    fn select_skills_keeping_description_ties_break_on_name_not_input_order() {
        // Equal counts (the common fresh-session case: everything at 0) —
        // must not depend on whatever order the caller's Vec happened to be
        // in; alphabetically-first name wins the tiebreak deterministically.
        let names = ["zebra", "apple"];
        let counts = [0u32, 0];
        let lens = [50usize, 50];
        // Budget fits exactly one entry.
        let keeps = select_skills_keeping_description(&names, &counts, &lens, 55, 5);
        assert_eq!(
            keeps,
            vec![false, true],
            "apple (alphabetically first) should win the tie"
        );
    }

    #[test]
    fn select_skills_keeping_description_drops_everyone_under_a_near_zero_budget() {
        let names = ["a"];
        let counts = [99u32];
        let lens = [50usize];
        let keeps = select_skills_keeping_description(&names, &counts, &lens, 1, 5);
        assert_eq!(keeps, vec![false]);
    }

    #[test]
    fn combined_skill_text_capped_appends_when_to_use() {
        let combined = combined_skill_text_capped("Reviews code", Some("Use after edits"));
        assert_eq!(combined, "Reviews code Use after edits");
    }

    #[test]
    fn combined_skill_text_capped_handles_absent_when_to_use() {
        let combined = combined_skill_text_capped("Reviews code", None);
        assert_eq!(combined, "Reviews code");
    }

    #[test]
    fn combined_skill_text_capped_enforces_1536_char_ceiling_independent_of_listing_budget() {
        let long_desc = "a".repeat(2000);
        let combined = combined_skill_text_capped(&long_desc, Some(&"b".repeat(200)));
        assert_eq!(combined.len(), SKILL_COMBINED_TEXT_CAP);
    }

    #[test]
    fn truncate_at_char_boundary_never_splits_a_multibyte_char() {
        // Every char here is 3 bytes (UTF-8) — a naive byte-index cut at an
        // odd offset would panic or produce invalid UTF-8.
        let s = "审查代码变更的正确性";
        for n in 0..s.len() + 2 {
            let truncated = truncate_at_char_boundary(s, n);
            assert!(truncated.len() <= n.min(s.len()));
            // Must still be valid UTF-8 (would already have panicked in
            // `truncate_at_char_boundary` itself if not, but assert the
            // round-trip explicitly as the actual behavioral guarantee).
            let _ = truncated.chars().count();
        }
    }

    #[test]
    fn truncate_at_char_boundary_returns_whole_string_when_under_limit() {
        assert_eq!(truncate_at_char_boundary("short", 100), "short");
    }

    #[test]
    fn parse_set_natural_language() {
        assert_eq!(budget_target("set 1B output tokens"), Some(1_000_000_000));
    }

    #[test]
    fn parse_budget_natural_language() {
        assert_eq!(
            budget_target("budget 100k tokens for testing"),
            Some(100_000)
        );
    }

    #[test]
    fn parse_no_directive() {
        assert_eq!(budget_target("hello world"), None);
    }

    #[test]
    fn parse_empty_string() {
        assert_eq!(budget_target(""), None);
    }

    #[test]
    fn strip_shorthand_500k() {
        assert_eq!(strip_budget("+500k do this task"), "do this task");
    }

    #[test]
    fn strip_spend_natural_language() {
        assert_eq!(
            strip_budget("spend 2M tokens refactor this code"),
            "refactor this code"
        );
    }

    #[test]
    fn strip_no_directive() {
        assert_eq!(strip_budget("hello world"), "hello world");
    }

    #[test]
    fn test_token_budget_exceeds_limit() {
        // Above 2B should be rejected
        assert_eq!(budget_target("+999999999999999999"), None);
    }

    #[test]
    fn prompt_context_includes_os() {
        let settings = base::interface::settings::Settings {
            model: base::interface::settings::ModelSettings {
                api_type: base::provider::ApiType::Anthropic,
                base_url: "https://api.example.com".into(),
                auth_token: "test".into(),
                model_name: "test-model".into(),
                max_tokens: 4096,
                thinking_mode: ThinkingMode::Auto,
                effort: None,
                fallback_model: None,
            },
            paths: base::interface::settings::PathSettings {
                user_data_dir: "/tmp/user".into(),
                global_data_dir: "/tmp/global".into(),
                local_data_dir: "/tmp/local".into(),
                scope: "code".into(),
            },
            execution: Default::default(),
            compaction: Default::default(),
            sandbox: Default::default(),
            plugins: Default::default(),
            instruction_file: None,
            prompt_append: None,
            prompt_override: None,
            recorder: None,
            telemetry_url: None,
            memory_enabled: true,
            disable_skill_shell_execution: false,
            path_style: Default::default(),
            permission_mode: base::interface::settings::PermissionMode::Default,
            permission_rules: Vec::new(),
            local_permission_rules: Vec::new(),
            allow_client_permission_override: false,
            telemetry_enabled: false,
            hooks_config: None,
            project_trust: Default::default(),
            project_layer: Default::default(),
            mcp_servers: Default::default(),
            providers: Default::default(),
            default_provider: None,
            task_models: Default::default(),
            language: None,
            session_dir: None,
            scripts: Vec::new(),
        };
        let ctx = build_prompt_context(
            &settings,
            None,
            None,
            None,
            "test-model",
            false,
            &base::interface::environment::SystemEnvironment,
        );
        assert_eq!(ctx.os, std::env::consts::OS);

        // `ctx.shell` comes from `FrozenContext.shell`, and `ctx.model_name`
        // from the caller-supplied effective model, so after an
        // overloaded-fallback switch the prompt describes the fallback model.
        let frozen = base::frozen::FrozenContext {
            shell: Some("/usr/bin/zsh".into()),
            ..Default::default()
        };
        let ctx_with_frozen = build_prompt_context(
            &settings,
            Some(&frozen),
            None,
            None,
            "fallback-model",
            false,
            &base::interface::environment::SystemEnvironment,
        );
        assert_eq!(ctx_with_frozen.shell, "/usr/bin/zsh");
        assert_eq!(ctx_with_frozen.model_name, "fallback-model");
        assert_ne!(ctx_with_frozen.model_name, settings.model.model_name);
    }

    /// Answers directly (no tool calls) and records every message it was sent,
    /// so the test can check what the model actually saw.
    struct RecordingAnswerModel {
        seen: Arc<std::sync::Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl base::interface::model::Model for RecordingAnswerModel {
        fn api_type(&self) -> base::provider::ApiType {
            base::provider::ApiType::Anthropic
        }
        async fn stream(
            &self,
            _prompt_blocks: Vec<base::interface::prompt::PromptBlock>,
            _tools: Vec<base::interface::model::ToolDef>,
            messages: Vec<ModelMessage>,
            _params: base::interface::model::StreamParams,
            _cancel: tokio_util::sync::CancellationToken,
        ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError>
        {
            let mut seen = self.seen.lock().unwrap();
            for m in &messages {
                for b in &m.content {
                    if let ModelContentBlock::Text { text } = b {
                        seen.push(text.clone());
                    }
                }
            }
            let events = vec![
                Ok(base::interface::model::ModelEvent::TextDelta {
                    text: "answering directly".into(),
                }),
                Ok(base::interface::model::ModelEvent::EndTurn {
                    stop_reason: "end_turn".into(),
                    usage: Default::default(),
                }),
            ];
            Ok(Box::new(futures::stream::iter(events)))
        }
    }

    /// Answers directly and reports a caller-controlled `Usage`, so budget
    /// tests can drive `total_tokens_used` without needing a real provider.
    struct FixedUsageModel {
        usage: base::interface::model::Usage,
    }

    #[async_trait::async_trait]
    impl base::interface::model::Model for FixedUsageModel {
        fn api_type(&self) -> base::provider::ApiType {
            base::provider::ApiType::Anthropic
        }
        async fn stream(
            &self,
            _prompt_blocks: Vec<base::interface::prompt::PromptBlock>,
            _tools: Vec<base::interface::model::ToolDef>,
            _messages: Vec<ModelMessage>,
            _params: base::interface::model::StreamParams,
            _cancel: tokio_util::sync::CancellationToken,
        ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError>
        {
            let events = vec![
                Ok(base::interface::model::ModelEvent::TextDelta { text: "ok".into() }),
                Ok(base::interface::model::ModelEvent::EndTurn {
                    stop_reason: "end_turn".into(),
                    usage: self.usage,
                }),
            ];
            Ok(Box::new(futures::stream::iter(events)))
        }
    }

    /// A policy is handed every figure the provider reported, cache included.
    ///
    /// What counts as spent is the policy's judgement, and it cannot make one
    /// on figures it never receives — and on a provider with the cache warm,
    /// the cache fields are most of what the turn read.
    #[tokio::test]
    async fn a_policy_is_told_what_the_cache_cost() {
        use std::sync::Mutex;

        #[derive(Default)]
        struct Recording(Mutex<Vec<base::interface::budget_policy::Spend>>);

        impl base::interface::budget_policy::BudgetPolicy for Recording {
            fn on_usage(
                &self,
                spend: &base::interface::budget_policy::Spend,
            ) -> base::interface::budget_policy::Spending {
                self.0.lock().unwrap().push(*spend);
                base::interface::budget_policy::Spending::WithinBudget
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let reported = base::interface::model::Usage {
            input_tokens: 400,
            output_tokens: 300,
            cache_creation_input_tokens: 2_000,
            cache_read_input_tokens: 30_000,
        };
        let model: Arc<dyn base::interface::model::Model> =
            Arc::new(FixedUsageModel { usage: reported });
        let scene: Arc<dyn base::interface::scene::Scene> =
            Arc::new(scene::code::CodeScene::default());
        let policy = Arc::new(Recording::default());

        let (telemetry_tx, _telemetry_rx) = tokio::sync::mpsc::channel(16);
        let (mut agent, _event_rx, _input_tx) = crate::agent::Builder::new()
            .scene(scene)
            .model(model)
            .settings(Arc::new(recall_test_settings(tmp.path())))
            .telemetry_handle(telemetry::TelemetryHandle::new(telemetry_tx))
            .budget_policy(policy.clone())
            .skip_warmup(true)
            .build()
            .expect("build should succeed");

        agent
            .process_turn(
                InputMessage::User {
                    content: "hello".into(),
                    attachments: vec![],
                    turn_id: "t1".into(),
                },
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .expect("process_turn should succeed");

        let seen = policy.0.lock().unwrap();
        let spend = seen
            .first()
            .expect("the policy was never asked about this turn");
        assert_eq!(
            *spend,
            base::interface::budget_policy::Spend::from(reported),
            "the policy was given a different turn than the one the provider reported"
        );
        assert_eq!(
            spend.total_tokens(),
            700,
            "the older reading has to keep meaning what it meant"
        );
        assert_eq!(
            spend.all_tokens(),
            32_700,
            "reading everything is what the four fields are for"
        );
    }

    /// `max_budget_tokens` must stop the turn on real reported usage — no
    /// dollar conversion, no per-model price table to keep in sync.
    #[tokio::test]
    async fn max_budget_tokens_stops_the_turn_on_reported_usage() {
        let tmp = tempfile::tempdir().unwrap();
        let model: Arc<dyn base::interface::model::Model> = Arc::new(FixedUsageModel {
            usage: base::interface::model::Usage {
                input_tokens: 800,
                output_tokens: 300,
                ..Default::default()
            },
        });
        let scene: Arc<dyn base::interface::scene::Scene> =
            Arc::new(scene::code::CodeScene::default());

        let mut settings = recall_test_settings(tmp.path());
        settings.execution = base::interface::settings::ExecutionSettings {
            max_budget_tokens: Some(1000),
            ..Default::default()
        };

        let (telemetry_tx, mut telemetry_rx) = tokio::sync::mpsc::channel(16);
        let (mut agent, _event_rx, _input_tx) = crate::agent::Builder::new()
            .scene(scene)
            .model(model)
            .settings(Arc::new(settings))
            .telemetry_handle(telemetry::TelemetryHandle::new(telemetry_tx))
            .skip_warmup(true)
            .build()
            .expect("build should succeed");

        let outcome = agent
            .process_turn(
                InputMessage::User {
                    content: "hello".into(),
                    attachments: vec![],
                    turn_id: "t1".into(),
                },
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .expect("process_turn should succeed");

        assert_eq!(
            outcome.stop_reason, "budget_exceeded",
            "800 + 300 = 1100 reported tokens should trip a 1000-token budget"
        );

        // A budget-stopped turn must not be a telemetry blind spot: both a
        // `budget_enforced(TurnStopped)` and the turn's own `turn_complete`
        // have to land, or this exit path silently disappears from any
        // downstream dashboard/alert built on the event stream.
        let mut kinds = Vec::new();
        while let Ok(event) = telemetry_rx.try_recv() {
            kinds.push(event.kind().to_string());
        }
        assert!(
            kinds.contains(&"budget_enforced".to_string()),
            "got: {kinds:?}"
        );
        assert!(
            kinds.contains(&"turn_complete".to_string()),
            "got: {kinds:?}"
        );
    }

    /// A selection call still running when recall's budget runs out is
    /// stopped, not left spending in the background.
    #[tokio::test]
    async fn a_recall_past_its_budget_is_stopped() {
        struct Probe(Arc<tokio::sync::Notify>);
        impl Drop for Probe {
            fn drop(&mut self) {
                self.0.notify_one();
            }
        }
        struct SlowSelection {
            dropped: Arc<tokio::sync::Notify>,
            answer: RecordingAnswerModel,
        }
        #[async_trait::async_trait]
        impl base::interface::model::Model for SlowSelection {
            fn api_type(&self) -> base::provider::ApiType {
                base::provider::ApiType::Anthropic
            }
            async fn stream(
                &self,
                prompt_blocks: Vec<base::interface::prompt::PromptBlock>,
                tools: Vec<base::interface::model::ToolDef>,
                messages: Vec<ModelMessage>,
                params: base::interface::model::StreamParams,
                cancel: tokio_util::sync::CancellationToken,
            ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError>
            {
                let selecting = params.origin.as_ref().and_then(|o| o.purpose())
                    == Some(base::interface::model::call_purpose::MEMORY);
                if selecting {
                    let _probe = Probe(self.dropped.clone());
                    futures::future::pending::<()>().await;
                }
                self.answer
                    .stream(prompt_blocks, tools, messages, params, cancel)
                    .await
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(MemoryStore::new(
            tmp.path().join("user"),
            tmp.path().join("local"),
        ));
        store
            .persist_batch(
                (0..6)
                    .map(|i| DurableMemory {
                        name: format!("m{i}"),
                        description: format!("memory {i}"),
                        memory_type: MemoryType::Project,
                        content: String::new(),
                        source_session_id: String::new(),
                        confidence: 0.9,
                        last_seen: "2026-08-01T00:00:00Z".into(),
                        recall_count: 0,
                    })
                    .collect(),
            )
            .unwrap();
        let dropped = Arc::new(tokio::sync::Notify::new());
        let model = SlowSelection {
            dropped: dropped.clone(),
            answer: RecordingAnswerModel {
                seen: Default::default(),
            },
        };
        let (mut agent, _event_rx, _input_tx) = crate::agent::Builder::new()
            .scene(Arc::new(scene::code::CodeScene::default()))
            .model(Arc::new(model))
            .settings(Arc::new(recall_test_settings(tmp.path())))
            .memory_store(store)
            .skip_warmup(true)
            .build()
            .expect("build should succeed");

        let outcome = agent
            .process_turn(
                InputMessage::User {
                    content: "anything".into(),
                    attachments: vec![],
                    turn_id: "t1".into(),
                },
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(outcome.stop_reason, "end_turn");
        tokio::time::timeout(std::time::Duration::from_secs(30), dropped.notified())
            .await
            .expect("the selection call outlived its turn's recall budget");
    }

    /// Short conversational turns without tool calls are most turns. The
    /// recalled memories must reach the model in the *current* turn, and must carry their content, not
    /// just the one-line description that exists to rank them.
    #[tokio::test]
    async fn recall_reaches_the_model_on_a_turn_with_no_tool_calls() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(MemoryStore::new(
            tmp.path().join("user"),
            tmp.path().join("local"),
        ));
        // One entry: `select_memories_from` short-circuits below its
        // max_results and selects it without a model call.
        store
            .persist_batch(vec![DurableMemory {
                name: "deploy-window".into(),
                description: "when deploys are allowed".into(),
                memory_type: MemoryType::Project,
                content: "Deploys only on Tuesdays, never on a Friday.".into(),
                source_session_id: String::new(),
                confidence: 0.9,
                last_seen: "2026-08-01T00:00:00Z".into(),
                recall_count: 0,
            }])
            .unwrap();

        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let model: Arc<dyn base::interface::model::Model> =
            Arc::new(RecordingAnswerModel { seen: seen.clone() });
        let scene: Arc<dyn base::interface::scene::Scene> =
            Arc::new(scene::code::CodeScene::default());

        let (mut agent, _event_rx, _input_tx) = crate::agent::Builder::new()
            .scene(scene)
            .model(model)
            .settings(Arc::new(recall_test_settings(tmp.path())))
            .memory_store(store)
            .skip_warmup(true)
            .build()
            .expect("build should succeed");

        let outcome = agent
            .process_turn(
                InputMessage::User {
                    content: "can I ship this today?".into(),
                    attachments: vec![],
                    turn_id: "t1".into(),
                },
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .expect("process_turn should succeed");
        assert_eq!(outcome.stop_reason, "end_turn");

        let seen = seen.lock().unwrap();
        let transcript = seen.join("\n");
        assert!(
            transcript.contains("deploy-window"),
            "recalled memory never reached the model: {transcript}"
        );
        assert!(
            transcript.contains("Deploys only on Tuesdays"),
            "recall carried only the description, not the memory content: {transcript}"
        );
    }

    /// A catalog another owner swaps in place: the next turn's
    /// `reload_changed` finds nothing to do, and the change is announced
    /// anyway because this session was shown the old list.
    #[tokio::test]
    async fn a_skill_swapped_into_a_shared_catalog_is_announced() {
        struct Swappable(std::sync::Mutex<Vec<&'static str>>);
        impl base::interface::skill_catalog::SkillCatalog for Swappable {
            fn list(&self) -> Vec<base::frozen::SkillEntry> {
                self.0
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|n| base::frozen::SkillEntry {
                        name: (*n).into(),
                        description: "d".into(),
                        user_invocable: true,
                        ..Default::default()
                    })
                    .collect()
            }
            fn get(&self, name: &str) -> Option<base::frozen::SkillEntry> {
                self.list().into_iter().find(|e| e.name == name)
            }
            fn body(&self, _: &str) -> Option<String> {
                None
            }
            fn reload_changed(&self) -> usize {
                0
            }
            fn discover_for_paths(&self, _: &[PathBuf]) -> Vec<base::frozen::SkillEntry> {
                Vec::new()
            }
            fn activate_for_paths(&self, _: &[PathBuf]) -> Vec<base::frozen::SkillEntry> {
                Vec::new()
            }
            fn invocation_count(&self, _: &str) -> u32 {
                0
            }
            fn last_invoked_seq(&self, _: &str) -> u64 {
                0
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (mut agent, mut event_rx, _input_tx) = crate::agent::Builder::new()
            .scene(Arc::new(scene::code::CodeScene::default()))
            .model(Arc::new(RecordingAnswerModel { seen: seen.clone() }))
            .settings(Arc::new(recall_test_settings(tmp.path())))
            .skip_warmup(true)
            .build()
            .expect("build should succeed");
        let catalog = Arc::new(Swappable(std::sync::Mutex::new(vec!["early", "gone"])));
        agent.skills = catalog.clone();

        let turn = |id: &str| InputMessage::User {
            content: "hello".into(),
            attachments: vec![],
            turn_id: id.into(),
        };
        let changes = |rx: &mut crate::agent::EventReceiver| {
            let mut out = Vec::new();
            while let Ok(event) = rx.try_recv() {
                if let AgentEvent::SkillsChanged { added, removed, .. } = event {
                    out.push((added, removed));
                }
            }
            out
        };
        let cancel = tokio_util::sync::CancellationToken::new;
        agent.process_turn(turn("t1"), cancel()).await.unwrap();
        assert!(
            changes(&mut event_rx).is_empty(),
            "the first turn lists them all"
        );

        *catalog.0.lock().unwrap() = vec!["early", "latecomer"];
        agent.process_turn(turn("t2"), cancel()).await.unwrap();
        assert_eq!(
            changes(&mut event_rx),
            vec![(vec!["latecomer".to_string()], vec!["gone".to_string()])]
        );
        assert!(seen
            .lock()
            .unwrap()
            .iter()
            .any(|t| t.contains("\"latecomer\" was just added")));

        agent.process_turn(turn("t3"), cancel()).await.unwrap();
        assert!(changes(&mut event_rx).is_empty(), "announced once");
    }

    /// `/name` expands through the catalog — the expansion the `Skill` tool
    /// uses — and its grants hold for the turn it starts, not just until
    /// that turn clears the previous one's.
    #[tokio::test]
    async fn a_slash_command_expands_through_the_catalog_and_grants_for_its_turn() {
        struct Expanding;
        impl base::interface::skill_catalog::SkillCatalog for Expanding {
            fn list(&self) -> Vec<base::frozen::SkillEntry> {
                vec![base::frozen::SkillEntry {
                    name: "build".into(),
                    description: "d".into(),
                    user_invocable: true,
                    ..Default::default()
                }]
            }
            fn get(&self, name: &str) -> Option<base::frozen::SkillEntry> {
                self.list().into_iter().find(|e| e.name == name)
            }
            fn body(&self, _: &str) -> Option<String> {
                Some("the raw body".into())
            }
            fn expand<'a>(
                &'a self,
                _: &'a str,
                args: &'a str,
                _: &'a ToolContext,
                _: &'a dyn base::interface::permission::Permission,
            ) -> futures::future::BoxFuture<
                'a,
                Option<Result<base::interface::skill_catalog::SkillExpansion, String>>,
            > {
                Box::pin(async move {
                    Some(Ok(base::interface::skill_catalog::SkillExpansion {
                        text: format!("EXPANDED for {args}"),
                        grants: vec![("Bash".into(), Some("make".into()))],
                    }))
                })
            }
            fn reload_changed(&self) -> usize {
                0
            }
            fn discover_for_paths(&self, _: &[PathBuf]) -> Vec<base::frozen::SkillEntry> {
                Vec::new()
            }
            fn activate_for_paths(&self, _: &[PathBuf]) -> Vec<base::frozen::SkillEntry> {
                Vec::new()
            }
            fn invocation_count(&self, _: &str) -> u32 {
                0
            }
            fn last_invoked_seq(&self, _: &str) -> u64 {
                0
            }
        }

        #[derive(Default)]
        struct Grants(std::sync::Mutex<Vec<String>>);
        #[async_trait::async_trait]
        impl base::interface::permission::Permission for Grants {
            async fn check(
                &self,
                _: &str,
                _: &serde_json::Value,
                _: &std::path::Path,
                _: &str,
            ) -> base::interface::permission::PermissionOutcome {
                base::interface::permission::PermissionOutcome::Permit
            }
            fn add_temporary_allow(&self, tool: &str, pattern: Option<&str>) {
                self.0.lock().unwrap().push(format!("{tool}({pattern:?})"));
            }
            fn clear_temporary_allows(&self) {
                self.0.lock().unwrap().push("clear".into());
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (mut agent, _event_rx, _input_tx) = crate::agent::Builder::new()
            .scene(Arc::new(scene::code::CodeScene::default()))
            .model(Arc::new(RecordingAnswerModel { seen: seen.clone() }))
            .settings(Arc::new(recall_test_settings(tmp.path())))
            .skip_warmup(true)
            .build()
            .expect("build should succeed");
        let catalog: Arc<dyn base::interface::skill_catalog::SkillCatalog> = Arc::new(Expanding);
        agent.skills = catalog.clone();
        agent.commands = Arc::new(crate::commands::CommandRegistry::from_skill_catalog(
            catalog,
        ));
        let grants = Arc::new(Grants::default());
        agent.permission = grants.clone();

        agent
            .process_turn(
                InputMessage::User {
                    content: "/build release".into(),
                    attachments: vec![],
                    turn_id: "t1".into(),
                },
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .unwrap();

        assert!(seen
            .lock()
            .unwrap()
            .iter()
            .any(|t| t.contains("EXPANDED for release") && !t.contains("the raw body")));
        assert_eq!(
            *grants.0.lock().unwrap(),
            vec!["clear".to_string(), "Bash(Some(\"make\"))".to_string()]
        );
    }

    fn recall_test_settings(root: &std::path::Path) -> base::interface::settings::Settings {
        base::interface::settings::Settings {
            model: base::interface::settings::ModelSettings {
                api_type: base::provider::ApiType::Anthropic,
                base_url: "https://api.example.com".into(),
                auth_token: "test".into(),
                model_name: "test-model".into(),
                max_tokens: 4096,
                thinking_mode: ThinkingMode::Off,
                effort: None,
                fallback_model: None,
            },
            paths: base::interface::settings::PathSettings {
                user_data_dir: root.join("user-data"),
                global_data_dir: root.join("global-data"),
                local_data_dir: root.join("local-data"),
                scope: "code".into(),
            },
            execution: Default::default(),
            compaction: Default::default(),
            sandbox: Default::default(),
            plugins: Default::default(),
            instruction_file: None,
            prompt_append: None,
            prompt_override: None,
            recorder: None,
            telemetry_url: None,
            memory_enabled: true,
            disable_skill_shell_execution: false,
            path_style: Default::default(),
            permission_mode: base::interface::settings::PermissionMode::Default,
            permission_rules: Vec::new(),
            local_permission_rules: Vec::new(),
            allow_client_permission_override: false,
            telemetry_enabled: false,
            hooks_config: None,
            project_trust: Default::default(),
            project_layer: Default::default(),
            mcp_servers: Default::default(),
            providers: Default::default(),
            default_provider: None,
            task_models: Default::default(),
            language: None,
            session_dir: None,
            scripts: Vec::new(),
        }
    }
}

/// Extract durable memories from recent conversation messages using a lightweight
/// model call, then persist them to the MemoryStore. Runs asynchronously after
/// each complete turn — failures are silently logged, never block the user.
///
/// `model`/`model_name` are resolved by the caller — normally via
/// `TaskRouter::model_for("memory")` / `model_name_for("memory")` when a
/// `task_models.memory` override is configured, falling
/// back to the main conversation model otherwise, mirroring
/// `AgentTool::Inner::model_for_subagent`'s pattern for the `"subagent"`
/// task type. `prompt_intro` is the scene's `memory_extraction_prompt()`
/// override, if any — see `Scene::memory_extraction_prompt`.
pub(crate) async fn extract_memories_after_turn(
    store: Arc<MemoryStore>,
    messages: &[ModelMessage],
    model: &dyn base::interface::model::Model,
    model_name: &str,
    prompt_intro: Option<&str>,
    environment: &dyn base::interface::environment::Environment,
    session_id: &str,
) {
    // Only extract if there are messages worth analyzing
    if messages.len() < 2 {
        return;
    }

    const CAP: usize = base::interface::memory::MAX_MEMORIES;
    let counting = store.clone();
    let held = tokio::task::spawn_blocking(move || counting.load_all().len())
        .await
        .unwrap_or(0);
    if held >= CAP {
        tracing::warn!(
            held,
            cap = CAP,
            "the memory store is full; no new memories are extracted until some are removed"
        );
        return;
    }

    // Build a lightweight prompt asking the model to extract memories
    let recent: Vec<&ModelMessage> = messages.iter().rev().take(20).collect();
    let messages_text: String = recent
        .iter()
        .rev()
        .filter_map(|m| {
            let texts: Vec<&str> = m
                .content
                .iter()
                .filter_map(|b| {
                    if let ModelContentBlock::Text { text } = b {
                        Some(text.as_str())
                    } else {
                        None
                    }
                })
                .collect();
            if texts.is_empty() {
                None
            } else {
                Some(texts.join("\n"))
            }
        })
        .collect::<Vec<_>>()
        .join("\n---\n");

    if messages_text.is_empty() {
        return;
    }

    const DEFAULT_MEMORY_EXTRACTION_INTRO: &str = "\
Extract any durable memories from this conversation excerpt. A durable \
memory is a fact about the user, project, or workflow that should persist \
across sessions. Only extract memories that are NOT derivable from the \
current codebase or git history.";
    let intro = prompt_intro.unwrap_or(DEFAULT_MEMORY_EXTRACTION_INTRO);
    let prompt = format!(
        "{intro}\n\n\
For each memory, return a JSON object with:\n\
- name: short kebab-case slug\n\
- description: 1-line summary used to decide relevance during recall\n\
- content: the fact; for feedback/project, follow with **Why:** and **How to apply:** lines\n\
- type: one of [user, feedback, project, reference]\n\
- confidence: 0.0-1.0 (default 0.8)\n\n\
Return only a JSON array of memories. If nothing is worth saving, return []."
    );

    use base::interface::settings::ThinkingMode;
    let request_messages = vec![ModelMessage {
        role: MessageRole::User,
        content: vec![
            ModelContentBlock::Text { text: prompt },
            ModelContentBlock::Text {
                text: messages_text,
            },
        ],
    }];
    let params = base::interface::model::StreamParams {
        model: model_name.to_string(),
        max_tokens: 2000,
        thinking_mode: ThinkingMode::Off,
        effort: None,
        fallback_model: None,
        cache_edits: vec![],
        // A call the session pays for, outside any turn. Saying so is what
        // lets it be counted; the recall half of this feature has always
        // said it (`base::memory`), and this half never did.
        origin: Some(base::interface::model::CallOrigin::auxiliary(
            session_id,
            base::interface::model::call_purpose::MEMORY,
        )),
        input_map: None,
        provider_search: None,
    };
    let mut full_text = String::new();
    let stream_result = model
        .stream(
            vec![],
            vec![],
            request_messages,
            params,
            tokio_util::sync::CancellationToken::new(),
        )
        .await;

    let Ok(mut stream) = stream_result else {
        return;
    };

    use futures::StreamExt;
    while let Some(Ok(event)) = stream.next().await {
        if let base::interface::model::ModelEvent::TextDelta { text } = event {
            full_text.push_str(&text);
        }
    }

    // Parse extracted memories
    let memories: Vec<DurableMemory> =
        if let Ok(parsed) = serde_json::from_str::<Vec<serde_json::Value>>(&full_text) {
            parsed
                .iter()
                .filter_map(|mem| {
                    let name = mem["name"].as_str()?.to_string();
                    let description = mem["description"].as_str().unwrap_or("").to_string();
                    let content = mem["content"].as_str().unwrap_or("").to_string();
                    let memory_type = match mem["type"].as_str().unwrap_or("user") {
                        "feedback" => MemoryType::Feedback,
                        "project" => MemoryType::Project,
                        "reference" => MemoryType::Reference,
                        _ => MemoryType::User,
                    };
                    let confidence = mem["confidence"].as_f64().unwrap_or(0.8);
                    if name.is_empty() || confidence < 0.3 {
                        return None;
                    }
                    let timestamp = environment
                        .now()
                        .format(&time::format_description::well_known::Iso8601::DEFAULT)
                        .unwrap_or_else(|_| "2026-01-01T00:00:00Z".to_string());
                    Some(DurableMemory {
                        name,
                        description,
                        memory_type,
                        content,
                        source_session_id: String::new(),
                        confidence,
                        last_seen: timestamp,
                        recall_count: 0,
                    })
                })
                .collect()
        } else {
            Vec::new()
        };

    // A conversation that quoted a key is no reason to keep the key.
    let memories: Vec<DurableMemory> = memories
        .into_iter()
        .filter(|memory| {
            let found = base::interface::memory::scan_for_secrets(memory);
            if !found.is_empty() {
                let kinds: Vec<&str> = found.iter().map(|f| f.kind).collect();
                tracing::warn!(memory = %memory.name, ?kinds, "not saving an extracted memory that holds a secret");
            }
            found.is_empty()
        })
        .collect();
    if memories.is_empty() {
        return;
    }

    // Route by the type the model was asked to classify each memory with.
    // `project` memories are about *this* working directory ("the merge
    // freeze starts Thursday", "the auth rewrite is compliance-driven") and
    // belong in the project-local directory; everything else describes the
    // user or external resources and is global. Persisting the whole batch
    // globally — which is what the unscoped `persist_batch` does for new
    // names — leaked one project's context into every other project.
    let scoped: Vec<base::interface::memory::ScopedMemory> = memories
        .into_iter()
        .map(|memory| {
            let scope = match memory.memory_type {
                MemoryType::Project => base::interface::memory::MemoryScope::Local,
                _ => base::interface::memory::MemoryScope::User,
            };
            base::interface::memory::ScopedMemory { memory, scope }
        })
        .collect();
    let persisted = tokio::task::spawn_blocking(move || {
        let held: std::collections::HashSet<String> =
            store.load_all().into_iter().map(|m| m.name).collect();
        let mut room = CAP.saturating_sub(held.len());
        let (admitted, refused): (Vec<_>, Vec<_>) = scoped.into_iter().partition(|sm| {
            held.contains(&sm.memory.name)
                || (room > 0 && {
                    room -= 1;
                    true
                })
        });
        if !refused.is_empty() {
            tracing::warn!(
                refused = refused.len(),
                cap = CAP,
                "the memory store is full; new memories past the cap are not saved"
            );
        }
        store.persist_batch_scoped(admitted)?;
        Ok::<_, base::interface::memory::MemoryError>(())
    })
    .await;
    match persisted {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::debug!(error = %e, "auto memory extraction: persist failed"),
        Err(e) => tracing::debug!(error = %e, "auto memory extraction: persist task failed"),
    }
}

#[cfg(test)]
mod attachment_tests {
    use super::*;
    use crate::agent::Attachment;

    /// A 1x1 PNG. Real bytes so the extension-based media-type detection and
    /// the base64 round-trip are both exercised.
    const TINY_PNG: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89,
    ];

    #[tokio::test]
    async fn image_file_becomes_an_image_block_not_text() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("shot.png");
        std::fs::write(&p, TINY_PNG).unwrap();

        let blocks = resolve_attachments(&[Attachment::File {
            path: p.to_string_lossy().into_owned(),
        }])
        .await;

        assert_eq!(blocks.len(), 1);
        let ModelContentBlock::Image { media_type, data } = &blocks[0] else {
            panic!("expected an Image block, got {:?}", blocks[0]);
        };
        assert_eq!(media_type, "image/png");
        // Base64 of the real bytes, not a Debug rendering or a path string.
        use base64::Engine as _;
        assert_eq!(
            data,
            &base64::engine::general_purpose::STANDARD.encode(TINY_PNG)
        );
    }

    #[tokio::test]
    async fn non_image_file_is_inlined_as_labelled_text() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("notes.md");
        std::fs::write(&p, "hello 世界").unwrap();

        let blocks = resolve_attachments(&[Attachment::File {
            path: p.to_string_lossy().into_owned(),
        }])
        .await;

        let ModelContentBlock::Text { text } = &blocks[0] else {
            panic!("expected a Text block");
        };
        assert!(text.contains("hello 世界"));
        assert!(text.contains("notes.md"), "path label missing: {text}");
    }

    /// Clipboard paste: no file exists, the host hands over decoded bytes.
    #[tokio::test]
    async fn pre_decoded_image_passes_through() {
        let blocks = resolve_attachments(&[Attachment::Image {
            media_type: "image/jpeg".into(),
            data: "QUJD".into(),
        }])
        .await;
        assert!(matches!(
            &blocks[0],
            ModelContentBlock::Image { media_type, data }
                if media_type == "image/jpeg" && data == "QUJD"
        ));
    }

    /// An unreadable attachment must not fail the turn — the user still wants
    /// their message sent, and the model needs to be able to explain why it
    /// cannot see what they referred to.
    #[tokio::test]
    async fn unreadable_attachment_degrades_to_an_error_note() {
        let blocks = resolve_attachments(&[Attachment::File {
            path: "/nonexistent/nope.png".into(),
        }])
        .await;
        let ModelContentBlock::Text { text } = &blocks[0] else {
            panic!("expected a Text block explaining the failure");
        };
        assert!(text.contains("error="), "got {text}");
        assert!(text.contains("nope.png"));
    }

    #[tokio::test]
    async fn oversized_image_is_refused_rather_than_inlined() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("huge.png");
        std::fs::write(&p, vec![0u8; (MAX_ATTACHMENT_IMAGE_BYTES + 1) as usize]).unwrap();

        let blocks = resolve_attachments(&[Attachment::File {
            path: p.to_string_lossy().into_owned(),
        }])
        .await;

        let ModelContentBlock::Text { text } = &blocks[0] else {
            panic!("oversized image should degrade to text, not inline");
        };
        assert!(text.contains("too large"), "got {text}");
    }

    #[tokio::test]
    async fn no_attachments_produces_no_blocks() {
        assert!(resolve_attachments(&[]).await.is_empty());
    }
}

#[cfg(test)]
mod extract_memories_tests {
    use super::*;
    use std::sync::Mutex;

    /// Records the `StreamParams`/request text it was called with instead of
    /// hitting a real provider, and returns a canned "[]" response so
    /// `extract_memories_after_turn` completes without persisting anything.
    struct CapturingModel {
        captured: Mutex<Option<(base::interface::model::StreamParams, String)>>,
    }

    impl CapturingModel {
        fn new() -> Self {
            Self {
                captured: Mutex::new(None),
            }
        }
    }

    #[async_trait::async_trait]
    impl base::interface::model::Model for CapturingModel {
        fn api_type(&self) -> base::provider::ApiType {
            base::provider::ApiType::Anthropic
        }
        async fn stream(
            &self,
            _prompt_blocks: Vec<base::interface::prompt::PromptBlock>,
            _tools: Vec<base::interface::model::ToolDef>,
            messages: Vec<ModelMessage>,
            params: base::interface::model::StreamParams,
            _cancel: tokio_util::sync::CancellationToken,
        ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError>
        {
            let prompt_text = messages
                .first()
                .and_then(|m| m.content.first())
                .and_then(|b| match b {
                    ModelContentBlock::Text { text } => Some(text.clone()),
                    _ => None,
                })
                .unwrap_or_default();
            *self.captured.lock().unwrap() = Some((params, prompt_text));
            let events = vec![Ok(base::interface::model::ModelEvent::TextDelta {
                text: "[]".to_string(),
            })];
            Ok(Box::new(futures::stream::iter(events)))
        }
    }

    fn user_msg(text: &str) -> ModelMessage {
        ModelMessage {
            role: MessageRole::User,
            content: vec![ModelContentBlock::Text {
                text: text.to_string(),
            }],
        }
    }

    fn tmp_memory_store() -> Arc<MemoryStore> {
        let base =
            std::env::temp_dir().join(format!("agix-test-memstore-{}", uuid::Uuid::new_v4()));
        Arc::new(MemoryStore::new(base.join("user"), base.join("local")))
    }

    #[tokio::test]
    async fn model_name_flows_into_stream_params() {
        let store = tmp_memory_store();
        let messages = vec![user_msg("hello"), user_msg("world")];
        let model = CapturingModel::new();

        extract_memories_after_turn(
            store.clone(),
            &messages,
            &model,
            "custom-vendor-model",
            None,
            &base::interface::environment::SystemEnvironment,
            "test-session",
        )
        .await;

        let (params, _) = model
            .captured
            .lock()
            .unwrap()
            .take()
            .expect("stream() was called");
        assert_eq!(params.model, "custom-vendor-model");
    }

    #[tokio::test]
    async fn no_prompt_override_uses_default_codebase_intro() {
        let store = tmp_memory_store();
        let messages = vec![user_msg("hello"), user_msg("world")];
        let model = CapturingModel::new();

        extract_memories_after_turn(
            store.clone(),
            &messages,
            &model,
            "m",
            None,
            &base::interface::environment::SystemEnvironment,
            "test-session",
        )
        .await;

        let (_, prompt_text) = model
            .captured
            .lock()
            .unwrap()
            .take()
            .expect("stream() was called");
        assert!(prompt_text.contains("codebase or git history"));
    }

    #[tokio::test]
    async fn scene_prompt_override_replaces_default_intro() {
        let store = tmp_memory_store();
        let messages = vec![user_msg("hello"), user_msg("world")];
        let model = CapturingModel::new();

        extract_memories_after_turn(
            store.clone(),
            &messages,
            &model,
            "m",
            Some("Extract facts about the user's chat preferences."),
            &base::interface::environment::SystemEnvironment,
            "test-session",
        )
        .await;

        let (_, prompt_text) = model
            .captured
            .lock()
            .unwrap()
            .take()
            .expect("stream() was called");
        assert!(prompt_text.contains("chat preferences"));
        assert!(!prompt_text.contains("codebase or git history"));
        // Fixed JSON-schema instructions must survive regardless of override.
        assert!(prompt_text.contains("kebab-case slug"));
    }

    /// Returns a canned memory extraction: one `project` memory and one `user`
    /// memory, so the routing of each type can be observed on disk.
    struct ExtractingModel;

    #[async_trait::async_trait]
    impl base::interface::model::Model for ExtractingModel {
        fn api_type(&self) -> base::provider::ApiType {
            base::provider::ApiType::Anthropic
        }
        async fn stream(
            &self,
            _prompt_blocks: Vec<base::interface::prompt::PromptBlock>,
            _tools: Vec<base::interface::model::ToolDef>,
            _messages: Vec<ModelMessage>,
            _params: base::interface::model::StreamParams,
            _cancel: tokio_util::sync::CancellationToken,
        ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError>
        {
            let json = r#"[
              {"name":"release-freeze","description":"merge freeze","content":"Freeze after 2026-03-05.","type":"project","confidence":0.9},
              {"name":"user-is-a-go-dev","description":"user background","content":"Ten years of Go.","type":"user","confidence":0.9}
            ]"#;
            let events = vec![Ok(base::interface::model::ModelEvent::TextDelta {
                text: json.to_string(),
            })];
            Ok(Box::new(futures::stream::iter(events)))
        }
    }

    /// The extraction prompt asks the model to classify each memory, and
    /// `project` means "about this working directory". Persisting the whole
    /// batch through the unscoped write path landed every project memory in the
    /// global directory, where it followed the user into unrelated projects.
    #[tokio::test]
    async fn project_memories_are_extracted_into_the_local_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let user_dir = tmp.path().join("user");
        let local_dir = tmp.path().join("local");
        let store = Arc::new(MemoryStore::new(user_dir.clone(), local_dir.clone()));
        let messages = vec![user_msg("hello"), user_msg("world")];

        extract_memories_after_turn(
            store.clone(),
            &messages,
            &ExtractingModel,
            "m",
            None,
            &base::interface::environment::SystemEnvironment,
            "test-session",
        )
        .await;

        assert!(
            local_dir.join("release-freeze.md").exists(),
            "type: project must be written to the project-local dir"
        );
        assert!(
            !user_dir.join("release-freeze.md").exists(),
            "project memory leaked into the global dir"
        );
        assert!(
            user_dir.join("user-is-a-go-dev.md").exists(),
            "type: user must be written to the global dir"
        );
        assert!(!local_dir.join("user-is-a-go-dev.md").exists());
        // Both indices reflect their own contents.
        let local_index =
            std::fs::read_to_string(local_dir.join(base::interface::memory::INDEX_FILE)).unwrap();
        assert!(local_index.contains("release-freeze"));
        assert!(!local_index.contains("user-is-a-go-dev"));
    }

    /// Hands back `self.0` as the extraction call's answer.
    struct Answering(&'static str);

    #[async_trait::async_trait]
    impl base::interface::model::Model for Answering {
        fn api_type(&self) -> base::provider::ApiType {
            base::provider::ApiType::Anthropic
        }
        async fn stream(
            &self,
            _prompt_blocks: Vec<base::interface::prompt::PromptBlock>,
            _tools: Vec<base::interface::model::ToolDef>,
            _messages: Vec<ModelMessage>,
            _params: base::interface::model::StreamParams,
            _cancel: tokio_util::sync::CancellationToken,
        ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError>
        {
            let events = vec![Ok(base::interface::model::ModelEvent::TextDelta {
                text: self.0.to_string(),
            })];
            Ok(Box::new(futures::stream::iter(events)))
        }
    }

    async fn extract_with(store: &Arc<MemoryStore>, answer: &'static str) {
        extract_memories_after_turn(
            store.clone(),
            &[user_msg("hello"), user_msg("world")],
            &Answering(answer),
            "m",
            None,
            &base::interface::environment::SystemEnvironment,
            "test-session",
        )
        .await;
    }

    #[tokio::test]
    async fn an_extracted_memory_holding_a_key_is_not_saved() {
        let store = tmp_memory_store();
        extract_with(
            &store,
            r#"[
              {"name":"deploy-key","description":"how to deploy","content":"use token sk-abcdefghijklmnopqrstuvwx","type":"project"},
              {"name":"deploy-day","description":"when to deploy","content":"Tuesdays.","type":"project"}
            ]"#,
        )
        .await;
        let names: Vec<String> = store.load_all().into_iter().map(|m| m.name).collect();
        assert_eq!(names, vec!["deploy-day".to_string()]);
    }

    fn old_memories(count: usize) -> Vec<DurableMemory> {
        (0..count)
            .map(|i| DurableMemory {
                name: format!("m{i}"),
                description: String::new(),
                memory_type: MemoryType::User,
                content: String::new(),
                source_session_id: String::new(),
                confidence: 0.9,
                last_seen: "2020-01-01T00:00:00Z".into(),
                recall_count: 0,
            })
            .collect()
    }

    fn names_in(store: &MemoryStore) -> std::collections::BTreeSet<String> {
        store.load_all().into_iter().map(|m| m.name).collect()
    }

    #[tokio::test]
    async fn a_full_store_stops_extraction_and_loses_nothing() {
        const CAP: usize = base::interface::memory::MAX_MEMORIES;
        let store = tmp_memory_store();
        store.persist_batch(old_memories(CAP)).unwrap();
        let before = names_in(&store);
        extract_with(
            &store,
            r#"[{"name":"one-more","description":"d","content":"c","type":"user"}]"#,
        )
        .await;
        assert_eq!(names_in(&store), before);
    }

    #[tokio::test]
    async fn extraction_fills_the_store_up_to_its_cap_and_no_further() {
        const CAP: usize = base::interface::memory::MAX_MEMORIES;
        let store = tmp_memory_store();
        store.persist_batch(old_memories(CAP - 1)).unwrap();
        extract_with(
            &store,
            r#"[
              {"name":"m0","description":"d","content":"updated","type":"user"},
              {"name":"first-new","description":"d","content":"c","type":"user"},
              {"name":"second-new","description":"d","content":"c","type":"user"}
            ]"#,
        )
        .await;
        let names = names_in(&store);
        assert_eq!(names.len(), CAP);
        assert!(names.contains("first-new") && !names.contains("second-new"));
        let m0 = store
            .load_all()
            .into_iter()
            .find(|m| m.name == "m0")
            .unwrap();
        assert_eq!(
            m0.content, "updated",
            "a memory already held is still updated"
        );
    }
}

#[cfg(test)]
mod tool_result_content_tests {
    use super::*;
    use base::tool::{ToolResultBlock, ToolResultContent};

    /// A 1x1 PNG. Real bytes so the base64 round-trip is exercised end to end.
    const TINY_PNG: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89,
    ];

    fn png_b64() -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(TINY_PNG)
    }

    /// Build the user message a tool round produces: one `ToolResult` block
    /// carrying the rendered text, plus whatever images were lifted out.
    fn tool_result_message(content: ToolResultContent) -> ModelMessage {
        let (text, images) = render_tool_result_content(content);
        let mut messages = vec![ModelMessage {
            role: MessageRole::User,
            content: vec![ModelContentBlock::ToolResult {
                tool_use_id: "toolu_1".into(),
                content: text,
                is_error: Some(false),
            }],
        }];
        if !images.is_empty() {
            attach_tool_result_images(&mut messages, images);
        }
        messages.pop().unwrap()
    }

    /// #40: a structured MCP result with text *and* an image must reach the
    /// model as readable text plus a real image block — never as a Rust `Debug`
    /// dump of the intermediate `ToolResultBlock` structs.
    #[test]
    fn mixed_text_and_image_blocks_render_as_text_plus_image_block() {
        let msg = tool_result_message(ToolResultContent::Blocks(vec![
            ToolResultBlock::text("Chart for Q3 revenue:"),
            ToolResultBlock::image("image/png", png_b64()),
            ToolResultBlock::text("Source: ledger.csv"),
        ]));

        assert_eq!(msg.content.len(), 2, "expected tool_result + image");

        let ModelContentBlock::ToolResult { content, .. } = &msg.content[0] else {
            panic!(
                "first block must be the tool_result, got {:?}",
                msg.content[0]
            );
        };
        assert!(
            content.contains("Chart for Q3 revenue:"),
            "lost text: {content}"
        );
        assert!(
            content.contains("Source: ledger.csv"),
            "lost text: {content}"
        );
        // The exact symptom of the bug: Rust struct field names reaching the model.
        assert!(
            !content.contains("ToolResultBlock"),
            "Debug output leaked: {content}"
        );
        assert!(
            !content.contains("block_type"),
            "Debug output leaked: {content}"
        );
        // The base64 must not be inlined into the text either.
        assert!(!content.contains(&png_b64()), "base64 leaked into text");

        let ModelContentBlock::Image { media_type, data } = &msg.content[1] else {
            panic!("second block must be an Image, got {:?}", msg.content[1]);
        };
        assert_eq!(media_type, "image/png");
        assert_eq!(data, &png_b64());
    }

    /// A text-only `Blocks` result is plain concatenated text — the overwhelmingly
    /// common structured-MCP shape, and the one the Debug rendering mangled worst
    /// (the text was inside `text: Some("…")` and therefore unreadable).
    #[test]
    fn text_only_blocks_render_as_plain_concatenated_text() {
        let (text, images) = render_tool_result_content(ToolResultContent::Blocks(vec![
            ToolResultBlock::text("line one"),
            ToolResultBlock::text("line two"),
        ]));
        assert_eq!(text, "line one\nline two");
        assert!(images.is_empty());
    }

    /// `Text` content is passed through untouched.
    #[test]
    fn text_content_is_unchanged() {
        let (text, images) = render_tool_result_content(ToolResultContent::Text("hello".into()));
        assert_eq!(text, "hello");
        assert!(images.is_empty());
    }

    /// An image-only result still needs non-empty tool_result text.
    #[test]
    fn image_only_result_gets_a_placeholder_text() {
        let (text, images) =
            render_tool_result_content(ToolResultContent::Blocks(vec![ToolResultBlock::image(
                "image/png",
                png_b64(),
            )]));
        assert_eq!(text, "[image]");
        assert_eq!(images.len(), 1);
    }

    /// API-validity guard: `tool_result` blocks must lead the user turn, so
    /// images are appended after *all* of them — never interleaved.
    #[test]
    fn images_are_appended_after_every_tool_result_block() {
        let mut messages = vec![ModelMessage {
            role: MessageRole::User,
            content: vec![
                ModelContentBlock::ToolResult {
                    tool_use_id: "toolu_1".into(),
                    content: "first".into(),
                    is_error: Some(false),
                },
                ModelContentBlock::ToolResult {
                    tool_use_id: "toolu_2".into(),
                    content: "second".into(),
                    is_error: Some(false),
                },
            ],
        }];
        attach_tool_result_images(
            &mut messages,
            vec![PendingToolImage {
                media_type: "image/png".into(),
                data: png_b64(),
            }],
        );

        let content = &messages[0].content;
        assert_eq!(content.len(), 3);
        assert!(matches!(content[0], ModelContentBlock::ToolResult { .. }));
        assert!(matches!(content[1], ModelContentBlock::ToolResult { .. }));
        assert!(matches!(content[2], ModelContentBlock::Image { .. }));
    }

    /// Images attach to the tool-result message even when a tool's deferred
    /// `new_messages` pushed something after it.
    #[test]
    fn images_skip_past_later_injected_messages() {
        let mut messages = vec![
            ModelMessage {
                role: MessageRole::User,
                content: vec![ModelContentBlock::ToolResult {
                    tool_use_id: "toolu_1".into(),
                    content: "result".into(),
                    is_error: Some(false),
                }],
            },
            ModelMessage {
                role: MessageRole::User,
                content: vec![ModelContentBlock::Text {
                    text: "injected".into(),
                }],
            },
        ];
        attach_tool_result_images(
            &mut messages,
            vec![PendingToolImage {
                media_type: "image/png".into(),
                data: png_b64(),
            }],
        );
        assert_eq!(
            messages[0].content.len(),
            2,
            "image must join the tool_result message"
        );
        assert_eq!(
            messages[1].content.len(),
            1,
            "injected message must be untouched"
        );
    }

    /// The whole point of the sibling-block shape: an image costs a flat
    /// per-image estimate, not `base64_len / 4`. Inlining a 1 MB screenshot as
    /// text reads as ~350k tokens and trips compaction on the first image.
    #[test]
    fn image_tool_result_is_estimated_at_the_flat_image_rate() {
        // ~256 KB of base64 — as text this would estimate to ~65k tokens.
        let big_b64 = "A".repeat(256 * 1024);
        let msg = tool_result_message(ToolResultContent::Blocks(vec![
            ToolResultBlock::text("[Image: shot.png]"),
            ToolResultBlock::image("image/png", big_b64.clone()),
        ]));

        let estimate = compaction::grouping::estimate_tokens(std::slice::from_ref(&msg));
        let as_text_would_be = big_b64.len() / 4;
        assert!(
            estimate < as_text_would_be / 10,
            "image estimated by payload length ({estimate} vs {as_text_would_be})"
        );
        assert!(
            estimate >= base::interface::model::IMAGE_TOKEN_ESTIMATE,
            "image should cost at least the flat per-image estimate, got {estimate}"
        );
    }
}

/// Per-turn prompt assembly: what actually reaches the model each request.
#[cfg(test)]
mod prompt_assembly_tests {
    use super::*;
    use base::error::ToolError;
    use base::interface::scene::{ExecutionParams, Scene, TokenBudget};
    use base::interface::settings::Settings;
    use base::tool::{
        InMemoryToolRegistry, PermissionDecision, ProgressSender, PromptContext, Tool, ToolContext,
        ToolResult,
    };
    use serde_json::{json, Value};

    const GUIDE_BODY: &str = "# Sub-agent usage guide\n\nDispatch a sub-agent when \
                              the search space is wide and only the conclusion matters.";

    /// A tool with a substantial `Tool::prompt()` — the `AgentTool` /
    /// `TeamCreateTool` shape, whose guides were unreachable.
    struct GuidedTool;
    #[async_trait::async_trait]
    impl Tool for GuidedTool {
        fn name(&self) -> &str {
            "Guided"
        }
        fn description(&self) -> &str {
            "Launch a sub-agent."
        }
        fn input_schema(&self) -> Value {
            json!({"type": "object", "properties": {"prompt": {"type": "string"}}})
        }
        async fn prompt(&self, _: &PromptContext) -> String {
            GUIDE_BODY.to_string()
        }
        async fn check_permissions(&self, _: &Value, _: &ToolContext) -> PermissionDecision {
            PermissionDecision::allow()
        }
        async fn call(
            &self,
            _: Value,
            _: ToolContext,
            _: ProgressSender,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::text("ok"))
        }
    }

    /// A tool that never overrides `prompt()` — must be left completely
    /// alone, both in `build_tool_defs` and in `ToolSearch`.
    struct PlainTool;
    #[async_trait::async_trait]
    impl Tool for PlainTool {
        fn name(&self) -> &str {
            "Plain"
        }
        fn description(&self) -> &str {
            "Reads a file."
        }
        fn input_schema(&self) -> Value {
            json!({"type": "object"})
        }
        async fn call(
            &self,
            _: Value,
            _: ToolContext,
            _: ProgressSender,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::text("ok"))
        }
    }

    struct DummyModel;
    #[async_trait::async_trait]
    impl base::interface::model::Model for DummyModel {
        fn api_type(&self) -> base::provider::ApiType {
            base::provider::ApiType::Anthropic
        }
        async fn stream(
            &self,
            _: Vec<PromptBlock>,
            _: Vec<ToolDef>,
            _: Vec<ModelMessage>,
            _: base::interface::model::StreamParams,
            _: CancellationToken,
        ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError>
        {
            unimplemented!("prompt assembly is exercised without issuing requests")
        }
    }

    /// A scene whose only distinguishing features are the tool whitelist and
    /// the API-call ceiling, so tests can control whether `ToolSearch` is
    /// reachable and what `execution_params()` declares.
    struct TestScene {
        allowed: Vec<String>,
        max_api_calls: u32,
    }
    impl Scene for TestScene {
        fn id(&self) -> &str {
            "test"
        }
        fn name(&self) -> &str {
            "Test"
        }
        fn description(&self) -> &str {
            "test scene"
        }
        fn build_system_prompt(&self, _: &ScenePromptContext) -> Vec<PromptBlock> {
            vec![PromptBlock::system("skeleton")]
        }
        fn tools(&self) -> Vec<String> {
            self.allowed.clone()
        }
        fn token_budget(&self) -> TokenBudget {
            TokenBudget {
                compact_threshold: 150_000,
                compact_keep_recent: 20,
            }
        }
        fn execution_params(&self) -> ExecutionParams {
            ExecutionParams {
                max_api_calls_per_turn: self.max_api_calls,
            }
        }
    }

    fn test_scene(allowed: Vec<String>) -> Arc<dyn Scene> {
        Arc::new(TestScene {
            allowed,
            max_api_calls: 200,
        })
    }

    fn test_settings(root: &std::path::Path) -> Settings {
        use base::interface::settings::*;
        Settings {
            model: ModelSettings {
                api_type: base::provider::ApiType::Anthropic,
                base_url: String::new(),
                auth_token: String::new(),
                model_name: "test".into(),
                max_tokens: 2000,
                thinking_mode: ThinkingMode::Auto,
                effort: None,
                fallback_model: None,
            },
            paths: PathSettings {
                user_data_dir: root.to_path_buf(),
                global_data_dir: root.to_path_buf(),
                local_data_dir: root.to_path_buf(),
                scope: "code".into(),
            },
            execution: ExecutionSettings::default(),
            compaction: CompactionConfig::default(),
            sandbox: SandboxConfig::default(),
            plugins: Default::default(),
            instruction_file: None,
            prompt_append: None,
            prompt_override: None,
            recorder: None,
            telemetry_url: None,
            session_dir: None,
            memory_enabled: false,
            disable_skill_shell_execution: false,
            path_style: Default::default(),
            permission_mode: PermissionMode::default(),
            permission_rules: Vec::new(),
            local_permission_rules: Vec::new(),
            allow_client_permission_override: false,
            telemetry_enabled: false,
            hooks_config: None,
            project_trust: Default::default(),
            project_layer: Default::default(),
            mcp_servers: Default::default(),
            providers: Default::default(),
            default_provider: None,
            task_models: Default::default(),
            language: None,
            scripts: Vec::new(),
        }
    }

    /// An `Agent` wired with `GuidedTool`, `PlainTool` and the real
    /// `ToolSearch` and nothing else, so `build_tool_defs()` output is
    /// exactly these three and easy to assert on.
    fn agent_with(settings: Settings, scene_allowed: Vec<String>) -> Agent {
        let registry = Arc::new(InMemoryToolRegistry::new());
        registry.register(Arc::new(GuidedTool));
        registry.register(Arc::new(PlainTool));
        registry.register(Arc::new(tools::tool_search::ToolSearchTool::new(
            registry.clone(),
        )));
        let (agent, _rx, _tx) = crate::agent::Builder::new()
            .scene(test_scene(scene_allowed))
            .model(Arc::new(DummyModel))
            .tools(registry)
            .settings(Arc::new(settings))
            .skip_warmup(true)
            .build()
            .expect("builder should succeed");
        agent
    }

    /// The whole point of the progressive-disclosure design: `build_tool_defs`
    /// advertises that a guide exists without paying for its body. If this
    /// ever inverts, every request grows by ~85 KB across the real tool set.
    #[tokio::test]
    async fn tool_defs_point_at_the_guide_instead_of_carrying_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = agent_with(test_settings(dir.path()), vec![]);

        let defs = agent.build_tool_defs().await;
        let guided = defs.iter().find(|d| d.name == "Guided").unwrap();

        assert!(
            guided
                .description
                .contains(r#"ToolSearch{"query":"select:Guided"}"#),
            "documented tool must tell the model how to fetch its guide, got: {}",
            guided.description
        );
        assert!(
            !guided.description.contains("Dispatch a sub-agent when"),
            "the guide BODY must not be in the per-request tools payload: {}",
            guided.description
        );
        // Payload growth is one short pointer line, not the document.
        let overhead = guided.description.len() - GuidedTool.description().len();
        assert!(
            overhead < 80,
            "pointer should be a single short line, added {overhead} bytes"
        );
        assert!(
            !defs.iter().any(|d| d.description.contains(GUIDE_BODY)),
            "no tool def may carry a full usage guide"
        );
    }

    /// A tool without a `prompt()` override comes through byte-identical —
    /// the default `Tool::detailed_prompt` derivation must report `None` for
    /// it rather than "the description differs from itself".
    #[tokio::test]
    async fn undocumented_tool_def_is_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = agent_with(test_settings(dir.path()), vec![]);

        let defs = agent.build_tool_defs().await;
        let plain = defs.iter().find(|d| d.name == "Plain").unwrap();
        assert_eq!(plain.description, "Reads a file.");
    }

    /// Pointing at a fetcher the scene filtered out is a dead end: the model
    /// would be told a guide exists with no way to reach it.
    #[tokio::test]
    async fn no_pointer_when_the_scene_excludes_tool_search() {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = agent_with(
            test_settings(dir.path()),
            vec!["Guided".into(), "Plain".into()], // whitelist without ToolSearch
        );

        let defs = agent.build_tool_defs().await;
        assert!(!defs.iter().any(|d| d.name == "ToolSearch"));
        let guided = defs.iter().find(|d| d.name == "Guided").unwrap();
        assert_eq!(guided.description, "Launch a sub-agent.");
    }

    use crate::module::fakes::{mcp_module, mcp_tool, FakeMcp};

    #[tokio::test]
    async fn sync_mcp_adapters_adds_previously_unseen_tools() {
        let tools = base::tool::InMemoryToolRegistry::new();
        let mgr = FakeMcp(vec![mcp_tool("test-server", "new-tool")]);

        sync_mcp_adapters(&tools, &mgr, |_| true);
        assert!(tools.get("mcp__test-server__new-tool").is_some());
    }

    #[tokio::test]
    async fn sync_mcp_adapters_registers_each_tool_once() {
        let tools = base::tool::InMemoryToolRegistry::new();
        let mgr = FakeMcp(vec![mcp_tool("test-server", "existing-tool")]);

        sync_mcp_adapters(&tools, &mgr, |_| true);
        sync_mcp_adapters(&tools, &mgr, |_| true);

        let count = tools
            .list()
            .iter()
            .filter(|t| t.name() == "mcp__test-server__existing-tool")
            .count();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn sync_mcp_adapters_takes_the_relisted_instance_and_drops_vanished_tools() {
        let tools = base::tool::InMemoryToolRegistry::new();
        let before = FakeMcp(vec![
            mcp_tool("test-server", "kept"),
            mcp_tool("test-server", "gone"),
        ]);
        sync_mcp_adapters(&tools, &before, |_| true);

        let after = FakeMcp(vec![mcp_tool("test-server", "kept")]);
        sync_mcp_adapters(&tools, &after, |_| true);

        assert!(
            tools.get("mcp__test-server__gone").is_none(),
            "a tool the server no longer lists must stop being callable"
        );
        let kept = tools.get("mcp__test-server__kept").unwrap();
        assert!(Arc::ptr_eq(&kept, &after.0[0]));
    }

    #[tokio::test]
    async fn sync_mcp_adapters_leaves_builtins_alone() {
        let tools = base::tool::InMemoryToolRegistry::new();
        let builtin: Arc<dyn base::tool::Tool> = Arc::new(PlainTool);
        tools.register(builtin.clone());
        let mgr = FakeMcp(Vec::new());

        sync_mcp_adapters(&tools, &mgr, |_| true);
        assert!(Arc::ptr_eq(&tools.get("Plain").unwrap(), &builtin));
    }

    #[tokio::test]
    async fn an_mcp_adapter_is_advertised_once_and_not_reported_as_shadowed() {
        let dir = tempfile::tempdir().unwrap();

        let (mut agent, _rx, _tx) = crate::agent::Builder::new()
            .scene(test_scene(vec![]))
            .model(Arc::new(DummyModel))
            .tools(Arc::new(InMemoryToolRegistry::new()))
            .settings(Arc::new(test_settings(dir.path())))
            .module(mcp_module(vec![mcp_tool("test-server", "lookup")]))
            .skip_warmup(true)
            .build()
            .unwrap();

        let defs = agent.build_tool_defs().await;
        let matching: Vec<_> = defs
            .iter()
            .filter(|d| d.name == "mcp__test-server__lookup")
            .collect();
        assert_eq!(matching.len(), 1);
        assert_eq!(matching[0].source.as_deref(), Some("mcp:test-server"));
        assert!(
            agent.mcp_shadow_warned.is_empty(),
            "the dispatch copy of an MCP adapter is not a builtin shadowing it"
        );
    }

    /// A session rooted in a monorepo sub-package must still see the
    /// repository-root conventions.
    #[tokio::test]
    async fn parent_agents_md_reaches_the_prompt_with_nearest_last() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("repo");
        let child = parent.join("packages").join("api");
        std::fs::create_dir_all(&child).unwrap();
        std::fs::write(parent.join("AGENTS.md"), "ROOT-CONVENTION: use tabs").unwrap();
        std::fs::write(child.join("AGENTS.md"), "PACKAGE-CONVENTION: use spaces").unwrap();

        let frozen = base::frozen::FrozenContext::collect(
            child.clone(),
            &base::paths::ConfigPaths::new(child.join(".state"), child.join(".agix"), "code"),
            &base::interface::environment::SystemEnvironment,
            &base::frozen::git::EngineGit::Direct,
        )
        .await;
        let mut settings = test_settings(dir.path());
        settings.paths.local_data_dir = child.clone();
        let (agent, _rx, _tx) = crate::agent::Builder::new()
            .scene(test_scene(vec![]))
            .model(Arc::new(DummyModel))
            .tools(Arc::new(InMemoryToolRegistry::new()))
            .settings(Arc::new(settings))
            .frozen(frozen)
            .skip_warmup(true)
            .build()
            .unwrap();

        let text = agent.build_instruction_context();
        assert!(
            text.contains("ROOT-CONVENTION"),
            "parent AGENTS.md must reach the prompt: {text}"
        );
        assert!(text.contains("PACKAGE-CONVENTION"));

        // Documented precedence: outermost first, nearest last, nearest wins.
        let root_at = text.find("ROOT-CONVENTION").unwrap();
        let pkg_at = text.find("PACKAGE-CONVENTION").unwrap();
        assert!(
            root_at < pkg_at,
            "root must come first so the nearest file is read last: {text}"
        );
        assert!(
            text.contains("later") && text.contains("wins"),
            "precedence must be stated, not merely implied by order: {text}"
        );
    }

    /// The single-file (non-monorepo) case stays byte-identical to what was
    /// injected before parent files were merged in — no headers, no
    /// precedence preamble, no new tokens for the common case.
    #[tokio::test]
    async fn single_instruction_file_is_injected_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("solo");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("AGENTS.md"), "ONLY-CONVENTION").unwrap();

        let frozen = base::frozen::FrozenContext::collect(
            root.clone(),
            &base::paths::ConfigPaths::new(root.join(".state"), root.join(".agix"), "code"),
            &base::interface::environment::SystemEnvironment,
            &base::frozen::git::EngineGit::Direct,
        )
        .await;
        let mut settings = test_settings(dir.path());
        settings.paths.local_data_dir = root.clone();
        let (agent, _rx, _tx) = crate::agent::Builder::new()
            .scene(test_scene(vec![]))
            .model(Arc::new(DummyModel))
            .tools(Arc::new(InMemoryToolRegistry::new()))
            .settings(Arc::new(settings))
            .frozen(frozen)
            .skip_warmup(true)
            .build()
            .unwrap();

        assert_eq!(agent.build_instruction_context(), "ONLY-CONVENTION");
    }

    /// Both recovery paths (overloaded → fallback model, prompt-too-long →
    /// compact and retry) share `build_prompt_blocks` with the normal path —
    /// and that function is the only `assemble_prompt` call site — so
    /// asserting the shared builder carries skills asserts all three paths.
    #[tokio::test]
    async fn rebuilt_prompt_still_carries_skills_and_scene_skeleton() {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = agent_with(test_settings(dir.path()), vec![]);

        struct OneSkill(base::frozen::SkillEntry);
        impl base::interface::skill_catalog::SkillCatalog for OneSkill {
            fn list(&self) -> Vec<base::frozen::SkillEntry> {
                vec![self.0.clone()]
            }
            fn get(&self, name: &str) -> Option<base::frozen::SkillEntry> {
                (self.0.name == name).then(|| self.0.clone())
            }
            fn body(&self, _: &str) -> Option<String> {
                None
            }
            fn reload_changed(&self) -> usize {
                0
            }
            fn discover_for_paths(&self, _: &[PathBuf]) -> Vec<base::frozen::SkillEntry> {
                Vec::new()
            }
            fn activate_for_paths(&self, _: &[PathBuf]) -> Vec<base::frozen::SkillEntry> {
                Vec::new()
            }
            fn invocation_count(&self, _: &str) -> u32 {
                0
            }
            fn last_invoked_seq(&self, _: &str) -> u64 {
                0
            }
        }
        agent.skills = Arc::new(OneSkill(base::frozen::SkillEntry {
            name: "release-checklist".into(),
            description: "Runs the pre-release checklist".into(),
            user_invocable: true,
            ..Default::default()
        }));

        let joined = agent
            .build_prompt_blocks("test")
            .await
            .iter()
            .map(|b| b.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            joined.contains("release-checklist"),
            "the shared prompt builder — used by the normal path and both \
             recovery paths — must carry the skill inventory: {joined}"
        );
        assert!(joined.contains("skeleton"), "scene skeleton missing");
    }

    /// A scene's declared ceiling binds: the turn's ceiling is `min(settings,
    /// scene)`, whichever is stricter. The rule lives in `LimitsPolicy::new`,
    /// which the agent builder feeds these two values.
    #[test]
    fn scene_execution_params_tighten_the_api_call_ceiling() {
        let dir = tempfile::tempdir().unwrap();
        let settings = test_settings(dir.path());
        assert_eq!(settings.execution.max_api_calls_per_turn, 25);

        let resolve = |scene_limit: u32| {
            let scene = TestScene {
                allowed: vec![],
                max_api_calls: scene_limit,
            };
            settings
                .execution
                .max_api_calls_per_turn
                .min(scene.execution_params().max_api_calls_per_turn)
        };

        assert_eq!(resolve(3), 3, "a scene tighter than settings must win");
        assert_eq!(
            resolve(5_000),
            25,
            "a scene cannot widen past the deployment-wide setting"
        );
    }

    mod input_map {
        use super::super::{resolve_input_map, user_text_fingerprint};
        use base::interface::model::{InputSpan, MessageRole, ModelContentBlock, ModelMessage};

        fn user(text: &str) -> ModelMessage {
            ModelMessage {
                role: MessageRole::User,
                content: vec![ModelContentBlock::Text { text: text.into() }],
            }
        }

        fn assistant(text: &str) -> ModelMessage {
            ModelMessage {
                role: MessageRole::Assistant,
                content: vec![ModelContentBlock::Text { text: text.into() }],
            }
        }

        fn pending(text: &str) -> (u64, Vec<InputSpan>) {
            (
                user_text_fingerprint(&user(text)).unwrap(),
                vec![InputSpan {
                    source: base::interface::model::input_source::USER_PROMPT.into(),
                    block: 0,
                    range: Some((0, text.len())),
                }],
            )
        }

        #[test]
        fn points_at_the_message_it_was_built_from() {
            let messages = vec![user("older"), assistant("reply"), user("this turn")];
            let map = resolve_input_map(Some(&pending("this turn")), &messages).unwrap();
            assert_eq!(map.user_message, 2);
            assert_eq!(map.spans.len(), 1);
        }

        /// Compaction rewrites the message list between the push and the
        /// request, so the index has to be found, not remembered. Here the
        /// same message sits two slots earlier than when it was pushed.
        #[test]
        fn survives_the_message_list_being_rewritten() {
            let pending = pending("this turn");
            let before = vec![user("a"), assistant("b"), user("this turn")];
            let after_compaction = vec![user("summary"), user("this turn")];
            assert_eq!(
                resolve_input_map(Some(&pending), &before)
                    .unwrap()
                    .user_message,
                2
            );
            assert_eq!(
                resolve_input_map(Some(&pending), &after_compaction)
                    .unwrap()
                    .user_message,
                1
            );
        }

        /// A turn's later steps append tool-result user messages after the
        /// prompt, so the search runs from the end and must not stop on those.
        #[test]
        fn is_not_confused_by_tool_result_messages_appended_after_it() {
            let messages = vec![
                user("this turn"),
                assistant("calling a tool"),
                user("<tool_result>…</tool_result>"),
            ];
            assert_eq!(
                resolve_input_map(Some(&pending("this turn")), &messages)
                    .unwrap()
                    .user_message,
                0
            );
        }

        /// Compaction folding the message away means there is nothing to point
        /// at. Absent is the honest answer; an index would name another message.
        #[test]
        fn a_compacted_away_message_yields_no_map() {
            let messages = vec![user("summary of everything"), assistant("ok")];
            assert!(resolve_input_map(Some(&pending("this turn")), &messages).is_none());
        }

        #[test]
        fn no_pending_input_yields_no_map() {
            assert!(resolve_input_map(None, &[user("anything")]).is_none());
        }

        /// An image-first message has no text to fingerprint, so it can never
        /// be mistaken for the prompt.
        #[test]
        fn a_message_with_no_leading_text_has_no_fingerprint() {
            let image_only = ModelMessage {
                role: MessageRole::User,
                content: vec![ModelContentBlock::Image {
                    media_type: "image/png".into(),
                    data: "AAAA".into(),
                }],
            };
            assert!(user_text_fingerprint(&image_only).is_none());
        }
    }
}

#[cfg(test)]
mod dangling_tool_use_tests {
    use super::*;

    fn msg(role: MessageRole, content: Vec<ModelContentBlock>) -> ModelMessage {
        ModelMessage { role, content }
    }

    fn tool_use(id: &str) -> ModelContentBlock {
        ModelContentBlock::ToolUse {
            id: id.into(),
            name: "Bash".into(),
            input: serde_json::json!({}),
        }
    }

    fn tool_result(id: &str) -> ModelContentBlock {
        ModelContentBlock::ToolResult {
            tool_use_id: id.into(),
            content: "ok".into(),
            is_error: None,
        }
    }

    fn result_ids(m: &ModelMessage) -> Vec<&str> {
        m.content
            .iter()
            .filter_map(|b| match b {
                ModelContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_transcript_that_ends_on_a_tool_use_gets_a_result_after_it() {
        let out = pair_dangling_tool_uses(vec![
            msg(
                MessageRole::User,
                vec![ModelContentBlock::Text { text: "go".into() }],
            ),
            msg(MessageRole::Assistant, vec![tool_use("a")]),
        ]);
        assert_eq!(out.len(), 3);
        assert_eq!(out[2].role, MessageRole::User);
        assert_eq!(result_ids(&out[2]), ["a"]);
    }

    #[test]
    fn a_partly_answered_batch_gets_the_missing_results_first() {
        let out = pair_dangling_tool_uses(vec![
            msg(MessageRole::Assistant, vec![tool_use("a"), tool_use("b")]),
            msg(
                MessageRole::User,
                vec![
                    tool_result("b"),
                    ModelContentBlock::Text {
                        text: "next".into(),
                    },
                ],
            ),
        ]);
        assert_eq!(out.len(), 2);
        assert_eq!(result_ids(&out[1]), ["a", "b"]);
    }

    #[test]
    fn results_that_answer_no_tool_use_before_them_are_dropped() {
        let out = pair_dangling_tool_uses(vec![
            msg(MessageRole::User, vec![tool_result("ghost")]),
            msg(MessageRole::Assistant, vec![tool_use("a")]),
            msg(
                MessageRole::User,
                vec![tool_result("a"), tool_result("a"), tool_result("x")],
            ),
        ]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].role, MessageRole::Assistant);
        assert_eq!(result_ids(&out[1]), ["a"]);
    }

    #[test]
    fn results_in_later_user_messages_join_the_first() {
        let out = pair_dangling_tool_uses(vec![
            msg(MessageRole::Assistant, vec![tool_use("a"), tool_use("b")]),
            msg(MessageRole::User, vec![tool_result("a")]),
            msg(MessageRole::User, vec![tool_result("b")]),
        ]);
        assert_eq!(out.len(), 2);
        assert_eq!(result_ids(&out[1]), ["a", "b"]);
    }

    #[test]
    fn a_result_is_awaited_only_for_an_unanswered_call_of_the_last_reply() {
        let transcript = vec![
            msg(MessageRole::Assistant, vec![tool_use("old")]),
            msg(MessageRole::User, vec![tool_result("old")]),
            msg(MessageRole::Assistant, vec![tool_use("a"), tool_use("b")]),
            msg(MessageRole::User, vec![tool_result("a")]),
        ];
        assert!(awaits_result(&transcript, "b"));
        assert!(!awaits_result(&transcript, "a"));
        assert!(!awaits_result(&transcript, "old"));
        assert!(!awaits_result(&transcript, "made-up"));
    }

    #[test]
    fn a_paired_transcript_is_left_alone() {
        let input = vec![
            msg(MessageRole::Assistant, vec![tool_use("a")]),
            msg(MessageRole::User, vec![tool_result("a")]),
            msg(
                MessageRole::Assistant,
                vec![ModelContentBlock::Text {
                    text: "done".into(),
                }],
            ),
        ];
        let out = pair_dangling_tool_uses(input.clone());
        assert_eq!(format!("{out:?}"), format!("{input:?}"));
    }
}

#[cfg(test)]
mod model_failure_tests {
    use super::*;
    use base::interface::model::ModelError;
    use base::interface::recovery_policy::ModelFailure;

    fn classify(e: &ModelError) -> ModelFailure<'_> {
        // Rendered text is only carried by `Other`; the tests below never
        // look inside it.
        model_failure(e, "")
    }

    /// What real providers answer a request too big for the model with.
    #[test]
    fn the_providers_size_refusals_are_context_too_long() {
        for refusal in [
            ModelError::Api {
                status: 400,
                message: r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 212034 tokens > 200000 maximum"}}"#.into(),
            },
            ModelError::Api {
                status: 400,
                message: "This model's maximum context length is 65536 tokens. However, you requested 70000 tokens".into(),
            },
            ModelError::Api {
                status: 413,
                message: "request entity too large".into(),
            },
            ModelError::Internal("prompt too long".into()),
        ] {
            assert!(
                matches!(classify(&refusal), ModelFailure::ContextTooLong { .. }),
                "{refusal:?}"
            );
        }
    }

    #[test]
    fn other_bad_requests_and_server_errors_are_not() {
        for other in [
            ModelError::Api {
                status: 400,
                message: "max_tokens: must be a positive integer".into(),
            },
            ModelError::Api {
                status: 500,
                message: "prompt is too long".into(),
            },
            ModelError::Network("the response ended before the model finished it".into()),
            ModelError::RateLimited,
        ] {
            assert!(
                matches!(classify(&other), ModelFailure::Other { .. }),
                "{other:?}"
            );
        }
        assert_eq!(classify(&ModelError::Overloaded), ModelFailure::Overloaded);
    }
}
