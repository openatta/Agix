//! The QuickJS side of a run: one thread, one single-threaded async runtime,
//! one script. Tool calls leave through a channel and come back as resolved
//! promises, so `Promise.all` in the script is real concurrency on the
//! engine's own runtime.

use rquickjs::prelude::Async;
use rquickjs::{
    AsyncContext, AsyncRuntime, CatchResultExt, CaughtError, Function, Object, Promise,
};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

const PRELUDE: &str = include_str!("prelude.js");

pub(crate) struct CallRequest {
    pub name: String,
    pub input: String,
    /// `{"ok":true,"value":…}` or `{"ok":false,"error":"…"}`.
    pub reply: oneshot::Sender<String>,
}

/// What the script reaches outside itself.
#[derive(Clone)]
pub(crate) struct Host {
    pub calls: mpsc::UnboundedSender<CallRequest>,
    pub log: Arc<dyn Fn(String) + Send + Sync>,
    pub notify: Arc<dyn Fn(String) + Send + Sync>,
    pub stop_requested: Arc<AtomicBool>,
    pub messages: Arc<Mutex<Vec<String>>>,
    pub args: String,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ScriptEnd {
    /// The script's return value, as JSON text.
    Returned(String),
    /// What it threw, with its stack when there is one.
    Threw(String),
    /// Stopped from outside: out of time or memory, or told to.
    Stopped,
}

pub(crate) struct Run {
    pub code: String,
    pub host: Host,
    pub memory_limit: usize,
    /// [`now_ms`] after which the script is interrupted; moved later when it
    /// starts a sub-agent.
    pub deadline_ms: Arc<AtomicU64>,
    pub hard_stop: CancellationToken,
}

/// Milliseconds on a monotonic clock, so a wall-clock step neither cuts a
/// run short nor lets it run past its limit.
pub(crate) fn now_ms() -> u64 {
    static ORIGIN: std::sync::LazyLock<std::time::Instant> =
        std::sync::LazyLock::new(std::time::Instant::now);
    ORIGIN.elapsed().as_millis() as u64
}

/// Start the script on its own thread; the receiver resolves when it ends.
pub(crate) fn spawn(run: Run) -> oneshot::Receiver<ScriptEnd> {
    let (done, ended) = oneshot::channel();
    let spawned = std::thread::Builder::new()
        .name("agix-runcode".into())
        .spawn(move || {
            let end = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt.block_on(execute(run)),
                Err(e) => ScriptEnd::Threw(format!("could not start the script runtime: {e}")),
            };
            let _ = done.send(end);
        });
    if let Err(e) = spawned {
        let (done, ended) = oneshot::channel();
        let _ = done.send(ScriptEnd::Threw(format!(
            "could not start the script thread: {e}"
        )));
        return ended;
    }
    ended
}

async fn execute(run: Run) -> ScriptEnd {
    let hard_stop = run.hard_stop.clone();
    let end = tokio::select! {
        biased;
        _ = hard_stop.cancelled() => ScriptEnd::Stopped,
        end = evaluate(run) => end,
    };
    // A stop that lands while the script is finishing still counts: what it
    // returned may rest on calls the stop cut short.
    if hard_stop.is_cancelled() {
        ScriptEnd::Stopped
    } else {
        end
    }
}

async fn evaluate(run: Run) -> ScriptEnd {
    let runtime = match AsyncRuntime::new() {
        Ok(rt) => rt,
        Err(e) => return ScriptEnd::Threw(format!("could not create the script engine: {e}")),
    };
    runtime.set_memory_limit(run.memory_limit).await;
    let (deadline, stop) = (run.deadline_ms.clone(), run.hard_stop.clone());
    runtime
        .set_interrupt_handler(Some(Box::new(move || {
            stop.is_cancelled() || now_ms() > deadline.load(Ordering::Relaxed)
        })))
        .await;
    let context = match AsyncContext::full(&runtime).await {
        Ok(c) => c,
        Err(e) => return ScriptEnd::Threw(format!("could not create the script context: {e}")),
    };

    let host = run.host;
    let code = run.code;
    let source = wrap(&code);
    let deadline = run.deadline_ms.clone();
    let stopped = run.hard_stop.clone();
    let end = context
        .async_with(async |ctx| {
            if let Err(e) = install(&ctx, host) {
                return ScriptEnd::Threw(format!("could not set up the script globals: {e}"));
            }
            let evaluated: rquickjs::Result<Promise> =
                ctx.eval_with_options(source, eval_options());
            let promise = match evaluated.catch(&ctx) {
                Ok(p) => p,
                Err(e) => return ScriptEnd::Threw(explain(e, &code)),
            };
            match promise.into_future::<String>().await.catch(&ctx) {
                Ok(json) => ScriptEnd::Returned(json),
                Err(e) => ScriptEnd::Threw(explain(e, &code)),
            }
        })
        .await;
    match end {
        ScriptEnd::Threw(_)
            if stopped.is_cancelled() || now_ms() > deadline.load(Ordering::Relaxed) =>
        {
            ScriptEnd::Stopped
        }
        other => other,
    }
}

