//! 会话的订阅者与待答提示：订阅/退订、本轮终止帧、中断、`respond_to_prompt`。

use crate::session_pool::*;

impl SessionPool {
    /// The hub of live `session_id`.
    pub(in crate::session_pool) async fn hub_of(
        &self,
        session_id: &str,
    ) -> Option<Arc<SessionHub>> {
        self.sessions
            .lock()
            .await
            .get(session_id)
            .map(|live| live.hub.clone())
    }

    /// Hand `event`, which belongs to no turn, to whoever watches
    /// `session_id`.
    pub(in crate::session_pool) async fn publish_between_turns(
        &self,
        session_id: &str,
        event: SessionEvent,
    ) {
        if let Some(hub) = self.hub_of(session_id).await {
            hub.publish(&frame_between_turns(session_id, event));
        }
    }

    /// Register `watcher` as a watcher of `session_id`.
    ///
    /// Returns the transcript watermark and any unanswered asks, which
    /// together are what a newly opened tab needs to catch up: subscribe
    /// first, then read history up to `last_seq`, then apply the frames that
    /// arrived meanwhile. Reading first and subscribing second loses
    /// everything produced in between — the reason this returns a watermark
    /// at all.
    pub async fn subscribe_session(
        &self,
        session_id: &str,
        watcher: Arc<dyn SessionWatcher>,
    ) -> Result<Subscribed, PoolError> {
        self.refuse_scene_mismatch(session_id).await?;
        let hub = self
            .hub_of(session_id)
            .await
            .ok_or_else(|| PoolError::session_not_found(session_id))?;
        let pending = hub.subscribe(watcher);
        Ok(Subscribed {
            session_id: session_id.to_string(),
            last_seq: self.transcript_len(session_id).await,
            pending,
            resumed: false,
            scene_inferred: None,
        })
    }

    /// [`Self::subscribe_session`], resuming `session_id` first when it is
    /// not in memory — with `session.resume`'s refusals, and no `options`.
    ///
    /// Not what a plain subscribe does: resuming reads the transcript and
    /// takes a slot in a capped pool, and a host drawing a session list
    /// subscribes to rows it is only looking at.
    pub async fn subscribe_resuming(
        &self,
        session_id: &str,
        watcher: Arc<dyn SessionWatcher>,
    ) -> Result<Subscribed, PoolError> {
        match self.subscribe_session(session_id, watcher.clone()).await {
            Err(PoolError::SessionNotFound { .. }) => {}
            subscribed => return subscribed,
        }
        let resumed = self.resume_session(session_id, false, None).await?;
        let subscribed = self.subscribe_session(session_id, watcher).await?;
        Ok(Subscribed {
            resumed: true,
            scene_inferred: Some(resumed.scene_inferred),
            ..subscribed
        })
    }

    /// Stop handing this session's frames to `watcher`.
    pub async fn unsubscribe_session(&self, session_id: &str, watcher: WatcherId) {
        if let Some(hub) = self.hub_of(session_id).await {
            hub.unsubscribe(watcher);
        }
    }

    /// A watcher went away — a connection closed: forget it everywhere.
    ///
    /// That is all a disconnect means: the turn keeps running, because
    /// another connection may be watching and a teardown would cascade into
    /// deleting the session's sidechain transcripts.
    pub async fn drop_watcher(&self, watcher: WatcherId) {
        let sessions = self.sessions.lock().await;
        for live in sessions.values() {
            live.hub.unsubscribe(watcher);
        }
    }

    /// How many messages this session's transcript holds — its watermark, in
    /// the same positions `session.history` pages by and `step_end` /
    /// `turn_complete` report as `seq`.
    pub(in crate::session_pool) async fn transcript_len(&self, session_id: &str) -> usize {
        let Ok(store) = self.require_history_store() else {
            return 0;
        };
        self.load_entries(session_id)
            .await
            .map(|entries| {
                history::transcript::projected_lengths_with(&entries, store.projection())
                    .last()
                    .copied()
                    .unwrap_or(0)
            })
            .unwrap_or(0)
    }

    /// Send the running turn's last frame, if the ticket's entry is still the
    /// session's. Marked under the same lock `session.close` takes the
    /// session out with, so every watcher gets exactly one: this one, or the
    /// one the close sends ([`Self::end_cut_off_turn`]). Returns whether the
    /// session was still there to send it to.
    pub(in crate::session_pool) async fn publish_terminal(
        &self,
        ticket: &TurnTicket,
        frame: &SessionFrame,
        except: Option<WatcherId>,
    ) -> bool {
        let mut sessions = self.sessions.lock().await;
        let Some(live) = sessions
            .get_mut(&ticket.sid)
            .filter(|live| live.turn.generation == ticket.generation)
        else {
            return false;
        };
        live.turn.mark_terminated(ticket.generation);
        live.hub.publish_except(frame, except);
        true
    }

