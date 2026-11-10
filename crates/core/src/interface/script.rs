//! `ScriptEngine` — the cheap tier of extension backend.
//!
//! A hook point today can be answered three ways: a Rust implementation
//! compiled in, a command hook that pays for a subprocess, or a prompt hook
//! that pays for a model call. There is nothing between "recompile the engine"
//! and "spawn a process", so doing something small at a hook point — rewrite
//! one line of a prompt, tag a tool result, decide whether to retry — costs
//! either a release or several milliseconds and a PID.
//!
//! This is the missing tier: run a piece of the operator's own code, in this
//! process, in microseconds.
//!
//! # The engine is a separate crate, behind a feature
//!
//! This module is the contract and the governance around it, never an
//! interpreter: `base` has no internal dependencies and linking a JavaScript
//! runtime into it would put one in every build of everything. `script-host`
//! is the carrier crate — the `ScriptCarrier` that enforces the budget, the
//! adapters for the bindable points and QuickJS behind this trait — and it is
//! an optional dependency of `daemon` under the `scripts` feature.
//!
//! [`RefusingEngine`] is what runs when a host wired nothing, and
//! [`FnScriptEngine`] gives a host the governance around logic written in
//! Rust.
//!
//! # Governance is outside the engine
//!
//! The timeout and the call quota are enforced by `script_host::carrier::ScriptCarrier`,
//! on this side of the trait, because an engine that enforced its own limits would be
//! an engine that could choose not to. What the carrier *cannot* do from here
//! is stop a script that never yields: `tokio::time::timeout` abandons a
//! future, it does not interrupt a busy loop. An engine implementation must
//! provide its own interruption — every serious one has a fuel or watchdog
//! mechanism — and the carrier's timeout is the second line, not the first.

use std::sync::atomic::Ordering;
use std::time::Duration;

use async_trait::async_trait;

use crate::prompt::BlockOrigin;

/// A piece of code to run, and whose it is.
#[derive(Debug, Clone)]
pub struct ScriptSource {
    /// What to call this in errors and logs — usually its path.
    pub id: String,
    /// Whose code this is. Decides what it is allowed to do at a hook point;
    /// see [`crate::interface::prompt_assembly::Authority`].
    pub origin: BlockOrigin,
    pub code: String,
}

/// What a script may spend.
#[derive(Debug, Clone, Copy)]
pub struct ScriptLimits {
    /// Wall clock for one call.
    pub timeout: Duration,
    /// How many times it may run in a turn. Bounds a hook that is called per
    /// tool result from turning a pathological turn into a pathological bill.
    pub calls_per_turn: u32,
}

impl Default for ScriptLimits {
    fn default() -> Self {
        Self {
            // Generous for the work this tier is for — a string rewrite, a
            // decision — and short enough that a stuck script is a hiccup
            // rather than an outage.
            timeout: Duration::from_millis(100),
            calls_per_turn: 1_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScriptError {
    /// No engine is available to run it.
    NoEngine,
    /// It ran too long and was abandoned.
    TimedOut { after: Duration },
    /// It has already run as often as it may this turn.
    QuotaExhausted { calls_per_turn: u32 },
    /// The engine refused or the script failed.
    Failed(String),
}

impl std::fmt::Display for ScriptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoEngine => f.write_str(
                "no script engine is available in this build; a host supplies one through \
                 `script_host`",
            ),
            Self::TimedOut { after } => write!(f, "script exceeded its {after:?} budget"),
            Self::QuotaExhausted { calls_per_turn } => {
                write!(f, "script already ran {calls_per_turn} times this turn")
            }
            Self::Failed(e) => write!(f, "script failed: {e}"),
        }
    }
}

/// Runs a script.
///
/// One call, one entry point, JSON in and JSON out — the same shape a command
/// hook already speaks, so a hook point does not need a second input contract
/// to gain a second backend.
#[async_trait]
pub trait ScriptEngine: Send + Sync {
    /// Call `entry` in `script` with `input`.
    ///
    /// Implementations must be able to interrupt a script that does not
    /// return — see the module docs. `limits` is passed for that reason, not
    /// so the engine can decide whether to honor it; the carrier enforces the
    /// same budget from outside regardless.
    async fn eval(
        &self,
        script: &ScriptSource,
        entry: &str,
        input: serde_json::Value,
        limits: &ScriptLimits,
    ) -> Result<serde_json::Value, ScriptError>;

    /// The same call, from somewhere that cannot await.
    ///
    /// Most hook points are synchronous by contract and always will be: a
    /// prompt variable provider is a plain `Fn`, a tool-result transformer
    /// takes `&self` and returns nothing. A script bound to one of those has
    /// to run somewhere, and the only honest answer is the calling thread.
    ///
    /// That is affordable because a script's clock is enforced inside the
    /// interpreter rather than by a future that can be dropped — the deadline
    /// travels with `limits`, so the worst case is one thread held for
    /// `limits.timeout`, not until the script decides to stop. An engine
    /// whose work is genuinely asynchronous should not implement this; the
    /// default refuses rather than blocking a runtime from inside itself.
    fn eval_blocking(
        &self,
        _script: &ScriptSource,
        _entry: &str,
        _input: serde_json::Value,
        _limits: &ScriptLimits,
    ) -> Result<serde_json::Value, ScriptError> {
        Err(ScriptError::Failed(
            "this script engine cannot be called from a synchronous hook point".into(),
        ))
    }
}

