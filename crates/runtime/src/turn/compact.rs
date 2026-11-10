//! 压缩：自动/手动/恢复三种触发、PreCompact 钩子、压缩后的恢复上下文与上报。

use super::process::{hook_permission_mode, running_task_summary};
use super::{holds_session_context, CompactSummary, CompactTrigger, NotCompacted, TurnError};
use crate::agent::Agent;
use base::interface::event::AgentEvent;
use base::interface::model::{MessageRole, ModelContentBlock, ModelMessage};
use std::collections::HashSet;
use tokio_util::sync::CancellationToken;

impl Agent {
    /// Build the summarizer used by compaction's FullCompact layer.
    ///
    /// Routed through the task-model router under the `"compact"` task type,
    /// so a `task_models.compact` entry can point summarization at a
    /// cheaper/faster model than the conversation. With no router configured
    /// it uses the conversation's own model and model name.
    pub(super) fn compaction_summarizer(&self) -> Option<compaction::llm_summary::LlmSummarizer> {
        let (model, model_name) = match &self.llm.task_router {
            Some(router) => (
                self.accounted(router.model_for("compact")),
                router
                    .model_name_for("compact")
                    .unwrap_or(&self.settings.model.model_name)
                    .to_string(),
            ),
            None => (
                self.llm.model.clone(),
                self.settings.model.model_name.clone(),
            ),
        };
        if model_name.is_empty() {
            return None;
        }
        Some(
            compaction::llm_summary::LlmSummarizer::new(model, model_name)
                .for_session(self.session.session_id.clone()),
        )
    }

    /// Compact session messages if token budget is exceeded.
    pub(crate) async fn compact_if_needed(&mut self, cancel: &CancellationToken) {
        // Every judgement below belongs to the compactor: what has gone stale,
        // whether to compact early, when to warn, and when the budget forces
        // it. The order they are asked in is the loop's.
        let compactor = self.compaction.compactor.clone();
        let cached_due = self.compaction.cached_mc.should_run();
        let cached_keep_recent = self.compaction.cached_mc.keep_recent();
        let ages = self.session.message_ages();
        let time_based = self.compaction.time_based_mc_config.clone();
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
            self.compaction.tool_results_ever_cleared = true;
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
            self.compaction.cached_mc.record_pass(ids);
        }

