//! 会话收尾：按会话加锁、驱逐、停止、删除、带子会话的关闭，以及整池 `shutdown_all`。

use crate::session_pool::*;

impl SessionPool {
    /// Take `live` out of memory as [`Self::stop_removed`] does, for a
    /// session that may be brought back.
    pub(in crate::session_pool) fn evict(&self, sid: &str, live: &LiveSession) {
        self.live.stop_removed(sid, live, EndReason::Evict);
        live.core.reap_process_groups_later();
        self.specs.release(sid);
    }

    /// `parent_session_id`'s **sidechains**, or `[]` if there's no history
    /// store or the scan itself fails — logged either way so
    /// `session.close`/`.delete` reporting "0 sidechains" is distinguishable
    /// in the daemon log from "there genuinely were none" instead of silently
    /// looking the same as the empty case.
    ///
    /// Children are filtered by `session_kind`, not taken whole.
    /// `parent_session_id` records two different relationships: a sidechain,
    /// which the parent spawned and which cannot outlive it, and a fork,
    /// which merely records where it was copied from. Cascading over the raw
    /// child list deletes the forks too, which is the opposite of what a fork
    /// is for.
    pub(in crate::session_pool) async fn fetch_child_sidechains(
        &self,
        parent_session_id: &str,
    ) -> Vec<base::session::SessionId> {
        let Some(store) = self.directory.store() else {
            return Vec::new();
        };
        // `HistoryStore::child_sessions`' default impl is O(n) in the total
        // number of sessions this project has ever recorded (full
        // load-and-parse per file — see its doc comment); there's no cheap
        // index to check "does this session have sidechains" first, so
        // every `session.close`/`.delete` pays this once regardless of
        // whether the session actually has any. `delete_session` (this
        // function's other caller) shares this one scan rather than running
        // it twice.
        let children = match store.child_sessions(parent_session_id).await {
            Ok(children) => children,
            Err(e) => {
                warn!(
                    parent_session_id,
                    error = %e,
                    "failed to scan for child sessions; reporting no sidechains"
                );
                return Vec::new();
            }
        };
        let mut sidechains = Vec::with_capacity(children.len());
        for child in children {
            let facts = self.directory.facts_of(&child.to_string()).await;
            if matches!(facts.session_kind, history::entry::SessionKind::Sidechain) {
                sidechains.push(child);
            }
        }
        sidechains
    }

    /// Cancel (if live) and delete each of `children`, one `with_session_lock`
    /// critical section per child. Best-effort throughout — see
    /// `shutdown_session`'s doc comment.
    pub(in crate::session_pool) async fn delete_sidechain_list(
        &self,
        parent_session_id: &str,
        children: Vec<base::session::SessionId>,
    ) -> usize {
        let Some(store) = self.directory.store() else {
            return 0;
        };
        let mut deleted = 0;
        for child in children {
            let child_str = child.to_string();
            let ok = self
                .live
                .with_session_lock(&child_str, async {
                    if let Some(live) = self.live.remove(&child_str).await {
                        self.end_cut_off_turn(&child_str, &live, SessionEvent::Closed);
                        self.live.stop_removed(&child_str, &live, EndReason::Close);
                        live.core.reap_process_groups_later();
                    }
                    match store.delete(child).await {
                        Ok(()) => {
                            self.scheduler.remove_session(&child_str);
                            self.emit_event(api::DaemonEvent::SessionDeleted {
                                session_id: child_str.clone(),
                                parent_session_id: Some(parent_session_id.to_string()),
                            });
                            true
                        }
                        Err(e) => {
                            warn!(
                                session_id = %child_str,
                                parent_session_id,
                                error = %e,
                                "failed to delete sidechain during cascade; leaving for startup GC"
                            );
                            false
                        }
                    }
                })
                .await;
            if ok {
                deleted += 1;
            }
        }
        deleted
    }