/// The engine when there is none.
///
/// Every call fails, with a message that says what is missing rather than
/// what went wrong. A hook point wired to scripts in a build with no engine
/// should be inert and legible, not mysterious.
pub struct RefusingEngine;

#[async_trait]
impl ScriptEngine for RefusingEngine {
    async fn eval(
        &self,
        _script: &ScriptSource,
        _entry: &str,
        _input: serde_json::Value,
        _limits: &ScriptLimits,
    ) -> Result<serde_json::Value, ScriptError> {
        Err(ScriptError::NoEngine)
    }
}

/// An "engine" that is a Rust closure.
///
/// The second implementation the contract needs, and not only for tests: a
/// host that wants the hook-point plumbing — the quota, the budget, the
/// origin-based authority — without an interpreter can express its logic
/// natively and get all of it.
pub struct FnScriptEngine<F>(pub F);

#[async_trait]
impl<F, Fut> ScriptEngine for FnScriptEngine<F>
where
    F: Fn(&ScriptSource, &str, serde_json::Value) -> Fut + Send + Sync,
    Fut: std::future::Future<Output = Result<serde_json::Value, ScriptError>> + Send,
{
    async fn eval(
        &self,
        script: &ScriptSource,
        entry: &str,
        input: serde_json::Value,
        _limits: &ScriptLimits,
    ) -> Result<serde_json::Value, ScriptError> {
        (self.0)(script, entry, input).await
    }
}

/// What one script call did, once the point it was bound to had its say.
///
/// The distinction the outcomes exist to draw: a script that ran and asked for
/// nothing, a script that never ran, and a script that asked for something it
/// may not do all leave the engine in the same state. Only a record can tell
/// them apart, and "the extension is inert" versus "the extension is being
/// held back" is exactly the question an operator asks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScriptOutcome {
    /// The script ran and the point took its answer.
    ///
    /// Taken, not necessarily different: a script that answers with the value
    /// a field already holds still gets this. What a point can honestly report
    /// is whether it acted on what it was told, and an observer script that
    /// deliberately changes nothing should still read as having run.
    Applied,
    /// The script ran and the point is as it was — because the script asked
    /// for no change, or because it answered in a shape this point cannot act
    /// on. Those two are one outcome on purpose: most points define them to
    /// mean the same thing, and `detail` says which it was where the point
    /// can tell.
    NoChange { detail: Option<String> },
    /// The call itself did not complete.
    Failed { error: ScriptError },
    /// The script asked for something its origin does not permit. One record
    /// per refused edit, so a pass that was half permitted reads as one.
    Refused { detail: String },
}

/// One line of the ledger.
#[derive(Debug, Clone)]
pub struct ScriptRecord {
    pub point: String,
    /// The script's id — its path, normally.
    pub script: String,
    pub entry: String,
    /// Which user turn this call belongs to. Zero is before the first turn
    /// began, which is when the registering points make their identity calls.
    pub turn: u32,
    pub outcome: ScriptOutcome,
}

/// Every script call, in the order the points made them.
///
/// A script cannot keep a record of its own: each call gets a fresh runtime
/// with no filesystem and nothing carried over, so the only thing it can say
/// is its return value at its own point. Whether it ran at all — and if it did
/// not, why — is only answerable from out here.
///
/// Bounded, because a long session at a per-tool-call point would otherwise
/// grow one forever, and the count of what was dropped is kept: a record that
/// fell off the end and a call that never happened must not read the same.
pub struct ScriptLedger {
    entries: std::sync::Mutex<std::collections::VecDeque<ScriptRecord>>,
    dropped: std::sync::atomic::AtomicUsize,
    capacity: usize,
}

impl Default for ScriptLedger {
    fn default() -> Self {
        Self::with_capacity(4096)
    }
}

impl ScriptLedger {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            entries: std::sync::Mutex::new(std::collections::VecDeque::new()),
            dropped: std::sync::atomic::AtomicUsize::new(0),
            capacity: capacity.max(1),
        }
    }

    pub fn append(&self, record: ScriptRecord) {
        let mut entries = match self.entries.lock() {
            Ok(e) => e,
            Err(poisoned) => poisoned.into_inner(),
        };
        while entries.len() >= self.capacity {
            entries.pop_front();
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        entries.push_back(record);
    }

    pub fn records(&self) -> Vec<ScriptRecord> {
        match self.entries.lock() {
            Ok(e) => e.iter().cloned().collect(),
            Err(poisoned) => poisoned.into_inner().iter().cloned().collect(),
        }
    }

    /// How many records fell off the front. Non-zero means [`records`] is a
    /// tail, not the whole story.
    ///
    /// [`records`]: Self::records
    pub fn dropped(&self) -> usize {
        self.dropped.load(Ordering::Relaxed)
    }
}
