//! 会话创建：`create_session` 和 `create` 的装配。

use crate::session_pool::*;

impl SessionPool {
    /// `session.create` — the explicit multi-project entry point, with an
    /// optional `scene` (resolved against the active scenes — see
    /// `SceneCatalog::resolve`; omitted means the pool's default scene). Builds and
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
    ) -> Result<api::SessionCreated, PoolError> {
        let Some(scene) = self.scenes.resolve(scene_id) else {
            return Err(PoolError::SceneNotActive {
                scene: scene_id.unwrap_or("").to_string(),
            });
        };

        // Settings are resolved against *this session's* scene, not the
        // daemon's. The `Default` arm goes through the same call as the others
        // so that a non-startup scene on the default project still picks up
        // its own tier; `settings_for_project` short-circuits to the
        // hot-swappable own settings for the one pair that is the pool's
        // own.
        // A scene that requires a project is refused here, not later: with no
        // project root there is nothing for its tools to read or write, and
        // the failure is far more legible at creation than at the first turn.
        if scene.requires_project && matches!(project_root, ProjectSelector::NoProject) {
            return Err(PoolError::ProjectRequired {
                scene: scene.id.clone(),
            });
        }

        let session_scene_id = scene.id.clone();
        let (binding, project_root_for_meta) = match project_root {
            ProjectSelector::Default => (
                self.binding_for(Some(self.cwd()), &session_scene_id).await,
                Some(self.cwd().to_path_buf()),
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

        let scene_id_for_result = scene.id.clone();
        let project_trust = binding.settings.trust();
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
            let sessions = self.live.lock().await;
            sessions
                .get(&created_id)
                .map(|live| format_instant(live.created_at))
                .unwrap_or_default()
        };

        Ok(api::SessionCreated {
            session_id: created_id,
            scene: scene_id_for_result,
            session_kind: api::SessionKindName::Primary,
            project_root: project_root_for_meta.map(|p| p.display().to_string()),
            project_trust: project_trust.into(),
            created_at,
        })
    }

    /// 创建新 session 并启动 Agent 后台 run loop。
    /// `options` 仅在新创建 session 时生效；已有 session 时忽略。
    ///
    /// `base_settings`/`project_root_for_meta` are the already-resolved
    /// settings for whichever project this session belongs to (see
    /// `settings_for_project`) and the value to stamp on `Meta.project_root`
    /// (`None` = no-project session) — callers that just want "this pool's
    /// default project" pass `self.bindings.pool_settings().await` /
    /// `Some(self.cwd().to_path_buf())`.
    pub(in crate::session_pool) async fn create(
        &self,
        session_id: String,
        scene: Arc<SceneSpec>,
        binding: Arc<ProjectBinding>,
        project_root_for_meta: Option<PathBuf>,
        options: Option<&SessionOptions>,
        resume: bool,
    ) -> Result<String, CreateError> {
        self.live.refuse_while_shutting_down()?;
        self.live.wait_until_stopped(&session_id).await?;
        let claim = self.claim_log(&session_id).await?;
        let spec = self.specs.spec_for(&session_id, resume, options).await;
        self.directory.note_served(project_root_for_meta.as_deref());
        let scene_id = scene.id.clone();
        // The `Builder` takes ownership of the scene; the session keeps a
        // handle of its own because `run_turn` has to ask it things.
        let scene_for_session = scene.clone();

        // A rebuild keeps the ledger: the session goes on, and so do the
        // groups its earlier commands started.
        let process_groups = if binding.settings.sandbox.reap_process_groups {
            let kept = self
                .live
                .lock()
                .await
                .get(&session_id)
                .and_then(|live| live.process_groups.clone());
            Some(kept.unwrap_or_else(platform::GroupLedger::new))
        } else {
            None
        };
        let (mcp_server_count, mcp_module) = self
            .mcp_session_module(project_root_for_meta.as_deref(), &scene_id)
            .await;
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
            .unwrap_or_else(|| self.cwd().to_path_buf());
        let built = self.factory.build(BuildRequest {
            session_id: &session_id,
            scene,
            binding: &binding,
            project_root: project_root_for_meta.as_deref(),
            script_root: &script_root,
            spec: &spec,
            resume,
            scene_registry: self.scenes.registry(),
            cron_store: self.scheduler.store(),
            package_scripts: self.plugins.script_bindings(),
            mcp_module,
            process_groups: process_groups.clone(),
        })?;
        self.plugins.note_script_faults(built.script_faults);
        let BuiltEngine {
            mut agent,
            event_rx,
            input_tx,
            namer,
            scripts: script_ledger,
            settings: settings_snapshot,
            ..
        } = built;
        let state = agent.session_state();
        let delegator = agent.delegator();
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
            if let Some(store) = self.directory.store() {
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
                self.specs.persist(&session_id, &spec).await;
            }
        }

        // A log that does not load back is the caller's error: going on with
        // an empty context would carry the conversation on with none of what
        // came before, appended to the very log that holds it.
        let mut stored_name = None;
        let mut has_user_turn = false;
        if resume {
            let previous_state = self
                .live
                .lock()
                .await
                .get(&session_id)
                .and_then(|live| live.state.clone());
            self.stop_live_agent(&session_id).await?;
            let resume_start = std::time::Instant::now();
            let resume_result = agent.load_transcript(&session_id).await;
            let outcome = match &resume_result {
                Ok(()) => telemetry::ResumeOutcome::Succeeded,
                Err(_) => telemetry::ResumeOutcome::Failed,
            };
            let (projected_message_count, entry_count) =
                self.directory.history_counts(&session_id).await;
            let entries = self
                .directory
                .load_entries(&session_id)
                .await
                .unwrap_or_default();
            let compact_boundary_count = entries
                .iter()
                .filter(|e| matches!(e.entry, history::entry::LogEntry::Compact { .. }))
                .count();
            let sidechain_entry_count = entries.iter().filter(|e| e.is_sidechain).count();
            has_user_turn = entries.iter().any(|e| {
                !e.is_sidechain && matches!(e.entry, history::entry::LogEntry::User { .. })
            });
            stored_name = self.directory.stats_of(&session_id).await.title;
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
                    if let Some(live) = self.live.lock().await.remove(&session_id) {
                        self.evict(&session_id, &live);
                    }
                }
                return Err(format!("its transcript could not be loaded: {e}").into());
            }
            agent.restore_state_from_log().await;
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

        let mut sessions = self.live.lock().await;
        // Checked again under the lock `shutdown_all` drains the map with: a
        // session inserted after that would outlive the shutdown.
        if let Err(e) = self.live.refuse_while_shutting_down() {
            cancel.cancel();
            return Err(e.into());
        }

        // 容量检查：驱逐最久未活跃的 session。只有这次 insert 真的会让
        // session **总数**增加时才需要驱逐——`session_id` 已经在 map 里的话
        // （config-generation 触发的原地重建，见 `run_turn`），这次操作是
        // "替换同一个 key"，不是"新增一个"，`sessions.len()` 不会变，不该
        // 因为这次重建就凭空多驱逐一个无关的、真正最久未用的 session。
        {
            if let Some((evicted, live)) = self.live.make_room(&mut sessions, &session_id) {
                self.evict(&evicted, &live);
                info!(sid = %evicted, "session evicted (LRU, pool full)");
                self.emit_event(api::DaemonEvent::SessionEvicted {
                    session_id: evicted.to_string(),
                    reason: api::EvictReason::Capacity,
                });
            }
        }

        let (event_rx, arrived) = relay_events(event_rx);
        let turn = TurnSlot::new(self.live.next_generation(), event_rx);
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
            binding: binding.stamp.clone(),
            state: Some(state.clone()),
            delegator: Some(delegator),
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
        if resume && goal.is_some() {
            hub.publish(&frame_between_turns(
                &session_id,
                SessionEvent::GoalChanged(goal),
            ));
        }
        if !rebuilt {
            self.emit_event(api::DaemonEvent::session_loaded(
                resume,
                session_id.clone(),
                scene_id.to_string(),
                project_root_for_meta.clone(),
            ));
        }
        info!(%session_id, "session created");
        Ok(session_id)
    }

    /// Longest name `session.rename` accepts.
    ///
    /// A name is a label in somebody's session list, not a description. The
    /// cap is here rather than at the wire layer because it is the engine's
    /// answer, and a host that embeds the engine gets the same one.
    pub(in crate::session_pool) const MAX_SESSION_NAME: usize = 200;
}
