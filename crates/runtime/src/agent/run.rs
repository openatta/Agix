//! `Agent`'s lifecycle: the `run` loop, warmup, session management, goal rounds and the read-only accessors.

use super::*;

impl Agent {
    pub fn steer_handle(&self) -> crate::steering::SteerHandle {
        crate::steering::SteerHandle(self.steering.clone())
    }

    /// The session's own state — its todos, tasks, permission mode — shared
    /// with every tool call, for a host that shows or acts on it.
    pub fn session_state(&self) -> Arc<base::context::SessionState> {
        self.session_state.clone()
    }

    /// How this session starts sub-agents, for a host that starts one itself
    /// (`AgentSpawner::start_agent`). Refuses when no module provided one.
    pub fn agent_spawner(&self) -> Arc<dyn base::interface::agent_spawner::AgentSpawner> {
        self.agent_spawner.clone()
    }

    /// The slash commands this session will actually resolve.
    ///
    /// For a host rendering a completion popup or a command palette: this is
    /// the same registry the turn loop dispatches against, and its
    /// skill-derived half is live, so what the user is shown cannot drift
    /// from what typing it will do. Hosts that keep their own catalog to get
    /// lower-latency updates should still reconcile against this one when
    /// [`AgentEvent::SkillsChanged`] arrives.
    ///
    /// Hands out the `Arc`, not a borrow: `run()` takes `&mut self` for the
    /// life of the session, so a host that has spawned the engine has no
    /// `&Agent` left to ask. Take this before spawning and keep it.
    pub fn commands(&self) -> Arc<crate::commands::CommandRegistry> {
        Arc::clone(&self.commands)
    }

    /// The diagnostics registered for this session, for a host that wants to
    /// know whether the engine is well.
    ///
    /// Hands out the `Arc` for the same reason [`Agent::commands`] does:
    /// `run()` holds `&mut self` for the life of the session, so a host that
    /// has spawned the engine has no `&Agent` left to ask. Take this before
    /// spawning, and call [`HealthChecks::report`] whenever something asks —
    /// a status endpoint, a supervisor, a log line every minute.
    ///
    /// [`HealthChecks::report`]: base::interface::health::HealthChecks::report
    pub fn health(&self) -> Arc<base::interface::health::HealthChecks> {
        Arc::clone(&self.health)
    }