        // Enforce per-message tool result budget BEFORE compaction.
        let budget_modified = compactor.trim_tool_results(&mut self.session.messages);
        if budget_modified > 0 {
            self.compaction.tool_results_ever_cleared = true;
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
                circuit_open: self.compaction.compaction_state.circuit_open,
            },
        ) {
            compaction::compact::ReactiveOutcome::Compacted => {
                self.compaction.tool_results_ever_cleared = true;
                self.compaction.compaction_state.record_success();
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
                self.compaction.compaction_state.record_failure();
                tracing::warn!(
                    failures = self.compaction.compaction_state.consecutive_failures,
                    "reactive micro-compact had no effect"
                );
            }
            compaction::compact::ReactiveOutcome::NotTriggered => {}
        }

        let threshold = budget.compact_threshold;
        if let Some(reminder) = compactor.context_warning(&compaction::compact::BudgetState {
            context_tokens: self.context_tokens(),
            threshold,
            warned_already: self.compaction.compact_warning_issued,
        }) {
            self.session.push_message(ModelMessage {
                role: MessageRole::User,
                content: vec![ModelContentBlock::Text { text: reminder }],
            });
            self.compaction.compact_warning_issued = true;
        }
        // Asked again after the reminder, not before it: the reminder is
        // itself context, and a conversation one message short of the
        // threshold crosses it here rather than a turn later.
        if !compactor.should_compact(&compaction::compact::BudgetState {
            context_tokens: self.context_tokens(),
            threshold,
            warned_already: self.compaction.compact_warning_issued,
        }) {
            return;
        }
        // The threshold is on the whole request, so the conversation gets what
        // the fixed part of it leaves. Where that is nothing, no conversation
        // could fit, and the threshold bounds the conversation by itself.
        let room = match threshold.checked_sub(self.compaction.request_overhead_tokens) {
            Some(room) if room > 0 => room,
            _ if self.session.token_count() > threshold => threshold,
            _ => return,
        };
        let context_tokens = self.context_tokens();
        if self.compaction.auto_backoff.holds(context_tokens) {
            return;
        }
        let target = room.saturating_sub(self.recovery_reserve());
        match self
            .run_compaction(
                target,
                budget.compact_keep_recent,
                CompactTrigger::Auto,
                cancel,
            )
            .await
        {
            Ok(_) | Err(NotCompacted::Cancelled) => {}
            Err(e) => {
                self.compaction
                    .auto_backoff
                    .missed(context_tokens, threshold);
                tracing::warn!(
                    misses = self.compaction.auto_backoff.misses(),
                    context_tokens,
                    threshold,
                    "automatic compaction {e}; not tried again for a while"
                );
            }
        }
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
        cancel: &CancellationToken,
    ) -> Result<CompactSummary, NotCompacted> {
        if !self.pre_compact_hooks_allow(target, trigger).await {
            return Err(NotCompacted::Declined);
        }
        let compactor = self.compaction.compactor.clone();
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
                    cancel,
                )
                .await
                {
                    Ok(ok) => {
                        tracing::info!("compaction: LLM summary produced (FullCompact)");
                        Some(ok)
                    }
                    // An interrupt can also surface as the summary request's
                    // own error, depending on where the stream was when it hit.
                    Err(e)
                        if cancel.is_cancelled()
                            || matches!(e, compaction::compact::CompactError::Cancelled) =>
                    {
                        tracing::info!("compaction: interrupted while summarizing; left as it was");
                        return Err(NotCompacted::Cancelled);
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
                    let _ = self.telemetry.telemetry_handle.record(
                        telemetry::TelemetryEvent::compact_action(
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
                        ),
                    );
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
            self.compaction.tool_results_ever_cleared = true;
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
        self.compaction.compact_warning_issued = false;
        self.compaction.auto_backoff = AutoCompactBackoff::default();

        // Two pieces of context are injected once per session, as
        // `<system-reminder>` user messages, and are therefore ordinary
        // compaction victims: the opening context message — session facts and
        // the project instructions — gated by `session_context_injected`, and
        // recalled memories, whose names go into `frozen.already_surfaced` so
        // they are not re-injected turn after turn. Neither is part of the
        // recovery above. Resetting both re-injects nothing here; it makes the
        // next turn's injection paths eligible again. A strategy that kept the
        // opening message keeps the flag.
        self.prompt.session_context_injected = holds_session_context(&self.session.messages);
        if let Some(ref mut frozen) = self.prompt.frozen {
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
            .gate
            .hooks
            .has_hooks_for(hooks::config::HookEvent::PostCompact)
        {
            let _ = self
                .gate
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
    pub(super) async fn pre_compact_hooks_allow(
        &self,
        target: usize,
        trigger: CompactTrigger,
    ) -> bool {
        if !self
            .gate
            .hooks
            .has_hooks_for(hooks::config::HookEvent::PreCompact)
        {
            return true;
        }
        let hook_result = self
            .gate
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
    pub(super) fn post_compact_recovery(
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
            .prompt
            .invoked_skills
            .iter()
            .map(|name| compaction::compact::InvokedSkillRecord {
                name: name.clone(),
                content: self.tooling.skills.body(name),
                last_seq: self.tooling.skills.last_invoked_seq(name),
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
    pub(super) fn recovery_reserve(&self) -> usize {
        compaction::grouping::estimate_tokens(&self.post_compact_recovery(&[], &[], true))
    }

    /// The `compact` event and its telemetry.
    pub(super) fn report_compaction(
        &self,
        summary: &CompactSummary,
        projection: Option<&compaction::compact::SnipProjection>,
        started: std::time::Instant,
    ) {
        let _ = self.channels.event_tx.send(AgentEvent::CompactAction {
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
            .telemetry
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

    /// Shorten the conversation after the provider refused it for size.
    ///
    /// The refusal is the measurement here, not the budget: the conversation
    /// that went out was too big whatever the estimate made of it, so the
    /// target is half of what was sent rather than the configured threshold,
    /// which it may well not have reached. An interrupt while the summary is
    /// written is not a failure: the caller sees `cancel` and ends the turn
    /// as cancelled.
    pub(crate) async fn compact_for_retry(
        &mut self,
        refusal: &base::interface::model::ModelError,
        cancel: &CancellationToken,
    ) -> Result<(), TurnError> {
        tracing::warn!(error = %refusal, "refused for size; compacting, then retrying");
        let budget = self.context_budget();
        let sent = compaction::grouping::estimate_tokens(self.session.messages());
        let target = (sent / 2)
            .min(budget.compact_threshold)
            .saturating_sub(self.recovery_reserve());
        let keep = budget.compact_keep_recent.min(5);
        match self
            .run_compaction(target, keep, CompactTrigger::Recovery, cancel)
            .await
        {
            Ok(_) | Err(NotCompacted::Cancelled) => Ok(()),
            Err(why) => Err(TurnError::Model(format!(
                "{refusal}; compacting the conversation to retry {why}"
            ))),
        }
    }
}

/// Automatic compactions in a row that left the conversation as it was. Each
/// one makes the next wait twice as many due checks as the last, up to
/// [`MAX_WAIT`](Self::MAX_WAIT), unless the context grows by a tenth of the
/// threshold first: more conversation may be something to drop.
#[derive(Debug, Default)]
pub(crate) struct AutoCompactBackoff {
    misses: u32,
    checks_left: u32,
    resume_above: usize,
}

impl AutoCompactBackoff {
    const FIRST_WAIT: u32 = 2;
    const MAX_WAIT: u32 = 32;

    pub(crate) fn misses(&self) -> u32 {
        self.misses
    }

    /// Whether this due check is skipped.
    pub(crate) fn holds(&mut self, context_tokens: usize) -> bool {
        if self.checks_left == 0 || context_tokens > self.resume_above {
            self.checks_left = 0;
            return false;
        }
        self.checks_left -= 1;
        true
    }

    pub(crate) fn missed(&mut self, context_tokens: usize, threshold: usize) {
        self.misses = self.misses.saturating_add(1);
        self.checks_left = (Self::FIRST_WAIT << (self.misses - 1).min(16)).min(Self::MAX_WAIT);
        self.resume_above = context_tokens.saturating_add(threshold / 10);
    }
}