    /// Remove `session_id` from the live map (if present) and cascade-delete
    /// `children` — the shared body of `shutdown_session` (which scans for
    /// its own children) and `delete_session` (which reuses a scan it
    /// already did for its response body).
    pub(in crate::session_pool) async fn shutdown_session_with_children(
        &self,
        session_id: &str,
        children: Vec<base::session::SessionId>,
    ) -> usize {
        if let Some(live) = self.live.remove(session_id).await {
            self.end_cut_off_turn(session_id, &live, SessionEvent::Closed);
            self.live.stop_removed(session_id, &live, EndReason::Close);
            info!(%session_id, "session removed");
            self.emit_event(api::DaemonEvent::SessionClosed {
                session_id: session_id.to_string(),
                reason: api::CloseReason::Closed,
            });
            // Its engine also writes them as it stops, but a caller that
            // reads the log as soon as this returns must find them there.
            self.record_goal_changes(session_id, live.core.state())
                .await;
            live.core.process_groups().reap(PROCESS_GROUP_GRACE).await;
        }
        // The in-memory copy only: a session resumed later reads its spec back
        // from its log.
        self.specs.forget(session_id);
        self.delete_sidechain_list(session_id, children).await
    }

    /// `session.close` — destroys the runtime entity; the parent's
    /// transcript stays on disk and remains resumable. This
    /// **cascades**: every sidechain of this session (sub-agent / team
    /// member) is deleted too — a closed parent has no path left to reach
    /// them (`session.list {parent_session_id}` only makes sense while the
    /// parent might still resume that context; once it's gone, so are they).
    /// Returns how many sidechains were
    /// actually deleted.
    ///
    /// A sidechain still mid-run (background / team member) is cancelled
    /// before its transcript is deleted, but this doesn't wait for that
    /// cancellation to finish landing on disk (N2's suggested grace window
    /// is intentionally not implemented here — it would make an interactive
    /// `session.close` call noticeably slower for what's normally a rare
    /// race). A delete that loses that race, or fails for any other reason,
    /// is logged and left for the startup GC to clean up later (N1) — it
    /// never fails `session.close` itself.
    ///
    /// Waits out a rebuild or resume of the same id in flight: either would
    /// otherwise put the session back in memory after this returned.
    pub async fn shutdown_session(&self, session_id: &str) -> Result<usize, PoolError> {
        self.refuse_scene_mismatch(session_id).await?;
        let children = self.fetch_child_sidechains(session_id).await;
        Ok(self
            .live
            .with_session_lock(session_id, async {
                self.shutdown_session_with_children(session_id, children)
                    .await
            })
            .await)
    }

    /// `session.delete` — irreversibly deletes the parent's own transcript
    /// too (not just the runtime entity `session.close` destroys), cascading
    /// sidechains the same way. `dry_run: true` reports what *would* be
    /// deleted (sidechain count) without deleting anything, so a host can
    /// show a confirmation dialog first.
    pub async fn delete_session(
        &self,
        session_id: &str,
        dry_run: bool,
    ) -> Result<api::DeleteReport, PoolError> {
        self.refuse_scene_mismatch(session_id).await?;
        let store = self.directory.require_store()?;
        let sid = base::session::SessionId::parse(session_id)
            .map_err(|_| PoolError::InvalidRequest(format!("invalid session_id: {session_id}")))?;

        // One scan shared by both the (possibly dry-run) report below and
        // the actual cascade delete — `shutdown_session_with_children`
        // takes this same `Vec` instead of re-scanning, so the two can't
        // disagree.
        let children = self.fetch_child_sidechains(session_id).await;
        let sidechain_ids: Vec<String> = children.iter().map(|id| id.to_string()).collect();

        if dry_run {
            return Ok(api::DeleteReport {
                session_id: session_id.to_string(),
                deleted: false,
                sidechains_deleted: sidechain_ids.len(),
                sidechain_ids,
            });
        }

        // Cascade and the primary delete share one `with_session_lock`
        // critical section — see that method's doc comment for the resume
        // race this closes.
        let (sidechains_deleted, delete_result) = self
            .live
            .with_session_lock(session_id, async {
                let deleted = self
                    .shutdown_session_with_children(session_id, children)
                    .await;
                let result = store.delete(sid).await;
                (deleted, result)
            })
            .await;
        delete_result.map_err(|e| PoolError::Internal(e.to_string()))?;
        let scratch =
            base::paths::session_scratch_dir(&self.bindings.paths().global_root(), session_id);
        let _ = std::fs::remove_dir_all(scratch);
        self.scheduler.remove_session(session_id);
        self.emit_event(api::DaemonEvent::SessionDeleted {
            session_id: session_id.to_string(),
            parent_session_id: None,
        });

        Ok(api::DeleteReport {
            session_id: session_id.to_string(),
            deleted: true,
            sidechains_deleted,
            sidechain_ids,
        })
    }

