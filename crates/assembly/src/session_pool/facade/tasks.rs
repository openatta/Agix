//! 会话内后台任务与子 Agent：事件转发、任务查询与停止、`start_agent`、上下文请求。

use crate::session_pool::parts::phase::SessionOp;
use crate::session_pool::*;

fn delegate_error(e: base::interface::delegate::DelegateError) -> PoolError {
    use base::interface::delegate::DelegateError;
    match e {
        DelegateError::Limit(m) | DelegateError::Depth(m) => PoolError::TaskLimit(m),
        DelegateError::Unsupported(m) => PoolError::SpawnUnsupported(m),
        DelegateError::Invalid(m) => PoolError::InvalidRequest(m),
        DelegateError::Build(m) => PoolError::Internal(m),
        other => PoolError::Internal(other.to_string()),
    }
}

impl SessionPool {
    /// While no turn holds `event_rx`, pass on what the engine still says: a
    /// background sub-agent's question and progress, a team stage. Woken by
    /// `arrived` when the engine sends something; when a turn holds the
    /// receiver, waits for `idle` and looks again, since what arrived as the
    /// turn ended is still there. A turn that starts takes the receiver as
    /// before; this only ever tries the lock, so it never makes a turn wait.
    pub(in crate::session_pool) fn forward_events_between_turns(
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

    /// Push the task events of `core` to whoever watches the session, for as
    /// long as the session is in memory — across every engine it runs on.
    /// Outside any turn, so `turn_id` is `null`.
    pub(in crate::session_pool) fn forward_task_events(core: &SessionCore) {
        let mut events = core.state().subscribe_tasks();
        let hub = Arc::downgrade(core.hub());
        let session_id = core.id().to_string();
        tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(event) => {
                        let Some(hub) = hub.upgrade() else { return };
                        if let base::context::TaskEvent::Finished { task } = &event {
                            hub.forget_asks_of_task(&task.task_id);
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
    pub async fn session_tasks(&self, session_id: &str) -> Result<api::SessionTasks, PoolError> {
        if !self.session_exists(session_id).await {
            return Err(PoolError::no_session(session_id));
        }
        let state = self.live_state(session_id).await;
        let mut tasks = state.as_ref().map(|s| s.task_infos()).unwrap_or_default();
        let members = self
            .live
            .with_live(session_id, |live| live.live_members())
            .await
            .unwrap_or_default();
        let member_entries: Vec<_> = members
            .into_iter()
            .map(|member| member_task(member, &tasks))
            .collect();
        tasks.extend(member_entries);
        tasks.extend(
            self.scheduler
                .list(Some(session_id))
                .into_iter()
                .map(|job| base::context::TaskInfo {
                    task_id: job.id,
                    kind: base::context::TaskType::CronJob,
                    status: base::context::TaskStatus::Waiting,
                    started_ms: job.created_ms,
                    ended_ms: None,
                    detail: serde_json::json!({
                        "cron": job.cron,
                        "prompt": job.prompt,
                        "recurring": job.recurring,
                        "durable": job.durable,
                        "expires_ms": job.expires_ms,
                    }),
                    error: None,
                    waiting_for: Vec::new(),
                    usage: None,
                    structured_output: None,
                }),
        );
        Ok(api::SessionTasks {
            active: state.is_some(),
            tasks,
        })
    }

    /// `session.taskOutput`: part of what one of `session.tasks`' entries has
    /// produced — what the model's `TaskOutput` reads.
    pub async fn task_output(
        &self,
        session_id: &str,
        task_id: &str,
        read: TaskOutputRead,
    ) -> Result<api::TaskOutput, PoolError> {
        if !self.session_exists(session_id).await {
            return Err(PoolError::no_session(session_id));
        }
        let unknown = || PoolError::TaskNotFound {
            session_id: session_id.to_string(),
            task_id: task_id.to_string(),
        };
        let state = self.live_state(session_id).await.ok_or_else(unknown)?;
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
        Ok(api::TaskOutput {
            output: slice.text,
            truncated,
            status: task.info().status,
            from_byte: slice.from_byte,
            next_byte: slice.next_byte,
            dropped: slice.dropped,
            closed: slice.closed,
        })
    }

    /// `session.stopTask`: stop one of `session.tasks`' entries. A scheduled
    /// job is cancelled; running work is stopped the way the model's
    /// `TaskStop` stops it — a shell command's whole process group goes. A
    /// team member's id, as a refused rebuild names it, stops that member.
    /// `false` if there is no such running task or member.
    pub async fn stop_task(&self, session_id: &str, task_id: &str) -> bool {
        if self.scheduler.remove_for(session_id, task_id) {
            return true;
        }
        if self
            .live_state(session_id)
            .await
            .is_some_and(|s| s.cancel_running_task(task_id))
        {
            return true;
        }
        self.stop_member(session_id, task_id).await
    }

    /// Stop the team member `member_id` names, as `TeamDelete` would.
    async fn stop_member(&self, session_id: &str, member_id: &str) -> bool {
        let found = self
            .live
            .with_live(session_id, |live| {
                let delegator = live.engine.delegator.clone()?;
                let member = live
                    .live_members()
                    .into_iter()
                    .find(|m| m.id == member_id)?;
                Some((delegator, member.seat))
            })
            .await
            .flatten();
        let Some((delegator, seat)) = found else {
            return false;
        };
        delegator.stop_member(&seat).await;
        true
    }

    /// `session.messageTask`: hand a message to a running background
    /// sub-agent, the way the model's `TaskMessage` does. A task that does not
    /// take it is an answer, not an error; a session not in memory is.
    pub async fn message_task(
        &self,
        session_id: &str,
        task_id: &str,
        message: String,
    ) -> Result<api::MessageDelivery, PoolError> {
        let state = self.live_state(session_id).await.ok_or_else(|| {
            PoolError::session_gone(session_id, format!("session `{session_id}` is not running"))
        })?;
        Ok(
            match tools::task_message::deliver(&state, task_id, message) {
                Ok(()) => api::MessageDelivery {
                    delivered: true,
                    reason: None,
                },
                Err(reason) => api::MessageDelivery {
                    delivered: false,
                    reason: Some(reason),
                },
            },
        )
    }

    /// `session.startAgent`: a sub-agent the host starts under `session_id`,
    /// in the background, whether or not a turn is running. A session not in
    /// memory is resumed for it.
    pub async fn start_agent(
        &self,
        session_id: &str,
        request: base::interface::delegate::DelegateRequest,
    ) -> Result<api::AgentStarted, PoolError> {
        use base::interface::delegate::{DelegateMode, Delegated};
        if !matches!(request.mode, DelegateMode::Host { .. }) {
            return Err(PoolError::InvalidRequest(
                "a host starts a sub-agent as DelegateMode::Host".into(),
            ));
        }
        let not_found = || PoolError::no_session(session_id);
        if !self.session_exists(session_id).await {
            return Err(not_found());
        }
        self.ensure_live(session_id, None).await?;
        let Some(delegator) = self
            .live
            .with_live(session_id, |live| live.engine.delegator.clone())
            .await
            .flatten()
        else {
            return Err(not_found());
        };
        match delegator.delegate(request).await {
            Ok(Delegated::Started(started)) => Ok(api::AgentStarted {
                task_id: started.task_id,
                agent_session_id: started.agent_session_id,
            }),
            Ok(Delegated::Finished(_)) => Err(PoolError::Internal(
                "the sub-agent finished before it was started".into(),
            )),
            Err(e) => Err(delegate_error(e)),
        }
    }

    /// `session.inspectContext`: what the next model call would carry.
    pub async fn inspect_context(&self, session_id: &str) -> Result<api::ContextReport, PoolError> {
        match self
            .context_request(session_id, runtime::agent::ContextRequest::Inspect)
            .await?
        {
            runtime::agent::ContextAnswer::Inspected(report) => Ok(report),
            other => Err(unexpected_answer(other)),
        }
    }

    /// `session.dropMessages`: the positions dropped.
    pub async fn drop_messages(
        &self,
        session_id: &str,
        at: Vec<usize>,
        total: Option<usize>,
    ) -> Result<Vec<usize>, PoolError> {
        match self
            .context_request(
                session_id,
                runtime::agent::ContextRequest::Drop { at, total },
            )
            .await?
        {
            runtime::agent::ContextAnswer::Dropped(at) => Ok(at),
            other => Err(unexpected_answer(other)),
        }
    }

    /// `session.editMessage`: the position edited.
    pub async fn edit_message(
        &self,
        session_id: &str,
        at: usize,
        text: String,
        total: Option<usize>,
    ) -> Result<usize, PoolError> {
        match self
            .context_request(
                session_id,
                runtime::agent::ContextRequest::Edit { at, text, total },
            )
            .await?
        {
            runtime::agent::ContextAnswer::Edited(at) => Ok(at),
            other => Err(unexpected_answer(other)),
        }
    }

    /// Ask a session's engine about, or change, its context
    /// (`session.inspectContext`, `session.dropMessages`,
    /// `session.editMessage`). A session not in memory is resumed for it.
    /// The session is held in `Exclusive(Context)` from the check to the
    /// answer, so no turn, rebuild or other context request starts in
    /// between; one that is doing any of those answers `SESSION_BUSY` — its
    /// context is moving under the question. The engine writes a change to
    /// the log itself, so a resume reproduces it.
    async fn context_request(
        &self,
        session_id: &str,
        request: runtime::agent::ContextRequest,
    ) -> Result<runtime::agent::ContextAnswer, PoolError> {
        if !self.session_exists(session_id).await {
            return Err(PoolError::no_session(session_id));
        }
        self.ensure_live(session_id, None).await?;
        self.wait_out_finishing_turn(session_id).await;
        let (input_tx, core, generation) = self
            .live
            .with_live_mut(session_id, |live| {
                if let Some(busy) = live.turn.busy(session_id) {
                    return Err(PoolError::SessionBusy(busy));
                }
                live.turn
                    .begin_exclusive(SessionOp::Context)
                    .map_err(|_| PoolError::Internal("an idle slot refused".into()))?;
                Ok((
                    live.engine.input_tx.clone(),
                    live.core.clone(),
                    live.engine.generation,
                ))
            })
            .await
            .ok_or_else(|| PoolError::no_session(session_id))??;
        let pool = self.this.clone();
        let operation = tokio::spawn(async move {
            let answer = ask_engine(input_tx, request).await;
            if let Some(pool) = pool.upgrade() {
                pool.end_context_operation(&core, generation).await;
            }
            answer
        });
        operation.await.unwrap_or_else(|e| {
            Err(PoolError::Internal(format!(
                "the context request failed: {e}"
            )))
        })
    }

    async fn end_context_operation(&self, core: &Arc<SessionCore>, generation: Generation) {
        self.live
            .with_live_mut(core.id(), |live| {
                if live.runs(core, generation) {
                    live.turn.end_exclusive(SessionOp::Context);
                }
            })
            .await;
    }
}

async fn ask_engine(
    input_tx: InputSender,
    request: runtime::agent::ContextRequest,
) -> Result<runtime::agent::ContextAnswer, PoolError> {
    let (reply, answer) = tokio::sync::oneshot::channel();
    input_tx
        .send(InputMessage::Context { request, reply })
        .map_err(|_| PoolError::engine_unreachable("the session's engine has stopped"))?;
    tokio::time::timeout(Duration::from_secs(30), answer)
        .await
        .map_err(|_| PoolError::engine_unreachable("the engine did not answer"))?
        .map_err(|_| PoolError::engine_unreachable("the engine went away"))?
        .map_err(|e| match e {
            runtime::agent::ContextError::Refused(why) => PoolError::InvalidRequest(why),
            not_recorded => PoolError::Internal(not_recorded.to_string()),
        })
}

fn unexpected_answer(answer: runtime::agent::ContextAnswer) -> PoolError {
    PoolError::Internal(format!(
        "the engine answered a different context request: {answer:?}"
    ))
}

/// A live persistent member as `session.tasks` lists it: running while one
/// of the `teammate` tasks of its seat is.
fn member_task(
    member: base::interface::delegate::LiveMember,
    tasks: &[base::context::TaskInfo],
) -> base::context::TaskInfo {
    use base::context::{TaskStatus, TaskType};
    let detail = serde_json::json!({"team": member.seat.team, "member": member.seat.member});
    let running = tasks.iter().any(|t| {
        t.kind == TaskType::Teammate
            && matches!(t.status, TaskStatus::Running | TaskStatus::Waiting)
            && t.detail == detail
    });
    base::context::TaskInfo {
        task_id: member.id,
        kind: TaskType::Member,
        status: if running {
            TaskStatus::Running
        } else {
            TaskStatus::Idle
        },
        started_ms: 0,
        ended_ms: None,
        detail,
        error: None,
        waiting_for: Vec::new(),
        usage: None,
        structured_output: None,
    }
}
