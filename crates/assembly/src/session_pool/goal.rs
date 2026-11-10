//! 会话目标：目标轮次的续跑、目标变更与目标事件转发。

use super::*;

impl SessionPool {
    /// The goal round to start once a turn that ended with `stop_reason`
    /// gives its slot back, if it leaves `state`'s goal active and armed.
    /// Asked before the slot is given back, so a rebuild that takes the slot
    /// next carries the round over.
    pub(super) async fn goal_round_after(
        &self,
        sid: &str,
        state: &base::context::SessionState,
        stop_reason: &str,
    ) -> Option<base::context::goal::GoalRound> {
        if !runtime::turn::CONTINUES_A_GOAL.contains(&stop_reason) {
            return None;
        }
        let round = state.goal_next_round();
        self.record_goal_changes(sid, state).await;
        round
    }

    pub(super) fn start_goal_round(&self, sid: &str, round: base::context::goal::GoalRound) {
        if let Some(pool) = self.this.get().and_then(|p| p.upgrade()) {
            tokio::spawn(goal_round_task(pool, sid.to_string(), round));
        }
    }

    /// Start the next goal round of an idle session, if its goal wants one.
    pub(super) async fn continue_goal(&self, sid: &str) {
        let Some(state) = self.live_state(sid).await else {
            return;
        };
        if let Some(round) = self.goal_round_after(sid, &state, "end_turn").await {
            self.start_goal_round(sid, round);
        }
    }

    pub(super) async fn run_goal_round(&self, sid: String, round: base::context::goal::GoalRound) {
        info!(%sid, turn_id = %round.turn_id, "goal round started");
        let request = TurnRequest {
            session_id: Some(sid.clone()),
            message: round.message,
            attachments: Vec::new(),
            turn_id: round.turn_id.clone(),
            options: None,
            on_busy: OnBusy::Queue,
            initiator: Initiator::Goal,
        };
        let response = self.run_turn(request, None).await;
        if let Some(state) = self.live_state(&sid).await {
            state.goal_abandon_round(&round.turn_id);
        }
        if let Err(e) = response {
            warn!(%sid, error = %e, "goal round failed");
        }
    }

    pub(super) async fn live_state(&self, sid: &str) -> Option<Arc<base::context::SessionState>> {
        self.sessions
            .lock()
            .await
            .get(sid)
            .and_then(|live| live.state.clone())
    }

    /// Write the goal changes `sid`'s log does not have yet.
    pub(super) async fn record_goal_changes(&self, sid: &str, state: &base::context::SessionState) {
        let unrecorded = state.unrecorded_goal_changes().await;
        let (Some(store), Ok(parsed)) = (
            self.history_store.as_ref(),
            base::session::SessionId::parse(sid),
        ) else {
            return;
        };
        for goal in &unrecorded.changes {
            let entry = history::entry::LogEntry::Extension {
                ns: base::context::goal::GOAL_NS.to_string(),
                event: base::context::goal::GOAL_EVENT.to_string(),
                payload: serde_json::json!({ "goal": goal }),
            };
            if let Err(e) = store.append(parsed, entry).await {
                warn!(%sid, error = %e, "failed to record a goal change");
            }
        }
    }

    /// `session.goal`: the session's goal, read from its log when the session
    /// is not in memory (and then reported disarmed).
    pub async fn session_goal(&self, sid: &str) -> Result<serde_json::Value, PoolError> {
        if let Some(state) = self.live_state(sid).await {
            return Ok(serde_json::json!({ "goal": state.goal_view() }));
        }
        let entries = self.load_entries(sid).await?;
        let goal: Option<base::context::goal::Goal> = entries
            .iter()
            .rev()
            .find_map(|env| match &env.entry {
                history::entry::LogEntry::Extension { ns, event, payload }
                    if ns == base::context::goal::GOAL_NS
                        && event == base::context::goal::GOAL_EVENT =>
                {
                    Some(payload["goal"].clone())
                }
                _ => None,
            })
            .and_then(|goal| serde_json::from_value(goal).ok());
        Ok(serde_json::json!({
            "goal": base::context::goal::GoalSlot::restored(goal).view()
        }))
    }

    /// `session.setGoal` / `session.updateGoal`: a host acts on the goal with
    /// a person's authority. A goal left active and armed on an idle session
    /// starts its next round at once.
    pub async fn change_session_goal(
        &self,
        sid: &str,
        change: GoalChange,
    ) -> Result<serde_json::Value, PoolError> {
        self.refuse_scene_mismatch(sid).await?;
        if self.live_state(sid).await.is_none() {
            self.resume_unchecked(sid, false, None).await?;
        }
        let state = self
            .live_state(sid)
            .await
            .ok_or_else(|| PoolError::session_not_found(sid))?;
        let human = base::context::goal::TurnOrigin::Human;
        let current = || {
            state
                .goal_view()
                .map(|view| (view.goal.id, view.goal.revision))
                .unwrap_or_default()
        };
        let invalid = |e: base::context::goal::GoalError| PoolError::InvalidRequest(e.to_string());
        let pausing = matches!(change, GoalChange::Pause);
        match change {
            GoalChange::Set {
                objective,
                max_rounds,
            } => {
                state
                    .goal_create(objective, max_rounds, human)
                    .map_err(invalid)?;
            }
            GoalChange::Clear => state.goal_clear(human).map_err(invalid)?,
            GoalChange::Pause => {
                let (id, rev) = current();
                state
                    .goal_update(&id, rev, base::context::goal::GoalAction::Pause, human)
                    .map_err(invalid)?;
            }
            GoalChange::Resume => {
                let (id, rev) = current();
                state
                    .goal_update(&id, rev, base::context::goal::GoalAction::Resume, human)
                    .map_err(invalid)?;
            }
            GoalChange::Edit {
                objective,
                max_rounds,
            } => {
                let (id, rev) = current();
                state
                    .goal_update(
                        &id,
                        rev,
                        base::context::goal::GoalAction::Edit {
                            objective,
                            max_rounds,
                        },
                        human,
                    )
                    .map_err(invalid)?;
            }
        }
        self.record_goal_changes(sid, &state).await;

        let running = self
            .sessions
            .lock()
            .await
            .get(sid)
            .map(|live| live.turn.running().map(|turn| turn.initiator));
        match running {
            Some(Some(Initiator::Goal)) if pausing => {
                self.interrupt_turn(sid).await?;
            }
            Some(None) => self.continue_goal(sid).await,
            _ => {}
        }
        Ok(serde_json::json!({ "goal": state.goal_view() }))
    }

    /// Push `session_id`'s goal changes to whoever watches the session.
    pub(super) fn forward_goal_events(
        session_id: &str,
        hub: &Arc<SessionHub>,
        state: &Arc<base::context::SessionState>,
    ) {
        let mut events = state.subscribe_goal();
        let hub = Arc::downgrade(hub);
        let session_id = session_id.to_string();
        tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(goal) => {
                        let Some(hub) = hub.upgrade() else { return };
                        hub.publish(&frame_between_turns(
                            &session_id,
                            SessionEvent::GoalChanged(goal),
                        ));
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
        });
    }
}