    /// `live`, just taken out of the map, was running a turn whose last
    /// frame has not gone out: send its watchers an `error` frame in its
    /// place. Its initiator learns the same from its `run_turn` result.
    pub(in crate::session_pool) fn end_cut_off_turn(&self, session_id: &str, live: &LiveSession) {
        let Some(turn) = live.turn.running().filter(|turn| !turn.terminated) else {
            return;
        };
        let frame = SessionFrame {
            session_id: Arc::from(session_id),
            turn_id: Some(Arc::from(turn.turn_id.as_str())),
            event: SessionEvent::Closed,
        };
        live.hub.publish_except(&frame, turn.origin);
    }

    /// The turn currently running on `session_id`, if any. `session.get`
    /// reports it as `turn_state`; a missing or unknown session reads as idle,
    /// same as one sitting between turns.
    pub async fn current_turn_of(&self, session_id: &str) -> Option<String> {
        self.sessions
            .lock()
            .await
            .get(session_id)
            .and_then(|live| live.turn.current_turn_id())
    }

    /// Cancel the in-flight turn, keeping the session.
    ///
    /// `EngineCommand::CancelTurn` cancels the turn's own child token, not
    /// the session's — the agent loop keeps running and the next `run_turn`
    /// is served normally. It names the turn, so it lands on that one even
    /// when its message has not reached the engine yet, and on nothing when
    /// it has already ended. Disarming the goal is what stops a goal round
    /// from following a turn that has ended but not yet given its slot back.
    /// Returns whether there was a turn to interrupt; there being none is an
    /// answer, not an error.
    pub async fn interrupt_session(&self, session_id: &str) -> Result<bool, PoolError> {
        self.refuse_scene_mismatch(session_id).await?;
        self.interrupt_turn(session_id).await
    }

    pub(in crate::session_pool) async fn interrupt_turn(
        &self,
        session_id: &str,
    ) -> Result<bool, PoolError> {
        let (input_tx, running, state) = {
            let sessions = self.sessions.lock().await;
            let Some(live) = sessions.get(session_id) else {
                return Err(PoolError::session_not_found(session_id));
            };
            (
                live.input_tx.clone(),
                live.turn.current_turn_id(),
                live.state.clone(),
            )
        };
        let Some(turn_id) = running else {
            return Ok(false);
        };
        if let Some(state) = state {
            state.goal_disarm();
        }
        let _ = input_tx.send(InputMessage::System {
            kind: EngineCommand::CancelTurn {
                turn_id: Some(turn_id),
            },
            content: String::new(),
        });
        Ok(true)
    }

    /// `session.respondToPrompt` — deliver the host's answer to a pending
    /// `prompt` frame (see `run_turn`'s event loop). Unlike `shutdown_session`,
    /// an unknown `session_id` here is a real caller error (answering a prompt
    /// for a session that isn't running can't silently no-op the way closing
    /// an already-closed session can). An answer of the other `prompt_type`
    /// than the open prompt it names is refused rather than dropped, so a
    /// client that confused the two learns of it.
    pub async fn respond_to_prompt(
        &self,
        session_id: &str,
        prompt_id: String,
        answer: PromptAnswer,
    ) -> Result<(), PoolError> {
        let (input_tx, hub) = {
            let sessions = self.sessions.lock().await;
            let Some(live) = sessions.get(session_id) else {
                return Err(PoolError::session_not_found(session_id));
            };
            if let Some(asked) = live
                .hub
                .ask_kind(&prompt_id)
                .filter(|asked| *asked != answer.kind())
            {
                return Err(PoolError::InvalidRequest(format!(
                    "prompt `{prompt_id}` is a {} prompt, not a {}",
                    asked.as_str(),
                    answer.kind().as_str()
                )));
            }
            (live.input_tx.clone(), live.hub.clone())
        };

        // Straight to the engine, whose input demultiplexer wakes the blocked
        // tool call. A `prompt_id` nobody is waiting on (already answered,
        // timed out, stale) is a silent no-op — the documented behaviour.
        let _ = input_tx.send(match answer {
            PromptAnswer::Permission(decision) => InputMessage::PermissionResponse {
                prompt_id: prompt_id.clone(),
                decision,
            },
            PromptAnswer::Question(answer) => InputMessage::QuestionResponse {
                prompt_id: prompt_id.clone(),
                answer,
            },
        });
        hub.forget_ask(&prompt_id);
        Ok(())
    }
}
