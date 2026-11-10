//! 会话的订阅者与待答提示：订阅/退订、广播、中断、`respond_to_prompt`。

use super::*;

impl SessionPool {
    /// Record a raised ask so late subscribers can be handed it.
    pub(super) async fn remember_prompt<T: serde::Serialize>(
        &self,
        session_id: &str,
        (prompt_id, task_id): &Ask,
        frame: &T,
    ) {
        let Ok(frame) = serde_json::to_value(frame) else {
            return;
        };
        let mut sessions = self.sessions.lock().await;
        if let Some(live) = sessions.get_mut(session_id) {
            live.pending_prompts.insert(
                prompt_id.clone(),
                PendingPrompt {
                    frame,
                    task_id: task_id.clone(),
                },
            );
        }
    }

    /// Forget an ask: answered, or the turn that raised it ended.
    pub(super) async fn forget_prompt(&self, session_id: &str, prompt_id: &str) {
        let mut sessions = self.sessions.lock().await;
        if let Some(live) = sessions.get_mut(session_id) {
            live.pending_prompts.remove(prompt_id);
        }
    }

    /// Forget the asks of a background task that ended.
    pub(super) async fn forget_prompts_of_task(&self, session_id: &str, task_id: &str) {
        let mut sessions = self.sessions.lock().await;
        if let Some(live) = sessions.get_mut(session_id) {
            live.pending_prompts
                .retain(|_, prompt| prompt.task_id.as_deref() != Some(task_id));
        }
    }

    /// Register `client` as a watcher of `session_id`.
    ///
    /// Returns the transcript watermark and any unanswered permission asks,
    /// which together are what a newly opened tab needs to catch up:
    /// subscribe first, then read history up to `last_seq`, then apply the
    /// frames that arrived meanwhile. Reading first and subscribing second
    /// loses everything produced in between — the reason this returns a
    /// watermark at all.
    pub async fn subscribe_session(
        &self,
        session_id: &str,
        client: Arc<crate::rpc::Client>,
    ) -> Result<serde_json::Value, RpcError> {
        let pending: Vec<serde_json::Value> = {
            let mut sessions = self.sessions.lock().await;
            let Some(live) = sessions.get_mut(session_id) else {
                return Err((
                    codes::SESSION_NOT_FOUND,
                    format!("session not found: {session_id}"),
                ));
            };
            live.subscribers.insert(client.id(), client);
            live.pending_prompts
                .values()
                .map(|prompt| prompt.frame.clone())
                .collect()
        };

        Ok(serde_json::json!({
            "session_id": session_id,
            "last_seq": self.transcript_len(session_id).await,
            "pending_prompts": pending,
        }))
    }

    /// Stop sending this session's frames to `connection`.
    pub async fn unsubscribe_session(
        &self,
        session_id: &str,
        connection: crate::rpc::ConnectionId,
    ) {
        let mut sessions = self.sessions.lock().await;
        if let Some(live) = sessions.get_mut(session_id) {
            live.subscribers.remove(&connection);
        }
    }

    /// A connection went away: forget it everywhere.
    ///
    /// That is all a disconnect means: the turn keeps running, because
    /// another connection may be watching and a teardown would cascade into
    /// deleting the session's sidechain transcripts.
    pub async fn drop_connection(&self, connection: crate::rpc::ConnectionId) {
        let mut sessions = self.sessions.lock().await;
        for live in sessions.values_mut() {
            live.subscribers.remove(&connection);
        }
    }

