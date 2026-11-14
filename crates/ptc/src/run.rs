//! One RunCode program from admission to outcome: takes a run slot, wires the
//! host the script talks to, runs the engine beside the call dispatcher, and
//! turns how it ended into the text and value the model sees.

use super::*;

fn not_started(why: &str) -> Outcome {
    Outcome {
        text: format!("The program did not start: {why}."),
        value: None,
        is_error: true,
    }
}

async fn acquire_run_slot(
    run_slots: Option<&Arc<Semaphore>>,
    turn_cancel: &CancellationToken,
    task: &RunningTask,
) -> Result<Option<tokio::sync::OwnedSemaphorePermit>, Outcome> {
    let Some(slots) = run_slots else {
        return Ok(None);
    };
    tokio::select! {
        slot = slots.clone().acquire_owned() => match slot {
            Ok(slot) => Ok(Some(slot)),
            Err(_) => Err(not_started("no program can run in this process any more")),
        },
        _ = turn_cancel.cancelled() => Err(not_started("the turn was interrupted")),
        _ = task.cancel.cancelled() => Err(not_started("it was stopped")),
    }
}

struct HostWiring {
    calls: mpsc::UnboundedSender<CallRequest>,
    program_log: Arc<Mutex<ProgramLog>>,
    stop_requested: Arc<AtomicBool>,
    messages: Arc<Mutex<Vec<String>>>,
    notices: u32,
    background: bool,
}

fn build_host(
    wiring: HostWiring,
    task: &Arc<RunningTask>,
    session: &Arc<SessionState>,
    args: Option<&Value>,
) -> Host {
    let log_task = task.clone();
    let (log_buffer, notify_log) = (wiring.program_log.clone(), wiring.program_log);
    let notices_left = Arc::new(AtomicU32::new(wiring.notices));
    let (notify_task, notify_session) = (task.clone(), session.clone());
    let background = wiring.background;
    Host {
        calls: wiring.calls,
        log: Arc::new(move |text: String| {
            log_task.append_output(&format!("{text}\n"));
            log_buffer
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(&text);
        }),
        notify: Arc::new(move |text: String| {
            notify_task.append_output(&format!("[notify] {text}\n"));
            if !background {
                notify_log
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(&text);
            } else if take_one(&notices_left) {
                notify_session.post_notice(
                    None,
                    format!(
                        "<task-update>\n<task-id>{}</task-id>\n{text}\n</task-update>",
                        notify_task.task_id
                    ),
                );
            }
        }),
        stop_requested: wiring.stop_requested,
        messages: wiring.messages,
        args: args.map(Value::to_string).unwrap_or_else(|| "{}".into()),
    }
}

fn run_summary(calls_log: &CallLog, program_log: &Mutex<ProgramLog>) -> String {
    let made = calls_log.made.load(Ordering::SeqCst);
    let failed = calls_log.failed.load(Ordering::SeqCst);
    format!(
        "({made} tool calls, {failed} failed){}",
        program_log.lock().unwrap_or_else(|e| e.into_inner()).tail()
    )
}

fn outcome_of(
    ended: ScriptEnd,
    summary: &str,
    calls_log: &CallLog,
    limits: &Limits,
    cause: &StopCause,
    deadline: &AtomicU64,
) -> Outcome {
    match ended {
        ScriptEnd::Returned(json) => {
            let value = serde_json::from_str::<Value>(&json).ok();
            let (text, cut) = truncate(&json, limits.result_chars);
            let note = if cut {
                format!(" — result cut at {} characters", limits.result_chars)
            } else {
                String::new()
            };
            Outcome {
                text: format!("{text}\n{summary}{note}"),
                value: if cut { None } else { value },
                is_error: false,
            }
        }
        ScriptEnd::Threw(message) => Outcome {
            text: format!(
                "The program threw:\n{}\n{summary}{}\n\
                 The full guide to what a program can use: ToolSearch select:RunCode.",
                cut_note(&message, limits.result_chars),
                completed_calls(calls_log)
            ),
            value: None,
            is_error: true,
        },
        ScriptEnd::Stopped => {
            let why = cause.get().unwrap_or_else(|| {
                if engine::now_ms() >= deadline.load(Ordering::Relaxed) {
                    "it ran out of time".to_string()
                } else {
                    "it was stopped".to_string()
                }
            });
            Outcome {
                text: format!(
                    "The program did not finish: {why}.\n{summary}{}",
                    completed_calls(calls_log)
                ),
                value: None,
                is_error: true,
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn execute(
    input: RunCodeInput,
    dispatch: Arc<dyn ToolDispatch>,
    task: &Arc<RunningTask>,
    session: &Arc<SessionState>,
    limits: &Limits,
    run_slots: Option<&Arc<Semaphore>>,
    turn_cancel: CancellationToken,
    background: bool,
) -> Outcome {
    let _slot = match acquire_run_slot(run_slots, &turn_cancel, task).await {
        Ok(slot) => slot,
        Err(outcome) => return outcome,
    };
    let started = engine::now_ms();
    let deadline = Arc::new(AtomicU64::new(started + limits.time.as_millis() as u64));
    let hard_stop = CancellationToken::new();
    let stop_requested = Arc::new(AtomicBool::new(false));
    let cause = Arc::new(StopCause::default());
    let messages = Arc::new(Mutex::new(Vec::new()));
    let _ = task.inbox.set(Arc::new(CodeInbox(messages.clone())));
    let calls_log = Arc::new(CallLog {
        entries: Mutex::new(Vec::new()),
        failed: AtomicU32::new(0),
        made: AtomicU32::new(0),
    });

    let watchers = CancellationToken::new();
    spawn_watchers(
        task,
        &turn_cancel,
        &deadline,
        &hard_stop,
        &stop_requested,
        &cause,
        &watchers,
        limits,
    );

    let (calls_tx, calls_rx) = mpsc::unbounded_channel();
    let program_log = Arc::new(Mutex::new(ProgramLog::new(limits.log_chars)));
    let host = build_host(
        HostWiring {
            calls: calls_tx,
            program_log: program_log.clone(),
            stop_requested: stop_requested.clone(),
            messages,
            notices: limits.notices,
            background,
        },
        task,
        session,
        input.args.as_ref(),
    );

    let mut dispatcher = tokio::spawn(dispatch_calls(
        calls_rx,
        dispatch,
        task.clone(),
        limits.clone(),
        deadline.clone(),
        started,
        hard_stop.clone(),
        cause.clone(),
        calls_log.clone(),
    ));

    let ended = engine::spawn(Run {
        code: input.code,
        host,
        memory_limit: limits.memory_bytes,
        deadline_ms: deadline.clone(),
        hard_stop: hard_stop.clone(),
    })
    .await
    .unwrap_or(ScriptEnd::Stopped);
    hard_stop.cancel();
    watchers.cancel();
    if tokio::time::timeout(limits.stop_grace, &mut dispatcher)
        .await
        .is_err()
    {
        // Dropping the dispatcher's JoinSet aborts every sub-call still in it.
        dispatcher.abort();
        task.append_output(&format!(
            "(sub-calls still running {}s after the program ended were abandoned)\n",
            limits.stop_grace.as_secs()
        ));
    }
    task.report_progress();

    let summary = run_summary(&calls_log, &program_log);
    let outcome = outcome_of(ended, &summary, &calls_log, limits, &cause, &deadline);
    task.append_output(&format!("{}\n", outcome.text));
    outcome
}
