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
//! is the carrier crate — QuickJS behind this trait — and it is an optional
//! dependency of `daemon` under the `scripts` feature, the same way the wasm
//! carrier sits behind `plugins`. A build can carry one carrier, both, or
//! neither.
//!
//! [`RefusingEngine`] is what runs when a host wired nothing, and
//! [`FnScriptEngine`] gives a host the governance around logic written in
//! Rust.
//!
//! # Governance belongs here, not in the engine
//!
//! The timeout and the call quota are enforced by [`ScriptCarrier`], on this
//! side of the trait, because an engine that enforced its own limits would be
//! an engine that could choose not to. What the carrier *cannot* do from here
//! is stop a script that never yields: `tokio::time::timeout` abandons a
//! future, it does not interrupt a busy loop. An engine implementation must
//! provide its own interruption — every serious one has a fuel or watchdog
//! mechanism — and the carrier's timeout is the second line, not the first.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
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
                 `ScriptCarrier`",
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
/// One call, one entry point, JSON in and JSON out — the same shape the
/// command and wasm backends already speak, so a hook point does not need a
/// third input contract to gain a third backend.
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

/// One script bound to a hook point, with its budget enforced around it.
pub struct ScriptCarrier {
    engine: Arc<dyn ScriptEngine>,
    script: ScriptSource,
    /// The catalog id of the point this is bound to. Held here rather than
    /// passed in at each call so it is written down once, where the binding
    /// was checked against the catalog, and cannot be mistyped by an adapter.
    point: String,
    limits: ScriptLimits,
    calls_this_turn: AtomicU32,
    turn: AtomicU32,
    ledger: Option<Arc<ScriptLedger>>,
}

impl ScriptCarrier {
    pub fn new(
        engine: Arc<dyn ScriptEngine>,
        script: ScriptSource,
        point: impl Into<String>,
        limits: ScriptLimits,
    ) -> Self {
        Self {
            engine,
            script,
            point: point.into(),
            limits,
            calls_this_turn: AtomicU32::new(0),
            turn: AtomicU32::new(0),
            ledger: None,
        }
    }

    /// Record every call this carrier's point makes decisions about.
    pub fn with_ledger(mut self, ledger: Arc<ScriptLedger>) -> Self {
        self.ledger = Some(ledger);
        self
    }

    /// Write down what the point did with this call.
    ///
    /// Called by the adapter rather than by [`call`](Self::call), because the
    /// carrier can only see whether the script answered — whether the answer
    /// was usable, and whether it was allowed, is the point's own verdict.
    pub fn record(&self, entry: &str, outcome: ScriptOutcome) {
        let Some(ledger) = &self.ledger else {
            return;
        };
        ledger.append(ScriptRecord {
            point: self.point.clone(),
            script: self.script.id.clone(),
            entry: entry.to_string(),
            turn: self.turn.load(Ordering::Relaxed),
            outcome,
        });
    }

    pub fn script(&self) -> &ScriptSource {
        &self.script
    }

    /// The catalog id of the point this is bound to.
    pub fn point(&self) -> &str {
        &self.point
    }

    /// Whose code this is, which is what decides its authority at a point.
    pub fn origin(&self) -> &BlockOrigin {
        &self.script.origin
    }

    /// A new turn has started; the quota resets.
    pub fn begin_turn(&self) {
        self.calls_this_turn.store(0, Ordering::Relaxed);
        self.turn.fetch_add(1, Ordering::Relaxed);
    }

    /// Run the script, within its budget.
    ///
    /// A script that exhausts its quota or its clock fails *this call* and
    /// nothing else: the turn continues without whatever the script would have
    /// contributed. A hook point that cannot survive its extension failing is
    /// a hook point that hands every extension author a way to break the
    /// engine.
    pub async fn call(
        &self,
        entry: &str,
        input: serde_json::Value,
    ) -> Result<serde_json::Value, ScriptError> {
        let used = self.calls_this_turn.fetch_add(1, Ordering::Relaxed);
        if used >= self.limits.calls_per_turn {
            return Err(ScriptError::QuotaExhausted {
                calls_per_turn: self.limits.calls_per_turn,
            });
        }
        match tokio::time::timeout(
            self.limits.timeout,
            self.engine.eval(&self.script, entry, input, &self.limits),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(ScriptError::TimedOut {
                after: self.limits.timeout,
            }),
        }
    }

