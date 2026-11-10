//! `ScriptCarrier` — one script bound to one hook point, with the timeout, the
//! per-turn call quota and the ledger enforced on this side of the engine
//! trait. An engine that enforced its own limits could choose not to; the
//! carrier wraps any `Arc<dyn ScriptEngine>`.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use base::interface::script::{
    ScriptEngine, ScriptError, ScriptLedger, ScriptLimits, ScriptOutcome, ScriptRecord,
    ScriptSource,
};
use base::prompt::BlockOrigin;

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

    pub fn limits(&self) -> &ScriptLimits {
        &self.limits
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
    ///
    /// Called from a runtime worker, the worker is handed off for the length
    /// of the call, so a slow script holds one thread rather than every task
    /// queued behind it on that worker.
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
        let run = || {
            self.engine
                .eval_blocking(&self.script, entry, input, &self.limits)
        };
        let on_worker = tokio::runtime::Handle::try_current()
            .is_ok_and(|h| h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread);
        if on_worker {
            tokio::task::block_in_place(run)
        } else {
            run()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::interface::script::{FnScriptEngine, RefusingEngine};
    use std::time::Duration;

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
