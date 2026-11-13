//! 后台任务：配置监视、定时任务调度、空闲回收（janitor）。

use crate::session_pool::*;

impl SessionPool {
    /// Some change notifications were lost, so any tier may have changed:
    /// reload this pool's own pair, and drop every other pair's MCP
    /// connections so none outlives the settings it was built from.
    pub(in crate::session_pool) async fn reload_after_lost_changes(&self) {
        #[cfg(feature = "mcp")]
        {
            let own = (Some(self.cwd.clone()), self.scene_id().to_string());
            self.forget_mcp_built_from(&crate::watch_hub::Tier::Global, Some(&own))
                .await;
        }
        self.reload_settings(None, self.scene_id()).await;
    }

    /// React to what a binding is built from changing on disk, for **every**
    /// project this daemon serves rather than only its own.
    ///
    /// A settings change invalidates exactly the bindings merged from the tier
    /// that changed; a change to this daemon's own tiers also goes through the
    /// full reload, since the merged settings, the task router and the MCP
    /// reconcile all hang off it. A skill or agent-type directory change is
    /// re-read into the catalogs in place — sessions share them, so nothing
    /// needs rebuilding.
    ///
    /// The pool's own tiers are watched for as long as the pool lives, not
    /// only while some binding on them happens to exist.
    pub fn start_config_watcher(self: &Arc<Self>) {
        let Some(mut changes) = self.watch.as_ref().map(|hub| hub.changes()) else {
            return;
        };
        let pool = self.clone();
        tokio::spawn(async move {
            use crate::watch_hub::{Tier, WatchTarget};
            {
                let settings = pool.settings.read().await.clone();
                let own = (Some(pool.cwd.clone()), settings.paths.scope.clone());
                let (tokens, _) = pool.watch_tiers_of(&settings, &own);
                *pool
                    .pool_watch
                    .lock()
                    .expect("pool_watch is never held across a panic") = tokens;
            }
            loop {
                let key = match changes.recv().await {
                    Ok(key) => key,
                    // Lagged: some notifications were dropped, which means
                    // something changed — reload rather than skip it.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        pool.reload_after_lost_changes().await;
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                };
                match key.target {
                    WatchTarget::Settings => {}
                    WatchTarget::Skills | WatchTarget::Agents => {
                        info!(tier = ?key.tier, target = ?key.target, "a catalog changed on disk");
                        pool.refresh_catalogs(&key.tier, key.target).await;
                        continue;
                    }
                    // Plugins are a global/scene concern with a refresh of
                    // their own (`plugin.reload`).
                    _ => continue,
                }
                let ours = match &key.tier {
                    Tier::Project(root) => root == &pool.cwd,
                    Tier::Global | Tier::Scene(_) => true,
                };
                if ours {
                    info!(tier = ?key.tier, "settings changed on disk; reloading");
                    let scene = match &key.tier {
                        Tier::Scene(scene) => scene.clone(),
                        _ => pool.scene_id().to_string(),
                    };
                    #[cfg(feature = "mcp")]
                    let reconciled = (Some(pool.cwd.clone()), scene.clone());
                    #[cfg(feature = "mcp")]
                    pool.forget_mcp_built_from(&key.tier, Some(&reconciled))
                        .await;
                    pool.reload_settings_from(None, &scene, Some(&key.tier))
                        .await;
                } else {
                    info!(tier = ?key.tier, "settings changed on disk");
                    #[cfg(feature = "mcp")]
                    pool.forget_mcp_built_from(&key.tier, None).await;
                    pool.invalidate_bindings(Some(&key.tier)).await;
                }
            }
        });
    }

    /// Fire scheduled jobs as they come due, for as long as the pool lives.
    ///
    /// A due job runs as a turn of its session through [`Self::run_turn`],
    /// the same path a client's message takes, so it queues behind a running
    /// turn, is announced as `turn_started` / `turn_completed` with initiator
    /// `"schedule"`, and its frames reach whoever watches the session. A
    /// session that is not in memory is resumed for it. Jobs missed while the
    /// daemon was down are not caught up.
    pub fn start_scheduler(self: &Arc<Self>) {
        let launcher: std::sync::Weak<dyn TurnLauncher> = self.this.clone();
        self.scheduler.start(launcher);
    }

    /// Fire every job due at `now`, each as its session's turn, and return
    /// once those turns have ended. The scheduler calls this every minute.
    pub async fn run_due_jobs(&self, now: time::OffsetDateTime) {
        self.scheduler.run_due(self, now).await;
    }

    /// Schedule `prompt` to run as a turn of `session_id` on `cron`.
    pub async fn schedule_add(
        &self,
        new: api::NewSchedule,
    ) -> Result<tools::cron::CronJob, PoolError> {
        let job = Scheduler::prepare(new)?;
        let session_id = job.session_id.as_deref().unwrap_or_default();
        if !self.session_exists(session_id).await {
            return Err(PoolError::no_session(session_id));
        }
        Ok(self.scheduler.add(job))
    }

    /// Scheduled jobs, all of them or one session's.
    pub fn schedule_list(&self, session_id: Option<&str>) -> Vec<tools::cron::CronJob> {
        self.scheduler.list(session_id)
    }

    pub fn schedule_remove(&self, id: &str) -> bool {
        self.scheduler.remove(id)
    }

    pub fn start_janitor(self: &Arc<Self>) {
        let pool = self.clone();
        // Sweep interval scales with `idle_timeout` so a short configured
        // timeout is actually honored. Clamped to [30s, 300s] so a very short
        // timeout doesn't spin the loop and a very long one doesn't sweep
        // needlessly often.
        let sweep_interval =
            (pool.idle_timeout / 4).clamp(Duration::from_secs(30), Duration::from_secs(300));
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(sweep_interval).await;
                let mut sessions = pool.sessions.lock().await;
                let to_evict = idle_session_ids(&sessions, Instant::now(), pool.idle_timeout);
                for sid in &to_evict {
                    if let Some(live) = sessions.remove(sid) {
                        pool.evict(sid, &live);
                        info!(%sid, "session evicted (idle timeout)");
                        pool.emit_event(api::DaemonEvent::SessionEvicted {
                            session_id: sid.to_string(),
                            reason: api::EvictReason::Idle,
                        });
                    }
                }
                if !to_evict.is_empty() {
                    debug!(
                        evicted = to_evict.len(),
                        remaining = sessions.len(),
                        "janitor run"
                    );
                }
            }
        });
    }
}

#[async_trait::async_trait]
impl TurnLauncher for SessionPool {
    async fn launch_scheduled(&self, job: tools::cron::CronJob) {
        let Some(session_id) = job.session_id.clone() else {
            warn!(job_id = %job.id, "scheduled job belongs to no session; dropping it");
            self.scheduler.remove(&job.id);
            return;
        };
        info!(job_id = %job.id, %session_id, "scheduled job fired");
        let request = TurnRequest {
            session_id: Some(session_id.clone()),
            message: job.prompt.clone(),
            attachments: Vec::new(),
            turn_id: format!("schedule-{}-{}", job.id, Id::new()),
            options: None,
            on_busy: OnBusy::Queue,
            initiator: Initiator::Schedule,
        };
        let response = self.run_turn(request, None).await;
        if let Err(e) = response {
            warn!(job_id = %job.id, %session_id, error = %e, "scheduled job's turn failed");
        }
    }
}
