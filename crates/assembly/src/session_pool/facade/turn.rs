//! 回合执行：`run_turn` 及其回合槽位、自动命名的采纳。

use crate::session_pool::*;

impl SessionPool {
    /// Give the receiver back and mark the session idle again, if the
    /// ticket's entry is still the session's — a session closed or replaced
    /// while the turn ran is not this turn's to touch. Done under the
    /// sessions lock, so nothing sees the slot free while the turn still
    /// reads as running. Whatever the turn's own engine was asking died with
    /// the turn; a background task's asks stay. Returns whether the entry was
    /// still the session's.
    pub(in crate::session_pool) async fn release_turn_slot(
        &self,
        ticket: &TurnTicket,
        event_rx: EventReceiver,
    ) -> bool {
        let mut sessions = self.sessions.lock().await;
        *ticket.rx.lock().await = Some(event_rx);
        let Some(live) = sessions
            .get_mut(&ticket.sid)
            .filter(|live| live.turn.generation == ticket.generation)
        else {
            return false;
        };
        if let Some(turn) = live.turn.release(ticket.generation, &ticket.turn_id) {
            self.emit_event(api::DaemonEvent::TurnCompleted {
                session_id: ticket.sid.to_string(),
                turn_id: ticket.turn_id.to_string(),
                initiator: turn.initiator,
            });
            live.hub.retain_task_asks();
        }
        live.turn.idle.notify_waiters();
        true
    }

