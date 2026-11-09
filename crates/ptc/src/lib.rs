//! `RunCode` — the model writes a JavaScript program that calls this
//! session's tools and sub-agents, the engine runs it in an embedded QuickJS,
//! and only the program's return value comes back to the conversation.
//!
//! Sub-calls go through the session's own dispatch (`ToolContext::dispatch`):
//! the same validation, hooks, middleware, telemetry and permission checks as
//! a call the model makes directly, except that one needing approval fails
//! instead of asking. A run is a running task of kind `code`. Design:
//! `docs/ptc.md`.

mod engine;

use async_trait::async_trait;
use base::context::{RunningStatus, RunningTask, SessionState, TaskType};
use base::error::ToolError;
use base::interface::tool_dispatch::{CallableTool, ToolDispatch};
use base::tool::{
    PermissionDecision, ProgressSender, PromptContext, Tool, ToolContext, ToolRegistry, ToolResult,
};
use engine::{CallRequest, Host, Run, ScriptEnd};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, Semaphore};
use tokio_util::sync::CancellationToken;

pub const TOOL_NAME: &str = "RunCode";

/// How far one run may go.
#[derive(Debug, Clone)]
pub struct Limits {
    pub time: Duration,
    /// The time limit once the program has started a sub-agent.
    pub time_with_agents: Duration,
    pub calls: u32,
    pub agents: u32,
    pub parallel: usize,
    pub result_chars: usize,
    pub call_result_chars: usize,
    pub memory_bytes: usize,
    pub code_chars: usize,
    pub notices: u32,
    /// How much of the program's log comes back with its result.
    pub log_chars: usize,
    /// How long a run asked to stop gets to wrap up before it is cut off.
    pub stop_grace: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            time: Duration::from_secs(5 * 60),
            time_with_agents: Duration::from_secs(60 * 60),
            calls: 1_000,
            agents: 100,
            parallel: 8,
            result_chars: 50_000,
            call_result_chars: 200_000,
            memory_bytes: 64 * 1024 * 1024,
            code_chars: 100_000,
            notices: 10,
            log_chars: 4_000,
            stop_grace: Duration::from_secs(10),
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RunCodeInput {
    /// The JavaScript program. End it with `return <JSON value>`; that value
    /// is the result.
    pub code: String,
    /// JSON the program reads as `args`.
    #[serde(default)]
    pub args: Option<Value>,
    /// One line saying what the program does, shown in the task list.
    #[serde(default)]
    pub description: Option<String>,
    /// Run in the background: return a task id now; the result arrives as a
    /// notification and through TaskOutput.
    #[serde(default)]
    pub background: bool,
}

pub struct RunCodeTool {
    description: String,
    guide: String,
    limits: Limits,
    run_slots: Arc<Semaphore>,
}

/// How many programs may run at once in this process, foreground and
/// background together: each one is a thread of its own.
pub fn default_run_slots() -> Arc<Semaphore> {
    static SLOTS: std::sync::LazyLock<Arc<Semaphore>> = std::sync::LazyLock::new(|| {
        let cpus = std::thread::available_parallelism().map_or(4, |n| n.get());
        Arc::new(Semaphore::new(cpus * 2))
    });
    SLOTS.clone()
}

impl RunCodeTool {
    /// `RunCode` for a session whose registry is `tools`: its description
    /// lists the tools a program may call there.
    pub fn for_registry(tools: &dyn ToolRegistry) -> Self {
        let mut callable: Vec<CallableTool> = tools
            .list()
            .into_iter()
            .filter(|t| t.callable_from_code())
            .map(|t| CallableTool {
                name: t.name().to_string(),
                description: t
                    .short_description()
                    .unwrap_or_else(|| t.description().to_string()),
                input_schema: base::tool::compact_schema(t.full_input_schema()),
                result: t.code_result().map(str::to_string),
            })
            .collect();
        callable.sort_by(|a, b| a.name.cmp(&b.name));
        Self::new(&callable, Limits::default())
    }

    pub fn new(callable: &[CallableTool], limits: Limits) -> Self {
        Self {
            description: describe(callable),
            guide: guide(callable),
            limits,
            run_slots: default_run_slots(),
        }
    }

