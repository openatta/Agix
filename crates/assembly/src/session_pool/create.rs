//! 会话创建：`create_session`、`create` 的装配，以及会话规格（`SessionSpec`）的读写。

use super::*;

impl SessionPool {
    /// `session.create` — the explicit multi-project entry point, with an
    /// optional `scene` (resolved against `active_scenes` — see
    /// `resolve_scene`; omitted means the pool's default scene). Builds and
    /// starts a new primary session bound to `project_root`, or this pool's
    /// default project for `ProjectSelector::Default`. Unlike the implicit
    /// `run_turn(session_id: None)` path, this is the one place a caller
    /// can ask for a session bound to a project or scene other than the
    /// pool's own.
    pub async fn create_session(
        &self,
        scene_id: Option<&str>,
        project_root: ProjectSelector,
        options: Option<&SessionOptions>,
    ) -> Result<serde_json::Value, PoolError> {
        let Some(scene) = self.resolve_scene(scene_id).await else {
            return Err(PoolError::SceneNotActive {
                scene: scene_id.unwrap_or("").to_string(),
            });
        };

        // Settings are resolved against *this session's* scene, not the
        // daemon's. The `Default` arm goes through the same call as the others
        // so that a non-startup scene on the default project still picks up
        // its own tier; `settings_for_project` short-circuits to the
        // hot-swappable `self.settings` for the one pair that is the pool's
        // own.
        // A scene that requires a project is refused here, not later: with no
        // project root there is nothing for its tools to read or write, and
        // the failure is far more legible at creation than at the first turn.
        if scene.requires_project() && matches!(project_root, ProjectSelector::NoProject) {
            return Err(PoolError::ProjectRequired {
                scene: scene.id().to_string(),
            });
        }

        let session_scene_id = scene.id().to_string();
        let (binding, project_root_for_meta) = match project_root {
            ProjectSelector::Default => (
                self.binding_for(Some(&self.cwd.clone()), &session_scene_id)
                    .await,
                Some(self.cwd.clone()),
            ),
            ProjectSelector::NoProject => (self.binding_for(None, &session_scene_id).await, None),
            ProjectSelector::Path(p) => {
                if !p.is_dir() {
                    return Err(PoolError::ProjectNotFound { path: p });
                }
                (self.binding_for(Some(&p), &session_scene_id).await, Some(p))
            }
        };

        binding
            .check_model_choice(
                options.and_then(|o| o.provider.as_deref()),
                options.and_then(|o| o.model.as_deref()),
            )
            .map_err(PoolError::InvalidRequest)?;

        let scene_id_for_result = scene.id().to_string();
        let project_trust = binding.settings.project_layer.trust;
        let sid = Id::new().to_string();
        let created_id = self
            .create(
                sid,
                scene,
                binding,
                project_root_for_meta.clone(),
                options,
                false,
            )
            .await?;

        let created_at = {
            let sessions = self.sessions.lock().await;
            sessions
                .get(&created_id)
                .map(|live| format_instant(live.created_at))
                .unwrap_or_default()
        };

        Ok(serde_json::json!({
            "session_id": created_id,
            "scene": scene_id_for_result,
            "session_kind": "primary",
            "project_root": project_root_for_meta.map(|p| p.display().to_string()),
            "project_trust": project_trust.as_str(),
            "created_at": created_at,
        }))
    }