    /// Run the script from a synchronous hook point, within the same budget.
    ///
    /// The quota is the same counter, so a script bound at both a
    /// synchronous and an asynchronous point shares one budget rather than
    /// getting two. The clock is the engine's, for the reason given on
    /// [`ScriptEngine::eval_blocking`]: there is no future here to time out.
    pub fn call_blocking(
        &self,
        entry: &str,
        input: serde_json::Value,
    ) -> Result<serde_json::Value, ScriptError> {
        let used = self.calls_this_turn.fetch_add(1, Ordering::Relaxed);
        if used >= self.limits.calls_per_turn {
            return Err(ScriptError::QuotaExhausted {
                calls_per_turn: self.limits.calls_per_turn,
            });
        }
        self.engine
            .eval_blocking(&self.script, entry, input, &self.limits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn script(origin: BlockOrigin) -> ScriptSource {
        ScriptSource {
            id: "./.agix/scripts/prompt.js".into(),
            origin,
            code: "export function onAssemble(p) { return p }".into(),
        }
    }

    fn carrier(engine: Arc<dyn ScriptEngine>, limits: ScriptLimits) -> ScriptCarrier {
        ScriptCarrier::new(
            engine,
            script(BlockOrigin::Script("./.agix/scripts/prompt.js".into())),
            "prompt.assemble",
            limits,
        )
    }

    #[tokio::test]
    async fn with_no_engine_a_call_says_what_is_missing() {
        let c = carrier(Arc::new(RefusingEngine), ScriptLimits::default());
        let err = c
            .call("onAssemble", serde_json::json!({}))
            .await
            .unwrap_err();
        assert_eq!(err, ScriptError::NoEngine);
        assert!(err.to_string().contains("no script engine"), "{err}");
    }

    #[tokio::test]
    async fn a_script_that_never_returns_is_abandoned_at_its_budget() {
        let engine = Arc::new(FnScriptEngine(|_: &ScriptSource, _: &str, _| async {
            std::future::pending::<()>().await;
            unreachable!()
        }));
        let c = carrier(
            engine,
            ScriptLimits {
                timeout: Duration::from_millis(20),
                ..Default::default()
            },
        );
        let started = std::time::Instant::now();
        let err = c
            .call("onAssemble", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, ScriptError::TimedOut { .. }), "{err:?}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the caller must not wait on a script that never returns"
        );
    }

    /// The isolation that matters: one carrier's stuck script costs its own
    /// call and nothing else's.
    #[tokio::test]
    async fn one_stuck_script_does_not_stop_another_from_running() {
        let stuck = carrier(
            Arc::new(FnScriptEngine(|_: &ScriptSource, _: &str, _| async {
                std::future::pending::<()>().await;
                unreachable!()
            })),
            ScriptLimits {
                timeout: Duration::from_millis(50),
                ..Default::default()
            },
        );
        let fine = carrier(
            Arc::new(FnScriptEngine(
                |_: &ScriptSource, _: &str, input| async move { Ok(input) },
            )),
            ScriptLimits::default(),
        );

        let (stuck_out, fine_out) = tokio::join!(
            stuck.call("onAssemble", serde_json::json!({})),
            fine.call("onAssemble", serde_json::json!({"ok": true}))
        );
        assert!(matches!(stuck_out, Err(ScriptError::TimedOut { .. })));
        assert_eq!(fine_out, Ok(serde_json::json!({"ok": true})));
    }

    #[tokio::test]
    async fn the_quota_bounds_a_turn_and_resets_with_it() {
        let engine = Arc::new(FnScriptEngine(|_: &ScriptSource, _: &str, _| async {
            Ok(serde_json::json!("ran"))
        }));
        let c = carrier(
            engine,
            ScriptLimits {
                calls_per_turn: 2,
                ..Default::default()
            },
        );

        assert!(c.call("f", serde_json::json!({})).await.is_ok());
        assert!(c.call("f", serde_json::json!({})).await.is_ok());
        assert_eq!(
            c.call("f", serde_json::json!({})).await.unwrap_err(),
            ScriptError::QuotaExhausted { calls_per_turn: 2 }
        );

        c.begin_turn();
        assert!(
            c.call("f", serde_json::json!({})).await.is_ok(),
            "a new turn is a new budget"
        );
    }

    /// The ledger is a tail, not a log, and it says so. A record that fell
    /// off the front and a call that never happened must not read alike.
    #[test]
    fn a_full_ledger_drops_the_oldest_and_counts_what_it_dropped() {
        let ledger = ScriptLedger::with_capacity(2);
        for i in 0..5 {
            ledger.append(ScriptRecord {
                point: "tool.result".into(),
                script: "s.js".into(),
                entry: format!("call{i}"),
                turn: 1,
                outcome: ScriptOutcome::Applied,
            });
        }
        let kept: Vec<String> = ledger.records().into_iter().map(|r| r.entry).collect();
        assert_eq!(kept, vec!["call3", "call4"]);
        assert_eq!(ledger.dropped(), 3);
    }

    /// The turn a call belongs to comes from the carrier, so a record can say
    /// "this fired on the second turn" — which is the only way to check a
    /// quota that resets per turn, or a point that must fire once per turn.
    #[tokio::test]
    async fn a_record_carries_the_turn_the_call_belonged_to() {
        let ledger = Arc::new(ScriptLedger::new());
        let c = ScriptCarrier::new(
            Arc::new(RefusingEngine),
            script(BlockOrigin::Script("./.agix/scripts/prompt.js".into())),
            "tool.result",
            ScriptLimits::default(),
        )
        .with_ledger(ledger.clone());

        c.record("onResult", ScriptOutcome::Applied);
        c.begin_turn();
        c.record("onResult", ScriptOutcome::Applied);

        let turns: Vec<u32> = ledger.records().into_iter().map(|r| r.turn).collect();
        assert_eq!(
            turns,
            vec![0, 1],
            "zero is before the first turn, where the registering points make \
             their identity calls"
        );
        let first = &ledger.records()[0];
        assert_eq!(first.point, "tool.result");
        assert_eq!(first.script, "./.agix/scripts/prompt.js");
    }

    /// A carrier nobody asked to record is not an error and not a panic — the
    /// ledger is an observation the host opts into, not a dependency of the
    /// carrier working.
    #[test]
    fn a_carrier_with_no_ledger_records_into_nothing() {
        let c = carrier(Arc::new(RefusingEngine), ScriptLimits::default());
        c.record("onResult", ScriptOutcome::Applied);
    }

    /// Provenance rides with the script, because it is what a hook point uses
    /// to decide what the script may do — see `prompt_assembly::Authority`.
    #[test]
    fn a_script_carries_whose_it_is() {
        let own = carrier(Arc::new(RefusingEngine), ScriptLimits::default());
        assert!(own.origin().is_local());

        let downloaded = ScriptCarrier::new(
            Arc::new(RefusingEngine),
            script(BlockOrigin::Plugin("example".into())),
            "prompt.assemble",
            ScriptLimits::default(),
        );
        assert!(!downloaded.origin().is_local());
    }
}

/// A script bound to the prompt-assembly hook point.
///
/// The bridge between "the operator wrote some code" and "the prompt came out
/// different": the script is handed the assembled blocks as JSON, returns the
/// blocks it wants, and every difference is applied through
/// [`PromptAssembly`](crate::interface::prompt_assembly::PromptAssembly) —
/// which is what subjects it to the same authority rules as any other
/// extension. A script the operator wrote may rewrite anything; a script that
/// arrived with a downloaded plugin may add.
///
/// A script that fails, times out or returns nonsense leaves the prompt as it
/// was. That is not politeness: a prompt half-edited by a script that died
/// mid-pass is worse than an unedited one, because nothing downstream can tell
/// which it is looking at.
pub struct PromptAssemblyScript {
    carrier: Arc<ScriptCarrier>,
    entry: String,
}

impl PromptAssemblyScript {
    /// `entry` is the function the script exports, e.g. `onAssemble`.
    ///
    /// The carrier is shared rather than owned because the quota it counts is
    /// per *turn*, and something outside the adapter has to say when a turn
    /// began. A carrier moved into an adapter can never be told.
    pub fn new(carrier: Arc<ScriptCarrier>, entry: impl Into<String>) -> Self {
        Self {
            carrier,
            entry: entry.into(),
        }
    }