fn eval_options() -> rquickjs::context::EvalOptions {
    let mut options = rquickjs::context::EvalOptions::default();
    options.global = true;
    options.strict = false;
    options.filename = Some("code.js".into());
    options
}

/// The model's code inside an async function, starting on the wrapper's own
/// first line so line numbers in errors match what it wrote.
fn wrap(code: &str) -> String {
    format!(
        "(async () => {{ const __result = await (async () => {{ {code}\n}})(); \
         return JSON.stringify(__result === undefined ? null : __result); }})()"
    )
}

fn install(ctx: &rquickjs::Ctx<'_>, host: Host) -> rquickjs::Result<()> {
    let bridge = Object::new(ctx.clone())?;
    let calls = host.calls.clone();
    bridge.set(
        "call",
        Function::new(
            ctx.clone(),
            Async(move |name: String, input: String| {
                let calls = calls.clone();
                async move {
                    let (reply, answer) = oneshot::channel();
                    if calls.send(CallRequest { name, input, reply }).is_err() {
                        return Ok::<_, rquickjs::Error>(stopped_answer());
                    }
                    Ok(answer.await.unwrap_or_else(|_| stopped_answer()))
                }
            }),
        )?,
    )?;
    let log = host.log.clone();
    bridge.set(
        "log",
        Function::new(ctx.clone(), move |text: String| log(text))?,
    )?;
    let notify = host.notify.clone();
    bridge.set(
        "notify",
        Function::new(ctx.clone(), move |text: String| notify(text))?,
    )?;
    let stop = host.stop_requested.clone();
    bridge.set(
        "stopRequested",
        Function::new(ctx.clone(), move || stop.load(Ordering::Relaxed))?,
    )?;
    let inbox = host.messages.clone();
    bridge.set(
        "messages",
        Function::new(ctx.clone(), move || {
            let taken: Vec<String> =
                std::mem::take(&mut *inbox.lock().unwrap_or_else(|e| e.into_inner()));
            serde_json::to_string(&taken).unwrap_or_else(|_| "[]".into())
        })?,
    )?;
    let args = host.args.clone();
    bridge.set("args", Function::new(ctx.clone(), move || args.clone())?)?;
    ctx.globals().set("__host", bridge)?;
    ctx.eval::<(), _>(PRELUDE)
}

fn stopped_answer() -> String {
    r#"{"ok":false,"error":"the run is stopping; no more calls are made"}"#.to_string()
}

/// An exception as the model should read it: the message, where, and what to
/// do instead when it reached for something that is not here.
fn explain(e: CaughtError<'_>, code: &str) -> String {
    match e {
        CaughtError::Exception(ex) => {
            let message = ex.message().unwrap_or_default();
            let stack = ex.stack().unwrap_or_default();
            let syntax_error = ex
                .get::<_, String>("name")
                .is_ok_and(|n| n == "SyntaxError");
            if let Some(line) = syntax_error
                .then(|| module_statement_line(code, &stack))
                .flatten()
            {
                return format!(
                    "line {line} uses import/export, but this runs as a script: there are no \
                     modules here. Drop it and use tools.* (or callTool) for any I/O."
                );
            }
            if message.starts_with("could not load module") {
                return format!(
                    "{message}: there are no modules here — use tools.* (or callTool) for any I/O"
                );
            }
            if stack.trim().is_empty() {
                message
            } else {
                format!("{message}\n{}", stack.trim_end())
            }
        }
        CaughtError::Value(v) => format!("{v:?}"),
        CaughtError::Error(e) => e.to_string(),
    }
}

/// The line a syntax error points at, when that line is an `import` or
/// `export` statement.
fn module_statement_line(code: &str, stack: &str) -> Option<usize> {
    let at = stack.find("code.js:")? + "code.js:".len();
    let line: usize = stack[at..]
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()?;
    let text = code.lines().nth(line.checked_sub(1)?)?.trim_start();
    ["import", "export"]
        .iter()
        .any(|kw| {
            text.strip_prefix(kw)
                .and_then(|rest| rest.chars().next())
                .is_some_and(|c| !(c.is_alphanumeric() || c == '_' || c == '$'))
        })
        .then_some(line)
}
