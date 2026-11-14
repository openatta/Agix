//! 会话恢复与重建：恢复、场景校验、空闲会话的重建与 `resume_or_create`。

use crate::session_pool::*;

impl SessionPool {
    /// See [`SceneCheck`]. Safe to call unconditionally, including for a
    /// session id with no on-disk history at all (degrades to `Inferred`).
    ///
    /// Matches against any *currently active* scene (`SceneCatalog::is_active`),
    /// not just this pool's default — a session legitimately created via
    /// `session.create {scene: "harnix"}` after `scene.activate {scene:
    /// "harnix"}` is exactly as resumable/closeable/forkable as one under the
    /// default scene; only a scene this daemon isn't currently serving at
    /// all counts as a mismatch.
    pub(in crate::session_pool) async fn check_scene(&self, session_id: &str) -> SceneCheck {
        match self.directory.facts_of(session_id).await.scene {
            None => SceneCheck::Inferred,
            Some(s) if self.scenes.is_active(&s) => SceneCheck::Matches,
            Some(s) => SceneCheck::Mismatch(s),
        }
    }

    /// Refuse to act on `session_id` across a scene boundary: a session
    /// recorded under a scene this daemon does not serve is not reattached,
    /// closed, forked or written to here. `Ok(true)` when its scene could
    /// only be inferred.
    pub(in crate::session_pool) async fn refuse_scene_mismatch(
        &self,
        session_id: &str,
    ) -> Result<bool, PoolError> {
        match self.check_scene(session_id).await {
            SceneCheck::Inferred => Ok(true),
            SceneCheck::Matches => Ok(false),
            SceneCheck::Mismatch(recorded) => Err(PoolError::SceneMismatch {
                session_id: session_id.to_string(),
                recorded,
                requested: self.scene_id().to_string(),
            }),
        }
    }

    /// A sidechain (sub-agent/team-member) session that has a
    /// `LogEntry::SessionEnd` marker has run its one-shot task to
    /// conclusion and has no continuation semantics — `session.resume`
    /// must reject it with `SIDECHAIN_TERMINAL` instead of silently
    /// reattaching to a "finished" transcript. Primary sessions and
    /// sidechains with no marker (still running, or cut off externally)
    /// return `None` and proceed normally.
    pub async fn sidechain_terminal_state(
        &self,
        session_id: &str,
    ) -> Option<(Option<String>, history::entry::SessionEndState)> {
        self.directory.sidechain_terminal_state(session_id).await
    }

    /// `session.resume` — the explicit, inspectable form of what
    /// `session.run_turn` has always done implicitly when handed a known
    /// `session_id`.
    ///
    /// Reattaches (loading the transcript back into the engine's context via
    /// the same `resume_or_create` → `Agent::load_transcript` path
    /// `run_turn` uses) and reports what was reattached to, so a restarted
    /// client can tell "I'm continuing a 40-message conversation" from "this
    /// id was never used".
    ///
    /// `create_if_missing` is the one place this deliberately differs from
    /// `run_turn`: an id with no history is an error here by default
    /// (`SESSION_NOT_FOUND`), because an explicit "resume" that silently
    /// invents an empty session is how a client ends up rendering a blank
    /// window and calling it a restored one. `run_turn` keeps its
    /// create-on-miss behavior unchanged — sending a message to an unknown
    /// id is a different intent.
    ///
    /// An already-live session is reported as `already_active` and is *not*
    /// recreated: recreating swaps the `Agent` out from under any turn
    /// currently running on it (see `create()`'s same-sid replace path).
    /// This is also this method's fast path: an already-active id returns
    /// straight from the in-memory `sessions` map, never acquiring
    /// `with_session_lock` or reading `sidechain_terminal_state`'s full
    /// transcript off disk — both only matter for an id this pool isn't
    /// already actively serving.
    ///
    /// The result says whether the session's scene was only inferred
    /// (`scene_inferred`): a log too old to record one is resumed under this
    /// daemon's.
    pub async fn resume_session(
        &self,
        session_id: &str,
        create_if_missing: bool,
        options: Option<&SessionOptions>,
    ) -> Result<api::ResumeReport, PoolError> {
        let scene_inferred = self.refuse_scene_mismatch(session_id).await?;
        let report = self
            .resume_unchecked(session_id, create_if_missing, options)
            .await?;
        Ok(api::ResumeReport {
            scene_inferred,
            ..report
        })
    }

