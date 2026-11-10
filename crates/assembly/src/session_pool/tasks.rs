//! 会话内后台任务与子 Agent：事件转发、任务查询与停止、`start_agent`、上下文请求。

use super::*;

impl SessionPool {
    /// While no turn holds `event_rx`, pass on what the engine still says: a
    /// background sub-agent's question and progress, a team stage. Woken by
    /// `arrived` when the engine sends something; when a turn holds the
    /// receiver, waits for `idle` and looks again, since what arrived as the
    /// turn ended is still there. A turn that starts takes the receiver as
    /// before; this only ever tries the lock, so it never makes a turn wait.
    pub(super) fn forward_events_between_turns(
        session_id: &str,
        hub: &Arc<SessionHub>,
        event_rx: &Arc<AsyncMutex<Option<EventReceiver>>>,
        arrived: &Arc<tokio::sync::Notify>,
        idle: &Arc<tokio::sync::Notify>,
        cancel: &CancellationToken,
    ) {
        let (hub, event_rx, arrived, idle, cancel) = (
            hub.clone(),
            event_rx.clone(),
            arrived.clone(),
            idle.clone(),
            cancel.clone(),
        );
        let session_key: Arc<str> = Arc::from(session_id);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = arrived.notified() => {}
                }
                let waiting = loop {
                    let returned = idle.notified();
                    tokio::pin!(returned);
                    returned.as_mut().enable();
                    if let Ok(mut slot) = event_rx.try_lock() {
                        if let Some(rx) = slot.as_mut() {
                            let mut waiting = Vec::new();
                            while let Ok(event) = rx.try_recv() {
                                waiting.push(event);
                            }
                            break waiting;
                        }
                    }
                    tokio::select! {
                        _ = cancel.cancelled() => return,
                        _ = returned => {}
                    }
                };
                for event in waiting {
                    if emitted_outside_turns(&event) {
                        publish_agent_event(&hub, &session_key, None, event);
                    }
                }
            }
        });
    }

    /// Push `session_id`'s task events to whoever watches the session, for as
    /// long as its engine lives. Outside any turn, so `turn_id` is `null`.
    pub(super) fn forward_task_events(
        session_id: &str,
        hub: &Arc<SessionHub>,
        state: &Arc<base::context::SessionState>,
    ) {
        let mut events = state.subscribe_tasks();
        let hub = Arc::downgrade(hub);
        let session_id = session_id.to_string();
        tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(event) => {
                        let Some(hub) = hub.upgrade() else { return };
                        if event["kind"] == "task_finished" {
                            if let Some(task_id) = event["task"]["task_id"].as_str() {
                                hub.forget_asks_of_task(task_id);
                            }
                        }
                        hub.publish(&frame_between_turns(&session_id, SessionEvent::Task(event)));
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
        });
    }

    /// `session.tasks`: the work this session has running — sub-agents, team
    /// members, shell commands, monitors, and the jobs scheduled for it.
    /// `active` is whether its engine is in memory; a session that is not has
    /// only its scheduled jobs.
    pub async fn session_tasks(&self, session_id: &str) -> serde_json::Value {
        let state = self
            .sessions
            .lock()
            .await
            .get(session_id)
            .and_then(|live| live.state.clone());
        let mut tasks = state.as_ref().map(|s| s.task_infos()).unwrap_or_default();
        tasks.extend(self.schedule.list_for(session_id).into_iter().map(|job| {
            serde_json::json!({
                "task_id": job.id,
                "kind": "cron",
                "status": "waiting",
                "started_ms": job.created_ms,
                "ended_ms": null,
                "detail": {
                    "cron": job.cron,
                    "prompt": job.prompt,
                    "recurring": job.recurring,
                    "durable": job.durable,
                    "expires_ms": job.expires_ms,
                },
            })
        }));
        serde_json::json!({"active": state.is_some(), "tasks": tasks})
    }

    /// `session.taskOutput`: part of what one of `session.tasks`' entries has
    /// produced — what the model's `TaskOutput` reads.
    pub async fn task_output(
        &self,
        session_id: &str,
        task_id: &str,
        read: TaskOutputRead,
    ) -> Result<serde_json::Value, PoolError> {
        if !self.session_exists(session_id).await {
            return Err(PoolError::no_session(session_id));
        }
        let unknown = || PoolError::TaskNotFound {
            session_id: session_id.to_string(),
            task_id: task_id.to_string(),
        };
        let state = self
            .sessions
            .lock()
            .await
            .get(session_id)
            .and_then(|live| live.state.clone())
            .ok_or_else(unknown)?;
        let task = state.find_running_task(task_id).ok_or_else(unknown)?;
        let (slice, truncated) = match read {
            TaskOutputRead::Tail(tail_bytes) => {
                let slice = task.output_tail(tail_bytes);
                let truncated = slice.from_byte > 0;
                (slice, truncated)
            }
            TaskOutputRead::From(from_byte) => match task.output_from(from_byte) {
                Ok(slice) => {
                    let truncated = slice.dropped > 0;
                    (slice, truncated)
                }
                Err(next_byte) => {
                    return Err(PoolError::OutputOutOfRange {
                        task_id: task_id.to_string(),
                        next_byte,
                    })
                }
            },
        };
        Ok(serde_json::json!({
            "output": slice.text,
            "truncated": truncated,
            "status": task.info()["status"],
            "from_byte": slice.from_byte,
            "next_byte": slice.next_byte,
            "dropped": slice.dropped,
            "final": slice.closed,
        }))
    }

    /// `session.stopTask`: stop one of `session.tasks`' entries. A scheduled
    /// job is cancelled; running work is stopped the way the model's
    /// `TaskStop` stops it — a shell command's whole process group goes.
    /// `false` if there is no such running task.
    pub async fn stop_task(&self, session_id: &str, task_id: &str) -> bool {
        if self.schedule.remove_for(session_id, task_id) {
            return true;
        }
        let state = self
            .sessions
            .lock()
            .await
            .get(session_id)
            .and_then(|live| live.state.clone());
        state.is_some_and(|s| s.cancel_running_task(task_id))
    }

    /// `session.messageTask`: hand a message to a running background
    /// sub-agent, the way the model's `TaskMessage` does. `None` when the
    /// session is not in memory; otherwise why the task did not take it.
    pub async fn message_task(
        &self,
        session_id: &str,
        task_id: &str,
        message: String,
    ) -> Option<Result<(), String>> {
        let state = self
            .sessions
            .lock()
            .await
            .get(session_id)
            .and_then(|live| live.state.clone())?;
        Some(tools::task_message::deliver(&state, task_id, message))
    }

    /// `session.startAgent`: a sub-agent the host starts under `session_id`,
    /// in the background, whether or not a turn is running. A session not in
    /// memory is resumed for it.
    pub async fn start_agent(
        &self,
        session_id: &str,
        request: base::interface::agent_spawner::AgentRequest,
    ) -> Result<serde_json::Value, PoolError> {
        use base::interface::agent_spawner::StartAgentError;
        let not_found = || PoolError::no_session(session_id);
        if !self.session_exists(session_id).await {
            return Err(not_found());
        }
        self.ensure_live(session_id, None).await?;
        let (state, spawner, cancel) = {
            let sessions = self.sessions.lock().await;
            let live = sessions.get(session_id).ok_or_else(not_found)?;
            (
                live.state.clone(),
                live.spawner.clone(),
                live.cancel.child_token(),
            )
        };
        let (Some(state), Some(spawner)) = (state, spawner) else {
            return Err(not_found());
        };
        let started = spawner
            .start_agent(request, state, cancel)
            .await
            .map_err(|e| match e {
                StartAgentError::Limit(m) => PoolError::TaskLimit(m),
                StartAgentError::Unsupported(m) => PoolError::SpawnUnsupported(m),
            })?;
        Ok(serde_json::json!({
            "task_id": started.task_id,
            "agent_session_id": started.agent_session_id,
        }))
    }

    /// Ask a session's engine about, or change, its context
    /// (`session.inspectContext`, `session.dropMessages`,
    /// `session.editMessage`). A session not in memory is resumed for it; one
    /// running a turn answers `SESSION_BUSY` — its context is moving under
    /// the question. A change is applied to the engine first and then written
    /// to the log as an `agix.context` entry, so a resume reproduces it.
    pub async fn context_request(
        &self,
        session_id: &str,
        request: runtime::agent::ContextRequest,
    ) -> Result<serde_json::Value, PoolError> {
        if !self.session_exists(session_id).await {
            return Err(PoolError::no_session(session_id));
        }
        self.ensure_live(session_id, None).await?;
        let input_tx = {
            let sessions = self.sessions.lock().await;
            let live = sessions
                .get(session_id)
                .ok_or_else(|| PoolError::no_session(session_id))?;
            if let Some(turn) = &live.current_turn {
                return Err(PoolError::SessionBusy(Busy::context_moving(
                    session_id, turn,
                )));
            }
            live.input_tx.clone()
        };
        let (reply, answer) = tokio::sync::oneshot::channel();
        input_tx
            .send(InputMessage::Context {
                request: request.clone(),
                reply,
            })
            .map_err(|_| {
                PoolError::EngineUnreachable("the session's engine has stopped".to_string())
            })?;
        let result = tokio::time::timeout(Duration::from_secs(30), answer)
            .await
            .map_err(|_| PoolError::EngineUnreachable("the engine did not answer".to_string()))?
            .map_err(|_| PoolError::EngineUnreachable("the engine went away".to_string()))?
            .map_err(PoolError::InvalidRequest)?;

        let edits: Vec<(&str, serde_json::Value)> = match &request {
            runtime::agent::ContextRequest::Inspect => Vec::new(),
            runtime::agent::ContextRequest::Drop { at, .. } => at
                .iter()
                .map(|i| ("drop", serde_json::json!({"at": i})))
                .collect(),
            runtime::agent::ContextRequest::Edit { at, text, .. } => {
                vec![("edit", serde_json::json!({"at": at, "text": text}))]
            }
        };
        if let (false, Some(store)) = (edits.is_empty(), self.history_store.as_ref()) {
            let sid = base::session::SessionId::parse(session_id)
                .map_err(|e| PoolError::InvalidRequest(e.to_string()))?;
            for (event, mut payload) in edits {
                payload["at_ms"] = serde_json::json!(std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0));
                let entry = history::entry::LogEntry::Extension {
                    ns: history::transcript::CONTEXT_NS.to_string(),
                    event: event.to_string(),
                    payload,
                };
                store.append(sid, entry).await.map_err(|e| {
                    PoolError::Internal(format!("the change was made but not logged: {e}"))
                })?;
            }
        }
        Ok(result)
    }
}