    /// Share `slots` instead of the process-wide [`default_run_slots`]: a
    /// program waits for one before it starts and holds it until it ends.
    pub fn with_run_slots(mut self, slots: Arc<Semaphore>) -> Self {
        self.run_slots = slots;
        self
    }

    /// Only a run the top-level conversation starts takes a slot. One inside
    /// a sub-agent is already bounded by the run that started that sub-agent,
    /// and making it wait for a slot its ancestors hold would deadlock them.
    fn slots_for(&self, ctx: &ToolContext) -> Option<Arc<Semaphore>> {
        (ctx.agent_depth == 0).then(|| self.run_slots.clone())
    }
}

const INTRO: &str = "\
Write a JavaScript program that calls this session's tools, and get back only its return \
value. Use it when a task needs many tool calls whose intermediate results you don't need to \
read one by one — searching many files, fetching many pages, fanning out sub-agents and \
merging what they find — and for a workflow or multi-agent orchestration the user asks for: \
stages are just awaits, a fan-out is Promise.all over agent(). For a few calls, call the tools \
directly.

Runs as a script (not a module) in an embedded QuickJS engine: ES2023 with the standard \
library (JSON, Math, Date, RegExp, Map/Set, Promise incl. all/allSettled) and top-level \
await. No require/import, npm packages, or Node/browser APIs (fs, process, Buffer, fetch, \
setTimeout). Globals: tools.<Name>(input) and callTool(name, input) → Promise of the result \
(an object when the tool returns structured content, else the same text the tool shows you \
when you call it directly; rejects on failure — unsure of a format, return a sample first); \
agent(prompt, {type, schema, label, model}) → Promise (an object when schema is given); \
args; log(text) / console.log — shown to the user as progress and handed back to you with \
the result (the last 4,000 characters); notify(text) to tell the conversation something while running \
in the background; stopRequested() — wrap up and return what you have when true; messages() \
— notes sent to this run with TaskMessage. End with `return <JSON value>`.

Calls go through the same permission checks as direct calls; one that would need the user's \
approval fails instead of asking — make that call directly. Limits: 5 minutes (60 once a \
sub-agent is started), 1,000 calls, 100 sub-agents, 8 at a time; the result is cut at \
50,000 characters.";

/// What every request carries: enough to decide to use it and to write a
/// first program. Everything else is in [`guide`].
const SUMMARY: &str = "\
Run a JavaScript program that calls this session's tools — in parallel, in loops or in stages \
— and get back only its return value. For batch work (many files, pages or items) and for \
workflows or multi-agent orchestration: agent(prompt, {type, schema}) starts a sub-agent. For \
a single task or a few calls, call the tools directly.

A script (QuickJS, ES2023, top-level await; no require/import or Node/browser APIs). \
tools.<Name>(input) and callTool(name, input) return Promises that reject on failure; log(text) \
reports progress. End with `return <JSON value>`.";

fn describe(callable: &[CallableTool]) -> String {
    let mut out = format!(
        "{SUMMARY}\n\nCallable here (MCP tools too, with callTool; the guide has the rest):"
    );
    for tool in callable.iter().filter(|t| t.name != "Agent") {
        out.push_str("\n  ");
        out.push_str(&call_shape(tool));
    }
    out
}

fn guide(callable: &[CallableTool]) -> String {
    let mut out = format!("{INTRO}\n\nTools callable here (MCP tools too, by name with callTool):");
    for tool in callable {
        out.push_str("\n  ");
        out.push_str(&signature(tool));
    }
    out
}

fn signature(tool: &CallableTool) -> String {
    let call = call_shape(tool);
    let summary = first_sentence(&tool.description, 110);
    if summary.is_empty() {
        call
    } else {
        format!("{call} — {summary}")
    }
}

fn call_shape(tool: &CallableTool) -> String {
    let props = tool.input_schema["properties"].as_object();
    let required: Vec<&str> = tool.input_schema["required"]
        .as_array()
        .map(|r| r.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let params: Vec<String> = props
        .map(|p| {
            p.keys()
                .map(|k| {
                    if required.contains(&k.as_str()) {
                        k.clone()
                    } else {
                        format!("{k}?")
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    let call = if is_identifier(&tool.name) {
        format!("tools.{}({{{}}})", tool.name, params.join(", "))
    } else {
        format!("callTool(\"{}\", {{{}}})", tool.name, params.join(", "))
    };
    match &tool.result {
        Some(shape) => format!("{call} → {shape}"),
        None => call,
    }
}

fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '$')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

fn first_sentence(text: &str, max_chars: usize) -> String {
    let line = text.lines().next().unwrap_or_default().trim();
    let end = line.find(". ").map(|i| i + 1).unwrap_or(line.len());
    let sentence = &line[..end];
    if sentence.chars().count() <= max_chars {
        sentence.to_string()
    } else {
        let cut: String = sentence.chars().take(max_chars).collect();
        format!("{cut}…")
    }
}

#[async_trait]
impl Tool for RunCodeTool {
    fn name(&self) -> &str {
        "RunCode"
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn input_schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(RunCodeInput)).unwrap_or(Value::Null)
    }
    async fn prompt(&self, _: &PromptContext) -> String {
        self.guide.clone()
    }
    fn needs_dispatch(&self) -> bool {
        true
    }
    async fn check_permissions(&self, _: &Value, _: &ToolContext) -> PermissionDecision {
        PermissionDecision::allow()
    }
    async fn call(
        &self,
        input: Value,
        ctx: ToolContext,
        _: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let input: RunCodeInput = serde_json::from_value(input)?;
        if input.code.chars().count() > self.limits.code_chars {
            return Ok(ToolResult::error_text(format!(
                "the program is longer than {} characters",
                self.limits.code_chars
            )));
        }
        let Some(dispatch) = ctx.dispatch.clone() else {
            return Ok(ToolResult::error_text(
                "RunCode cannot call tools in this context",
            ));
        };
        let description = input
            .description
            .clone()
            .unwrap_or_else(|| "program".to_string());

        if input.background {
            let task_id = format!("code-{}", base::id::Id::new());
            // The session ending cuts the run off; a TaskStop on its own
            // token, a child of that, still gets the grace period.
            let cancel = ctx.cancel.child_token();
            let task = ctx.session.register_task(
                task_id.clone(),
                TaskType::Code,
                cancel.child_token(),
                serde_json::json!({
                    "description": description,
                    "background": true,
                    "tool_use_id": ctx.tool_use_id,
                }),
            );
            let (session, limits, slots) = (
                ctx.session.clone(),
                self.limits.clone(),
                self.slots_for(&ctx),
            );
            tokio::spawn(async move {
                let outcome = execute(
                    input,
                    dispatch,
                    &task,
                    &session,
                    &limits,
                    slots.as_ref(),
                    cancel,
                    true,
                )
                .await;
                session.post_notice(Some(task.task_id.clone()), notification(&task, &outcome));
                session.finish_task(
                    &task.task_id,
                    if outcome.is_error {
                        RunningStatus::Failed(first_line(&outcome.text))
                    } else {
                        RunningStatus::Completed
                    },
                );
            });
            return Ok(ToolResult::text(format!(
                "Started {task_id} in the background. Its result arrives as a notification; \
                 TaskOutput reads it, TaskStop stops it, TaskMessage reaches messages()."
            )));
        }

        let scope = base::context::TaskScope::new(
            ctx.session.clone(),
            ctx.tool_use_id.clone(),
            TaskType::Code,
            CancellationToken::new(),
            serde_json::json!({
                "description": description,
                "background": false,
                "tool_use_id": ctx.tool_use_id,
            }),
        );
        let outcome = execute(
            input,
            dispatch,
            scope.task(),
            &ctx.session,
            &self.limits,
            self.slots_for(&ctx).as_ref(),
            ctx.cancel.clone(),
            false,
        )
        .await;
        let mut result = if outcome.is_error {
            ToolResult::error_text(outcome.text)
        } else {
            ToolResult::text(outcome.text)
        };
        result.structured_content = outcome.value;
        Ok(result)
    }
}

struct Outcome {
    text: String,
    value: Option<Value>,
    is_error: bool,
}

/// What the program logged, keeping only as much of the end as the result
/// can show.
struct ProgramLog {
    text: String,
    dropped: bool,
    keep_chars: usize,
}

impl ProgramLog {
    fn new(keep_chars: usize) -> Self {
        Self {
            text: String::new(),
            dropped: false,
            keep_chars,
        }
    }

    fn push(&mut self, line: &str) {
        self.text.push_str(line);
        self.text.push('\n');
        let keep_bytes = self.keep_chars.saturating_mul(4).max(1);
        if self.text.len() > keep_bytes.saturating_mul(2) {
            let mut cut = self.text.len() - keep_bytes;
            while !self.text.is_char_boundary(cut) {
                cut += 1;
            }
            self.text.drain(..cut);
            self.dropped = true;
        }
    }

    /// The end of it, for the result.
    fn tail(&self) -> String {
        log_tail(&self.text, self.keep_chars, self.dropped)
    }
}

fn log_tail(log: &str, max_chars: usize, dropped: bool) -> String {
    if log.trim().is_empty() {
        return String::new();
    }
    let total = log.chars().count();
    let (tail, note) = if total > max_chars || dropped {
        (
            log.chars()
                .skip(total.saturating_sub(max_chars))
                .collect::<String>(),
            format!(" (last {max_chars} characters)"),
        )
    } else {
        (log.to_string(), String::new())
    };
    format!("\nLog{note}:\n{}", tail.trim_end())
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or_default().to_string()
}

fn notification(task: &RunningTask, outcome: &Outcome) -> String {
    let status = if outcome.is_error { "failed" } else { "done" };
    let detail = task
        .detail
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let mut notice = format!(
        "<task-notification>\n<task-id>{}</task-id>\n<status>{status}</status>\n",
        task.task_id
    );
    if let Some(d) = detail["description"].as_str() {
        notice.push_str(&format!("<description>{d}</description>\n"));
    }
    notice.push_str(&format!("<result>\n{}\n</result>\n", outcome.text));
    notice.push_str("</task-notification>");
    notice
}

struct CodeInbox(Arc<Mutex<Vec<String>>>);

impl std::fmt::Debug for CodeInbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CodeInbox")
    }
}

impl base::context::TaskInbox for CodeInbox {
    fn deliver(&self, message: String) -> Result<(), String> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(message);
        Ok(())
    }
}

/// What ended a run early, for the message that says so.
#[derive(Default)]
struct StopCause(Mutex<Option<String>>);

impl StopCause {
    fn set(&self, cause: impl Into<String>) {
        let mut slot = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_none() {
            *slot = Some(cause.into());
        }
    }
    fn get(&self) -> Option<String> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// One record per finished sub-call, for the error report.
struct CallLog {
    entries: Mutex<Vec<String>>,
    failed: AtomicU32,
    made: AtomicU32,
}

#[allow(clippy::too_many_arguments)]
async fn execute(
    input: RunCodeInput,
    dispatch: Arc<dyn ToolDispatch>,
    task: &Arc<RunningTask>,
    session: &Arc<SessionState>,
    limits: &Limits,
    run_slots: Option<&Arc<Semaphore>>,
    turn_cancel: CancellationToken,
    background: bool,
) -> Outcome {
    let not_started = |why: &str| Outcome {
        text: format!("The program did not start: {why}."),
        value: None,
        is_error: true,
    };
    let _slot = match run_slots {
        None => None,
        Some(slots) => tokio::select! {
            slot = slots.clone().acquire_owned() => match slot {
                Ok(slot) => Some(slot),
                Err(_) => return not_started("no program can run in this process any more"),
            },
            _ = turn_cancel.cancelled() => return not_started("the turn was interrupted"),
            _ = task.cancel.cancelled() => return not_started("it was stopped"),
        },
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
    let log_task = task.clone();
    let program_log = Arc::new(Mutex::new(ProgramLog::new(limits.log_chars)));
    let (log_buffer, notify_log) = (program_log.clone(), program_log.clone());
    let notices_left = Arc::new(AtomicU32::new(limits.notices));
    let (notify_task, notify_session) = (task.clone(), session.clone());
    let host = Host {
        calls: calls_tx,
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
        stop_requested: stop_requested.clone(),
        messages,
        args: input
            .args
            .as_ref()
            .map(Value::to_string)
            .unwrap_or_else(|| "{}".into()),
    };

    let dispatcher = tokio::spawn(dispatch_calls(
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
    let mut dispatcher = dispatcher;
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

    let made = calls_log.made.load(Ordering::SeqCst);
    let failed = calls_log.failed.load(Ordering::SeqCst);
    let summary = format!(
        "({made} tool calls, {failed} failed){}",
        program_log.lock().unwrap_or_else(|e| e.into_inner()).tail()
    );
    let outcome = match ended {
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
                completed_calls(&calls_log)
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
                    completed_calls(&calls_log)
                ),
                value: None,
                is_error: true,
            }
        }
    };
    task.append_output(&format!("{}\n", outcome.text));
    outcome
}

#[allow(clippy::too_many_arguments)]
fn spawn_watchers(
    task: &Arc<RunningTask>,
    turn_cancel: &CancellationToken,
    deadline: &Arc<AtomicU64>,
    hard_stop: &CancellationToken,
    stop_requested: &Arc<AtomicBool>,
    cause: &Arc<StopCause>,
    done: &CancellationToken,
    limits: &Limits,
) {
    {
        let (task_cancel, hard_stop, stop_requested, cause, done, grace) = (
            task.cancel.clone(),
            hard_stop.clone(),
            stop_requested.clone(),
            cause.clone(),
            done.clone(),
            limits.stop_grace,
        );
        tokio::spawn(async move {
            tokio::select! {
                _ = done.cancelled() => return,
                _ = task_cancel.cancelled() => {}
            }
            stop_requested.store(true, Ordering::SeqCst);
            tokio::select! {
                _ = done.cancelled() => {}
                _ = tokio::time::sleep(grace) => {
                    cause.set(format!("it was asked to stop and did not wrap up within {}s", grace.as_secs()));
                    hard_stop.cancel();
                }
            }
        });
    }
    {
        let (turn_cancel, hard_stop, cause, done) = (
            turn_cancel.clone(),
            hard_stop.clone(),
            cause.clone(),
            done.clone(),
        );
        tokio::spawn(async move {
            tokio::select! {
                _ = done.cancelled() => {}
                _ = turn_cancel.cancelled() => {
                    cause.set("the turn was interrupted");
                    hard_stop.cancel();
                }
            }
        });
    }
    {
        let (deadline, hard_stop, cause, done) = (
            deadline.clone(),
            hard_stop.clone(),
            cause.clone(),
            done.clone(),
        );
        tokio::spawn(async move {
            loop {
                let left = deadline
                    .load(Ordering::Relaxed)
                    .saturating_sub(engine::now_ms());
                if left == 0 {
                    cause.set("it ran out of time");
                    hard_stop.cancel();
                    return;
                }
                tokio::select! {
                    _ = done.cancelled() => return,
                    _ = tokio::time::sleep(Duration::from_millis(left.min(1_000))) => {}
                }
            }
        });
    }
    {
        let (task, done) = (task.clone(), done.clone());
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = done.cancelled() => return,
                    _ = tokio::time::sleep(Duration::from_secs(1)) => task.report_progress(),
                }
            }
        });
    }
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_calls(
    mut calls: mpsc::UnboundedReceiver<CallRequest>,
    dispatch: Arc<dyn ToolDispatch>,
    task: Arc<RunningTask>,
    limits: Limits,
    deadline: Arc<AtomicU64>,
    started: u64,
    hard_stop: CancellationToken,
    cause: Arc<StopCause>,
    log: Arc<CallLog>,
) {
    let slots = Arc::new(Semaphore::new(limits.parallel.max(1)));
    let mut last_serial: Option<oneshot::Receiver<()>> = None;
    let agents = AtomicU32::new(0);
    let mut running = tokio::task::JoinSet::new();
    loop {
        let request = tokio::select! {
            r = calls.recv() => match r { Some(r) => r, None => break },
            _ = hard_stop.cancelled() => break,
        };
        let input: Value = match serde_json::from_str(&request.input) {
            Ok(v) => v,
            Err(e) => {
                let _ = request
                    .reply
                    .send(answer_err(&format!("the input is not JSON: {e}")));
                continue;
            }
        };
        let made = log.made.fetch_add(1, Ordering::SeqCst) + 1;
        if made > limits.calls {
            cause.set(format!("it made more than {} tool calls", limits.calls));
            hard_stop.cancel();
            let _ = request.reply.send(answer_err("the call limit is reached"));
            break;
        }
        if request.name == "Agent" {
            let n = agents.fetch_add(1, Ordering::SeqCst) + 1;
            if n > limits.agents {
                cause.set(format!("it started more than {} sub-agents", limits.agents));
                hard_stop.cancel();
                let _ = request
                    .reply
                    .send(answer_err("the sub-agent limit is reached"));
                break;
            }
            deadline.fetch_max(
                started + limits.time_with_agents.as_millis() as u64,
                Ordering::SeqCst,
            );
        }
        // A call that is not concurrency-safe starts once the one submitted
        // before it has finished, and only then takes a slot: waiting in
        // line must not keep reads from running.
        let (turn, after) = if dispatch.concurrency_safe(&request.name, &input) {
            (None, None)
        } else {
            let (done, next) = oneshot::channel::<()>();
            (Some(done), last_serial.replace(next))
        };
        let (dispatch, task, slots, log, cancel, max_chars) = (
            dispatch.clone(),
            task.clone(),
            slots.clone(),
            log.clone(),
            hard_stop.child_token(),
            limits.call_result_chars,
        );
        running.spawn(async move {
            let _turn = turn;
            if let Some(previous) = after {
                let _ = previous.await;
            }
            let _slot = slots.acquire_owned().await;
            let label = format!("{}({})", request.name, brief(&input));
            task.append_output(&format!("→ {label}\n"));
            let reply = match dispatch.call(&request.name, input, cancel).await {
                Ok(answer) if !answer.reported_failure => {
                    task.append_output(&format!("← {label} ok\n"));
                    let value = answer
                        .structured
                        .filter(|v| v.to_string().chars().count() <= max_chars)
                        .unwrap_or_else(|| Value::String(cut_note(&answer.text, max_chars)));
                    serde_json::json!({"ok": true, "value": value}).to_string()
                }
                Ok(answer) => fail(&log, &task, &label, &answer.text, max_chars),
                Err(e) => fail(&log, &task, &label, &e, max_chars),
            };
            let _ = request.reply.send(reply);
        });
    }
    while running.join_next().await.is_some() {}
}

