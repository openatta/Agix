//! 回合循环的一步：落盘、策略、压缩、调模型、记账、工具之间的收拾。

use super::end::StopReason;
use super::TurnLoopState;
use crate::agent::Agent;
use crate::turn::request::sync_mcp_adapters;
use crate::turn::request::ModelRequest;
use crate::turn::round::count_structured_output_calls;
use crate::turn::tool_env::engine_exec_from;
use crate::turn::tool_exec::execute_tool_with_telemetry;
use crate::turn::tool_exec::ToolExecCtx;
use crate::turn::tool_result::attach_tool_result_images;
use crate::turn::tool_result::PendingToolImage;
use crate::turn::TurnError;
use base::event::AgentEvent;
use base::interface::model::MessageRole;
use base::interface::model::ModelContentBlock;
use base::interface::model::ModelMessage;
use std::ops::ControlFlow;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

struct ModelCall {
    step: u32,
    result: crate::streaming::StreamResult,
    messages_before_step: usize,
    sent_message_count: usize,
    sent_tool_count: usize,
    started: std::time::Instant,
    images: Arc<std::sync::Mutex<Vec<PendingToolImage>>>,
    discontinued: Arc<std::sync::atomic::AtomicBool>,
}

impl Agent {
    pub(super) async fn step(
        &mut self,
        state: &mut TurnLoopState,
        cancel: &CancellationToken,
    ) -> Result<ControlFlow<StopReason>, TurnError> {
        self.checkpoint().await;
        if cancel.is_cancelled() {
            return Ok(ControlFlow::Break(StopReason::Cancelled));
        }
        if let base::interface::turn_policy::TurnStep::Stop { reason } = self
            .policies
            .turn_policy
            .before_model_call(&state.progress(""))
        {
            return Ok(ControlFlow::Break(StopReason::Policy(reason)));
        }

        self.compact_if_needed(cancel).await;
        if cancel.is_cancelled() {
            return Ok(ControlFlow::Continue(()));
        }

        if self.over_hard_cap() {
            return Ok(ControlFlow::Break(StopReason::ContextExceeded));
        }

        let call = self.call_model(state, cancel).await?;
        self.account_call(state, &call).await;
        if call.result.cancelled {
            return Ok(ControlFlow::Continue(()));
        }
        self.after_reply(&call, cancel).await;

        // A PostToolUse hook can request the turn end right here — every
        // tool_use emitted in this round already has its tool_result in
        // the session (execute_stream writes the reply together with all
        // of its results once the round is over), so there's no risk of
        // a dangling tool_use block without a matching result. Mirrors the Stop hook's
        // discontinue path below, just triggered from a different point.
        if call.discontinued.load(std::sync::atomic::Ordering::Relaxed) {
            tracing::info!("PostToolUse hook discontinued the turn");
            return Ok(ControlFlow::Break(StopReason::ToolHookDiscontinued));
        }

        // Structured output retry limit.
        state.structured_output_calls += self
            .session
            .messages
            .get(call.messages_before_step..)
            .map_or(0, count_structured_output_calls);
        if let base::interface::turn_policy::TurnStep::Stop { reason } = self
            .policies
            .turn_policy
            .after_model_call(&state.progress(&call.result.stop_reason))
        {
            tracing::warn!(
                structured_output_calls = state.structured_output_calls,
                %reason,
                "turn policy ended the turn"
            );
            return Ok(ControlFlow::Break(StopReason::Policy(reason)));
        }

        if self.charge_budget(state).is_break() {
            return Ok(ControlFlow::Break(StopReason::BudgetExceeded));
        }

        // If tools were executed during streaming, continue to next API call.
        if call.result.has_tool_uses {
            self.between_tool_steps(state, cancel).await;
            return Ok(ControlFlow::Continue(()));
        }

        Ok(self.reply_stopped(state, call.result, cancel).await)
    }

    async fn checkpoint(&mut self) {
        // Every step starts from a transcript that is on disk, so a
        // process that dies mid-turn loses at most the step in flight.
        // Here every tool_use already has its tool_result next to it.
        if let Err(e) = self.session.persist().await {
            tracing::warn!(error = %e, "failed to persist session");
        }
        let _ = self.record_goal_changes().await;
    }

