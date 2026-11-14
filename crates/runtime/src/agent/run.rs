//! `Agent`'s lifecycle: the `run` loop, warmup, session management, goal rounds and the read-only accessors.

use super::*;

/// A message on its way to the turn loop, with the token its turn runs under.
type Forwarded = (InputMessage, CancellationToken);

/// Hand `msg` to the turn loop under a fresh child of the session token. A
/// `User` message's token is what `CancelTurn` cancels from the moment it is
/// queued, not from when its turn starts: the loop may still be finishing the
/// turn before or re-running rewake hooks, and an interrupt the host sent for
/// this turn must not land on the finished one, whose background tasks still
/// hang off its token.
fn hand_to_turn_loop(
    fwd_tx: &mpsc::UnboundedSender<Forwarded>,
    current_turn_cancel: &std::sync::Mutex<TurnCancel>,
    session_cancel: &CancellationToken,
    msg: InputMessage,
) -> bool {
    let turn_cancel = session_cancel.child_token();
    if let InputMessage::User { turn_id, .. } = &msg {
        lock_turn_cancel(current_turn_cancel).queued(turn_id, turn_cancel.clone());
    }
    fwd_tx.send((msg, turn_cancel)).is_ok()
}

struct InputDemux {
    raw_rx: InputReceiver,
    fwd_tx: mpsc::UnboundedSender<Forwarded>,
    pending: PendingPermissions,
    questions: crate::elicitation::PendingQuestions,
    denials: Arc<std::sync::atomic::AtomicU32>,
    turn_cancel: Arc<std::sync::Mutex<TurnCancel>>,
    session_cancel: CancellationToken,
    steering: crate::steering::SharedSteerQueue,
}

impl InputDemux {
    async fn run(mut self) {
        while let Some(msg) = self.raw_rx.recv().await {
            match msg {
                InputMessage::Steer {
                    content,
                    message_id,
                } => {
                    let offered = crate::steering::lock(&self.steering).offer(
                        base::event::QueuedUserMessage {
                            message_id,
                            content,
                        },
                    );
                    if let Err(m) = offered {
                        let turn = InputMessage::User {
                            content: m.content,
                            attachments: vec![],
                            turn_id: m.message_id,
                        };
                        if !hand_to_turn_loop(
                            &self.fwd_tx,
                            &self.turn_cancel,
                            &self.session_cancel,
                            turn,
                        ) {
                            break;
                        }
                    }
                }
                InputMessage::PermissionResponse {
                    prompt_id,
                    decision,
                } => {
                    crate::turn::resolve_permission_response(
                        &self.pending,
                        &self.denials,
                        prompt_id,
                        decision,
                    );
                }
                InputMessage::QuestionResponse { prompt_id, answer } => {
                    crate::elicitation::resolve_question_response(
                        &self.questions,
                        &prompt_id,
                        answer,
                    );
                }
                InputMessage::System {
                    kind: EngineCommand::CancelTurn { turn_id },
                    ..
                } => {
                    tracing::info!(?turn_id, "Cancelling in-flight turn");
                    lock_turn_cancel(&self.turn_cancel).cancel(turn_id.as_deref());
                }
                // Receiver gone means the engine loop has exited;
                // nothing left to forward to.
                other => {
                    if !hand_to_turn_loop(
                        &self.fwd_tx,
                        &self.turn_cancel,
                        &self.session_cancel,
                        other,
                    ) {
                        break;
                    }
                }
            }
        }
    }
}

impl Agent {
    pub fn steer_handle(&self) -> crate::steering::SteerHandle {
        crate::steering::SteerHandle(self.channels.steering.clone())
    }

    /// The session's own state — its todos, tasks, permission mode — shared
    /// with every tool call, for a host that shows or acts on it.
    pub fn session_state(&self) -> Arc<base::context::SessionState> {
        self.session_state.clone()
    }

    /// Take over the state handed to [`Builder::session_state`]: from here
    /// on it carries this engine's env, tools and providers. Call it once the
    /// engine that ran the state before has stopped. An engine that built its
    /// own state is attached already.
    pub fn attach_session(&self) {
        self.session_state
            .attach(self.session_end.attachment.clone());
    }

