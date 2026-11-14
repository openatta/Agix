//! Phase 13: the `Agent` literal, one sub-struct at a time.

use super::*;

impl Builder {
    pub(super) fn assemble(mut self, w: Wired) -> (Agent, EventReceiver, InputSender) {
        let chains = w.chains;
        let (recovery_policy, turn_policy, budget_policy) =
            self.policies(&w.resolved.settings, &w.resolved.scene);
        let settings = match &self.spec.session_prompt {
            Some(prompt) => Arc::new(prompt.applied_to(&w.resolved.settings)),
            None => w.resolved.settings,
        };
        let session_end = SessionEnd {
            ended: w.hooks.session_ended,
            stopping: CancellationToken::new(),
            tools: w.resolved.tools.clone(),
            state: w.session_state.clone(),
            attachment: w.attachment,
            reason: EndReasonSlot::default(),
        };
        let gate = self.gate(
            w.resolved.permission,
            &w.events.tx,
            w.events.pending_permissions,
            w.events.pending_questions,
            w.hooks.hooks,
            chains.turn_starts,
        );
        let agent = Agent {
            scene: w.resolved.scene,
            exec: w.exec,
            config: w.config,
            session: w.session,
            session_state: w.session_state,
            current_turn_id: String::new(),
            session_end,
            identity: self.identity(w.agent_depth),
            llm: ModelRoute::new(
                w.resolved.model,
                self.host.task_router.take(),
                recovery_policy,
                chains.model_interceptors,
            ),
            tooling: Tooling::new(
                w.resolved.tools,
                Arc::new(w.skill_catalog),
                w.command_registry,
                w.mcp,
                Arc::new(w.hooks.delegator),
                chains.tool_middleware,
                chains.result_transformers,
                w.tool_policy,
            ),
            gate,
            prompt: self.prompt_context(
                chains.prompt_registry,
                chains.prompt_middleware,
                w.resolved.claude_md_content,
                w.resolved.rules,
            ),
            policies: Policies {
                turn_policy,
                budget_policy,
                health: Arc::new(base::interface::health::HealthChecks::from_vec(
                    chains.health_checks,
                )),
                environment: w.resolved.environment,
            },
            memory: self.memory(w.resolved.memory_store, chains.retrieval_hooks),
            compaction: Compaction::new(
                w.compactor,
                settings.values.compaction.cached_microcompact,
            ),
            settings,
            turn: Default::default(),
            last_had_tool_uses: true,
            repeat_guard: Default::default(),
            telemetry: Telemetry::new(w.resolved.telemetry_handle),
            channels: Channels {
                input_rx: w.input_rx,
                event_tx: w.events.tx,
                current_turn_cancel: Default::default(),
                steering: self.spec.steering.take().map(|s| s.0).unwrap_or_default(),
            },
        };
        (agent, w.events.rx, w.input_tx)
    }

    pub(super) fn policies(
        &self,
        settings: &LoadedSettings,
        scene: &SceneSpec,
    ) -> (
        Arc<dyn base::interface::recovery_policy::RecoveryPolicy>,
        Arc<dyn base::interface::turn_policy::TurnPolicy>,
        Arc<dyn base::interface::budget_policy::BudgetPolicy>,
    ) {
        let recovery_policy: Arc<dyn base::interface::recovery_policy::RecoveryPolicy> =
            self.host.recovery_policy.clone().unwrap_or_else(|| {
                Arc::new(base::interface::recovery_policy::DefaultRecovery::new(
                    settings.values.model.fallback_model.clone(),
                ))
            });
        let turn_policy: Arc<dyn base::interface::turn_policy::TurnPolicy> =
            self.ext.turn_policy.clone().unwrap_or_else(|| {
                Arc::new(base::interface::turn_policy::LimitsPolicy::new(
                    settings.values.execution.max_api_calls_per_turn,
                    scene.execution.max_api_calls_per_turn,
                    5,
                ))
            });
        let budget_policy: Arc<dyn base::interface::budget_policy::BudgetPolicy> =
            self.ext.budget_policy.clone().unwrap_or_else(|| {
                Arc::new(base::interface::budget_policy::EngineBudget::from_settings(
                    &settings.values,
                ))
            });
        (recovery_policy, turn_policy, budget_policy)
    }

    pub(super) fn identity(&mut self, agent_depth: u32) -> Identity {
        Identity {
            agent_depth,
            agent_type: self.spec.agent_type.take(),
            team_seat: self.spec.team_seat.take(),
            drive_goals: !self.spec.goal_rounds_from_owner,
            skip_warmup: self.spec.skip_warmup,
        }
    }

    pub(super) fn gate(
        &mut self,
        permission: PermissionSet,
        event_tx: &crate::event_bus::EventBus,
        pending_permissions: PendingPermissions,
        pending_questions: crate::elicitation::PendingQuestions,
        hooks: Arc<HookRunner>,
        turn_starts: Vec<crate::plugin::TurnStart>,
    ) -> Gate {
        let elicitation = self.ext.elicitation.take().unwrap_or_else(|| {
            let channel = crate::elicitation::ChannelElicitation::new(
                event_tx.clone(),
                pending_permissions.clone(),
            );
            Arc::new(if self.spec.question_prompts {
                channel.with_questions(pending_questions.clone())
            } else {
                channel
            })
        });
        Gate::new(
            permission,
            elicitation,
            pending_permissions,
            pending_questions,
            hooks,
            turn_starts,
        )
    }

    pub(super) fn prompt_context(
        &mut self,
        prompt_registry: Arc<dyn base::interface::prompt_registry::PromptRegistry>,
        prompt_middleware: Vec<Arc<dyn base::interface::prompt_middleware::PromptMiddleware>>,
        claude_md_content: Option<String>,
        rules: Arc<dyn base::interface::instruction_provider::RuleProvider>,
    ) -> PromptContext {
        PromptContext::new(
            prompt_registry,
            self.ext
                .prompt_assembler
                .take()
                .unwrap_or_else(|| Arc::new(base::interface::prompt_assembler::DefaultAssembler)),
            prompt_middleware,
            self.spec.frozen.take(),
            claude_md_content,
            rules,
        )
    }

    pub(super) fn memory(
        &mut self,
        memory_store: Arc<MemoryStore>,
        retrieval_hooks: Vec<Arc<dyn base::interface::memory_contracts::RetrievalHook>>,
    ) -> Memory {
        Memory::new(
            memory_store,
            self.host
                .memory_retriever
                .take()
                .unwrap_or_else(|| Arc::new(base::interface::memory_contracts::LlmRetriever)),
            retrieval_hooks,
            self.spec.memory_extraction,
        )
    }
}