    fn over_hard_cap(&self) -> bool {
        // A ceiling compaction was unable to get under. Ending the turn is
        // the only honest answer left: the request that would go out next
        // is one the deployment said must never be sent.
        if let Some(cap) = self.context_budget().hard_cap {
            let carried = self.context_tokens();
            if carried > cap {
                tracing::warn!(carried, cap, "context above the hard cap after compaction");
                return true;
            }
        }
        false
    }

    async fn start_step(&mut self, state: &mut TurnLoopState) -> (u32, ModelRequest) {
        // 2. Assemble the request
        let step = state.api_calls;
        state.api_calls += 1;
        self.emit(AgentEvent::StepStart {
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
                self.identity.agent_type.clone(),
            ),
        );
        let request = self
            .prepare_request(&state.model, state.max_tokens, origin.clone())
            .await;
        (step, request)
    }

    async fn call_model(
        &mut self,
        state: &mut TurnLoopState,
        cancel: &CancellationToken,
    ) -> Result<ModelCall, TurnError> {
        let (step, mut request) = self.start_step(state).await;
        // Counted before the request is handed on: the call's own
        // accounting needs the size of what went out.
        let sent_message_count = request.messages.len();
        let sent_tool_count = request.tool_defs.len();

        // 3. Call the model and 5. stream its reply, running tools as they
        //    arrive. 4. A call that fails before anything of it was kept
        //    goes to the recovery policy, a bounded number of times; the
        //    doing stays with the loop, because compacting, rebuilding a
        //    request and sending it are things only it can sequence.
        let call_started = std::time::Instant::now();
        state.turn_no = self.session.turn_count;
        let discontinued = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let tool_images: Arc<std::sync::Mutex<Vec<PendingToolImage>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let template = self.tool_exec_ctx(cancel, &discontinued, &tool_images);
        let mut recovering = crate::turn::request::RecoveringCall::after(state.had_tool_uses);
        let (result, messages_before_step) = loop {
            let messages_before_step = self.session.messages.len();
            let sent = request.clone();
            let attempt = self.stream_once(request, &template, cancel).await;
            let (failure, after_first_byte) = match attempt {
                Ok(result) => break (result, messages_before_step),
                Err(failure) => failure,
            };
            self.record_api_error(&state.model, &failure.error, call_started);
            if cancel.is_cancelled() {
                break (
                    crate::streaming::StreamResult::cancelled(),
                    messages_before_step,
                );
            }
            request = match self
                .recover_call(
                    &failure,
                    after_first_byte,
                    sent,
                    &mut recovering,
                    &mut state.model,
                    step,
                    cancel,
                )
                .await?
            {
                Some(next) => next,
                None => {
                    break (
                        crate::streaming::StreamResult::cancelled(),
                        messages_before_step,
                    )
                }
            };
            self.emit(AgentEvent::StepStart {
                step,
                turn_id: self.current_turn_id.clone(),
            });
        };

        Ok(ModelCall {
            step,
            result,
            messages_before_step,
            sent_message_count,
            sent_tool_count,
            started: call_started,
            images: tool_images,
            discontinued,
        })
    }

    /// The error carries whether the stream had opened.
    async fn stream_once(
        &mut self,
        request: ModelRequest,
        template: &ToolExecCtx,
        cancel: &CancellationToken,
    ) -> Result<crate::streaming::StreamResult, (crate::streaming::StreamFailure, bool)> {
        let stream = match self.send(request, cancel.clone()).await {
            Ok(stream) => stream,
            Err(error) => return Err((crate::streaming::StreamFailure::refused(error), false)),
        };
        let tools_for_safety = Arc::clone(&template.tools);
        let template = template.clone();
        let execute_tool = move |tool_use_id: String, name: String, input: serde_json::Value| {
            let exec_ctx = ToolExecCtx {
                tool_use_id,
                ..template.clone()
            };
            async move { execute_tool_with_telemetry(&exec_ctx, &name, input).await }
        };
        let is_concurrency_safe = move |name: &str, input: &serde_json::Value| {
            tools_for_safety
                .get(name)
                .map(|t| t.call_traits(input).concurrency_safe)
                .unwrap_or(false)
        };
        crate::streaming::execute_stream(
            stream,
            &mut self.session,
            &self.channels.event_tx,
            self.current_turn_id.clone(),
            execute_tool,
            is_concurrency_safe,
            cancel.clone(),
            &self.llm.model_interceptors,
            self.llm.model.provenance(),
        )
        .await
        .map_err(|failure| (failure, true))
    }

