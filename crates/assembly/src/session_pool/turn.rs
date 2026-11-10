//! 回合执行：`run_turn` 及其回合槽位、自动命名的采纳。

use super::*;

impl SessionPool {
    /// Give the turn slot `slot` back and mark the session idle again, if
    /// `slot` is still the session's — a session closed or replaced while the
    /// turn ran is not this turn's to touch. Done under the sessions lock, so
    /// nothing sees the slot free while `turn_id` still reads as running.
    /// Whatever the turn's own engine was asking died with the turn;
    /// a background task's asks stay. Returns whether `slot` was the
    /// session's.
    pub(super) async fn release_turn_slot(
        &self,
        sid: &str,
        slot: &Arc<AsyncMutex<Option<EventReceiver>>>,
        event_rx: EventReceiver,
        turn_id: &str,
    ) -> bool {
        let mut sessions = self.sessions.lock().await;
        *slot.lock().await = Some(event_rx);
        let Some(live) = sessions
            .get_mut(sid)
            .filter(|live| Arc::ptr_eq(&live.event_rx, slot))
        else {
            return false;
        };
        if live.current_turn.as_deref() == Some(turn_id) {
            live.current_turn = None;
            live.turn_connection = None;
            live.turn_terminated = false;
            self.emit_session_event(
                "turn_completed",
                sid,
                serde_json::json!({
                    "turn_id": turn_id,
                    "initiator": live.turn_initiator.take(),
                }),
            );
            live.pending_prompts
                .retain(|_, prompt| prompt.task_id.is_some());
        }
        live.idle.notify_waiters();
        true
    }