    /// What the script is given: one object per block, in order.
    fn encode(blocks: &[crate::prompt::PromptBlock]) -> serde_json::Value {
        serde_json::Value::Array(
            blocks
                .iter()
                .map(|b| {
                    serde_json::json!({
                        "name": b.name,
                        "content": b.content,
                    })
                })
                .collect(),
        )
    }
}

/// What came back, in the shape the applier can act on.
#[derive(serde::Deserialize)]
struct ReturnedBlock {
    name: Option<String>,
    content: String,
}

#[async_trait]
impl crate::interface::prompt_assembly::AsyncAssemblyHook for PromptAssemblyScript {
    async fn on_assemble_async(
        &self,
        assembly: &mut crate::interface::prompt_assembly::PromptAssembly,
        _ctx: &crate::interface::scene::ScenePromptContext<'_>,
    ) -> Result<(), String> {
        let before = Self::encode(assembly.blocks());
        let returned = match self.carrier.call(&self.entry, before).await {
            Ok(v) => v,
            Err(error) => {
                let message = error.to_string();
                self.carrier
                    .record(&self.entry, ScriptOutcome::Failed { error });
                return Err(message);
            }
        };

        let returned: Vec<ReturnedBlock> = match serde_json::from_value(returned) {
            Ok(blocks) => blocks,
            Err(e) => {
                let message = format!("script returned something that is not a block list: {e}");
                self.carrier.record(
                    &self.entry,
                    ScriptOutcome::NoChange {
                        detail: Some(message.clone()),
                    },
                );
                return Err(message);
            }
        };

        // Diffed rather than replaced wholesale, so every change goes through
        // the authority checks one at a time. Handing back a list and swapping
        // it in would let a script "add" a prompt that happens to be missing
        // the block it was not allowed to remove.
        let existing: Vec<(String, String)> = assembly
            .blocks()
            .iter()
            .filter_map(|b| b.name.clone().map(|n| (n, b.content.clone())))
            .collect();

        // A denied edit does not cancel the permitted ones. A script that
        // asked for one thing it may not do still gets everything it may —
        // the alternative punishes an author for a single overreach by
        // silently dropping the rest of their pass.
        let mut refused = Vec::new();
        let mut edits = 0usize;

        // Occurrences are paired in order, not matched by name alone. The
        // kernel gives every unnamed section of a scene the same name, so a
        // prompt normally holds several blocks called `scene.skeleton` — and
        // an edit addressed by name lands on the first of them. Comparing the
        // k-th returned block against the k-th existing one is what makes
        // handing the list back unchanged cost nothing; without it, an
        // identity pass rewrote the first section with the second's text.
        let mut seen: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
        for block in &returned {
            let Some(name) = block.name.as_deref() else {
                continue;
            };
            let occurrences: Vec<&String> = existing
                .iter()
                .filter(|(n, _)| n == name)
                .map(|(_, c)| c)
                .collect();
            let nth = seen.entry(name).or_insert(0);
            let index = *nth;
            *nth += 1;

            match occurrences.get(index) {
                // Handing a block back unchanged is not an edit, and must not
                // be charged as one: a script that returns the whole list to
                // change one line would otherwise need `modify` for all of it.
                Some(content) if **content == block.content => {}
                // An edit to one of several blocks sharing a name cannot be
                // expressed: the assembly addresses by name and would apply it
                // to the first. Refused rather than misapplied — a script that
                // silently edited the wrong section would be worse than one
                // that was told it could not.
                Some(_) if occurrences.len() > 1 => refused.push(format!(
                    "`{name}` names {} blocks, so an edit to one of them cannot be addressed",
                    occurrences.len()
                )),
                Some(_) => match assembly.modify(name, block.content.clone()) {
                    Ok(_) => edits += 1,
                    Err(e) => refused.push(e.to_string()),
                },
                None => {
                    assembly.push(
                        crate::prompt::PromptBlock::system(block.content.clone())
                            .named(name)
                            .from_origin(self.carrier.origin().clone()),
                    );
                    edits += 1;
                }
            }
        }

        // A name is removed when fewer blocks came back under it than went
        // out. Which one of several would go is unanswerable for the same
        // reason an edit to one of them is.
        for name in existing
            .iter()
            .map(|(n, _)| n.as_str())
            .collect::<std::collections::BTreeSet<_>>()
        {
            let went_out = existing.iter().filter(|(n, _)| n == name).count();
            let came_back = seen.get(name).copied().unwrap_or(0);
            if came_back >= went_out {
                continue;
            }
            if went_out > 1 {
                refused.push(format!(
                    "`{name}` names {went_out} blocks, so removing one of them cannot be addressed"
                ));
                continue;
            }
            match assembly.remove(name) {
                Ok(_) => edits += 1,
                Err(e) => refused.push(e.to_string()),
            }
        }

        for denial in &refused {
            self.carrier.record(
                &self.entry,
                ScriptOutcome::Refused {
                    detail: denial.clone(),
                },
            );
        }
        self.carrier.record(
            &self.entry,
            if edits > 0 {
                ScriptOutcome::Applied
            } else {
                ScriptOutcome::NoChange { detail: None }
            },
        );

        if refused.is_empty() {
            Ok(())
        } else {
            Err(refused.join("; "))
        }
    }
}

#[cfg(test)]
mod prompt_hook_tests {
    use super::*;
    use crate::interface::prompt_assembly::{
        run_async_assembly_hooks, AssemblyCapabilities, AsyncAssemblyHook, Authority,
    };
    use crate::prompt::{names, PromptBlock};