    /// Start the agent event loop. Runs until cancelled (caller calls `.cancel()` on the token)
    /// or the input channel closes. Does NOT consume self — the agent can be reused after stop.
    pub async fn run(&mut self, cancel: CancellationToken) {
        tracing::info!(scene = %self.scene.id(), "Engine started");

        // Emit SystemInit
        let tools: Vec<_> = self
            .tools
            .list()
            .iter()
            .map(|t| base::interface::event::ToolInfo {
                name: t.name().to_string(),
                description: t.description().to_string(),
            })
            .collect();
        let _ = self.event_tx.send(AgentEvent::SystemInit {
            scene: self.scene.id().to_string(),
            tools,
            mcp_servers: vec![],
        });

        // `Setup` then `SessionStart`, both fired here rather than in
        // `Builder::build()`: `build()` is sync, and a `Builder` that is
        // constructed but never run is not a session. This is the first point
        // at which the session is genuinely live.
        //
        // `Setup` runs *before* `SessionStart` and carries
        // the resolved environment, so a hook can validate or prepare the
        // workspace (fetch credentials, warm a cache, refuse to start on a
        // dirty tree) while there is still nothing to undo.
        {
            let scope = self.settings.paths.scope.clone();
            let model = self.settings.model.model_name.clone();
            self.fire_lifecycle_hook(hooks::HookEvent::Setup, |i| {
                i.with_reason(format!("scene={scope} model={model}"))
            })
            .await;
        }
        self.fire_lifecycle_hook(hooks::HookEvent::SessionStart, |i| i)
            .await;

        // `CwdChanged`: the session's working directory as resolved at
        // startup. Nothing in the engine relocates a live session's cwd, so a
        // single event here reports where work will happen.
        {
            let cwd = self.settings.paths.project_root().display().to_string();
            self.fire_lifecycle_hook(hooks::HookEvent::CwdChanged, |i| {
                i.with_reason(format!("session working directory: {cwd}"))
            })
            .await;
        }

        // If the previous session was interrupted while a permission prompt was
        // pending, re-inject the stored decision so the agent can continue.
        if let Some(decision) = self.orphaned_permission.take() {
            if !self.has_handled_orphaned_permission {
                tracing::info!(
                    ?decision,
                    "Recovering orphaned permission from previous session"
                );
                self.has_handled_orphaned_permission = true;
                let _ = self
                    .process_turn(
                        InputMessage::PermissionResponse {
                            prompt_id: "orphaned".into(),
                            decision,
                        },
                        cancel.clone(),
                    )
                    .await;
            }
        }
        self.has_handled_orphaned_permission = true;

        if !self.skip_warmup {
            self.warmup().await;
        }

        // The loop below `await`s `process_turn` inline, so while a turn is
        // running it is not reading input. Some messages only mean something
        // *during* a turn and would deadlock or arrive too late there: a
        // `PermissionResponse` or `QuestionResponse` the running turn is
        // blocked on, `CancelTurn`, and a `Steer` meant for the turn's next
        // step. This task owns the real receiver and handles those through
        // `Arc`-shared state; everything else is forwarded to the loop
        // untouched.
        let (fwd_tx, mut fwd_rx) = mpsc::unbounded_channel();
        // Weak, so a goal round can be queued without keeping the loop alive
        // after the real input channel closes.
        let own_input = fwd_tx.downgrade();
        {
            // `self.input_rx` is moved into the task; the field is left
            // holding a receiver whose sender is dropped immediately, so any
            // stray read of it yields `None` rather than blocking forever.
            let (_closed_tx, closed_rx) = mpsc::unbounded_channel();
            let mut raw_rx = std::mem::replace(&mut self.input_rx, closed_rx);
            let pending = self.pending_permissions.clone();
            let questions = self.pending_questions.clone();
            let denials = self.permission_denial_count.clone();
            let turn_cancel = self.current_turn_cancel.clone();
            let steering = self.steering.clone();
            tokio::spawn(async move {
                while let Some(msg) = raw_rx.recv().await {
                    match msg {
                        InputMessage::Steer {
                            content,
                            message_id,
                            ack,
                        } => {
                            let offered = crate::steering::lock(&steering).offer(
                                base::event::QueuedUserMessage {
                                    message_id,
                                    content,
                                },
                            );
                            match (offered, ack) {
                                (Ok(()), Some(ack)) => {
                                    let _ = ack.send(crate::steering::SteerAck::Accepted);
                                }
                                (Ok(()), None) => {}
                                (Err(_), Some(ack)) => {
                                    let _ = ack.send(crate::steering::SteerAck::NotRunning);
                                }
                                (Err(m), None) => {
                                    let turn = InputMessage::User {
                                        content: m.content,
                                        attachments: vec![],
                                        turn_id: m.message_id,
                                    };
                                    if fwd_tx.send(turn).is_err() {
                                        break;
                                    }
                                }
                            }
                        }
                        InputMessage::PermissionResponse {
                            prompt_id,
                            decision,
                        } => {
                            crate::turn::resolve_permission_response(
                                &pending, &denials, prompt_id, decision,
                            );
                        }
                        InputMessage::QuestionResponse { prompt_id, answer } => {
                            crate::elicitation::resolve_question_response(
                                &questions, &prompt_id, answer,
                            );
                        }
                        InputMessage::System {
                            kind: EngineCommand::CancelTurn,
                            ..
                        } => {
                            tracing::info!("Cancelling in-flight turn");
                            turn_cancel
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .cancel();
                        }
                        // Receiver gone means the engine loop has exited;
                        // nothing left to forward to.
                        other => {
                            if fwd_tx.send(other).is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        }

        let run_started = std::time::Instant::now();
        let exit_reason: &'static str;
        loop {
            self.hooks
                .check_rewakes(hooks::HookInput::lifecycle(
                    String::new(),
                    self.session.session_id.clone(),
                    self.settings.paths.project_root().display().to_string(),
                    crate::turn::hook_permission_mode(self.session_state.permission_mode()),
                ))
                .await;

            tokio::select! {
                _ = cancel.cancelled() => {
                    tracing::info!("Engine cancelled");
                    exit_reason = "cancelled";
                    break;
                }
                msg = fwd_rx.recv() => {
                    match msg {
                        Some(input) if self.drive_goals && !self.claims_goal_round(&input) => {
                            tracing::debug!("dropped a goal round that is no longer wanted");
                        }
                        Some(input) => {
                            // Each turn runs under its own child token, so
                            // `CancelTurn` interrupts one turn and leaves the
                            // session able to take the next message.
                            // Cancelling the session's own token still
                            // cascades into whatever turn is in flight.
                            let turn_cancel = cancel.child_token();
                            *self.current_turn_cancel.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = turn_cancel.clone();
                            match self.process_turn(input, turn_cancel).await {
                                Ok(outcome) => {
                                    if self.drive_goals {
                                        if let Some(round) = self.goal_round_after(&outcome.stop_reason).await {
                                            if let Some(tx) = own_input.upgrade() {
                                                let _ = tx.send(InputMessage::User {
                                                    content: round.message,
                                                    attachments: vec![],
                                                    turn_id: round.turn_id,
                                                });
                                            }
                                        }
                                    }
                                }
                                Err(crate::turn::TurnError::Shutdown) => {
                                    exit_reason = "shutdown_command";
                                    break;
                                }
                                Err(e) => {
                                    let _ = self.event_tx.send(AgentEvent::Error {
                                        code: "turn_error".into(),
                                        message: e.to_string(),
                                        turn_id: self.current_turn_id.clone(),
                                    });
                                }
                            }
                        }
                        None => {
                            tracing::info!("Input channel closed");
                            exit_reason = "input_channel_closed";
                            break;
                        }
                    }
                }
            }
        }
        // What a failed write left for the next step, and goal changes made
        // since the last one, would otherwise never reach the log.
        if let Err(e) = self.session.persist().await {
            tracing::warn!(error = %e, "failed to persist session");
        }
        self.record_goal_changes().await;
        // SessionEnd — one exit point for all three ways the loop ends
        // (shutdown command, input channel closed, cancellation), so a hook
        // that cleans up gets to run regardless of which one happened.
        self.fire_lifecycle_hook(hooks::HookEvent::SessionEnd, |i| i)
            .await;

        let duration_ms = run_started.elapsed().as_millis() as i64;
        let _ = self
            .telemetry_handle
            .record(telemetry::TelemetryEvent::session_end(
                &self.session.session_id,
                0,
                None,
                telemetry::SessionEndPayload {
                    duration_ms,
                    total_turns: self.session.turn_count,
                    total_api_calls: self.spent.api_calls,
                    total_tool_calls: self.spent.tool_calls,
                    total_permission_denials: self
                        .permission_denial_count
                        .load(std::sync::atomic::Ordering::Relaxed),
                    total_input_tokens: self.spent.usage.input_tokens.into(),
                    total_output_tokens: self.spent.usage.output_tokens.into(),
                    total_cache_creation: self.spent.usage.cache_creation_input_tokens.into(),
                    total_cache_read: self.spent.usage.cache_read_input_tokens.into(),
                    stop_reason: exit_reason.to_string(),
                },
            ));
        tracing::info!("Engine stopped");
    }

    /// Warm up the agent by pre-computing the frozen environment snapshot,
    /// creating the session-memory sidecar, and pre-connecting to the API
    /// endpoint. All operations run in parallel via `tokio::join!` to
    /// minimize startup latency.
    async fn warmup(&mut self) {
        // NOTE: the actual project root, not `local_data_dir` itself (which
        // is `<project_root>/.agix` — see `PathSettings::project_root` docs).
        let cwd = self.settings.paths.project_root();
        let paths = base::paths::ConfigPaths::from_settings(&self.settings.paths);
        let base_url = self.settings.model.base_url.clone();
        let session_memory = self.session.session_memory.clone();
        let engine_git = crate::turn::engine_git_from(&self.config, &self.session.session_id);

        let (frozen, _, _) = tokio::join!(
            // 1. Pre-compute the frozen environment snapshot (git status, branch, platform, etc.)
            base::frozen::FrozenContext::collect(
                cwd.clone(),
                &paths,
                &*self.environment,
                &engine_git
            ),
            // 2. Fire-and-forget pre-connect GET to the API base URL (warms TCP/TLS)
            async move {
                if !base_url.is_empty() {
                    let net = base::interface::exec::local::LocalNetwork::default();
                    let warming = base::interface::exec::Network::send(
                        &net,
                        base::interface::exec::HttpRequest::get(&base_url),
                        base::interface::exec::Origin::Operator,
                    );
                    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), warming).await;
                }
            },
            // 3. Create the session-memory sidecar file if this session has
            // one (see `Builder::build()`) and it doesn't already exist.
            async move {
                if let Some(sm) = session_memory {
                    if let Err(e) = sm.init_session_memory().await {
                        tracing::warn!(error = %e, "failed to init session_memory.md sidecar");
                    }
                }
            },
        );
        // Skills directories are deliberately not rescanned here: the watcher
        // `Builder::build()` attaches to `self.skills` is already live, and
        // `build()` scanned the same directories moments ago.
        self.frozen = Some(frozen);
        tracing::debug!("Startup warmup complete");
    }

    /// List persisted sessions. External interface (read-only, from HistoryStore).
    pub async fn list_sessions(
        &self,
    ) -> Result<Vec<session::session::SessionSummary>, session::session::SessionError> {
        self.session.list_sessions().await
    }

    /// Delete a persisted session from HistoryStore. External interface.
    pub async fn delete_session(&self, id: &str) -> Result<(), session::session::SessionError> {
        self.session.delete_session(id).await
    }

    /// Load a persisted session's messages from `HistoryStore` back into
    /// this agent's in-memory conversation state. Call before `run()` starts
    /// processing turns — it's a plain field replacement, not safe to race
    /// against an active turn.
    pub async fn resume_session(&mut self, id: &str) -> Result<(), session::session::SessionError> {
        self.session.resume(id).await?;
        self.session_context_injected = crate::turn::holds_session_context(&self.session.messages);
        let goal = self
            .session
            .latest_extension(
                base::context::goal::GOAL_NS,
                base::context::goal::GOAL_EVENT,
            )
            .await
            .and_then(|payload| serde_json::from_value(payload["goal"].clone()).ok());
        self.session_state.restore_goal(goal);
        Ok(())
    }

    /// The goal round to start now that a turn ended with `stop_reason`, if
    /// any. Only a turn that ended normally continues a goal; any other end
    /// disarms it, leaving the decision to a person. What this changes is
    /// recorded before it returns.
    pub async fn goal_round_after(
        &mut self,
        stop_reason: &str,
    ) -> Option<base::context::goal::GoalRound> {
        let round = if crate::turn::CONTINUES_A_GOAL.contains(&stop_reason) {
            self.session_state.goal_next_round()
        } else {
            None
        };
        self.record_goal_changes().await;
        round
    }

    /// Whether `input` may run: anything but a goal round does, and a goal
    /// round only when it is still the one outstanding.
    fn claims_goal_round(&self, input: &InputMessage) -> bool {
        let InputMessage::User { turn_id, .. } = input else {
            return true;
        };
        match base::context::goal::TurnOrigin::from_turn_id(turn_id) {
            base::context::goal::TurnOrigin::GoalRound { .. } => {
                self.session_state.goal_claim_round(turn_id)
            }
            _ => true,
        }
    }

    /// Access the performance collector.
    pub fn perf(&self) -> &PerfCollector {
        &self.perf
    }

    /// Access the tool registry (read-only).
    pub fn tools(&self) -> &dyn base::tool::ToolRegistry {
        self.tools.as_ref()
    }

    pub fn scene(&self) -> &Arc<dyn Scene> {
        &self.scene
    }

    /// Access the engine settings (read-only).
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Access telemetry recorder for external event recording.
    pub fn telemetry(&self) -> &dyn TelemetryRecorder {
        &self.telemetry_handle
    }

    /// Recorder status — None when no recording is configured.
    pub fn recorder(&self) -> Option<&base::interface::settings::RecorderConfig> {
        self.settings.recorder.as_ref()
    }

    /// Access the permission handler (read-only).
    pub fn permission(&self) -> &dyn Permission {
        &*self.permission
    }

    /// Access the memory store (read-only).
    pub fn memory(&self) -> &MemoryStore {
        &self.memory_store
    }

    pub fn skills(&self) -> &dyn base::interface::skill_catalog::SkillCatalog {
        self.skills.as_ref()
    }

    /// Access the hooks runner (read-only).
    pub fn hooks(&self) -> &HookRunner {
        &self.hooks
    }

    /// Access the MCP manager (read-only).
    pub fn mcp(&self) -> &dyn base::interface::mcp_servers::McpServers {
        self.mcp.as_ref()
    }

    /// Compact the conversation now, whatever the budget says — the same
    /// path automatic compaction takes, with an LLM summary whenever a
    /// summarizer is configured. Without one, the compactor's cascade aims at
    /// half of the history's current size. `None` when there was nothing to
    /// compact or a `PreCompact` hook declined.
    pub async fn compact_now(&mut self) -> Option<crate::turn::CompactSummary> {
        let target = (compaction::grouping::estimate_tokens(self.session.messages()) / 2).max(1);
        // Asked for, so it must compact something: a keep floor of every
        // round there is would leave the summary nothing to cover, and the
        // cascade would then drop those rounds unsummarized.
        let rounds = compaction::grouping::group_by_api_round(self.session.messages()).len();
        let keep = self
            .budget_policy
            .context_budget(&self.scene.token_budget())
            .compact_keep_recent
            .min(rounds.saturating_sub(1))
            .max(1);
        self.run_compaction(
            target,
            keep,
            crate::turn::CompactTrigger::Manual,
            &CancellationToken::new(),
        )
        .await
        .ok()
    }

    /// Convenience: run a single turn from a user message string.
    /// Sends events through the engine's event channel during execution.
    /// Returns `TurnOutcome` on completion.
    pub async fn run_turn(
        &mut self,
        content: String,
        turn_id: String,
        cancel: CancellationToken,
    ) -> Result<crate::turn::TurnOutcome, crate::turn::TurnError> {
        self.process_turn(
            InputMessage::User {
                content,
                attachments: vec![],
                turn_id,
            },
            cancel,
        )
        .await
    }
}
