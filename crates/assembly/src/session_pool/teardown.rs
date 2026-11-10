//! 会话收尾：按会话加锁、驱逐、停止、删除、带子会话的关闭，以及整池 `shutdown_all`。

use super::*;

impl SessionPool {
    /// Forget a session's `session.setModel` choice — it is deleted or
    /// closed, and an id is never reused.
    pub(super) fn forget_spec(&self, session_id: &str) {
        self.lock_specs().remove(session_id);
    }

    /// Stop `live`, just taken out of `sessions` under `sid`, without
    /// waiting for it: whoever brings `sid` back waits instead
    /// ([`Self::wait_until_stopped`]). Nothing it still produces reaches its
    /// watchers.
    pub(super) fn stop_removed(&self, sid: &str, live: &LiveSession) {
        live.cancel.cancel();
        live.hub.clear();
        let mut stopping = self
            .stopping
            .lock()
            .expect("stopping is never held across a panic");
        stopping.retain(|_, exited| !exited.is_cancelled());
        stopping.insert(sid.to_string(), live.exited.clone());
    }

    /// Wait, within [`AGENT_EXIT_GRACE`], for the engine `sid` last ran on
    /// to stop, if it has not.
    pub(super) async fn wait_until_stopped(&self, sid: &str) -> Result<(), CreateError> {
        let exited = self
            .stopping
            .lock()
            .expect("stopping is never held across a panic")
            .get(sid)
            .cloned();
        let Some(exited) = exited else {
            return Ok(());
        };
        if tokio::time::timeout(AGENT_EXIT_GRACE, exited.cancelled())
            .await
            .is_err()
        {
            warn!(%sid, "the session's previous engine did not stop within the grace period");
            return Err(CreateError::previous_engine_running(sid));
        }
        Ok(())
    }

    /// Take `live` out of memory as [`Self::stop_removed`] does, for a
    /// session that may be brought back.
    pub(super) fn evict(&self, sid: &str, live: &LiveSession) {
        self.stop_removed(sid, live);
        reap_process_groups_later(live);
        self.release_spec(sid);
    }

    /// Drop the spec of `sid`, now out of memory, when its log gives it
    /// back; one the log cannot — a recorder's progress, a session with no
    /// log — is kept, for at most `cap` sessions.
    pub(super) fn release_spec(&self, sid: &str) {
        let logged = self.history_store.is_some() && base::session::SessionId::parse(sid).is_ok();
        let records = self
            .lock_specs()
            .get(sid)
            .is_some_and(|spec| spec.recorder.is_some());
        if logged && !records {
            self.forget_spec(sid);
            return;
        }
        let oldest = {
            let mut kept = self.lock_kept_specs();
            kept.retain(|k| k != sid);
            kept.push_back(sid.to_string());
            if kept.len() > self.cap {
                kept.pop_front()
            } else {
                None
            }
        };
        if let Some(oldest) = oldest {
            self.forget_spec(&oldest);
        }
    }