    /// Cancelled when the session ends.
    pub(crate) fn session_ended(&self) -> &CancellationToken {
        &self.session_end.ended
    }

    /// How this session delegates, for a host that starts a sub-agent itself
    /// (`DelegateMode::Host`). Refuses when no module provided one.
    pub fn delegator(&self) -> Arc<dyn base::interface::delegate::Delegator> {
        self.tooling.delegator().clone()
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
        Arc::clone(self.tooling.commands())
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
        Arc::clone(&self.policies.health)
    }

    /// Start the agent event loop. Runs until cancelled (caller calls `.cancel()` on the token)
    /// or the input channel closes. Does NOT consume self — the agent can be reused after stop.
    pub async fn run(&mut self, cancel: CancellationToken) {
        let run_started = std::time::Instant::now();
        self.session_start(&cancel).await;
        let (mut fwd_rx, own_input) = self.spawn_input_demux(&cancel);
        let exit_reason = self.run_loop(&cancel, &mut fwd_rx, &own_input).await;
        self.session_end(run_started, exit_reason).await;
    }

    async fn session_start(&mut self, cancel: &CancellationToken) {
        tracing::info!(scene = %self.scene.id, "Engine started");

        // Emit SystemInit
        let tools: Vec<_> = self
            .tooling
            .tools()
            .list()
            .iter()
            .map(|t| base::event::ToolInfo {
                name: t.name().to_string(),
                description: t.description().to_string(),
            })
            .collect();
        self.emit(AgentEvent::SystemInit {
            scene: self.scene.id.clone(),
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
            let model = self.settings.values.model.model_name.clone();
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
            let cwd = self.settings.paths.working_dir().display().to_string();
            self.fire_lifecycle_hook(hooks::HookEvent::CwdChanged, |i| {
                i.with_reason(format!("session working directory: {cwd}"))
            })
            .await;
        }

        // If the previous session was interrupted while a permission prompt was
        // pending, re-inject the stored decision so the agent can continue.
        if let Some(decision) = self.gate.take_orphaned() {
            tracing::info!(
                ?decision,
                "Recovering orphaned permission from previous session"
            );
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

        if !self.identity.skip_warmup {
            self.warmup().await;
        }
    }

    fn spawn_input_demux(
        &mut self,
        cancel: &CancellationToken,
    ) -> (
        mpsc::UnboundedReceiver<Forwarded>,
        mpsc::WeakUnboundedSender<Forwarded>,
    ) {
        // The loop below `await`s `process_turn` inline, so while a turn is
        // running it is not reading input. Some messages only mean something
        // *during* a turn and would deadlock or arrive too late there: a
        // `PermissionResponse` or `QuestionResponse` the running turn is
        // blocked on, `CancelTurn`, and a `Steer` meant for the turn's next
        // step. This task owns the real receiver and handles those through
        // `Arc`-shared state; everything else is forwarded to the loop
        // untouched.
        let (fwd_tx, fwd_rx) = mpsc::unbounded_channel();
        // Weak, so a goal round can be queued without keeping the loop alive
        // after the real input channel closes.
        let own_input = fwd_tx.downgrade();
        {
            // `self.input_rx` is moved into the task; the field is left
            // holding a receiver whose sender is dropped immediately, so any
            // stray read of it yields `None` rather than blocking forever.
            let (_closed_tx, closed_rx) = mpsc::unbounded_channel();
            let raw_rx = std::mem::replace(&mut self.channels.input_rx, closed_rx);
            let crate::turn::DemuxHandles {
                pending,
                questions,
                denials,
            } = self.gate.demux_handles();
            let turn_cancel = self.channels.current_turn_cancel.clone();
            let session_cancel = cancel.clone();
            let steering = self.channels.steering.clone();
            tokio::spawn(
                InputDemux {
                    raw_rx,
                    fwd_tx,
                    pending,
                    questions,
                    denials,
                    turn_cancel,
                    session_cancel,
                    steering,
                }
                .run(),
            );
        }
        (fwd_rx, own_input)
    }

    async fn run_loop(
        &mut self,
        cancel: &CancellationToken,
        fwd_rx: &mut mpsc::UnboundedReceiver<Forwarded>,
        own_input: &mpsc::WeakUnboundedSender<Forwarded>,
    ) -> &'static str {
        let exit_reason: &'static str;
        loop {
            self.gate
                .hooks()
                .check_rewakes(self.session_ended(), self.lifecycle_input(String::new()))
                .await;

            tokio::select! {
                _ = cancel.cancelled() => {
                    tracing::info!("Engine cancelled");
                    exit_reason = "cancelled";
                    break;
                }
                msg = fwd_rx.recv() => {
                    match msg {
                        Some((input, _)) if self.identity.drive_goals && !self.claims_goal_round(&input) => {
                            tracing::debug!("dropped a goal round that is no longer wanted");
                        }
                        Some((input, turn_cancel)) => {
                            let user_turn = match &input {
                                InputMessage::User { turn_id, .. } => Some(turn_id.clone()),
                                _ => None,
                            };
                            let processed = self.process_turn(input, turn_cancel).await;
                            if let Some(turn_id) = &user_turn {
                                lock_turn_cancel(&self.channels.current_turn_cancel).ended(turn_id);
                            }
                            match processed {
                                Ok(outcome) => {
                                    if self.identity.drive_goals {
                                        if let Some(round) = self.goal_round_after(&outcome.stop_reason).await {
                                            if let Some(tx) = own_input.upgrade() {
                                                hand_to_turn_loop(
                                                    &tx,
                                                    &self.channels.current_turn_cancel,
                                                    cancel,
                                                    InputMessage::User {
                                                        content: round.message,
                                                        attachments: vec![],
                                                        turn_id: round.turn_id,
                                                    },
                                                );
                                            }
                                        }
                                    }
                                }
                                Err(crate::turn::TurnError::Shutdown) => {
                                    exit_reason = "shutdown_command";
                                    break;
                                }
                                // A user turn's failure was announced as it
                                // ended, with what its log took.
                                Err(_) if user_turn.is_some() => {}
                                Err(e) => {
                                    self.emit(AgentEvent::Error {
                                        failure: e.failure(),
                                        turn_id: self.current_turn_id.clone(),
                                        persisted: true,
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
        exit_reason
    }

    async fn session_end(&mut self, run_started: std::time::Instant, exit_reason: &'static str) {
        // What a failed write left for the next step, and goal changes made
        // since the last one, would otherwise never reach the log.
        let _ = self.flush_log().await;
        self.memory
            .finish(crate::turn::MEMORY_EXTRACTION_GRACE)
            .await;
        // SessionEnd — one exit point for all three ways the loop ends
        // (shutdown command, input channel closed, cancellation), so a hook
        // that cleans up gets to run regardless of which one happened.
        self.fire_lifecycle_hook(hooks::HookEvent::SessionEnd, |i| i)
            .await;

        let duration_ms = run_started.elapsed().as_millis() as i64;
        let spent = self.session_state.ledger().spent;
        self.telemetry
            .record(telemetry::TelemetryEvent::session_end(
                &self.session.session_id,
                0,
                None,
                telemetry::SessionEndPayload {
                    duration_ms,
                    total_turns: self.session.turn_count,
                    total_api_calls: spent.api_calls,
                    total_tool_calls: spent.tool_calls,
                    total_permission_denials: self.gate.denials(),
                    total_input_tokens: spent.usage.input_tokens.into(),
                    total_output_tokens: spent.usage.output_tokens.into(),
                    total_cache_creation: spent.usage.cache_creation_input_tokens.into(),
                    total_cache_read: spent.usage.cache_read_input_tokens.into(),
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
        let base_url = self.settings.values.model.base_url.clone();
        let session_memory = self.session.session_memory.clone();
        let network = self.exec.network.clone();
        let engine_git = crate::turn::engine_exec_from(
            &self.config,
            &self.exec,
            &self.session.session_id,
            self.settings.trust(),
        );

        let (frozen, _, _) = tokio::join!(
            // 1. Pre-compute the frozen environment snapshot (git status, branch, platform, etc.)
            crate::turn::collect_frozen(
                &self.settings.paths,
                &*self.policies.environment,
                &engine_git
            ),
            // 2. Fire-and-forget pre-connect GET to the API base URL (warms TCP/TLS)
            async move {
                if !base_url.is_empty() {
                    // Warms anything only while the model client and the
                    // session's network share `platform`'s connection pool.
                    let warming = network.send(
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
        self.prompt.set_frozen(frozen);
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
    pub async fn load_transcript(
        &mut self,
        id: &str,
    ) -> Result<(), session::session::SessionError> {
        self.session.resume(id).await?;
        self.prompt
            .set_context_injected(crate::turn::holds_session_context(&self.session.messages));
        Ok(())
    }

    /// Put back into the session's state what the loaded transcript's log
    /// recorded of it: the goal, and plan mode and todos. Only for a state
    /// no engine has run the session on since it was created; a state kept
    /// across engines already holds all of it.
    pub async fn restore_state_from_log(&self) {
        let goal = self
            .session
            .latest_extension(
                base::context::goal::GOAL_NS,
                base::context::goal::GOAL_EVENT,
            )
            .await
            .and_then(|payload| serde_json::from_value(payload["goal"].clone()).ok());
        let durable = self
            .session
            .latest_extension(
                base::context::SESSION_STATE_NS,
                base::context::SESSION_STATE_EVENT,
            )
            .await
            .and_then(|payload| serde_json::from_value(payload).ok());
        self.session_state.restore_from_log(goal, durable);
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
        let _ = self.record_goal_changes().await;
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
        self.telemetry.perf()
    }

    /// Access the tool registry (read-only).
    pub fn tools(&self) -> &dyn base::tool::ToolRegistry {
        self.tooling.tools().as_ref()
    }

    /// The tools this session advertises by name only — see
    /// `EngineConfig::deferred_tools`.
    pub fn deferred_tools(&self) -> &std::collections::HashSet<String> {
        &self.config.deferred_tools
    }

    pub fn scene(&self) -> &Arc<SceneSpec> {
        &self.scene
    }

    /// Access the engine settings (read-only).
    pub fn settings(&self) -> &LoadedSettings {
        &self.settings
    }

    /// Access telemetry recorder for external event recording.
    pub fn telemetry(&self) -> &dyn TelemetryRecorder {
        self.telemetry.handle()
    }

    /// Recorder status — None when no recording is configured.
    pub fn recorder(&self) -> Option<&base::settings::RecorderConfig> {
        self.settings.values.recorder.as_ref()
    }

    /// The session's permission: its policy and its grants.
    pub fn permission(&self) -> &PermissionSet {
        self.gate.permission()
    }

    /// Access the memory store (read-only).
    pub fn memory(&self) -> &MemoryStore {
        self.memory.store()
    }

    pub fn skills(&self) -> &dyn base::interface::skill_catalog::SkillCatalog {
        self.tooling.skills().as_ref()
    }

    /// Access the hooks runner (read-only).
    pub fn hooks(&self) -> &HookRunner {
        self.gate.hooks()
    }

    /// Access the MCP manager (read-only).
    pub fn mcp(&self) -> &dyn base::interface::mcp_servers::McpServers {
        self.tooling.mcp()
    }

    /// Compact the conversation now, whatever the budget says — the same
    /// path automatic compaction takes, with an LLM summary whenever a
    /// summarizer is configured. Without one, the compactor's cascade aims at
    /// half of the history's current size. `None` when there was nothing to
    /// compact, a `PreCompact` hook declined, or `cancel` fired while the
    /// summary was being written.
    pub async fn compact_now(
        &mut self,
        cancel: &CancellationToken,
    ) -> Option<crate::turn::CompactSummary> {
        let target = (self.session.token_count() / 2).max(1);
        // Asked for, so it must compact something: a keep floor of every
        // round there is would leave the summary nothing to cover, and the
        // cascade would then drop those rounds unsummarized.
        let rounds = compaction::grouping::group_by_api_round(self.session.messages()).len();
        let keep = self
            .policies
            .budget_policy
            .context_budget(&self.scene.token_budget)
            .compact_keep_recent
            .min(rounds.saturating_sub(1))
            .max(1);
        self.run_compaction(target, keep, crate::turn::CompactTrigger::Manual, cancel)
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