    pub(in crate::session_pool) async fn resume_unchecked(
        &self,
        session_id: &str,
        create_if_missing: bool,
        options: Option<&SessionOptions>,
    ) -> Result<api::ResumeReport, PoolError> {
        let resume_start = Instant::now();
        let settings = self.bindings.pool_settings().await;
        let mode = effective_permission_mode(&settings, options);

        // Already live: report state, touch nothing. The lock is released
        // before the (disk-reading) history lookup — `sessions` is the
        // pool's hottest mutex and every other holder keeps it CPU-only.
        let already_active = self.live.core_of(session_id).await.map(|core| {
            core.hub().touch();
            active_times(&core)
        });
        if let Some((name, created_at, last_active)) = already_active {
            let (message_count, entry_count) = self.directory.history_counts(session_id).await;
            self.events.record(telemetry::TelemetryEvent::resume_action(
                session_id,
                0,
                None,
                telemetry::ResumeActionPayload {
                    outcome: telemetry::ResumeOutcome::Succeeded,
                    source: "already_active".into(),
                    entry_count,
                    projected_message_count: message_count,
                    compact_boundary_count: 0,
                    sidechain_entry_count: 0,
                    warning_kind: None,
                    latency_ms: resume_start.elapsed().as_millis() as u64,
                },
            ));
            return Ok(self
                .resume_report(
                    session_id,
                    api::ResumeStatus::AlreadyActive,
                    message_count,
                    entry_count,
                    name,
                    created_at,
                    last_active,
                    mode,
                )
                .await);
        }

        // Not already active — serialize against a concurrent cascade
        // delete of this same id (see `with_session_lock`'s doc comment):
        // the terminal-state check and `resume_or_create` below must both
        // run as one atomic step relative to `delete_sidechain_list`/
        // `delete_session`, not race them.
        self.live
            .with_session_lock(session_id, async {
                // `docs/daemon_rpc_protocol.md` §3.3: a sidechain that already reached a terminal state has
                // no continuation semantics — reject before touching engine
                // state.
                if let Some((parent_session_id, state)) =
                    self.sidechain_terminal_state(session_id).await
                {
                    self.record_refused_resume(session_id, "sidechain_terminal", resume_start);
                    return Err(PoolError::SidechainTerminal {
                        session_id: session_id.to_string(),
                        parent_session_id,
                        final_state: state,
                    });
                }

                let has_history = match self.directory.has_history(session_id).await {
                    Ok(has_history) => has_history,
                    Err(e) => {
                        self.record_refused_resume(session_id, "unreadable", resume_start);
                        return Err(PoolError::Internal(e));
                    }
                };
                let (message_count, entry_count) = self.directory.history_counts(session_id).await;
                if !has_history && !create_if_missing {
                    self.record_refused_resume(session_id, "not_found", resume_start);
                    return Err(PoolError::session_gone(
                        session_id,
                        format!(
                            "session not found: {session_id} \
                         (pass create_if_missing: true to start a new session under this id)"
                        ),
                    ));
                }

                let raced = self
                    .live
                    .core_of(session_id)
                    .await
                    .map(|core| active_times(&core));
                if let Some((name, created_at, last_active)) = raced {
                    return Ok(self
                        .resume_report(
                            session_id,
                            api::ResumeStatus::AlreadyActive,
                            message_count,
                            entry_count,
                            name,
                            created_at,
                            last_active,
                            mode,
                        )
                        .await);
                }
                let core = self
                    .resume_or_create(session_id.to_string(), options)
                    .await
                    .map_err(PoolError::from)?;
                let (_, created_at, last_active) = active_times(&core);

                Ok(self
                    .resume_report(
                        core.id(),
                        api::ResumeStatus::loaded(has_history),
                        message_count,
                        entry_count,
                        None,
                        created_at,
                        last_active,
                        mode,
                    )
                    .await)
            })
            .await
    }

    fn record_refused_resume(&self, session_id: &str, why: &str, started: Instant) {
        self.events.record(telemetry::TelemetryEvent::resume_action(
            session_id,
            0,
            None,
            telemetry::ResumeActionPayload {
                outcome: telemetry::ResumeOutcome::Failed,
                source: why.into(),
                entry_count: 0,
                projected_message_count: 0,
                compact_boundary_count: 0,
                sidechain_entry_count: 0,
                warning_kind: Some(why.into()),
                latency_ms: started.elapsed().as_millis() as u64,
            },
        ));
    }