    fn tool_exec_ctx(
        &self,
        cancel: &CancellationToken,
        discontinued: &Arc<std::sync::atomic::AtomicBool>,
        images: &Arc<std::sync::Mutex<Vec<PendingToolImage>>>,
    ) -> ToolExecCtx {
        ToolExecCtx {
            tool_use_id: String::new(),
            tools: Arc::clone(&self.tooling.tools),
            cwd: self.settings.paths.working_dir().to_path_buf(),
            session_id: self.session.session_id.clone(),
            turn_no: self.session.turn_count,
            agent_depth: self.identity.agent_depth,
            telemetry_handle: self.telemetry.handle().clone(),
            turn_id: self.current_turn_id.clone(),
            cancel: cancel.clone(),
            hooks: Arc::clone(self.gate.hooks()),
            config: Arc::clone(&self.config),
            exec: self.exec.clone(),
            permission: self.gate.permission().clone(),
            session_state: Arc::clone(&self.session_state),
            elicitation: Arc::clone(self.gate.elicitation()),
            tool_middleware: Arc::clone(&self.tooling.tool_middleware),
            result_transformers: Arc::clone(&self.tooling.result_transformers),
            discontinued: Arc::clone(discontinued),
            images: Arc::clone(images),
            from_code: false,
            events: Some(self.channels.event_tx.clone()),
            turn: Arc::clone(&self.turn.scope),
        }
    }