    fn ctx() -> crate::interface::scene::ScenePromptContext<'static> {
        use std::borrow::Cow;
        crate::interface::scene::ScenePromptContext {
            cwd: Cow::Borrowed("/tmp"),
            os: Cow::Borrowed("linux"),
            shell: Cow::Borrowed("bash"),
            home_dir: Cow::Borrowed("/home/user"),
            date: Cow::Borrowed("2026-06-10"),
            model_name: Cow::Borrowed("test-model"),
            skills_text: None,
            mcp_instructions: None,
            session_memory: None,
            is_git: false,
            git_branch: None,
            is_worktree: false,
            git_status: None,
            language: None,
            scratchpad_dir: None,
            output_style_content: None,
            available_tools: None,
            tool_results_ever_cleared: false,
        }
    }

    fn prompt() -> Vec<PromptBlock> {
        vec![
            PromptBlock::system("you are an agent").named(names::SCENE_SKELETON),
            PromptBlock::system("skills: a, b").named(names::SKILLS_CATALOG),
            PromptBlock::system("rules: no rm -rf").named(names::RULES),
        ]
    }

    /// Stands in for a real interpreter: it receives the blocks as JSON,
    /// transforms them, and hands them back — exactly the contract a scripted
    /// `onAssemble` would satisfy.
    fn engine_that(
        f: impl Fn(Vec<serde_json::Value>) -> Vec<serde_json::Value> + Send + Sync + Clone + 'static,
    ) -> Arc<dyn ScriptEngine> {
        Arc::new(FnScriptEngine(
            move |_s: &ScriptSource, _e: &str, input: serde_json::Value| {
                let f = f.clone();
                async move {
                    let blocks: Vec<serde_json::Value> = serde_json::from_value(input)
                        .map_err(|e| ScriptError::Failed(e.to_string()))?;
                    Ok(serde_json::Value::Array(f(blocks)))
                }
            },
        ))
    }