    /// `mode` is what the session was created under; a live session reports
    /// the mode it is in now, which `EnterPlanMode` / `ExitPlanMode` move.
    #[allow(clippy::too_many_arguments)]
    pub(in crate::session_pool) async fn resume_report(
        &self,
        session_id: &str,
        status: api::ResumeStatus,
        message_count: usize,
        entry_count: usize,
        name: Option<String>,
        created_at: String,
        last_active: String,
        mode: base::settings::PermissionMode,
    ) -> api::ResumeReport {
        let (mode, project) = match self.live.core_of(session_id).await {
            Some(core) => (
                core.state().permission_mode(),
                Some((
                    core.project().map(Path::to_path_buf),
                    core.scene().id.clone(),
                )),
            ),
            None => (mode, None),
        };
        let (project_trust, settings) = match project {
            Some((project, scene)) => {
                let settings = self.settings_for_project(project.as_deref(), &scene).await;
                (settings.trust(), settings)
            }
            None => (
                base::project_trust::ProjectTrust::NoProject,
                self.bindings.pool_settings().await,
            ),
        };
        let prompts_enabled = mode != base::settings::PermissionMode::BypassPermissions
            || settings
                .managed
                .as_ref()
                .is_some_and(|m| m.asks_under_bypass());
        api::ResumeReport {
            session_id: session_id.to_string(),
            status,
            active: true,
            message_count,
            entry_count,
            name,
            created_at,
            last_active,
            scene: self.scenes.default_id().to_string(),
            project_trust: project_trust.into(),
            permission: api::PermissionReport {
                mode,
                prompts: prompts_enabled,
                prompt_timeout_secs: self.factory.permission_prompt_timeout_secs(&settings),
            },
            scene_inferred: false,
        }
    }

    /// Hold `session_id`'s log against other processes for as long as the
    /// returned claim lives — `Err` when another daemon has it open. An id
    /// that is not a session id has no log to hold.
    pub(in crate::session_pool) async fn claim_log(
        &self,
        session_id: &str,
    ) -> Result<Option<history::store::SessionClaim>, String> {
        let (Some(store), Ok(sid)) = (
            self.directory.store(),
            base::session::SessionId::parse(session_id),
        ) else {
            return Ok(None);
        };
        match store.claim(sid).await {
            Ok(claim) => Ok(Some(claim)),
            Err(e @ history::error::HistoryError::Locked(_)) => Err(e.to_string()),
            Err(e) => {
                warn!(%session_id, error = %e, "could not take the session log's writer lock");
                Ok(None)
            }
        }
    }

    /// Bring `sid` into memory under that id: resumed from its log when it
    /// has one, created empty otherwise. A failure is the caller's error —
    /// never a different session under a fresh id, which would carry the
    /// conversation on somewhere the host is not looking.
    pub(in crate::session_pool) async fn resume_or_create(
        &self,
        sid: String,
        options: Option<&SessionOptions>,
    ) -> Result<Arc<SessionCore>, CreateError> {
        let _claim = self.claim_log(&sid).await?;
        let has_history = self.directory.has_history(&sid).await?;

        // Rebuild against *this session's own* recorded project, not
        // always the pool default — otherwise a session created under
        // `session.create {project_root: X}` would silently come back with
        // project Y's settings the moment it needed reattaching (idle
        // eviction, daemon restart, explicit `session.resume`).
        let startup_scene = self.bindings.startup_scene().await;
        let (binding, project_root_for_meta, scene_id) =
            match self.directory.recorded_project(&sid).await {
                Some((schema_version, project_root, scene)) if schema_version >= 2 => {
                    let scene = scene.unwrap_or_else(|| startup_scene.clone());
                    match project_root {
                        Some(p) => {
                            let path = PathBuf::from(p);
                            (
                                self.binding_for(Some(&path), &scene).await,
                                Some(path),
                                scene,
                            )
                        }
                        None => (self.binding_for(None, &scene).await, None, scene),
                    }
                }
                _ => (
                    self.default_binding().await,
                    Some(self.cwd().to_path_buf()),
                    startup_scene.clone(),
                ),
            };

        // The scene this session recorded, not this pool's default. Settings
        // were already resolved against the recorded scene above; handing the
        // default scene to `create()` anyway gave the rebuilt session that
        // scene's system prompt and tool surface while it read the other
        // one's settings — two halves of one session disagreeing about which
        // scene it is. Every caller is scene-guarded (`check_scene`) before
        // reaching here, so an inactive scene at this point is a race, and
        // refusing is the only answer that doesn't silently change what the
        // session is.
        let Some(scene) = self.scenes.resolve(Some(&scene_id)) else {
            return Err(format!(
                "session `{sid}` was recorded under scene `{scene_id}`, which is not active on this daemon"
            )
            .into());
        };

        self.create(
            sid.clone(),
            scene,
            binding,
            project_root_for_meta,
            options,
            has_history,
        )
        .await
        .map_err(|e| e.context(|e| format!("session `{sid}` could not be brought back: {e}")))
    }