fn fail(log: &CallLog, task: &RunningTask, label: &str, error: &str, max_chars: usize) -> String {
    log.failed.fetch_add(1, Ordering::SeqCst);
    let line = format!("← {label} error: {}", first_line(error));
    task.append_output(&format!("{line}\n"));
    log.entries
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(line);
    answer_err(&cut_note(error, max_chars))
}

fn answer_err(error: &str) -> String {
    serde_json::json!({"ok": false, "error": error}).to_string()
}

fn completed_calls(log: &CallLog) -> String {
    let failures = log.entries.lock().unwrap_or_else(|e| e.into_inner());
    if failures.is_empty() {
        return String::new();
    }
    let shown: Vec<&String> = failures.iter().rev().take(20).collect();
    format!(
        "\nFailed calls (latest {}):\n{}\nCalls that succeeded may already have changed things; TaskOutput has the full call log.",
        shown.len(),
        shown
            .into_iter()
            .rev()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("\n")
    )
}

/// The input in a few words, for the call log.
fn brief(input: &Value) -> String {
    let text = match input {
        Value::Object(map) => map
            .iter()
            .take(2)
            .map(|(k, v)| {
                let v = match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                format!("{k}: {v}")
            })
            .collect::<Vec<_>>()
            .join(", "),
        other => other.to_string(),
    };
    truncate(&text, 80).0
}

/// `text` cut at `max_chars`, saying so when it was.
fn cut_note(text: &str, max_chars: usize) -> String {
    match truncate(text, max_chars) {
        (cut, true) => format!("{cut}\n… (cut at {max_chars} characters)"),
        (whole, false) => whole,
    }
}

fn truncate(text: &str, max_chars: usize) -> (String, bool) {
    if text.chars().count() <= max_chars {
        (text.to_string(), false)
    } else {
        (text.chars().take(max_chars).collect(), true)
    }
}

/// Take one from `left` unless it is already zero.
fn take_one(left: &AtomicU32) -> bool {
    let mut n = left.load(Ordering::SeqCst);
    while n > 0 {
        match left.compare_exchange(n, n - 1, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return true,
            Err(seen) => n = seen,
        }
    }
    false
}

#[cfg(test)]
mod tests;