    /// 执行一个 turn：发送消息 → 事件同步交给会话的订阅者 → 返回结局。
    /// `session_id` 为 `None` 时自动创建新 session。`watcher` 是发起方：它在
    /// 这一轮开始时加入订阅者，本轮的结局从返回值得知，不另收 `error` 帧。
    pub async fn run_turn(
        &self,
        request: TurnRequest,
        watcher: Option<Arc<dyn SessionWatcher>>,
    ) -> Result<TurnOutcome, PoolError> {
        let TurnRequest {
            session_id,
            message,
            attachments,
            turn_id,
            options,
            on_busy,
            initiator,
        } = request;
        let initiator_id = watcher.as_ref().map(|w| w.id());
        let sid = match session_id {
            Some(ref sid) => {
                // An unknown id still creates a session: its scene reads as
                // inferred. The daemon's own rounds go to sessions it already
                // serves.
                if initiator == Initiator::Client {
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
                        Err(Rebuild::Busy(_)) => Ok(sid.clone()),
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
                    self.scenes.default().clone(),
                    self.default_binding().await,
                    Some(self.cwd.clone()),
                    options.as_ref(),
                    false,
                )
                .await?
            }
        };

        // 必须**在发消息之前**占住会话：消息一旦 send 进去就归正在跑的那个
        // Agent 消费了，先发后占的话，调用方看到忙，消息却已经被别的 turn 吃掉。
        let mut steer_tried = false;
        let (input_tx, generation, rx, turn_state, hub) = loop {
            let idle = {
                let sessions = self.sessions.lock().await;
                let Some(live) = sessions.get(&sid) else {
                    return Err(PoolError::session_not_found(&sid));
                };
                live.turn.idle.clone()
            };
            let became_idle = idle.notified();
            tokio::pin!(became_idle);
            became_idle.as_mut().enable();
            let (input_tx, busy) = {
                let mut sessions = self.sessions.lock().await;
                let Some(live) = sessions.get_mut(&sid) else {
                    return Err(PoolError::session_not_found(&sid));
                };
                live.hub.touch();
                // A goal round runs only if it is still the one its goal
                // handed out: a pause, clear or change since then, or another
                // turn's round, means nobody wants it any more.
                if live.turn.is_idle()
                    && initiator == Initiator::Goal
                    && !live
                        .state
                        .as_ref()
                        .is_some_and(|state| state.goal_claim_round(&turn_id))
                {
                    return Ok(TurnOutcome::Skipped {
                        session_id: sid,
                        turn_id,
                    });
                }
                let running = RunningTurn::new(turn_id.clone(), initiator, initiator_id);
                if let Ok(generation) = live.turn.claim(running) {
                    self.emit_event(api::DaemonEvent::TurnStarted {
                        session_id: sid.to_string(),
                        turn_id: turn_id.to_string(),
                        initiator,
                    });
                    // Whoever starts a turn watches it. Not a special role —
                    // it lands in the same subscriber set as any tab that
                    // called `session.subscribe`, and leaves the same way.
                    if let Some(watcher) = &watcher {
                        live.hub.subscribe(watcher.clone());
                    }
                    break (
                        live.input_tx.clone(),
                        generation,
                        live.turn.rx.clone(),
                        live.state.clone(),
                        live.hub.clone(),
                    );
                }
                (live.input_tx.clone(), live.turn.busy(&sid))
            };
            let Some(busy) = busy else { continue };
            match on_busy {
                OnBusy::Reject => return Err(PoolError::SessionBusy(busy)),
                OnBusy::Steer
                    if !steer_tried && attachments.is_empty() && busy.current_turn_id.is_some() =>
                {
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
                        if let (Some(hub), Some(watcher)) = (self.hub_of(&sid).await, &watcher) {
                            hub.subscribe(watcher.clone());
                        }
                        return Ok(TurnOutcome::Steered {
                            session_id: sid,
                            turn_id,
                            into_turn_id: busy.current_turn_id,
                        });
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
        let ticket = TurnTicket {
            sid: sid.clone(),
            generation,
            turn_id: turn_id.clone(),
            rx,
        };
        let taken = ticket.rx.lock().await.take();
        let Some(mut event_rx) = taken else {
            if let Some(live) = self
                .sessions
                .lock()
                .await
                .get_mut(&sid)
                .filter(|live| live.turn.generation == generation)
            {
                live.turn.release(generation, &turn_id);
            }
            return Err(PoolError::Internal(format!(
                "session `{sid}` claimed a turn with no event receiver in its slot"
            )));
        };
        if let (Some(state), Initiator::Goal) = (&turn_state, initiator) {
            self.record_goal_changes(&sid, state).await;
        }

        // 发送用户消息
        let _ = input_tx.send(InputMessage::User {
            content: message.clone(),
            attachments,
            turn_id: turn_id.clone(),
        });

        let session_key: Arc<str> = Arc::from(sid.as_str());
        let turn_key: Arc<str> = Arc::from(turn_id.as_str());
        let frame_of = |event| SessionFrame {
            session_id: session_key.clone(),
            turn_id: Some(turn_key.clone()),
            event,
        };
        let api_calls;
        let ended_with;
        let mut tools_ran = false;
        loop {
            let Some(event) = event_rx.recv().await else {
                // The engine is gone. Dropping the entry is what lets the
                // next call on this id resume it from its transcript instead
                // of sending into a closed channel.
                self.publish_terminal(
                    &ticket,
                    &frame_of(SessionEvent::EngineStopped),
                    initiator_id,
                )
                .await;
                let dead = {
                    let mut sessions = self.sessions.lock().await;
                    let ours = sessions
                        .get(&sid)
                        .is_some_and(|live| live.turn.generation == generation);
                    if ours {
                        sessions.remove(&sid)
                    } else {
                        None
                    }
                };
                if let Some(dead) = dead {
                    self.evict(&sid, &dead);
                    dead.turn.idle.notify_waiters();
                    self.emit_event(api::DaemonEvent::SessionClosed {
                        session_id: sid.to_string(),
                        reason: api::CloseReason::EngineStopped,
                    });
                }
                warn!(%sid, "engine stopped before the turn completed");
                return Err(PoolError::engine_stopped(tools_ran));
            };
            match event {
                AgentEvent::SystemInit { .. } => continue,
                AgentEvent::TurnComplete {
                    ref stop_reason,
                    api_calls: ac,
                    context,
                    ..
                } => {
                    api_calls = ac;
                    ended_with = stop_reason.clone();
                    if context.is_some() {
                        if let Some(live) = self
                            .sessions
                            .lock()
                            .await
                            .get_mut(&sid)
                            .filter(|live| live.turn.generation == generation)
                        {
                            live.context = context;
                        }
                    }
                    let last = frame_of(SessionEvent::Agent(event));
                    if !self.publish_terminal(&ticket, &last, None).await {
                        return Err(PoolError::session_closed(tools_ran));
                    }
                    break;
                }
                AgentEvent::Error { ref failure, .. } => {
                    let failure = failure.clone();
                    // The caller learns this from its own result; every other
                    // watcher would otherwise see a turn that never ends.
                    let last = frame_of(SessionEvent::Agent(event));
                    self.publish_terminal(&ticket, &last, initiator_id).await;
                    self.release_turn_slot(&ticket, event_rx).await;
                    return Err(PoolError::Engine(failure));
                }
                event => {
                    tools_ran |= matches!(event, AgentEvent::ToolUse { .. });
                    publish_agent_event(&hub, &session_key, Some(&turn_key), event)
                }
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
        let ours = self.release_turn_slot(&ticket, event_rx).await;
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
            match sessions
                .get_mut(&sid)
                .filter(|live| live.turn.generation == generation)
            {
                // A slash command is not what the conversation is about.
                Some(live) if live.is_first_turn && !message.starts_with('/') => {
                    live.is_first_turn = false;
                    (!live.named_by_user)
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
                    session_name = self.adopt_generated_name(&sid, generation, name).await;
                }
                Ok(None) => {}
                Err(e) => {
                    warn!(%sid, error=%e, "session name generation failed");
                }
            }
        }

        Ok(TurnOutcome::Completed {
            session_id: sid,
            turn_id,
            name: session_name,
            api_calls,
        })
    }

    /// A name somebody typed while the naming call was out wins over the
    /// guess; otherwise the guess is stored like any other name, so a resume
    /// or a daemon restart finds it. A guess made for the entry of
    /// `generation` is dropped once another entry has the id.
    pub(in crate::session_pool) async fn adopt_generated_name(
        &self,
        sid: &str,
        generation: Generation,
        name: String,
    ) -> Option<String> {
        {
            let mut sessions = self.sessions.lock().await;
            let live = sessions
                .get_mut(sid)
                .filter(|live| live.turn.generation == generation)?;
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