    /// `sid` in memory: resumed from its log, or created, unless it already
    /// is. Serialized per id, so two callers racing on one cold session get
    /// one engine rather than the second replacing the first's.
    pub(in crate::session_pool) async fn ensure_live(
        &self,
        sid: &str,
        options: Option<&SessionOptions>,
    ) -> Result<String, CreateError> {
        self.live
            .with_session_lock(sid, async {
                if self.live.contains(sid).await {
                    return Ok(sid.to_string());
                }
                self.resume_or_create(sid.to_string(), options)
                    .await
                    .map(|core| core.id().to_string())
            })
            .await
    }

    /// Rebuild a live, idle session in place from its spec and log. Its
    /// turn slot is held from before the old engine stops until the new one
    /// is in the map, so no turn can start on, or be cut off by, the
    /// replacement; a session running a turn or background tasks is refused.
    pub(in crate::session_pool) async fn rebuild_idle(
        &self,
        sid: &str,
        options: Option<&SessionOptions>,
    ) -> Result<String, Rebuild> {
        self.live
            .with_session_lock(sid, async {
                let held = self.hold_for_rebuild(sid).await?;
                self.rebuild_holding(sid, held, options).await
            })
            .await
    }

    /// Move live, idle `sid` into `Rebuilding`, so no turn starts until the
    /// rebuild is aborted or the rebuilt entry replaces it. A session whose
    /// background tasks still run, or that has team members, idle or not, is
    /// not idle: they run under its engine and would be stopped with it.
    /// Callers hold `sid`'s session lock.
    pub(in crate::session_pool) async fn hold_for_rebuild(
        &self,
        sid: &str,
    ) -> Result<HeldSlot, Rebuild> {
        self.live
            .with_live_mut(sid, |live| {
                if let Some(busy) = live.turn.busy(sid) {
                    return Err(Rebuild::Busy(busy));
                }
                let running_tasks = live.background_work();
                if !running_tasks.is_empty() {
                    return Err(Rebuild::Busy(Busy::background_tasks(sid, running_tasks)));
                }
                live.turn
                    .begin_rebuild()
                    .map(|()| HeldSlot {
                        generation: live.engine.generation,
                    })
                    .map_err(|_| Rebuild::Busy(Busy::rebuilding(sid)))
            })
            .await
            .ok_or(Rebuild::NotLive)?
    }

    /// Rebuild `sid` on its spec as `change` leaves it, recording the spec in
    /// its log once the rebuild has taken it. A failed rebuild leaves the
    /// spec as it was, so the session, wherever it is now, is not left
    /// running on one spec while its log says another.
    pub(in crate::session_pool) async fn rebuild_on_changed_spec(
        &self,
        sid: &str,
        held: HeldSlot,
        change: impl FnOnce(&mut SessionSpec),
    ) -> Result<SessionSpec, Rebuild> {
        let (previous, changed) = self.specs.update(sid, change);
        self.project_model_choice(sid).await;
        if let Err(e) = self.rebuild_holding(sid, held, None).await {
            self.specs.restore(sid, previous);
            self.project_model_choice(sid).await;
            if !self.live.contains(sid).await {
                self.specs.release(sid);
            }
            return Err(e);
        }
        // A `setModel` that passed its idle check before this rebuild took the
        // slot may have changed the spec since `update`.
        let spec = self.specs.get(sid).unwrap_or(changed);
        self.specs.persist(sid, &spec).await;
        Ok(spec)
    }

    /// Rebuild `sid` while `held` keeps turns off it. A failed rebuild puts
    /// the session that is still there back to `Idle`.
    pub(in crate::session_pool) async fn rebuild_holding(
        &self,
        sid: &str,
        held: HeldSlot,
        options: Option<&SessionOptions>,
    ) -> Result<String, Rebuild> {
        let rebuilt = self.rebuild_engine(sid, &held, options).await;
        if rebuilt.is_err() {
            self.live
                .with_live_mut(sid, |live| {
                    if live.engine.generation == held.generation {
                        live.turn.abort_rebuild();
                    }
                })
                .await;
        }
        rebuilt.map(|()| sid.to_string()).map_err(Rebuild::Failed)
    }
}

/// A live session's name, and when it was created and last active, as
/// `session.resume` reports them.
fn active_times(core: &SessionCore) -> (Option<String>, String, String) {
    (
        core.naming().name.clone(),
        format_instant(core.created_at()),
        format_instant(core.hub().last_active()),
    )
}