    /// 关闭所有 session，等每个 Agent 把手上的一轮收尾落盘再返回。From the
    /// moment this starts no session is created; a second call waits for the
    /// first to finish.
    ///
    /// Each session is taken out under its session lock, as `session.close`
    /// takes it: a rebuild or resume in flight finishes first, or, past the
    /// point it sees the shutdown, gives up without starting an engine.
    pub async fn shutdown_all(&self) {
        let Some(_done) = self.live.begin_shutdown() else {
            self.live.shut_down().await;
            return;
        };
        let ids = self.live.ids().await;
        let (exits, ledgers): (Vec<CancellationToken>, Vec<_>) =
            futures::future::join_all(ids.iter().map(|sid| {
                self.live.with_session_lock(sid, async move {
                    let live = self.live.remove(sid).await?;
                    self.end_cut_off_turn(sid, &live, SessionEvent::Closed);
                    live.engine.stop(EndReason::Shutdown);
                    live.core.hub().clear();
                    info!(%sid, "session removed (shutdown)");
                    Some((live.engine.exited, live.core.process_groups().clone()))
                })
            }))
            .await
            .into_iter()
            .flatten()
            .unzip();
        let deadline = tokio::time::Instant::now() + AGENT_EXIT_GRACE;
        let all = futures::future::join_all(exits.iter().map(|e| e.cancelled()));
        if tokio::time::timeout_at(deadline, all).await.is_err() {
            warn!("some sessions did not stop within the grace period");
        }
        self.factory.background_hooks().drain(deadline).await;
        futures::future::join_all(
            ledgers
                .iter()
                .map(|ledger| ledger.reap(PROCESS_GROUP_GRACE)),
        )
        .await;
    }

    /// Stops the Agent running under `session_id`, if any, and waits for it
    /// to finish — so whatever reads its transcript next reads all of it.
    ///
    /// One that does not stop in time is cancelled all the same, so its
    /// entry is taken out of memory with it: left in the map, it would take
    /// the next turn into an engine that is no longer reading. Whoever brings
    /// the session back waits for that engine instead.
    pub(in crate::session_pool) async fn stop_live_agent(
        &self,
        session_id: &str,
    ) -> Result<(), CreateError> {
        let Some((exited, core, generation)) = self
            .live
            .with_live(session_id, |live| {
                live.engine.stop(EndReason::Rebuild);
                (
                    live.engine.exited.clone(),
                    live.core.clone(),
                    live.engine.generation,
                )
            })
            .await
        else {
            return Ok(());
        };
        if tokio::time::timeout(AGENT_EXIT_GRACE, exited.cancelled())
            .await
            .is_err()
        {
            warn!(%session_id, "previous agent did not stop within the grace period");
            let stopping = self
                .live
                .remove_if(session_id, |live| live.runs(&core, generation))
                .await;
            if let Some(stopping) = stopping {
                self.evict(session_id, &stopping);
                stopping.turn.idle.notify_waiters();
                self.emit_event(api::DaemonEvent::SessionEvicted {
                    session_id: session_id.to_string(),
                    reason: api::EvictReason::Rebuild,
                });
            }
            return Err(CreateError::previous_engine_running(session_id));
        }
        Ok(())
    }
}
