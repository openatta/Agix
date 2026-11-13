//! Phase 12: the tasks a session starts for itself once it is assembled.

use super::*;

pub(super) fn spawn_task_hooks(
    hooks: &Arc<HookRunner>,
    session_state: &Arc<base::context::SessionState>,
    hook_ambient: &hooks::payload::Ambient,
    session_ended: &CancellationToken,
    task_wake: Option<mpsc::UnboundedSender<()>>,
) {
    // `TaskCreated` fires when a running task starts and `TaskCompleted`
    // when it ends — a sub-agent, a shell command, a monitor, a team
    // member — whatever started it. A task ending is also the background
    // work an `async_rewake` hook waits on, unless the embedder gave a wake
    // channel of its own.
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        let hooks = hooks.clone();
        let mut events = session_state.subscribe_tasks();
        let ambient = hook_ambient.clone();
        let ended = session_ended.clone();
        handle.spawn(async move {
            loop {
                let event = tokio::select! {
                    _ = ended.cancelled() => return,
                    event = events.recv() => event,
                };
                match event {
                    Ok(base::context::TaskEvent::Started { task }) => {
                        if hooks.has_hooks_for(hooks::HookEvent::TaskCreated) {
                            let input = hooks::HookInput::from_ambient(&ambient, "TaskCreated")
                                .with_reason(serde_json::json!(task).to_string());
                            let _ = hooks.run(HookEvent::TaskCreated, &input, &ended).await;
                        }
                    }
                    Ok(base::context::TaskEvent::Finished { task }) => {
                        if hooks.has_hooks_for(hooks::HookEvent::TaskCompleted) {
                            let input = hooks::HookInput::from_ambient(&ambient, "TaskCompleted")
                                .with_reason(serde_json::json!(task).to_string());
                            let _ = hooks.run(HookEvent::TaskCompleted, &input, &ended).await;
                        }
                        if let Some(wake) = &task_wake {
                            let _ = wake.send(());
                        }
                    }
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
        });
    }
}

pub(super) fn spawn_cron(
    cron_store: Option<Arc<tools::cron::CronStore>>,
    fire_jobs_here: bool,
    input_tx: &InputSender,
) {
    // The scheduler half of the cron bridge: without this, `CronCreate`
    // would happily report "Scheduled recurring job <id>" for a job that
    // could never fire. Ticks once a minute (the resolution of a 5-field
    // cron expression) and feeds each due job's prompt back in as a user
    // turn. Ends by itself when the session's input channel closes.
    //
    // `Handle::try_current` because `build()` is sync and reachable from
    // non-async contexts, where `tokio::spawn` would panic; there the
    // tools still create/list/delete jobs, they just never fire.
    match (
        cron_store.filter(|_| fire_jobs_here),
        tokio::runtime::Handle::try_current(),
    ) {
        (None, _) => {}
        (Some(store), Ok(handle)) => {
            let tx = input_tx.clone();
            handle.spawn(async move {
                let mut ticker = tokio::time::interval(std::time::Duration::from_secs(60));
                ticker.tick().await; // first tick is immediate — skip it
                loop {
                    ticker.tick().await;
                    for job in store.pop_due() {
                        let msg = InputMessage::User {
                            content: job.prompt.clone(),
                            attachments: Vec::new(),
                            turn_id: format!("cron-{}", job.id),
                        };
                        if tx.send(msg).is_err() {
                            return; // session gone
                        }
                        tracing::info!(job_id = %job.id, cron = %job.cron, "cron job fired");
                    }
                }
            });
        }
        (Some(_), Err(_)) => {
            tracing::debug!(
                "no tokio runtime at Builder::build(); cron jobs will be stored but not fired"
            );
        }
    }
}