    fn script_hook(engine: Arc<dyn ScriptEngine>, origin: BlockOrigin) -> PromptAssemblyScript {
        PromptAssemblyScript::new(
            Arc::new(ScriptCarrier::new(
                engine,
                ScriptSource {
                    id: "./.agix/scripts/prompt.js".into(),
                    origin,
                    code: String::new(),
                },
                "prompt.assemble",
                ScriptLimits::default(),
            )),
            "onAssemble",
        )
    }

    /// Several blocks can share a name, and handing them all back unchanged
    /// must leave every one of them alone.
    ///
    /// The kernel gives every unnamed section of a scene the same name, so a
    /// real prompt holds several `scene.skeleton` blocks. Matching a returned
    /// block to an existing one by name alone finds the first every time: the
    /// second and third sections then read as edits *to the first*, and a
    /// script that asked for nothing rewrote the opening of the system prompt
    /// with the text of a later section. Occurrences are paired in order for
    /// this reason.
    #[tokio::test]
    async fn several_blocks_can_share_a_name_and_an_identity_pass_leaves_them_all() {
        let mut assembly = crate::interface::prompt_assembly::PromptAssembly::new(
            vec![
                PromptBlock::system("first section").named(names::SCENE_SKELETON),
                PromptBlock::system("second section").named(names::SCENE_SKELETON),
                PromptBlock::system("third section").named(names::SCENE_SKELETON),
            ],
            Authority::local(BlockOrigin::Script("./.agix/scripts/prompt.js".into())),
        );

        let hook = script_hook(
            engine_that(|blocks| blocks),
            BlockOrigin::Script("./.agix/scripts/prompt.js".into()),
        );
        hook.on_assemble_async(&mut assembly, &ctx())
            .await
            .expect("an identity pass asks for nothing and cannot be refused");

        let contents: Vec<&str> = assembly
            .blocks()
            .iter()
            .map(|b| b.content.as_str())
            .collect();
        assert_eq!(
            contents,
            vec!["first section", "second section", "third section"],
            "a pass that changed nothing rewrote the prompt"
        );
    }