    /// 执行一个 turn：发送消息 → 流式返回事件 → 返回结果。
    /// session_id 为 None 时自动创建新 session。
    #[allow(clippy::too_many_arguments)]
    pub async fn run_turn(
        &self,
        session_id: Option<String>,
        message: String,
        attachments: Vec<runtime::agent::Attachment>,
        turn_id: String,
        client: Arc<crate::rpc::Client>,
        options: Option<SessionOptions>,
        on_busy: OnBusy,
        initiator: &'static str,
    ) -> Result<serde_json::Value, PoolError> {
        let sid = match session_id {
            Some(ref sid) => {
                // An unknown id still creates a session: its scene reads as
                // inferred. The daemon's own rounds go to sessions it already
                // serves.
                if initiator == "client" {
                    self.refuse_scene_mismatch(sid).await?;
                }
                // ① Not in memory: resume or create. ② In memory on a binding
                // that is still current: use it. ③ In memory on a binding a
                // settings change invalidated: rebuild before this message, so
                // it is served on the new configuration.
                //
                // ③ only rebuilds an idle session. A rebuild replaces the
                // Agent, and replacing one mid-turn would cut that turn off,
                // as it would the background tasks running under it; a busy
                // session is served on the old configuration this once and
                // checked again next time — the flag stays set, so nothing
                // is missed.
                let stale = {
                    let sessions = self.sessions.lock().await;
                    sessions.get(sid).map(|live| {
                        live.binding_stale
                            .load(std::sync::atomic::Ordering::Relaxed)
                    })
                };
                let resolved = match stale {
                    None => self.ensure_live(sid, options.as_ref()).await,
                    Some(false) => Ok(sid.clone()),
                    Some(true) => match self.rebuild_idle(sid, options.as_ref()).await {
                        Ok(sid) => Ok(sid),
                        Err(Rebuild::Busy { .. }) => Ok(sid.clone()),
                        Err(Rebuild::NotLive) => self.ensure_live(sid, options.as_ref()).await,
                        Err(Rebuild::Failed(e)) => Err(e),
                    },
                };
                resolved?
            }
            None => {
                let sid = Id::new().to_string();
                self.create(
                    sid.clone(),
                    self.scene.clone(),
                    self.default_binding().await,
                    Some(self.cwd.clone()),
                    options.as_ref(),
                    false,
                )
                .await?
            }
        };

        // 取走 `event_rx` 就是"一个会话同一时刻只能有一个 turn"的实现，而且
        // 必须**在发消息之前**取：消息一旦 send 进去就归正在跑的那个 Agent
        // 消费了，先发后取的话，调用方看到忙，消息却已经被别的 turn 吃掉。
        let mut steer_tried = false;
        let (input_tx, event_rx_mutex, mut event_rx) = loop {
            let (input_tx, event_rx_mutex, busy_with, idle) = {
                let mut sessions = self.sessions.lock().await;
                let Some(live) = sessions.get_mut(&sid) else {
                    return Err(PoolError::session_not_found(&sid));
                };
                live.last_active = Instant::now();
                (
                    live.input_tx.clone(),
                    live.event_rx.clone(),
                    live.current_turn.clone(),
                    live.idle.clone(),
                )
            };
            let became_idle = idle.notified();
            tokio::pin!(became_idle);
            became_idle.as_mut().enable();
            let taken = event_rx_mutex.lock().await.take();
            if let Some(rx) = taken {
                let still_this_session = self
                    .sessions
                    .lock()
                    .await
                    .get(&sid)
                    .is_some_and(|live| Arc::ptr_eq(&live.event_rx, &event_rx_mutex));
                if still_this_session {
                    break (input_tx, event_rx_mutex, rx);
                }
                *event_rx_mutex.lock().await = Some(rx);
                continue;
            }
            match on_busy {
                OnBusy::Reject => {
                    return Err(PoolError::SessionBusy(Busy::turn_running(&sid, busy_with)));
                }
                OnBusy::Steer if !steer_tried && attachments.is_empty() => {
                    steer_tried = true;
                    let (ack, acked) = tokio::sync::oneshot::channel();
                    let _ = input_tx.send(InputMessage::Steer {
                        content: message.clone(),
                        message_id: turn_id.clone(),
                        ack: Some(ack),
                    });
                    if let Ok(Ok(runtime::steering::SteerAck::Accepted)) =
                        tokio::time::timeout(Duration::from_secs(5), acked).await
                    {
                        if let Some(live) = self.sessions.lock().await.get_mut(&sid) {
                            live.subscribers.insert(client.id(), client.clone());
                        }
                        return Ok(serde_json::json!({
                            "session_id": sid,
                            "turn_id": turn_id,
                            "delivery": "steer",
                            "into_turn_id": busy_with,
                        }));
                    }
                    // The running turn is already past its last step: this
                    // message waits for the next turn instead.
                    continue;
                }
                OnBusy::Steer | OnBusy::Queue => {}
            }
            // The timeout covers a session that was replaced or dropped while
            // we waited: its `idle` is never notified again.
            tokio::select! {
                _ = became_idle => {}
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
        };

        // A goal round runs only if it is still the one its goal handed out:
        // a pause, clear or change since then, or another turn's round, means
        // nobody wants it any more.
        let started = {
            let mut sessions = self.sessions.lock().await;
            match sessions
                .get_mut(&sid)
                .filter(|live| Arc::ptr_eq(&live.event_rx, &event_rx_mutex))
            {
                None => None,
                Some(live) => {
                    let wanted = initiator != "goal"
                        || live
                            .state
                            .as_ref()
                            .is_some_and(|state| state.goal_claim_round(&turn_id));
                    if wanted {
                        live.current_turn = Some(turn_id.clone());
                        live.turn_initiator = Some(initiator);
                        live.turn_connection = Some(client.id());
                        live.turn_terminated = false;
                        self.emit_session_event(
                            "turn_started",
                            &sid,
                            serde_json::json!({"turn_id": turn_id, "initiator": initiator}),
                        );
                        // Whoever starts a turn watches it. Not a special role
                        // — it lands in the same subscriber set as any tab that
                        // called `session.subscribe`, and leaves the same way.
                        live.subscribers.insert(client.id(), client.clone());
                    }
                    Some((wanted, live.state.clone()))
                }
            }
        };
        let turn_state = match started {
            Some((true, state)) => state,
            Some((false, _)) => {
                self.release_turn_slot(&sid, &event_rx_mutex, event_rx, &turn_id)
                    .await;
                return Ok(
                    serde_json::json!({"session_id": sid, "turn_id": turn_id, "skipped": true}),
                );
            }
            None => {
                *event_rx_mutex.lock().await = Some(event_rx);
                return Err(PoolError::session_not_found(&sid));
            }
        };
        if let (Some(state), "goal") = (&turn_state, initiator) {
            self.record_goal_changes(&sid, state).await;
        }

        // 发送用户消息
        let _ = input_tx.send(InputMessage::User {
            content: message.clone(),
            attachments,
            turn_id: turn_id.clone(),
        });

        let api_calls;
        let ended_with;
        loop {
            let event = event_rx.recv().await;
            if let Some((frame, ask)) = event
                .as_ref()
                .and_then(|event| background_frame(&sid, Some(&turn_id), event))
            {
                // Registered before it is sent: a tab that subscribes between
                // the two must still be handed the question.
                if let Some(ask) = &ask {
                    self.remember_prompt(&sid, ask, &frame).await;
                }
                self.broadcast_to_session(&sid, &frame).await;
                continue;
            }
            let Some(event) = event else {
                // The engine is gone. Dropping the entry is what lets the
                // next call on this id resume it from its transcript instead
                // of sending into a closed channel.
                let f = StreamFrame::event(
                    &sid,
                    &turn_id,
                    serde_json::json!({
                        "kind":"error","code":"engine_stopped",
                        "message":"engine stopped before the turn completed"
                    }),
                );
                self.broadcast_terminal(&sid, &event_rx_mutex, &f, Some(client.id()))
                    .await;
                let dead = {
                    let mut sessions = self.sessions.lock().await;
                    let ours = sessions
                        .get(&sid)
                        .is_some_and(|live| Arc::ptr_eq(&live.event_rx, &event_rx_mutex));
                    if ours {
                        sessions.remove(&sid)
                    } else {
                        None
                    }
                };
                if let Some(dead) = dead {
                    self.evict(&sid, &dead);
                    dead.idle.notify_waiters();
                    self.emit_session_event(
                        "session_closed",
                        &sid,
                        serde_json::json!({"reason": "engine_stopped"}),
                    );
                }
                warn!(%sid, "engine stopped before the turn completed");
                return Err(PoolError::EngineStopped);
            };
            let Some(body) = turn_event_body(&event) else {
                continue;
            };
            let f = StreamFrame::event(&sid, &turn_id, body);
            match event {
                AgentEvent::TurnComplete {
                    stop_reason,
                    api_calls: ac,
                    context,
                    ..
                } => {
                    api_calls = ac;
                    ended_with = stop_reason;
                    if context.is_some() {
                        if let Some(live) = self.sessions.lock().await.get_mut(&sid) {
                            live.context = context;
                        }
                    }
                    if !self
                        .broadcast_terminal(&sid, &event_rx_mutex, &f, None)
                        .await
                    {
                        return Err(PoolError::Engine(EngineFailure {
                            code: SESSION_CLOSED.to_string(),
                            message: SESSION_CLOSED_MESSAGE.to_string(),
                        }));
                    }
                    break;
                }
                AgentEvent::Error { code, message, .. } => {
                    // The caller learns this from its error response; every
                    // other watcher would otherwise see a turn that never ends.
                    self.broadcast_terminal(&sid, &event_rx_mutex, &f, Some(client.id()))
                        .await;
                    self.release_turn_slot(&sid, &event_rx_mutex, event_rx, &turn_id)
                        .await;
                    return Err(PoolError::Engine(EngineFailure { code, message }));
                }
                _ => self.broadcast_to_session(&sid, &f).await,
            }
        }

        // The turn is over. A client that vanished mid-stream is not part of
        // this: the turn ran to completion and landed in the transcript, and
        // whoever comes back reads it there. Cancelling on a broken writer
        // would cascade into deleting sidechain transcripts another tab is
        // still watching.

        let next_round = match &turn_state {
            Some(state) => self.goal_round_after(&sid, state, &ended_with).await,
            None => None,
        };
        let ours = self
            .release_turn_slot(&sid, &event_rx_mutex, event_rx, &turn_id)
            .await;
        if let (true, Some(round)) = (ours, next_round) {
            self.start_goal_round(&sid, round);
        }

        // Whether a session names itself is its *own* scene's call, not this
        // pool's default scene's: a `harnix` session created on a `--scene
        // code` daemon is still a harnix session, and the two scenes disagree
        // about this exactly as they disagree about the system prompt.
        //
        // The decision is taken under the lock and the naming call made
        // outside it — it is a model call, and the sessions map is the
        // pool's hottest mutex.
        let mut session_name = None;
        let naming = {
            let mut sessions = self.sessions.lock().await;
            match sessions.get_mut(&sid) {
                // A slash command is not what the conversation is about.
                Some(live) if live.is_first_turn && !message.starts_with('/') => {
                    live.is_first_turn = false;
                    (live.scene.auto_name_session() && !live.named_by_user)
                        .then(|| live.scene.session_name_prompt(&message))
                        .flatten()
                        .map(|prompt| (prompt, live.namer.clone()))
                }
                Some(live) => {
                    session_name = live.name.clone();
                    None
                }
                None => None,
            }
        };
        if let Some((prompt, namer)) = naming {
            match namer.name(&prompt).await {
                Ok(Some(name)) => {
                    session_name = self.adopt_generated_name(&sid, name).await;
                }
                Ok(None) => {}
                Err(e) => {
                    warn!(%sid, error=%e, "session name generation failed");
                }
            }
        }

        Ok(serde_json::json!({
            "session_id": sid,
            "turn_id": turn_id,
            "delivery": "turn",
            "name": session_name,
            "api_calls": api_calls,
        }))
    }

    /// A name somebody typed while the naming call was out wins over the
    /// guess; otherwise the guess is stored like any other name, so a resume
    /// or a daemon restart finds it.
    pub(super) async fn adopt_generated_name(&self, sid: &str, name: String) -> Option<String> {
        {
            let mut sessions = self.sessions.lock().await;
            let live = sessions.get_mut(sid)?;
            if live.named_by_user {
                return live.name.clone();
            }
            live.name = Some(name.clone());
        }
        if let (Some(store), Ok(parsed)) = (
            self.history_store.as_ref(),
            base::session::SessionId::parse(sid),
        ) {
            if let Err(e) = store.set_session_title(parsed, Some(name.clone())).await {
                warn!(%sid, error=%e, "could not store the generated session name");
            }
        }
        Some(name)
    }
}