    pub(super) fn lock_kept_specs(
        &self,
    ) -> std::sync::MutexGuard<'_, std::collections::VecDeque<String>> {
        self.kept_specs
            .lock()
            .expect("kept_specs is never held across a panic")
    }

    /// Run `fut` while holding this pool's per-`session_id` lock — see the
    /// `session_locks` field doc comment. Closes the race between a
    /// sidechain's cascade delete (`delete_sidechain_list`, `delete_session`)
    /// and a concurrent `session.resume(that_same_id, create_if_missing:
    /// true)`: whichever side gets here first runs to completion before
    /// the other starts, so a resume can't observe the id mid-cascade (files
    /// half gone) and a completed cascade can't delete out from under a
    /// resume that just finished recreating the id. It does *not* stop the
    /// id from being reused afterward — recreating a just-deleted sidechain
    /// as a fresh orphan session is still possible; the two operations
    /// serialize instead of interleaving.
    pub(super) async fn with_session_lock<T>(
        &self,
        session_id: &str,
        fut: impl std::future::Future<Output = T>,
    ) -> T {
        let lock = {
            let mut locks = self.session_locks.lock().await;
            locks
                .entry(session_id.to_string())
                .or_insert_with(|| Arc::new(AsyncMutex::new(())))
                .clone()
        };
        let result = {
            let _guard = lock.lock().await;
            fut.await
        };
        drop(lock);
        let mut locks = self.session_locks.lock().await;
        if locks
            .get(session_id)
            .is_some_and(|entry| Arc::strong_count(entry) == 1)
        {
            locks.remove(session_id);
        }
        result
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
    pub(super) async fn fetch_child_sidechains(
        &self,
        parent_session_id: &str,
    ) -> Vec<base::session::SessionId> {
        let Some(ref store) = self.history_store else {
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
            let facts = self.facts_of(&child.to_string()).await;
            if matches!(facts.session_kind, history::entry::SessionKind::Sidechain) {
                sidechains.push(child);
            }
        }
        sidechains
    }

    /// Cancel (if live) and delete each of `children`, one `with_session_lock`
    /// critical section per child. Best-effort throughout — see
    /// `shutdown_session`'s doc comment.
    pub(super) async fn delete_sidechain_list(
        &self,
        parent_session_id: &str,
        children: Vec<base::session::SessionId>,
    ) -> usize {
        let Some(ref store) = self.history_store else {
            return 0;
        };
        let mut deleted = 0;
        for child in children {
            let child_str = child.to_string();
            let ok = self
                .with_session_lock(&child_str, async {
                    {
                        let mut sessions = self.sessions.lock().await;
                        if let Some(live) = sessions.remove(&child_str) {
                            self.end_cut_off_turn(&child_str, &live);
                            self.stop_removed(&child_str, &live);
                            reap_process_groups_later(&live);
                        }
                    }
                    match store.delete(child).await {
                        Ok(()) => {
                            self.schedule.remove_session(&child_str);
                            self.emit_session_event(
                                "session_deleted",
                                &child_str,
                                serde_json::json!({"parent_session_id": parent_session_id}),
                            );
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
    pub(super) async fn shutdown_session_with_children(
        &self,
        session_id: &str,
        children: Vec<base::session::SessionId>,
    ) -> usize {
        let removed = {
            let mut sessions = self.sessions.lock().await;
            sessions.remove(session_id).map(|live| {
                self.end_cut_off_turn(session_id, &live);
                self.stop_removed(session_id, &live);
                info!(%session_id, "session removed");
                self.emit_session_event(
                    "session_closed",
                    session_id,
                    serde_json::json!({"reason": "closed"}),
                );
                (live.state, live.process_groups)
            })
        };
        let (state, process_groups) = removed.unwrap_or_default();
        // Its engine also writes them as it stops, but a caller that reads
        // the log as soon as this returns must find them there.
        if let Some(state) = state {
            self.record_goal_changes(session_id, &state).await;
        }
        if let Some(ledger) = process_groups {
            ledger.reap(PROCESS_GROUP_GRACE).await;
        }
        // The in-memory copy only: a session resumed later reads its spec back
        // from its log.
        self.forget_spec(session_id);
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
    pub async fn shutdown_session(&self, session_id: &str) -> Result<usize, PoolError> {
        self.refuse_scene_mismatch(session_id).await?;
        let children = self.fetch_child_sidechains(session_id).await;
        Ok(self
            .shutdown_session_with_children(session_id, children)
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
    ) -> Result<serde_json::Value, PoolError> {
        self.refuse_scene_mismatch(session_id).await?;
        let store = self.require_history_store()?;
        let sid = base::session::SessionId::parse(session_id)
            .map_err(|_| PoolError::InvalidRequest(format!("invalid session_id: {session_id}")))?;

        // One scan shared by both the (possibly dry-run) report below and
        // the actual cascade delete — `shutdown_session_with_children`
        // takes this same `Vec` instead of re-scanning, so the two can't
        // disagree.
        let children = self.fetch_child_sidechains(session_id).await;
        let sidechain_ids: Vec<String> = children.iter().map(|id| id.to_string()).collect();

        if dry_run {
            return Ok(serde_json::json!({
                "session_id": session_id,
                "deleted": false,
                "sidechains_deleted": sidechain_ids.len(),
                "sidechain_ids": sidechain_ids,
            }));
        }

        // Cascade and the primary delete share one `with_session_lock`
        // critical section — see that method's doc comment for the resume
        // race this closes.
        let (sidechains_deleted, delete_result) = self
            .with_session_lock(session_id, async {
                let deleted = self
                    .shutdown_session_with_children(session_id, children)
                    .await;
                let result = store.delete(sid).await;
                (deleted, result)
            })
            .await;
        delete_result.map_err(|e| PoolError::Internal(e.to_string()))?;
        let scratch = base::paths::session_scratch_dir(&self.paths.global_root(), session_id);
        let _ = std::fs::remove_dir_all(scratch);
        self.schedule.remove_session(session_id);
        self.emit_session_event("session_deleted", session_id, serde_json::json!({}));

        Ok(serde_json::json!({
            "session_id": session_id,
            "deleted": true,
            "sidechains_deleted": sidechains_deleted,
            "sidechain_ids": sidechain_ids,
        }))
    }

    /// 关闭所有 session，等每个 Agent 把手上的一轮收尾落盘再返回。From the
    /// moment this starts no session is created; a second call waits for the
    /// first to finish.
    pub async fn shutdown_all(&self) {
        if self
            .shutting_down
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            self.shut_down.cancelled().await;
            return;
        }
        let _done = self.shut_down.clone().drop_guard();
        let (exits, ledgers): (Vec<CancellationToken>, Vec<_>) = {
            let mut sessions = self.sessions.lock().await;
            sessions
                .drain()
                .map(|(sid, live)| {
                    self.end_cut_off_turn(&sid, &live);
                    live.cancel.cancel();
                    live.hub.clear();
                    info!(%sid, "session removed (shutdown)");
                    (live.exited, live.process_groups)
                })
                .unzip()
        };
        let all = futures::future::join_all(exits.iter().map(|e| e.cancelled()));
        if tokio::time::timeout(AGENT_EXIT_GRACE, all).await.is_err() {
            warn!("some sessions did not stop within the grace period");
        }
        futures::future::join_all(
            ledgers
                .iter()
                .flatten()
                .map(|ledger| ledger.reap(PROCESS_GROUP_GRACE)),
        )
        .await;
    }

    pub(super) fn refuse_while_shutting_down(&self) -> Result<(), String> {
        if self.shutting_down.load(std::sync::atomic::Ordering::SeqCst) {
            return Err("the daemon is shutting down".to_string());
        }
        Ok(())
    }

    /// Stops the Agent running under `session_id`, if any, and waits for it
    /// to finish — so whatever reads its transcript next reads all of it.
    ///
    /// One that does not stop in time is cancelled all the same, so its
    /// entry is taken out of memory with it: left in the map, it would take
    /// the next turn into an engine that is no longer reading. Whoever brings
    /// the session back waits for that engine instead.
    pub(super) async fn stop_live_agent(&self, session_id: &str) -> Result<(), CreateError> {
        let (exited, generation) = {
            let sessions = self.sessions.lock().await;
            let Some(live) = sessions.get(session_id) else {
                return Ok(());
            };
            live.cancel.cancel();
            (live.exited.clone(), live.turn.generation)
        };
        if tokio::time::timeout(AGENT_EXIT_GRACE, exited.cancelled())
            .await
            .is_err()
        {
            warn!(%session_id, "previous agent did not stop within the grace period");
            let stopping = {
                let mut sessions = self.sessions.lock().await;
                let ours = sessions
                    .get(session_id)
                    .is_some_and(|live| live.turn.generation == generation);
                if ours {
                    sessions.remove(session_id)
                } else {
                    None
                }
            };
            if let Some(stopping) = stopping {
                self.evict(session_id, &stopping);
                stopping.turn.idle.notify_waiters();
                self.emit_session_event(
                    "session_evicted",
                    session_id,
                    serde_json::json!({"reason": "rebuild"}),
                );
            }
            return Err(CreateError::previous_engine_running(session_id));
        }
        Ok(())
    }
}