    /// And an edit to one of them is refused rather than applied to whichever
    /// came first.
    #[tokio::test]
    async fn an_edit_to_one_of_several_blocks_sharing_a_name_is_refused() {
        let mut assembly = crate::interface::prompt_assembly::PromptAssembly::new(
            vec![
                PromptBlock::system("first section").named(names::SCENE_SKELETON),
                PromptBlock::system("second section").named(names::SCENE_SKELETON),
            ],
            Authority::local(BlockOrigin::Script("./.agix/scripts/prompt.js".into())),
        );

        let hook = script_hook(
            engine_that(|mut blocks| {
                blocks[1]["content"] = serde_json::json!("second section, edited");
                blocks
            }),
            BlockOrigin::Script("./.agix/scripts/prompt.js".into()),
        );
        let refused = hook
            .on_assemble_async(&mut assembly, &ctx())
            .await
            .expect_err("an edit nothing can address must be reported");
        assert!(refused.contains("names 2 blocks"), "{refused}");

        let contents: Vec<&str> = assembly
            .blocks()
            .iter()
            .map(|b| b.content.as_str())
            .collect();
        assert_eq!(
            contents,
            vec!["first section", "second section"],
            "the edit was applied to a block the script did not name"
        );
    }

    /// The acceptance case: a script rewrites a prompt block, and the prompt
    /// that goes out is different. No recompilation of the engine involved —
    /// the code came from outside it.
    #[tokio::test]
    async fn a_script_the_operator_wrote_rewrites_a_block() {
        let engine = engine_that(|blocks| {
            blocks
                .into_iter()
                .map(|mut b| {
                    if b["name"] == serde_json::json!(names::SKILLS_CATALOG) {
                        b["content"] = serde_json::json!("skills: (curated by script)");
                    }
                    b
                })
                .collect()
        });
        let hook = script_hook(
            engine,
            BlockOrigin::Script("./.agix/scripts/prompt.js".into()),
        );
        let hooks: Vec<(Arc<dyn AsyncAssemblyHook>, Authority)> = vec![(
            Arc::new(hook),
            Authority::local(BlockOrigin::Script("./.agix/scripts/prompt.js".into())),
        )];

        let out = run_async_assembly_hooks(prompt(), &hooks, &ctx()).await;
        let skills = out
            .iter()
            .find(|b| b.name.as_deref() == Some(names::SKILLS_CATALOG))
            .unwrap();
        assert_eq!(skills.content, "skills: (curated by script)");
    }