    /// 创建新 session 并启动 Agent 后台 run loop。
    /// `options` 仅在新创建 session 时生效；已有 session 时忽略。
    ///
    /// `base_settings`/`project_root_for_meta` are the already-resolved
    /// settings for whichever project this session belongs to (see
    /// `settings_for_project`) and the value to stamp on `Meta.project_root`
    /// (`None` = no-project session) — callers that just want "this pool's
    /// default project" pass `self.settings.read().await.clone()` /
    /// `Some(self.cwd.clone())`.
    pub(super) async fn create(
        &self,
        session_id: String,
        scene: Arc<dyn Scene>,
        binding: Arc<ProjectBinding>,
        project_root_for_meta: Option<PathBuf>,
        options: Option<&SessionOptions>,
        resume: bool,
    ) -> Result<String, CreateError> {
        self.refuse_while_shutting_down()?;
        self.wait_until_stopped(&session_id).await?;
        let claim = self.claim_log(&session_id).await?;
        let spec = self.spec_for(&session_id, resume, options).await;
        self.lock_kept_specs().retain(|k| k != &session_id);
        let options = spec.options.as_ref();
        self.note_served(project_root_for_meta.as_deref());
        let scene_id = scene.id().to_string();
        // The `Builder` takes ownership of the scene; the session keeps a
        // handle of its own because `run_turn` has to ask it things.
        let scene_for_session = scene.clone();
        let base_settings = binding.settings.clone();
        let task_router = binding.task_router.clone();

        // Wrap the model in a recorder if this session asked for one.
        //
        // Both the conversation's model *and* every routed provider get
        // wrapped, sharing one `Recorder` so they write into one recording.
        // Wrapping only the conversation's model left sub-agents, compaction
        // and memory extraction — everything reached through the router —
        // recording nothing whenever `providers` was configured.
        let recorder_config =
            options
                .and_then(|o| o.recorder.as_ref())
                .map(|recorder| RecorderConfig {
                    mode: match recorder.mode.as_str() {
                        "record" => RecorderMode::Record,
                        // `rerun` is driven from a recording, not from a live
                        // session, so a session asking for it just talks to the
                        // provider — see `telemetry::recorder::rerun`.
                        "rerun" => RecorderMode::Rerun,
                        _ => RecorderMode::Replay,
                    },
                    name: recorder.name.clone(),
                    root: recordings_root(&base_settings, recorder.dir.as_deref()),
                    on_divergence: if recorder.strict {
                        Divergence::Strict
                    } else {
                        Divergence::Warn
                    },
                });
        let clock = session_clock(options, recorder_config.as_ref(), &session_id);
        let provider_id = spec.provider();
        let conversation_model = match provider_id.as_deref() {
            None => binding.model.clone(),
            Some(id) => binding
                .provider_client(id)
                .ok_or_else(|| format!("provider `{id}` is not configured"))?,
        };
        let model_name = spec.model().or_else(|| {
            provider_id
                .as_ref()
                .and_then(|id| binding.settings.providers.get(id))
                .and_then(|config| config.default_model.clone())
        });
        let (model, task_router) = match recorder_config {
            None => (conversation_model, task_router),
            Some(config) => {
                let shared = spec
                    .recorder
                    .clone()
                    .unwrap_or_else(telemetry::recorder::Recorder::new);
                let wrap =
                    |provider_id: Option<&str>, inner: Arc<dyn base::interface::model::Model>| {
                        let mut m = RecorderModel::shared(
                            inner,
                            Some(config.clone()),
                            env!("CARGO_PKG_VERSION"),
                            shared.clone(),
                        );
                        if let Some(id) = provider_id {
                            m = m.with_provider_id(id);
                        }
                        Arc::new(m) as Arc<dyn base::interface::model::Model>
                    };
                (
                    wrap(provider_id.as_deref(), conversation_model),
                    task_router.map(|r| Arc::new(r.map_models(|id, inner| wrap(Some(id), inner)))),
                )
            }
        };

        // The prompt timeout is a daemon-level flag but the wait it bounds
        // lives in the engine, so it travels as a setting rather than as a
        // daemon-side wrapper around the permission. `Settings` is behind an
        // `Arc`, so override on a private clone rather than in place.
        // The mode is decided once and written into the snapshot as well:
        // the gate, the hook payloads and `Meta` all read it from there, and a
        // snapshot still holding the settings-file mode made every one of them
        // report a mode the session was not running in.
        let permission_mode = effective_permission_mode(&base_settings, options);
        let settings_snapshot = {
            let mut snap = (*base_settings).clone();
            snap.execution.permission_prompt_timeout_secs =
                self.permission_prompt_timeout.as_secs();
            snap.permission_mode = permission_mode;
            if let Some(name) = &model_name {
                snap.model.model_name = name.clone();
            }
            if let base::settings::Workspace::Scratch(_) = snap.paths.workspace {
                snap.paths.workspace = base::settings::Workspace::Scratch(
                    base::paths::session_scratch_dir(&snap.paths.global_data_dir, &session_id),
                );
            }
            Arc::new(snap)
        };
        let namer = SessionNamer::of(
            &session_id,
            task_router.as_deref(),
            &model,
            &settings_snapshot.model.model_name,
        );
        let permission = resolve_session_permission(
            self.permission.as_ref(),
            &settings_snapshot,
            permission_mode,
            options,
        );

        let mut builder = Builder::new()
            .scene(scene)
            .model(model)
            .tools(self.tools.clone())
            .settings(settings_snapshot.clone())
            .permission(permission)
            .session_id(session_id.clone())
            .commands_override(binding.commands.clone())
            .cron_store(self.schedule.clone())
            .question_prompts(options.is_some_and(SessionOptions::answers_questions));
        if let Some(ref store) = self.history_store {
            builder = builder.history_store(store.clone());
        }
        if let Some(clock) = clock {
            builder = builder.environment(clock);
        }
        if let Some(router) = task_router {
            builder = builder.task_router(router);
        }
        builder = builder.scene_registry(self.scene_registry());
        // A project's scripts belong to that project: the paths in its
        // settings resolve against it, and so does the question of whether a
        // file is inside it — which is what decides whether the script may
        // rewrite the prompt or only add to it. Binding a session against the
        // pool's own directory instead gets both wrong for every session
        // created with a `project_root` of its own: the paths miss, so the
        // whole set is dropped, and had they hit, the project's own code
        // would have been judged to come from outside it.
        let script_root = project_root_for_meta
            .clone()
            .unwrap_or_else(|| self.cwd.clone());
        let package_scripts = self.plugins.script_bindings();
        #[cfg(feature = "scripts")]
        let script_ledger = {
            let (bound, faults) = bind_scripts(&settings_snapshot, &script_root, &package_scripts);
            self.plugins.note_script_faults(faults);
            match bound {
                Some(bound) => {
                    let ledger = bound.ledger.clone();
                    builder = builder.plugin(Arc::new(script_host::ScriptPlugin::new(bound)));
                    Some(ledger)
                }
                None => None,
            }
        };
        #[cfg(not(feature = "scripts"))]
        let script_ledger = {
            let (_, faults) = bind_scripts(&settings_snapshot, &script_root, &package_scripts);
            self.plugins.note_script_faults(faults);
            None
        };

        // Give this session its own owned `McpManager` built from this
        // project's centrally-connected client handles (cheap `Arc` clones
        // — not a reconnect) — see `SessionPool.mcp_by_project`'s doc
        // comment for why this isn't shared as a single `Arc<McpManager>`
        // directly (`McpManager` needs `&mut self` for `refresh_tools()`,
        // which a shared `Agent` field can't give it). Skipped entirely
        // when nothing's connected, so the common "no MCP servers
        // configured" case pays zero cost.
        #[cfg(not(feature = "mcp"))]
        let mcp_server_count = 0;
        #[cfg(feature = "mcp")]
        let mcp_server_count = {
            let central = self
                .mcp_for_project(project_root_for_meta.as_deref(), &scene_id)
                .await;
            let count = central.server_count();
            if count > 0 || !central.pending().is_empty() {
                let mut per_session = central.share();
                per_session.refresh_tools().await;
                builder = builder.module(Arc::new(mcp::McpModule::new(per_session)));
            }
            count
        };
        // After MCP, so the `Agent` tool sees the servers it may grant a
        // delegate; skills after agents, so forked skills can fork.
        builder = builder.modules(binding.modules.session_modules());

        // On by default: `Builder::build()`'s fallback handle drops its
        // receiver on the spot, so without a recorder every event is lost.
        // The caller's `output` only chooses *where*, and
        // `telemetry_enabled: false` in settings.json opts out.
        //
        // `TELEMETRY_DIR/<session-id>.jsonl` under the local data dir keeps
        // one file per session, next to the transcripts, so a recording is
        // findable by session id without the caller having had to plan for
        // it in advance.
        // The default location is only used when `local_data_dir` is a real
        // absolute path. It is not always set (tests, embedders that never
        // configure `paths`), and joining onto an empty or relative value
        // would scatter `telemetry/` directories into whatever the process's
        // working directory happens to be — including a user's repository.
        // Recording nothing is much better than writing somewhere surprising.
        let telemetry_path = match options.and_then(|o| o.telemetry.as_ref()) {
            Some(t) => Some(std::path::PathBuf::from(&t.output)),
            None => {
                let dir = &settings_snapshot.paths.local_data_dir;
                dir.is_absolute()
                    .then(|| dir.join("telemetry").join(format!("{session_id}.jsonl")))
            }
        };
        if let (true, Some(telemetry_path)) = (settings_snapshot.telemetry_enabled, telemetry_path)
        {
            match FileRecorder::new(&telemetry_path) {
                Ok(rec) => {
                    let data_dir = &settings_snapshot.paths.local_data_dir;
                    if data_dir.is_absolute() && telemetry_path.starts_with(data_dir) {
                        base::paths::ensure_local_data_gitignore(data_dir);
                    }
                    // `telemetry` crate never touches the network itself —
                    // this is where the daemon decides where events go:
                    // always the per-session JSONL file, plus OTel when an
                    // endpoint is configured. `start_otel` installs the
                    // global meter/tracer providers, so that one is a
                    // process-level side effect done once, guarded by a
                    // `OnceLock` rather than per session (see `otel_sink`).
                    let mut recorders: Vec<std::sync::Arc<dyn telemetry::TelemetryRecorder>> =
                        vec![std::sync::Arc::new(rec)];
                    if let Some(otel) = otel_sink(settings_snapshot.telemetry_url.as_deref()) {
                        recorders.push(otel);
                    }
                    let telemetry_config = telemetry::TelemetryConfig {
                        enabled: true,
                        mode: telemetry::TelemetryMode::Enabled,
                        default_event_enabled: true,
                        ..Default::default()
                    };
                    match telemetry::spawn(telemetry_config, recorders) {
                        Ok((handle, consumer)) => {
                            tokio::spawn(consumer);
                            builder = builder.telemetry_handle(handle);
                        }
                        Err(e) => {
                            warn!(error = %e, "failed to start telemetry pipeline; this session records nothing");
                        }
                    }
                }
                Err(e) => {
                    warn!(
                        path = %telemetry_path.display(),
                        error = %e,
                        "could not open the session telemetry file; this session records nothing"
                    );
                }
            }
        }

        if let Some(prompt) = spec.prompt() {
            builder = builder.session_prompt(prompt);
        }
        if let Some(schema) = options.and_then(|o| o.output_schema.clone()) {
            builder = builder.output_schema(schema);
        }
        // A rebuild keeps the ledger: the session goes on, and so do the
        // groups its earlier commands started.
        let process_groups = if base_settings.sandbox.reap_process_groups {
            let kept = self
                .sessions
                .lock()
                .await
                .get(&session_id)
                .and_then(|live| live.process_groups.clone());
            Some(kept.unwrap_or_else(platform::GroupLedger::new))
        } else {
            None
        };
        if let Some(ledger) = &process_groups {
            builder = builder.process_groups(ledger.clone());
        }
        for plugin in &self.rust_plugins {
            builder = builder.plugin(plugin.clone());
        }
        if let Some(customize) = &self.customize {
            builder = customize(
                builder,
                &SessionCtx {
                    session_id: &session_id,
                    scene: &scene_id,
                    project_root: project_root_for_meta.as_deref(),
                    resumed: resume,
                },
            );
        }
        let (mut agent, event_rx, input_tx) = builder
            .goal_rounds_from_owner()
            .build()
            .map_err(|e| format!("build agent: {e}"))?;
        let state = agent.session_state();
        let spawner = agent.agent_spawner();
        let health = agent.health();
        // A rebuild goes on with the hub of the session it replaces.
        let hub = self
            .hub_of(&session_id)
            .await
            .unwrap_or_else(|| Arc::new(SessionHub::new()));
        Self::forward_task_events(&session_id, &hub, &state);
        Self::forward_goal_events(&session_id, &hub, &state);

        let _ = agent
            .telemetry()
            .record(telemetry::TelemetryEvent::session_start(
                &session_id,
                0,
                None,
                telemetry::SessionStartPayload {
                    permission_mode: serde_json::to_value(settings_snapshot.permission_mode)
                        .ok()
                        .and_then(|v| v.as_str().map(|s| s.to_string()))
                        .unwrap_or_else(|| "default".to_string()),
                    model: settings_snapshot.model.model_name.clone(),
                    max_tokens: settings_snapshot.model.max_tokens,
                    thinking_mode: !matches!(
                        settings_snapshot.model.thinking_mode,
                        base::settings::ThinkingMode::Off
                    ),
                    sandbox_enabled: !settings_snapshot.sandbox.dangerously_disable_sandbox,
                    resume_from: resume.then(|| session_id.clone()),
                    auth_modes: Vec::new(),
                    mcp_server_count,
                    plugin_count: self.plugins.count(),
                    skill_count: binding.modules.skill_count(),
                    output_format: "jsonrpc".to_string(),
                    started_at_ms: (time::OffsetDateTime::now_utc().unix_timestamp_nanos()
                        / 1_000_000) as i64,
                },
            ));

        // First `Meta` line for a genuinely new primary session — resumed
        // sessions already have one from when they were first created, and
        // must not get a second (that would break the "Meta is always
        // entries[0]" assumption `SessionManager::resume`/`session.resume`
        // rely on). Best-effort: a write failure here doesn't fail session
        // creation — the session still works, it's just not identifiable by
        // scene/project on a later resume (falls back to inference, same as
        // any other pre-v2 file).
        if !resume {
            if let Some(ref store) = self.history_store {
                if let Ok(sid) = base::session::SessionId::parse(&session_id) {
                    let project_root_str = project_root_for_meta
                        .as_ref()
                        .map(|p| p.display().to_string());
                    let meta = history::entry::LogEntry::Meta {
                        cwd: project_root_str.clone().unwrap_or_else(|| {
                            settings_snapshot
                                .paths
                                .global_data_dir
                                .display()
                                .to_string()
                        }),
                        started_at: store.environment().now(),
                        model: settings_snapshot.model.model_name.clone(),
                        permission_mode: serde_json::to_value(settings_snapshot.permission_mode)
                            .ok()
                            .and_then(|v| v.as_str().map(|s| s.to_string()))
                            .unwrap_or_else(|| "default".to_string()),
                        engine_version: env!("CARGO_PKG_VERSION").to_string(),
                        agix_version: env!("CARGO_PKG_VERSION").to_string(),
                        parent_session_id: None,
                        scene: Some(scene_id.clone()),
                        project_root: project_root_str,
                        session_kind: history::entry::SessionKind::Primary,
                        schema_version: history::entry::CURRENT_META_SCHEMA_VERSION,
                    };
                    if let Err(e) = store.append(sid, meta).await {
                        warn!(%session_id, error = %e, "failed to write session Meta entry");
                    }
                }
            }
            if spec.options.is_some() {
                self.persist_spec(&session_id, &spec).await;
            }
        }

        // A log that does not load back is the caller's error: going on with
        // an empty context would carry the conversation on with none of what
        // came before, appended to the very log that holds it.
        let mut stored_name = None;
        let mut has_user_turn = false;
        if resume {
            let previous_state = self
                .sessions
                .lock()
                .await
                .get(&session_id)
                .and_then(|live| live.state.clone());
            self.stop_live_agent(&session_id).await?;
            let resume_start = std::time::Instant::now();
            let resume_result = agent.resume_session(&session_id).await;
            let outcome = match &resume_result {
                Ok(()) => telemetry::ResumeOutcome::Succeeded,
                Err(_) => telemetry::ResumeOutcome::Failed,
            };
            let (projected_message_count, entry_count) = self.history_counts(&session_id).await;
            let entries = self.load_entries(&session_id).await.unwrap_or_default();
            let compact_boundary_count = entries
                .iter()
                .filter(|e| matches!(e.entry, history::entry::LogEntry::Compact { .. }))
                .count();
            let sidechain_entry_count = entries.iter().filter(|e| e.is_sidechain).count();
            has_user_turn = entries.iter().any(|e| {
                !e.is_sidechain && matches!(e.entry, history::entry::LogEntry::User { .. })
            });
            stored_name = self.stats_of(&session_id).await.title;
            let _ = agent
                .telemetry()
                .record(telemetry::TelemetryEvent::resume_action(
                    &session_id,
                    0,
                    None,
                    telemetry::ResumeActionPayload {
                        outcome,
                        source: "jsonl".into(),
                        entry_count,
                        projected_message_count,
                        compact_boundary_count,
                        sidechain_entry_count,
                        warning_kind: resume_result
                            .as_ref()
                            .err()
                            .map(|_| "resume_session_failed".to_string()),
                        latency_ms: resume_start.elapsed().as_millis() as u64,
                    },
                ));
            if let Err(e) = resume_result {
                // The old engine is already stopped; an entry left in the map
                // would take turns it can no longer run.
                if previous_state.is_some() {
                    if let Some(live) = self.sessions.lock().await.remove(&session_id) {
                        self.evict(&session_id, &live);
                    }
                }
                return Err(format!("its transcript could not be loaded: {e}").into());
            }
            if let Some(previous) = &previous_state {
                state.carry_session_state_from(previous);
            }
        }

        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        let exited = CancellationToken::new();
        let exited_on_drop = exited.clone().drop_guard();

        tokio::spawn(async move {
            let _exited_on_drop = exited_on_drop;
            let _claim = claim;
            let mut agent = agent;
            let _ = agent.run(cancel_clone).await;
        });

        let mut sessions = self.sessions.lock().await;
        // Checked again under the lock `shutdown_all` drains the map with: a
        // session inserted after that would outlive the shutdown.
        if let Err(e) = self.refuse_while_shutting_down() {
            cancel.cancel();
            return Err(e.into());
        }

        // 容量检查：驱逐最久未活跃的 session。只有这次 insert 真的会让
        // session **总数**增加时才需要驱逐——`session_id` 已经在 map 里的话
        // （config-generation 触发的原地重建，见 `run_turn`），这次操作是
        // "替换同一个 key"，不是"新增一个"，`sessions.len()` 不会变，不该
        // 因为这次重建就凭空多驱逐一个无关的、真正最久未用的 session。
        if !sessions.contains_key(&session_id) && sessions.len() >= self.cap {
            if let Some((evicted, live)) = evict_lru(&mut sessions) {
                self.evict(&evicted, &live);
                info!(sid = %evicted, "session evicted (LRU, pool full)");
                self.emit_session_event(
                    "session_evicted",
                    &evicted,
                    serde_json::json!({"reason": "capacity"}),
                );
            }
        }

        let (event_rx, arrived) = relay_events(event_rx);
        let turn = TurnSlot::new(self.generations.next(), event_rx);
        Self::forward_events_between_turns(
            &session_id,
            &hub,
            &turn.rx,
            &arrived,
            &turn.idle,
            &cancel,
        );
        let live = LiveSession {
            input_tx,
            turn,
            cancel,
            exited,
            context: None,
            scene: scene_for_session,
            project: project_root_for_meta.clone(),
            named_by_user: stored_name.is_some(),
            name: stored_name,
            created_at: Instant::now(),
            is_first_turn: !has_user_turn,
            namer,
            binding_stale: binding.stale.clone(),
            state: Some(state.clone()),
            spawner: Some(spawner),
            scripts: script_ledger,
            health,
            hub: hub.clone(),
            process_groups,
        };
        // `insert` returning `Some` means a live entry already existed under
        // this exact sid — only an in-place rebuild (`rebuild_idle`) gets
        // here. Its `CancellationToken` isn't tied to `Drop`, so the old
        // Agent's background loop would otherwise keep running forever as a
        // leaked task if we didn't cancel it here. Who watches the session
        // and what it is still being asked go on with it; a turn waiting for
        // the old entry's slot is woken to find the new one.
        let rebuilt = match sessions.insert(session_id.clone(), live) {
            Some(old) => {
                old.cancel.cancel();
                if !Arc::ptr_eq(&old.hub, &hub) {
                    hub.absorb(&old.hub);
                }
                old.turn.idle.notify_waiters();
                true
            }
            None => false,
        };
        drop(sessions);
        let goal = state.goal_view();
        if resume && !goal.is_null() {
            hub.publish(&frame_between_turns(
                &session_id,
                SessionEvent::GoalChanged(goal),
            ));
        }
        if !rebuilt {
            self.emit_session_event(
                if resume {
                    "session_resumed"
                } else {
                    "session_created"
                },
                &session_id,
                serde_json::json!({
                    "scene": scene_id,
                    "project_root": project_root_for_meta,
                }),
            );
        }
        info!(%session_id, "session created");
        Ok(session_id)
    }

