//! 把会话带进内存的两条编排：冷建（新建或从日志恢复：建 `SessionCore`、造引擎、
//! 挂上、读日志、插表、起转发器）和原地重建（给同一个 `SessionCore` 换下一代引擎：
//! 先造好，再停旧的，旧的停下之后才挂新的）。

use crate::session_pool::parts::factory::{Restore, SessionSeed};
use crate::session_pool::parts::live::{EngineSlot, Naming, SessionCore};
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
        let core = self
            .create(
                sid,
                scene,
                binding,
                project_root_for_meta.clone(),
                options,
                None,
            )
            .await?;

        Ok(api::SessionCreated {
            session_id: core.id().to_string(),
            scene: scene_id_for_result,
            session_kind: api::SessionKindName::Primary,
            project_root: project_root_for_meta.map(|p| p.display().to_string()),
            project_trust: project_trust.into(),
            created_at: format_instant(core.created_at()),
        })
    }

    /// Bring `session_id` into memory as a new session, or — given its `log`
    /// — as the session that log holds, and start its first engine.
    /// `options` only take effect on a session that has no spec yet.
    ///
    /// `binding`/`project_root_for_meta` are the already-resolved binding for
    /// whichever project this session belongs to and the value to stamp on
    /// `Meta.project_root` (`None` = no-project session).
    pub(in crate::session_pool) async fn create(
        &self,
        session_id: String,
        scene: Arc<SceneSpec>,
        binding: Arc<ProjectBinding>,
        project_root_for_meta: Option<PathBuf>,
        options: Option<&SessionOptions>,
        log: Option<&[history::entry::EnvelopedEntry]>,
    ) -> Result<Arc<SessionCore>, CreateError> {
        let resume = log.is_some();
        self.live.refuse_while_shutting_down()?;
        self.live.wait_until_stopped(&session_id).await?;
        let claim = self.claim_log(&session_id).await?;
        let spec = self.specs.spec_for(&session_id, log, options).await;
        self.directory.note_served(project_root_for_meta.as_deref());
        let core = SessionCore::new(
            &session_id,
            scene.clone(),
            project_root_for_meta.clone(),
            Naming {
                is_first_turn: true,
                ..Naming::default()
            },
        );
        let restore = if resume {
            Restore::FromLog
        } else {
            Restore::New
        };
        self.live.refuse_while_shutting_down()?;
        let generation = self.live.next_generation();
        let mut built = self
            .build_engine(&core, scene, &binding, &spec, restore, generation)
            .await?;
        self.live.refuse_while_shutting_down()?;

        // First `Meta` line for a genuinely new primary session — resumed
        // sessions already have one from when they were first created, and
        // must not get a second (that would break the "Meta is always
        // entries[0]" assumption `SessionManager::resume`/`session.resume`
        // rely on). Best-effort: a write failure here doesn't fail session
        // creation — the session still works, it's just not identifiable by
        // scene/project on a later resume (falls back to inference, same as
        // any other pre-v2 file).
        if !resume {
            self.write_meta(&core, &built.settings).await;
            if spec.options.is_some() {
                self.specs.persist(&session_id, &spec).await;
            }
        }

        built.agent.starts_as(if resume {
            StartSource::Resume
        } else {
            StartSource::New
        });
        built.agent.attach_session();
        // A log that does not load back is the caller's error: going on with
        // an empty context would carry the conversation on with none of what
        // came before, appended to the very log that holds it.
        if let Some(has_user_turn) = self
            .load_log(
                &mut built.agent,
                &session_id,
                restore,
                log.unwrap_or_default(),
            )
            .await?
        {
            let stored_name = self.directory.stats_of(&session_id).await.title;
            *core.naming() = Naming {
                named_by_user: stored_name.is_some(),
                name: stored_name,
                is_first_turn: !has_user_turn,
            };
        }

        let (engine, event_rx) = run_engine(built, generation, &binding, claim);
        let (event_rx, arrived) = relay_events(event_rx);
        let turn = TurnSlot::new(event_rx);
        Self::forward_events_between_turns(
            &session_id,
            core.hub(),
            &turn.rx,
            &arrived,
            &turn.idle,
            &engine.cancel,
        );
        let cancel = engine.cancel.clone();
        let live = LiveSession {
            core: core.clone(),
            engine,
            turn,
        };
        let served =
            |scene: &str| scene == self.scenes.default_id() || self.scenes.is_active(scene);
        let displaced = match self.live.insert_evicting(&session_id, live, served).await {
            Ok(displaced) => displaced,
            Err(e) => {
                cancel.cancel();
                return Err(e);
            }
        };
        if let Some((evicted, live)) = displaced.evicted {
            self.evict(&evicted, &live);
            info!(sid = %evicted, "session evicted (LRU, pool full)");
            self.emit_event(api::DaemonEvent::SessionEvicted {
                session_id: evicted.to_string(),
                reason: api::EvictReason::Capacity,
            });
        }
        if let Some(replaced) = displaced.replaced {
            self.live
                .stop_removed(&session_id, &replaced, EndReason::Rebuild);
            replaced.turn.idle.notify_waiters();
        }
        Self::forward_task_events(&core);
        Self::forward_goal_events(&core);

        let goal = core.state().goal_view();
        if resume && goal.is_some() {
            core.hub().publish(&frame_between_turns(
                &session_id,
                SessionEvent::GoalChanged(goal),
            ));
        }
        self.emit_event(api::DaemonEvent::session_loaded(
            resume,
            session_id.clone(),
            core.scene().id.to_string(),
            project_root_for_meta,
        ));
        info!(%session_id, "session created");
        Ok(core)
    }

    /// Put the next engine under live `sid`, whose slot `held` keeps turns
    /// off it. The engine is built first, and a session whose build fails
    /// goes on on the engine it has; the one it has is stopped before the
    /// new one takes over the session's state, so the two never run it side
    /// by side. The session's state, watchers and open questions are the
    /// same before and after; the transcript is read back from the log.
    pub(in crate::session_pool) async fn rebuild_engine(
        &self,
        sid: &str,
        held: &HeldSlot,
        options: Option<&SessionOptions>,
    ) -> Result<(), CreateError> {
        let core = self
            .live
            .core_of(sid)
            .await
            .ok_or_else(|| format!("session `{sid}` left memory before it was rebuilt"))?;
        self.catch_up_log(&core, held).await?;
        let claim = self.claim_log(sid).await?;
        let recorded = self.directory.read_log(sid).await?;
        let has_history = !recorded.is_empty();
        let spec = self
            .specs
            .spec_for(sid, has_history.then_some(&recorded[..]), options)
            .await;
        let scene_id = core.scene().id.clone();
        let Some(scene) = self.scenes.resolve(Some(&scene_id)) else {
            return Err(format!(
                "session `{sid}` was recorded under scene `{scene_id}`, which is not active on this daemon"
            )
            .into());
        };
        let binding = self.binding_for(core.project(), &scene_id).await;
        let restore = if has_history {
            Restore::Kept
        } else {
            Restore::New
        };
        let generation = self.live.next_generation();
        let mut built = self
            .build_engine(&core, scene, &binding, &spec, restore, generation)
            .await?;

        self.live.refuse_while_shutting_down()?;
        self.stop_live_agent(sid).await?;
        built.agent.starts_as(StartSource::Rebuild);
        built.agent.attach_session();
        // Read again: the old engine may have written to the log until it
        // stopped just now.
        let loaded = match self.directory.read_log(sid).await {
            Ok(log) => self.load_log(&mut built.agent, sid, restore, &log).await,
            Err(e) => Err(e.into()),
        };
        if let Err(e) = loaded {
            self.drop_unrunnable(sid, &core, held).await;
            return Err(e);
        }

        let (engine, event_rx) = run_engine(built, generation, &binding, claim);
        let (event_rx, arrived) = relay_events(event_rx);
        let cancel = engine.cancel.clone();
        let swapped = self
            .live
            .with_live_mut(sid, |live| {
                if !live.runs(&core, held.generation) {
                    return None;
                }
                live.replace_engine(engine, event_rx)?;
                Some((live.turn.rx.clone(), live.turn.idle.clone()))
            })
            .await
            .flatten();
        let Some((rx, idle)) = swapped else {
            cancel.cancel();
            return Err(format!("session `{sid}` left memory while it was rebuilt").into());
        };
        Self::forward_events_between_turns(sid, core.hub(), &rx, &arrived, &idle, &cancel);
        let goal = core.state().goal_view();
        if goal.is_some() {
            core.hub()
                .publish(&frame_between_turns(sid, SessionEvent::GoalChanged(goal)));
        }
        info!(%sid, generation = generation.get(), "session rebuilt");
        Ok(())
    }

    /// The old engine is already stopped; an entry left in the map would take
    /// turns it can no longer run.
    async fn drop_unrunnable(&self, sid: &str, core: &Arc<SessionCore>, held: &HeldSlot) {
        let Some(live) = self
            .live
            .remove_if(sid, |live| live.runs(core, held.generation))
            .await
        else {
            return;
        };
        self.evict(sid, &live);
        live.turn.idle.notify_waiters();
        self.emit_event(api::DaemonEvent::SessionEvicted {
            session_id: sid.to_string(),
            reason: api::EvictReason::Rebuild,
        });
    }

    /// Have the engine `held` keeps turns off write what `core`'s log lacks,
    /// when the last turn left it behind: the next engine reads the session
    /// back from that log. An engine that has gone already made its last try
    /// on the way out, and there is nothing left to ask.
    async fn catch_up_log(&self, core: &SessionCore, held: &HeldSlot) -> Result<(), CreateError> {
        if !core.log().is_behind() {
            return Ok(());
        }
        let sid = core.id();
        let Some(input_tx) = self
            .live
            .with_live(sid, |live| {
                (live.engine.generation == held.generation).then(|| live.engine.input_tx.clone())
            })
            .await
            .flatten()
        else {
            return Ok(());
        };
        let (reply, written) = tokio::sync::oneshot::channel();
        if input_tx.send(InputMessage::Flush { reply }).is_err() {
            return Ok(());
        }
        match tokio::time::timeout(LOG_CATCH_UP_WAIT, written).await {
            Ok(Ok(Ok(()))) => {
                core.log().set_behind(false);
                Ok(())
            }
            Ok(Err(_)) => Ok(()),
            Ok(Ok(Err(e))) => Err(CreateError::PersistFailed(format!(
                "session `{sid}` was not rebuilt: its log lacks part of what it holds, and \
                 writing it failed ({e}); it goes on as it was"
            ))),
            Err(_) => Err(CreateError::PersistFailed(format!(
                "session `{sid}` was not rebuilt: its log lacks part of what it holds, and \
                 its engine did not write it within {}s; it goes on as it was",
                LOG_CATCH_UP_WAIT.as_secs()
            ))),
        }
    }

    /// Engine `generation` of `core`, on `binding` and `spec`, not yet
    /// attached to the session's state.
    async fn build_engine(
        &self,
        core: &SessionCore,
        scene: Arc<SceneSpec>,
        binding: &ProjectBinding,
        spec: &SessionSpec,
        restore: Restore,
        generation: Generation,
    ) -> Result<BuiltEngine, CreateError> {
        let session_id = core.id();
        let (mcp_server_count, mcp_module) =
            self.mcp_session_module(core.project(), &scene.id).await;
        // A project's scripts belong to that project: the paths in its
        // settings resolve against it, and so does the question of whether a
        // file is inside it — which is what decides whether the script may
        // rewrite the prompt or only add to it. Binding a session against the
        // pool's own directory instead gets both wrong for every session
        // created with a `project_root` of its own: the paths miss, so the
        // whole set is dropped, and had they hit, the project's own code
        // would have been judged to come from outside it.
        let script_root = core
            .project()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.cwd().to_path_buf());
        let mut built = self.factory.build(BuildRequest {
            session_id,
            seed: SessionSeed {
                state: core.state().clone(),
                steer: core.steer().clone(),
                generation,
            },
            restore,
            scene,
            binding,
            project_root: core.project(),
            script_root: &script_root,
            spec,
            scene_registry: self.scenes.registry(),
            cron_store: self.scheduler.store(),
            package_scripts: self.plugins.script_bindings(),
            mcp_module,
            process_groups: binding
                .settings
                .values
                .sandbox
                .reap_process_groups
                .then(|| core.process_groups().clone()),
        })?;
        self.plugins
            .note_script_faults(std::mem::take(&mut built.script_faults));

        let settings = &built.settings;
        let mode_name = serde_json::to_value(settings.values.permission_mode)
            .ok()
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_else(|| "default".to_string());
        let _ = built
            .agent
            .telemetry()
            .record(telemetry::TelemetryEvent::session_start(
                session_id,
                0,
                None,
                telemetry::SessionStartPayload {
                    permission_mode: mode_name,
                    model: settings.values.model.model_name.clone(),
                    max_tokens: settings.values.model.max_tokens,
                    thinking_mode: !matches!(
                        settings.values.model.thinking_mode,
                        base::settings::ThinkingMode::Off
                    ),
                    sandbox_enabled: !settings.values.sandbox.dangerously_disable_sandbox,
                    resume_from: (restore != Restore::New).then(|| session_id.to_string()),
                    auth_modes: Vec::new(),
                    mcp_server_count,
                    plugin_count: self.plugins.count(),
                    skill_count: binding.modules.skill_count(),
                    output_format: "jsonrpc".to_string(),
                    started_at_ms: (time::OffsetDateTime::now_utc().unix_timestamp_nanos()
                        / 1_000_000) as i64,
                },
            ));
        Ok(built)
    }

    async fn write_meta(&self, core: &SessionCore, settings: &LoadedSettings) {
        let session_id = core.id();
        let (Some(store), Ok(sid)) = (
            self.directory.store(),
            base::session::SessionId::parse(session_id),
        ) else {
            return;
        };
        let project_root = core.project().map(|p| p.display().to_string());
        let meta = history::entry::LogEntry::Meta {
            cwd: project_root
                .clone()
                .unwrap_or_else(|| settings.paths.global_data_dir.display().to_string()),
            started_at: store.environment().now(),
            model: settings.values.model.model_name.clone(),
            permission_mode: serde_json::to_value(settings.values.permission_mode)
                .ok()
                .and_then(|v| v.as_str().map(|s| s.to_string()))
                .unwrap_or_else(|| "default".to_string()),
            engine_version: env!("CARGO_PKG_VERSION").to_string(),
            agix_version: env!("CARGO_PKG_VERSION").to_string(),
            parent_session_id: None,
            scene: Some(core.scene().id.clone()),
            project_root,
            session_kind: history::entry::SessionKind::Primary,
            schema_version: history::entry::CURRENT_META_SCHEMA_VERSION,
        };
        if let Err(e) = store.append(sid, meta).await {
            warn!(%session_id, error = %e, "failed to write session Meta entry");
        }
    }

    /// Read what `restore` says the engine starts from, out of `entries`
    /// (the session's log), into `agent`. For a log read back, whether it
    /// holds a turn of the person's.
    async fn load_log(
        &self,
        agent: &mut runtime::agent::Agent,
        session_id: &str,
        restore: Restore,
        entries: &[history::entry::EnvelopedEntry],
    ) -> Result<Option<bool>, CreateError> {
        if restore == Restore::New {
            return Ok(None);
        }
        let resume_start = std::time::Instant::now();
        let resume_result = agent.load_transcript(session_id, entries);
        let (projected_message_count, entry_count) = self.directory.counts_in(entries);
        let has_user_turn = entries
            .iter()
            .any(|e| !e.is_sidechain && matches!(e.entry, history::entry::LogEntry::User { .. }));
        let _ = agent
            .telemetry()
            .record(telemetry::TelemetryEvent::resume_action(
                session_id,
                0,
                None,
                telemetry::ResumeActionPayload {
                    outcome: match &resume_result {
                        Ok(()) => telemetry::ResumeOutcome::Succeeded,
                        Err(_) => telemetry::ResumeOutcome::Failed,
                    },
                    source: "jsonl".into(),
                    entry_count,
                    projected_message_count,
                    compact_boundary_count: entries
                        .iter()
                        .filter(|e| matches!(e.entry, history::entry::LogEntry::Compact { .. }))
                        .count(),
                    sidechain_entry_count: entries.iter().filter(|e| e.is_sidechain).count(),
                    warning_kind: resume_result
                        .as_ref()
                        .err()
                        .map(|_| "resume_session_failed".to_string()),
                    latency_ms: resume_start.elapsed().as_millis() as u64,
                },
            ));
        if let Err(e) = resume_result {
            return Err(format!("its transcript could not be loaded: {e}").into());
        }
        if restore == Restore::FromLog {
            agent.restore_state_from_log(entries);
        }
        Ok(Some(has_user_turn))
    }

    /// Longest name `session.rename` accepts.
    ///
    /// A name is a label in somebody's session list, not a description. The
    /// cap is here rather than at the wire layer because it is the engine's
    /// answer, and a host that embeds the engine gets the same one.
    pub(in crate::session_pool) const MAX_SESSION_NAME: usize = 200;
}

/// Start `built`'s run loop as engine `generation`, holding the session's
/// log for as long as it runs.
fn run_engine(
    built: BuiltEngine,
    generation: Generation,
    binding: &ProjectBinding,
    claim: Option<history::store::SessionClaim>,
) -> (EngineSlot, EventReceiver) {
    let BuiltEngine {
        agent,
        event_rx,
        input_tx,
        namer,
        scripts,
        ..
    } = built;
    let delegator = agent.delegator();
    let health = agent.health();
    let end_reason = agent.end_reason();
    let cancel = CancellationToken::new();
    let exited = CancellationToken::new();
    let exited_on_drop = exited.clone().drop_guard();
    let run_cancel = cancel.clone();
    tokio::spawn(async move {
        let _exited_on_drop = exited_on_drop;
        let _claim = claim;
        let mut agent = agent;
        let _ = agent.run(run_cancel).await;
    });
    (
        EngineSlot {
            generation,
            input_tx,
            cancel,
            end_reason,
            exited,
            namer,
            binding: binding.stamp.clone(),
            delegator: Some(delegator),
            scripts,
            health,
        },
        event_rx,
    )
}