    async fn account_call(&mut self, state: &mut TurnLoopState, call: &ModelCall) {
        // Any images the round's tools returned ride out-of-band (see
        // `ToolExecCtx::images`) and get appended to the tool-result message
        // `execute_stream` just pushed — after every `tool_result` block, as
        // the API requires.
        let pending_images =
            std::mem::take(&mut *call.images.lock().unwrap_or_else(|e| e.into_inner()));
        if !pending_images.is_empty() {
            attach_tool_result_images(&mut self.session.messages, pending_images);
        }

        let stream_result = &call.result;
        state.had_tool_uses = state.had_tool_uses || stream_result.has_tool_uses;
        state.tool_calls += stream_result.tool_calls;
        let stop_reason = &stream_result.stop_reason;
        let usage = stream_result.usage;
        state.last_call = usage;
        self.turn.last_call_usage = Some(usage);
        self.turn.truncated |= stream_result.truncated;
        if let Err(e) = self.session.persist().await {
            tracing::warn!(error = %e, "failed to persist session");
        }
        self.emit(AgentEvent::StepEnd {
            step: call.step,
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
        self.telemetry
            .record(telemetry::TelemetryEvent::api_request(
                &self.session.session_id,
                self.session.turn_count,
                Some(self.current_turn_id.clone()),
                telemetry::ApiRequestPayload {
                    model: state.model.clone(),
                    input_tokens: usage.input_tokens as u64,
                    output_tokens: usage.output_tokens as u64,
                    cache_creation_tokens: usage.cache_creation_input_tokens as u64,
                    cache_read_tokens: usage.cache_read_input_tokens as u64,
                    latency_ms: call.started.elapsed().as_millis() as u64,
                    stop_reason: stop_reason.clone(),
                    input_message_count: call.sent_message_count,
                    tool_count: call.sent_tool_count,
                    default_model: state.model == self.settings.values.model.model_name,
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
        state.spent = state.spent.plus(usage);
    }

    async fn after_reply(&mut self, call: &ModelCall, cancel: &CancellationToken) {
        let has_tool_uses = call.result.has_tool_uses;
        // `PostSampling`: one model response has been fully streamed and
        // is in the transcript. Purely observational: like the other lifecycle hooks, the
        // response is not modifiable from here (there is no defined
        // recovery path for a veto at this point).
        {
            let sr = call.result.stop_reason.clone();
            let out_tokens = call.result.usage.output_tokens;
            self.fire_turn_hook(hooks::HookEvent::PostSampling, cancel, |i| {
                i.with_reason(format!(
                    "model response complete (stop_reason={sr}, output_tokens={out_tokens})"
                ))
            })
            .await;
        }

        // A git-mutating Bash call this round means the session-start
        // `FrozenContext` git snapshot (branch/status/log) is now stale
        // — refresh just that slice.
        if has_tool_uses
            && self.settings.paths.project_root().is_some()
            && Self::included_git_mutating_bash_call(self.this_step())
        {
            if let Some(frozen) = self.prompt.frozen_mut() {
                frozen
                    .refresh_git(&engine_exec_from(
                        &self.config,
                        &self.exec,
                        &self.session.session_id,
                        self.settings.trust(),
                    ))
                    .await;
            }
        }
        if has_tool_uses {
            self.notice_repeated_calls();
        }
    }

    fn charge_budget(&mut self, state: &TurnLoopState) -> ControlFlow<()> {
        // Cumulative token budget, checked against the real usage the
        // provider reported for this call — no per-model price table,
        // no estimate. At 90% inject a warning; at 100% abort.
        //
        // The engine hands over all four figures and does not decide which
        // of them count as spent; that is the policy's judgement, and the
        // telemetry below records the reading the engine's own policy uses.
        let spend = base::interface::budget_policy::Spend::from(state.spent);
        let total_tokens_used = spend.total_tokens();
        match self.policies.budget_policy.on_usage(&spend) {
            base::interface::budget_policy::Spending::WithinBudget => {}
            base::interface::budget_policy::Spending::Warn { reminder, limit } => {
                tracing::warn!(
                    total_tokens_used,
                    budget = limit,
                    "approaching token budget limit; injecting continue reminder"
                );
                self.telemetry
                    .record(telemetry::TelemetryEvent::budget_enforced(
                        &self.session.session_id,
                        self.session.turn_count,
                        Some(self.current_turn_id.clone()),
                        telemetry::BudgetEnforcedPayload {
                            action: telemetry::BudgetEnforcedAction::WarningInjected,
                            total_tokens_used,
                            budget: limit,
                        },
                    ));
                self.session.push_message(ModelMessage {
                    role: MessageRole::User,
                    content: vec![ModelContentBlock::Text { text: reminder }],
                });
            }
            base::interface::budget_policy::Spending::Exhausted { limit } => {
                tracing::warn!(total_tokens_used, budget = limit, "token budget exceeded");
                self.telemetry
                    .record(telemetry::TelemetryEvent::budget_enforced(
                        &self.session.session_id,
                        self.session.turn_count,
                        Some(self.current_turn_id.clone()),
                        telemetry::BudgetEnforcedPayload {
                            action: telemetry::BudgetEnforcedAction::TurnStopped,
                            total_tokens_used,
                            budget: limit,
                        },
                    ));
                return ControlFlow::Break(());
            }
        }
        ControlFlow::Continue(())
    }

    async fn between_tool_steps(&mut self, state: &mut TurnLoopState, cancel: &CancellationToken) {
        // Collect async skill discovery prefetch (fired at turn start).
        if let Some(handle) = state.skill_discovery.take() {
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
                    self.session_state
                        .ledger()
                        .note_skills(new_names.iter().map(String::as_str));
                }
            }
        }

        // Activate conditional skills whose `paths` patterns match
        // files accessed by Read/Write/Edit tool operations this turn.
        {
            let file_paths = Self::extract_tool_file_paths(self.messages_this_turn());
            if !file_paths.is_empty() {
                let activated = self.tooling.skills.activate_for_paths(&file_paths);
                if !activated.is_empty() {
                    let names: Vec<&str> = activated.iter().map(|s| s.name.as_str()).collect();
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
                    self.session_state
                        .ledger()
                        .note_skills(activated.iter().map(|s| s.name.as_str()));
                }
            }
        }

        self.tooling.mcp.refresh_if_changed().await;
        sync_mcp_adapters(
            self.tooling.tools.as_ref(),
            self.tooling.mcp.as_ref(),
            |t| self.tooling.tool_policy.permits(t),
        );
        if !cancel.is_cancelled() {
            self.deliver_queued_user_messages(false, cancel).await;
        }
    }
}