    /// This session's spec: the one this process already holds, else the one
    /// its log recorded (for a resume), else a new one from `options`.
    pub(super) async fn spec_for(
        &self,
        session_id: &str,
        resume: bool,
        options: Option<&SessionOptions>,
    ) -> SessionSpec {
        if let Some(spec) = self.lock_specs().get(session_id) {
            return spec.clone();
        }
        let mut spec = if resume {
            self.recorded_spec(session_id).await
        } else {
            SessionSpec::default()
        };
        // A log written before specs were recorded has none; the options of
        // the call that brought the session back are the best there is, and
        // are recorded so the next resume finds them too.
        let adopted = resume && spec.options.is_none() && options.is_some();
        if spec.options.is_none() {
            spec.options = options.cloned();
        }
        // `recorder` is never recorded, so the call bringing the session back
        // is the only place it can come from.
        if let Some(recorder) = options.and_then(|o| o.recorder.clone()) {
            spec.options.get_or_insert_with(Default::default).recorder = Some(recorder);
        }
        if spec.options.as_ref().is_some_and(|o| o.recorder.is_some()) {
            spec.recorder = Some(telemetry::recorder::Recorder::new());
        }
        let spec = self
            .lock_specs()
            .entry(session_id.to_string())
            .or_insert(spec)
            .clone();
        if adopted {
            self.persist_spec(session_id, &spec).await;
        }
        spec
    }

