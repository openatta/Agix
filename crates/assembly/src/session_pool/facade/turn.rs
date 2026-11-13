//! 回合执行：`run_turn` 占住回合槽，把这一轮交给池 spawn 的回合驱动（`drive_turn`）
//! 跑完，自己只等它的结局；调用方被丢下，回合照样跑完并还槽。另有回合槽的归还与
//! 自动命名的采纳。

use crate::session_pool::parts::live::SessionCore;
use crate::session_pool::*;

/// What one attempt at the turn slot came to.
enum Claim {
    Taken(TurnTicket, InputSender),
    Busy(Busy, Arc<SessionCore>),
    Skipped,
}

/// A claimed turn, for its driver to run.
pub(in crate::session_pool) struct ClaimedTurn {
    ticket: TurnTicket,
    input_tx: InputSender,
    message: String,
    attachments: Vec<runtime::agent::Attachment>,
    initiator: Initiator,
    initiator_id: Option<WatcherId>,
}

/// A turn's driver as its own boxed future: the driver may start a goal
/// round, and a goal round runs `run_turn`, so the `Send` of one cannot be
/// inferred from the other.
fn turn_driver(
    pool: Arc<SessionPool>,
    turn: ClaimedTurn,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<TurnOutcome, PoolError>> + Send>> {
    Box::pin(async move { pool.drive_turn(turn).await })
}

impl SessionPool {
    /// Give the receiver back and mark the session idle again, if the
    /// ticket's engine still runs the session — a session closed or rebuilt
    /// while the turn ran is not this turn's to touch. Whatever the turn's
    /// own engine was asking died with the turn; a background task's asks
    /// stay. Returns whether the entry was still the ticket's.
    pub(in crate::session_pool) async fn release_turn_slot(
        &self,
        ticket: &TurnTicket,
        event_rx: EventReceiver,
    ) -> bool {
        ticket.settle();
        *ticket.rx.lock().await = Some(event_rx);
        let released = self
            .live
            .with_live_mut(ticket.sid(), |live| {
                if !live.runs(&ticket.core, ticket.generation) {
                    return None;
                }
                let turn = live.turn.release(&ticket.turn_id);
                live.turn.idle.notify_waiters();
                Some(turn)
            })
            .await
            .flatten();
        let Some(turn) = released else {
            return false;
        };
        if let Some(turn) = turn {
            self.emit_event(api::DaemonEvent::TurnCompleted {
                session_id: ticket.sid().to_string(),
                turn_id: ticket.turn_id.to_string(),
                initiator: turn.initiator,
            });
            ticket.core.hub().retain_task_asks();
        }
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
                // checked again next time — its stamp stays behind, so
                // nothing is missed. So is a session whose log could not be
                // caught up: the turn's own writes may well do it.
                let stale = self
                    .live
                    .with_live(sid, |live| live.engine.binding.clone())
                    .await
                    .map(|stamp| !self.bindings.is_current(&stamp));
                let resolved = match stale {
                    None => self.ensure_live(sid, options.as_ref()).await,
                    Some(false) => Ok(sid.clone()),
                    Some(true) => match self.rebuild_idle(sid, options.as_ref()).await {
                        Ok(sid) => Ok(sid),
                        Err(Rebuild::Busy(_)) => Ok(sid.clone()),
                        Err(Rebuild::Failed(CreateError::PersistFailed(e))) => {
                            warn!(%sid, error = %e, "serving the turn on the engine it has");
                            Ok(sid.clone())
                        }
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
                    Some(self.cwd().to_path_buf()),
                    options.as_ref(),
                    false,
                )
                .await?
                .id()
                .to_string()
            }
        };

        // 必须**在发消息之前**占住会话：消息一旦 send 进去就归正在跑的那个
        // Agent 消费了，先发后占的话，调用方看到忙，消息却已经被别的 turn 吃掉。
        let mut steer_tried = false;
        let (ticket, input_tx) = loop {
            let Some(idle) = self
                .live
                .with_live(&sid, |live| live.turn.idle.clone())
                .await
            else {
                return Err(PoolError::session_not_found(&sid));
            };
            let became_idle = idle.notified();
            tokio::pin!(became_idle);
            became_idle.as_mut().enable();
            let claim = self
                .live
                .with_live_mut(&sid, |live| {
                    live.core.hub().touch();
                    // A goal round runs only if it is still the one its goal
                    // handed out: a pause, clear or change since then, or
                    // another turn's round, means nobody wants it any more.
                    if live.turn.is_idle()
                        && initiator == Initiator::Goal
                        && !live.core.state().goal_claim_round(&turn_id)
                    {
                        return Claim::Skipped;
                    }
                    if let Some(busy) = live.turn.busy(&sid) {
                        return Claim::Busy(busy, live.core.clone());
                    }
                    let input_tx = live.engine.input_tx.clone();
                    let running = RunningTurn::new(turn_id.clone(), initiator, initiator_id);
                    if live.turn.claim(running).is_err() {
                        unreachable!("a slot that is not busy is idle");
                    }
                    self.emit_event(api::DaemonEvent::TurnStarted {
                        session_id: sid.to_string(),
                        turn_id: turn_id.to_string(),
                        initiator,
                    });
                    // Whoever starts a turn watches it. Not a special role —
                    // it lands in the same subscriber set as any tab that
                    // called `session.subscribe`, and leaves the same way.
                    if let Some(watcher) = &watcher {
                        live.core.hub().subscribe(watcher.clone());
                    }
                    let ticket = TurnTicket::new(
                        live.core.clone(),
                        live.engine.generation,
                        turn_id.clone(),
                        live.turn.rx.clone(),
                    );
                    Claim::Taken(ticket, input_tx)
                })
                .await
                .ok_or_else(|| PoolError::session_not_found(&sid))?;
            let (busy, core) = match claim {
                Claim::Taken(ticket, input_tx) => break (ticket, input_tx),
                Claim::Skipped => {
                    return Ok(TurnOutcome::Skipped {
                        session_id: sid,
                        turn_id,
                    })
                }
                Claim::Busy(busy, core) => (busy, core),
            };
            match on_busy {
                OnBusy::Reject => return Err(PoolError::SessionBusy(busy)),
                OnBusy::Steer
                    if !steer_tried && attachments.is_empty() && busy.current_turn_id.is_some() =>
                {
                    steer_tried = true;
                    let offered = core.steer().offer(turn_id.clone(), message.clone());
                    if offered == runtime::steering::SteerAck::Accepted {
                        if let Some(watcher) = &watcher {
                            core.hub().subscribe(watcher.clone());
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
        let (sid, generation) = (ticket.sid().to_string(), ticket.generation);
        let weak = self.this.clone();
        let core = ticket.core.clone();
        ticket.on_abandon(move || {
            let (Some(pool), Ok(runtime)) = (weak.upgrade(), tokio::runtime::Handle::try_current())
            else {
                return;
            };
            runtime.spawn(async move { pool.evict_faulted(&core, generation).await });
        });
        let Some(pool) = self.this.upgrade() else {
            self.release_claimed(&ticket).await;
            return Err(PoolError::Internal("the pool is going away".into()));
        };
        let claimed = ClaimedTurn {
            ticket,
            input_tx,
            message,
            attachments,
            initiator,
            initiator_id,
        };
        match tokio::spawn(turn_driver(pool, claimed)).await {
            Ok(outcome) => outcome,
            Err(e) => {
                warn!(%sid, error = %e, "the turn's driver failed");
                Err(PoolError::Engine(base::event::TurnFailure::new(
                    base::event::FailureKind::Internal,
                    format!("the turn's driver failed: {e}"),
                )))
            }
        }
    }

    /// Give back the slot `ticket` claimed, for a turn that never ran.
    async fn release_claimed(&self, ticket: &TurnTicket) {
        ticket.settle();
        self.live
            .with_live_mut(ticket.sid(), |live| {
                if live.runs(&ticket.core, ticket.generation) {
                    live.turn.release(&ticket.turn_id);
                }
            })
            .await;
    }

    /// Take out the entry a turn left without giving its slot back — its
    /// driver failed, and the engine's receiver went with it — and tell the
    /// session's watchers its turn is over.
    pub(in crate::session_pool) async fn evict_faulted(
        &self,
        core: &Arc<SessionCore>,
        generation: Generation,
    ) {
        let sid = core.id();
        let Some(live) = self
            .live
            .remove_if(sid, |live| live.runs(core, generation))
            .await
        else {
            return;
        };
        self.end_cut_off_turn(sid, &live, SessionEvent::EngineStopped);
        self.evict(sid, &live);
        live.turn.idle.notify_waiters();
        warn!(%sid, "session evicted: its turn ended without giving its slot back");
        self.emit_event(api::DaemonEvent::SessionEvicted {
            session_id: sid.to_string(),
            reason: api::EvictReason::EngineFault,
        });
    }

    /// Run a claimed turn to its end: send the message, hand the engine's
    /// events to the session's watchers, give the slot back, start the goal
    /// round that follows, name the session after its first turn.
    async fn drive_turn(&self, turn: ClaimedTurn) -> Result<TurnOutcome, PoolError> {
        let ClaimedTurn {
            ticket,
            input_tx,
            message,
            attachments,
            initiator,
            initiator_id,
        } = turn;
        let sid = ticket.sid().to_string();
        let turn_id = ticket.turn_id.clone();
        let taken = ticket.rx.lock().await.take();
        let Some(event_rx) = taken else {
            self.release_claimed(&ticket).await;
            return Err(PoolError::Internal(format!(
                "session `{sid}` claimed a turn with no event receiver in its slot"
            )));
        };
        if initiator == Initiator::Goal {
            self.record_goal_changes(&sid, ticket.core.state()).await;
        }

        let _ = input_tx.send(InputMessage::User {
            content: message.clone(),
            attachments,
            turn_id: turn_id.clone(),
        });
        let (api_calls, ended_with, event_rx) =
            self.relay_turn(&ticket, event_rx, initiator_id).await?;

        // The turn is over. A client that vanished mid-stream is not part of
        // this: the turn ran to completion and landed in the transcript, and
        // whoever comes back reads it there. Cancelling on a broken writer
        // would cascade into deleting sidechain transcripts another tab is
        // still watching.
        let next_round = self
            .goal_round_after(&sid, ticket.core.state(), &ended_with)
            .await;
        let ours = self.release_turn_slot(&ticket, event_rx).await;
        if let (true, Some(round)) = (ours, next_round) {
            self.start_goal_round(&sid, round);
        }
        let name = self.name_after_turn(&ticket, &message).await;

        Ok(TurnOutcome::Completed {
            session_id: sid,
            turn_id,
            name,
            api_calls,
        })
    }

    /// Hand the turn's events to the session's watchers until it completes,
    /// with its model call count and stop reason. A turn that fails, an
    /// engine that goes away and a session closed under the turn end it
    /// here, the slot given back or gone.
    async fn relay_turn(
        &self,
        ticket: &TurnTicket,
        mut event_rx: EventReceiver,
        initiator_id: Option<WatcherId>,
    ) -> Result<(u32, String, EventReceiver), PoolError> {
        let core = &ticket.core;
        let session_key: Arc<str> = Arc::from(ticket.sid());
        let turn_key: Arc<str> = Arc::from(ticket.turn_id.as_str());
        let frame_of = |event| SessionFrame {
            session_id: session_key.clone(),
            turn_id: Some(turn_key.clone()),
            event,
        };
        let mut tools_ran = false;
        loop {
            let Some(event) = event_rx.recv().await else {
                let stopped = frame_of(SessionEvent::EngineStopped);
                self.engine_gone_mid_turn(ticket, &stopped, initiator_id)
                    .await;
                return Err(PoolError::engine_stopped(tools_ran));
            };
            match event {
                AgentEvent::SystemInit { .. } => continue,
                AgentEvent::TurnComplete {
                    ref stop_reason,
                    api_calls,
                    context,
                    persisted,
                    ..
                } => {
                    let ended_with = stop_reason.clone();
                    if let Some(context) = context {
                        core.set_context(context);
                    }
                    core.log().set_behind(!persisted);
                    let last = frame_of(SessionEvent::Agent(event));
                    if !self.publish_terminal(ticket, &last, None).await {
                        ticket.settle();
                        return Err(PoolError::session_closed(tools_ran));
                    }
                    return Ok((api_calls, ended_with, event_rx));
                }
                AgentEvent::Error { ref failure, .. } => {
                    let failure = failure.clone();
                    // The caller learns this from its own result; every other
                    // watcher would otherwise see a turn that never ends.
                    let last = frame_of(SessionEvent::Agent(event));
                    self.publish_terminal(ticket, &last, initiator_id).await;
                    self.release_turn_slot(ticket, event_rx).await;
                    return Err(PoolError::Engine(failure));
                }
                event => {
                    tools_ran |= matches!(event, AgentEvent::ToolUse { .. });
                    publish_agent_event(core.hub(), &session_key, Some(&turn_key), event)
                }
            }
        }
    }

    /// The engine went away mid-turn. Dropping the entry is what lets the
    /// next call on this id resume it from its transcript instead of sending
    /// into a closed channel.
    async fn engine_gone_mid_turn(
        &self,
        ticket: &TurnTicket,
        stopped: &SessionFrame,
        initiator_id: Option<WatcherId>,
    ) {
        let sid = ticket.sid();
        self.publish_terminal(ticket, stopped, initiator_id).await;
        ticket.settle();
        let dead = self
            .live
            .remove_if(sid, |live| live.runs(&ticket.core, ticket.generation))
            .await;
        if let Some(dead) = dead {
            self.evict(sid, &dead);
            dead.turn.idle.notify_waiters();
            self.emit_event(api::DaemonEvent::SessionClosed {
                session_id: sid.to_string(),
                reason: api::CloseReason::EngineStopped,
            });
        }
        warn!(%sid, "engine stopped before the turn completed");
    }

    /// The session's name after a turn that opened with `message`: guessed
    /// now when this was its first turn and nobody named it, else the one it
    /// has.
    ///
    /// Whether a session names itself is its *own* scene's call, not this
    /// pool's default scene's: a `harnix` session created on a `--scene code`
    /// daemon is still a harnix session, and the two scenes disagree about
    /// this exactly as they disagree about the system prompt. The naming call
    /// is made with no lock held — it is a model call.
    async fn name_after_turn(&self, ticket: &TurnTicket, message: &str) -> Option<String> {
        let core = &ticket.core;
        let prompt = {
            let mut naming = core.naming();
            // A slash command is not what the conversation is about.
            if !naming.is_first_turn || message.starts_with('/') {
                return naming.name.clone();
            }
            naming.is_first_turn = false;
            if naming.named_by_user {
                return None;
            }
            core.scene().session_name_prompt(message)?
        };
        let namer = self
            .live
            .with_live(ticket.sid(), |live| {
                live.runs(core, ticket.generation)
                    .then(|| live.engine.namer.clone())
            })
            .await
            .flatten()?;
        match namer.name(&prompt).await {
            Ok(Some(name)) => self.adopt_generated_name(core, name).await,
            Ok(None) => None,
            Err(e) => {
                warn!(sid = %ticket.sid(), error=%e, "session name generation failed");
                None
            }
        }
    }

    /// A name somebody typed while the naming call was out wins over the
    /// guess; otherwise the guess is stored like any other name, so a resume
    /// or a daemon restart finds it. A guess for a session that has left
    /// memory since is dropped: the session under that id now, if any, is
    /// another one.
    pub(in crate::session_pool) async fn adopt_generated_name(
        &self,
        core: &Arc<SessionCore>,
        name: String,
    ) -> Option<String> {
        if !self.live.holds(core).await {
            return None;
        }
        {
            let mut naming = core.naming();
            if naming.named_by_user {
                return naming.name.clone();
            }
            naming.name = Some(name.clone());
        }
        let sid = core.id();
        if let (Some(store), Ok(parsed)) =
            (self.directory.store(), base::session::SessionId::parse(sid))
        {
            if let Err(e) = store.set_session_title(parsed, Some(name.clone())).await {
                warn!(%sid, error=%e, "could not store the generated session name");
            }
        }
        Some(name)
    }
}