    /// How many messages this session's transcript holds — its watermark, in
    /// the same positions `session.history` pages by and `step_end` /
    /// `turn_complete` report as `seq`.
    pub(super) async fn transcript_len(&self, session_id: &str) -> usize {
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

    /// Send one frame to every connection watching `session_id`.
    ///
    /// A subscriber that cannot take the frame — gone, or fallen far enough
    /// behind to fill its queue — is dropped here rather than waited on. One
    /// tab that stopped reading must not hold up the turn or the other tabs
    /// watching it. A subscriber dropped for falling behind is told so
    /// (`subscription_dropped`) and can subscribe again and catch up through
    /// `last_seq`, which is what makes dropping an acceptable answer.
    pub(super) async fn broadcast_to_session<T: serde::Serialize>(
        &self,
        session_id: &str,
        frame: &T,
    ) {
        self.broadcast_to_session_except(session_id, frame, None)
            .await;
    }

    /// [`Self::broadcast_to_session`], skipping one connection — the caller
    /// that is about to be told the same thing in its own response.
    pub(super) async fn broadcast_to_session_except<T: serde::Serialize>(
        &self,
        session_id: &str,
        frame: &T,
        except: Option<crate::rpc::ConnectionId>,
    ) {
        let json = match serde_json::to_string(frame) {
            Ok(json) => json,
            Err(e) => {
                tracing::warn!(error = %e, "could not serialize a session frame; dropping it");
                return;
            }
        };
        let mut sessions = self.sessions.lock().await;
        let Some(live) = sessions.get_mut(session_id) else {
            return;
        };
        live.last_active = Instant::now();
        live.subscribers.retain(|id, client| {
            Some(*id) == except || client.try_send_for(session_id, json.clone())
        });
    }

    /// Send the running turn's last frame, if `slot` is still the session's
    /// turn slot. Marked under the same lock `session.close` takes the
    /// session out with, so every watcher gets exactly one: this one, or the
    /// one the close sends ([`Self::end_cut_off_turn`]). Returns whether the
    /// session was still there to send it to.
    pub(super) async fn broadcast_terminal<T: serde::Serialize>(
        &self,
        session_id: &str,
        slot: &Arc<AsyncMutex<Option<EventReceiver>>>,
        frame: &T,
        except: Option<crate::rpc::ConnectionId>,
    ) -> bool {
        let json = serde_json::to_string(frame)
            .inspect_err(
                |e| tracing::warn!(error = %e, "could not serialize a session frame; dropping it"),
            )
            .ok();
        let mut sessions = self.sessions.lock().await;
        let Some(live) = sessions
            .get_mut(session_id)
            .filter(|live| Arc::ptr_eq(&live.event_rx, slot))
        else {
            return false;
        };
        live.turn_terminated = true;
        live.last_active = Instant::now();
        if let Some(json) = json {
            live.subscribers.retain(|id, client| {
                Some(*id) == except || client.try_send_for(session_id, json.clone())
            });
        }
        true
    }

    /// `live`, just taken out of the map, was running a turn whose last
    /// frame has not gone out: send its watchers an `error` frame in its
    /// place. Its initiator learns the same from its `run_turn` response.
    pub(super) fn end_cut_off_turn(&self, session_id: &str, live: &LiveSession) {
        let Some(turn_id) = live.current_turn.as_deref() else {
            return;
        };
        if live.turn_terminated {
            return;
        }
        let frame = StreamFrame::event(
            session_id,
            turn_id,
            serde_json::json!({
                "kind": "error",
                "code": SESSION_CLOSED,
                "message": SESSION_CLOSED_MESSAGE,
            }),
        );
        let Ok(json) = serde_json::to_string(&frame) else {
            return;
        };
        for (id, client) in &live.subscribers {
            if Some(*id) != live.turn_connection {
                client.try_send_for(session_id, json.clone());
            }
        }
    }

    /// The turn currently running on `session_id`, if any. `session.get`
    /// reports it as `turn_state`; a missing or unknown session reads as idle,
    /// same as one sitting between turns.
    pub async fn current_turn_of(&self, session_id: &str) -> Option<String> {
        self.sessions
            .lock()
            .await
            .get(session_id)
            .and_then(|live| live.current_turn.clone())
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
    pub async fn interrupt_session(&self, session_id: &str) -> Result<bool, RpcError> {
        let (input_tx, running, state) = {
            let sessions = self.sessions.lock().await;
            let Some(live) = sessions.get(session_id) else {
                return Err((
                    codes::SESSION_NOT_FOUND,
                    format!("session not found: {session_id}"),
                ));
            };
            (
                live.input_tx.clone(),
                live.current_turn.clone(),
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
    ) -> Result<(), RpcError> {
        let input_tx = {
            let sessions = self.sessions.lock().await;
            let Some(live) = sessions.get(session_id) else {
                return Err((
                    codes::SESSION_NOT_FOUND,
                    format!("session not found: {session_id}"),
                ));
            };
            let asked = live.pending_prompts.get(&prompt_id).and_then(|prompt| {
                prompt
                    .frame
                    .pointer("/params/event/prompt_type")
                    .and_then(|t| t.as_str())
                    .map(str::to_string)
            });
            if let Some(asked) = asked.filter(|asked| asked != answer.prompt_type()) {
                return Err((
                    codes::INVALID_PARAMS,
                    format!(
                        "prompt `{prompt_id}` is a {asked} prompt, not a {}",
                        answer.prompt_type()
                    ),
                ));
            }
            live.input_tx.clone()
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
        self.forget_prompt(session_id, &prompt_id).await;
        Ok(())
    }
}