    pub(super) fn lock_specs(&self) -> std::sync::MutexGuard<'_, HashMap<String, SessionSpec>> {
        self.specs
            .lock()
            .expect("specs mutex is never held across a panic")
    }

    pub(super) async fn recorded_spec(&self, session_id: &str) -> SessionSpec {
        let entries = self.load_entries(session_id).await.unwrap_or_default();
        let recorded = entries
            .iter()
            .rev()
            .find_map(|env| match &env.entry {
                history::entry::LogEntry::Extension { ns, event, payload }
                    if ns == SPEC_NS && event == SPEC_EVENT =>
                {
                    serde_json::from_value::<RecordedSpec>(payload.clone()).ok()
                }
                _ => None,
            })
            .unwrap_or_default();
        SessionSpec {
            options: recorded.options,
            model: recorded.model,
            provider: recorded.provider,
            prompt: recorded.prompt,
            recorder: None,
        }
    }

    pub(super) async fn persist_spec(&self, session_id: &str, spec: &SessionSpec) {
        let Some(store) = self.history_store.as_ref() else {
            return;
        };
        let Ok(sid) = base::session::SessionId::parse(session_id) else {
            return;
        };
        let recorded = RecordedSpec {
            options: spec.options.clone().map(|mut o| {
                o.recorder = None;
                o
            }),
            model: spec.model.clone(),
            provider: spec.provider.clone(),
            prompt: spec.prompt.clone(),
        };
        let Ok(payload) = serde_json::to_value(&recorded) else {
            return;
        };
        let entry = history::entry::LogEntry::Extension {
            ns: SPEC_NS.to_string(),
            event: SPEC_EVENT.to_string(),
            payload,
        };
        if let Err(e) = store.append(sid, entry).await {
            warn!(%session_id, error = %e, "failed to record the session spec");
        }
    }

    /// Change this session's spec in memory only, returning it as it was and
    /// as it is now.
    pub(super) fn change_spec(
        &self,
        session_id: &str,
        change: impl FnOnce(&mut SessionSpec),
    ) -> (Option<SessionSpec>, SessionSpec) {
        let mut specs = self.lock_specs();
        let previous = specs.get(session_id).cloned();
        let spec = specs.entry(session_id.to_string()).or_default();
        change(spec);
        (previous, spec.clone())
    }

    /// Longest name `session.rename` accepts.
    ///
    /// A name is a label in somebody's session list, not a description. The
    /// cap is here rather than at the wire layer because it is the engine's
    /// answer, and a host that embeds the engine gets the same one.
    pub(super) const MAX_SESSION_NAME: usize = 200;
}