    /// The same script, arriving with a downloaded plugin that declared
    /// nothing: its additions land, its deletion does not.
    #[tokio::test]
    async fn a_downloaded_script_is_held_to_the_same_rules_as_any_plugin() {
        let engine = engine_that(|blocks| {
            let mut kept: Vec<serde_json::Value> = blocks
                .into_iter()
                .filter(|b| b["name"] != serde_json::json!(names::RULES))
                .collect();
            kept.push(serde_json::json!({"name": "example.note", "content": "hello"}));
            kept
        });
        let hook = script_hook(engine, BlockOrigin::Plugin("example".into()));
        let hooks: Vec<(Arc<dyn AsyncAssemblyHook>, Authority)> = vec![(
            Arc::new(hook),
            Authority::plugin("example", AssemblyCapabilities::default()),
        )];

        let out = run_async_assembly_hooks(prompt(), &hooks, &ctx()).await;
        let names_out: Vec<&str> = out.iter().filter_map(|b| b.name.as_deref()).collect();
        assert!(
            names_out.contains(&names::RULES),
            "an undeclared removal must not take effect: {names_out:?}"
        );
        assert!(
            names_out.contains(&"example.note"),
            "its addition must stand: {names_out:?}"
        );
    }

    #[tokio::test]
    async fn a_script_that_returns_nonsense_leaves_the_prompt_alone() {
        let engine: Arc<dyn ScriptEngine> = Arc::new(FnScriptEngine(
            |_s: &ScriptSource, _e: &str, _i: serde_json::Value| async {
                Ok(serde_json::json!("not a block list"))
            },
        ));
        let hook = script_hook(
            engine,
            BlockOrigin::Script("./.agix/scripts/prompt.js".into()),
        );
        let hooks: Vec<(Arc<dyn AsyncAssemblyHook>, Authority)> = vec![(
            Arc::new(hook),
            Authority::local(BlockOrigin::Script("./.agix/scripts/prompt.js".into())),
        )];

        let out = run_async_assembly_hooks(prompt(), &hooks, &ctx()).await;
        assert_eq!(out, prompt(), "a broken script is not a prompt edit");
    }

    #[tokio::test]
    async fn a_script_that_hangs_leaves_the_prompt_alone() {
        let engine: Arc<dyn ScriptEngine> = Arc::new(FnScriptEngine(
            |_s: &ScriptSource, _e: &str, _i: serde_json::Value| async {
                std::future::pending::<()>().await;
                unreachable!()
            },
        ));
        let hook = PromptAssemblyScript::new(
            Arc::new(ScriptCarrier::new(
                engine,
                ScriptSource {
                    id: "./.agix/scripts/slow.js".into(),
                    origin: BlockOrigin::Script("./.agix/scripts/slow.js".into()),
                    code: String::new(),
                },
                "prompt.assemble",
                ScriptLimits {
                    timeout: Duration::from_millis(20),
                    ..Default::default()
                },
            )),
            "onAssemble",
        );
        let hooks: Vec<(Arc<dyn AsyncAssemblyHook>, Authority)> = vec![(
            Arc::new(hook),
            Authority::local(BlockOrigin::Script("./.agix/scripts/slow.js".into())),
        )];

        let out = run_async_assembly_hooks(prompt(), &hooks, &ctx()).await;
        assert_eq!(out, prompt(), "the turn goes on without the script's edits");
    }
}
