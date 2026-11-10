//! `memory.retrieval_hook` — the recall question before it is asked, the
//! recalled names before they are used.

use std::sync::Arc;

use crate::carrier::ScriptCarrier;
use base::interface::memory_contracts::{RetrievalHook, RetrievalRequest};
use base::interface::script::ScriptOutcome;

/// A script bound to both ends of recall.
///
/// # One entry, two phases
///
/// The point is two methods and a binding names one function, so the phase
/// travels in the input: the script is called once with `"phase": "before"`
/// and once with `"phase": "after"`, and answers whichever it cares about.
/// The alternative — a second function found by a naming convention — would
/// put half the configuration somewhere the binding never mentions, so a
/// misspelled name reads as a script that simply does nothing.
///
/// # What the script receives
///
/// ```json
/// {
///   "phase": "before",
///   "query": "what did we decide about deploys",
///   "limit": 5,
///   "alreadySurfaced": ["deploy-window"],
///   "recentTools": ["Bash", "Read"],
///   "modelName": "claude-opus-4",
///   "sessionId": "01J..."
/// }
/// ```
///
/// The `"after"` call carries the same fields plus `"names"`, the list the
/// retriever produced, best first.
///
/// # What the script returns
///
/// For `"before"`, an object. `query` (a string) becomes the question;
/// `limit` (a positive integer) becomes the ceiling. Both optional.
///
/// For `"after"`, an array of strings: the names to keep, in the order the
/// model should see them. A name that is not in the store is dropped
/// downstream, so inventing one is inert rather than an error.
///
/// Anything else in either phase — a number, an object where an array
/// belongs, an array with a non-string in it, nothing at all — leaves recall
/// exactly as it was. That is also what a script with a bug returns, and the
/// two should have the same harmless outcome.
///
/// # Authority
///
/// Nothing here branches on [`ScriptCarrier::origin`], because this point has
/// no reduced mode to branch into: there is no "may narrow but not widen"
/// version of a recall filter the way prompt assembly has an add-only version
/// of an edit. A script that may run here may do both halves of the job, and
/// deciding otherwise inside an adapter would be the adapter inventing a
/// permission rather than reading one.
pub struct RetrievalHookScript {
    carrier: Arc<ScriptCarrier>,
    entry: String,
}

impl RetrievalHookScript {
    pub fn new(carrier: Arc<ScriptCarrier>, entry: impl Into<String>) -> Self {
        Self {
            carrier,
            entry: entry.into(),
        }
    }

    fn encode(request: &RetrievalRequest, phase: &str) -> serde_json::Value {
        // Sorted because a `HashSet` iterates in an order that changes
        // between runs, and a script handed a different input on identical
        // state is a script nobody can debug.
        let mut surfaced: Vec<&str> = request
            .already_surfaced
            .iter()
            .map(String::as_str)
            .collect();
        surfaced.sort_unstable();

        serde_json::json!({
            "phase": phase,
            "query": request.query,
            "limit": request.limit,
            "alreadySurfaced": surfaced,
            "recentTools": request.recent_tools,
            "modelName": request.model_name,
            "sessionId": request.session_id,
        })
    }

    fn run(
        &self,
        request: &RetrievalRequest,
        phase: &str,
        extra: Option<(&str, serde_json::Value)>,
    ) -> Option<serde_json::Value> {
        let mut input = Self::encode(request, phase);
        if let Some((key, value)) = extra {
            if let Some(obj) = input.as_object_mut() {
                obj.insert(key.to_string(), value);
            }
        }
        match self.carrier.call_blocking(&self.entry, input) {
            Ok(v) => Some(v),
            Err(error) => {
                tracing::warn!(
                    script = %self.carrier.script().id,
                    phase,
                    error = %error,
                    "retrieval-hook script did not run; recall is unchanged"
                );
                self.carrier
                    .record(&self.entry, ScriptOutcome::Failed { error });
                None
            }
        }
    }

    /// This point calls one entry twice per recall, so its two records are
    /// told apart by their order — `before` then `after` — rather than by
    /// anything in the record itself.
    fn unusable(&self, detail: &str) {
        self.carrier.record(
            &self.entry,
            ScriptOutcome::NoChange {
                detail: Some(detail.to_string()),
            },
        );
    }
}

impl RetrievalHook for RetrievalHookScript {
    fn before_retrieve(&self, request: &mut RetrievalRequest) {
        let Some(returned) = self.run(request, "before", None) else {
            return;
        };
        let Some(obj) = returned.as_object() else {
            self.unusable("`before` returned something that is not an object");
            return;
        };

        // Read both out before writing either: a script that asks for a good
        // query and a nonsensical limit must leave the request untouched
        // rather than half moved.
        let query = match obj.get("query") {
            Some(v) => match v.as_str() {
                Some(s) => Some(s.to_string()),
                None => {
                    self.unusable("`before` returned a query that is not a string");
                    return;
                }
            },
            None => None,
        };
        let limit = match obj.get("limit") {
            Some(v) => match v.as_u64().filter(|n| *n > 0) {
                Some(n) => Some(n as usize),
                None => {
                    self.unusable("`before` returned a limit that is not a positive number");
                    return;
                }
            },
            None => None,
        };

        let asked = query.is_some() || limit.is_some();
        if let Some(query) = query {
            request.query = query;
        }
        if let Some(limit) = limit {
            request.limit = limit;
        }
        if asked {
            self.carrier.record(&self.entry, ScriptOutcome::Applied);
        } else {
            self.carrier
                .record(&self.entry, ScriptOutcome::NoChange { detail: None });
        }
    }

    fn after_retrieve(&self, request: &RetrievalRequest, names: &mut Vec<String>) {
        let extra = serde_json::Value::Array(
            names
                .iter()
                .map(|n| serde_json::Value::String(n.clone()))
                .collect(),
        );
        let Some(returned) = self.run(request, "after", Some(("names", extra))) else {
            return;
        };
        let Some(items) = returned.as_array() else {
            self.unusable("`after` returned something that is not an array");
            return;
        };
        let mut kept = Vec::with_capacity(items.len());
        for item in items {
            match item.as_str() {
                Some(s) => kept.push(s.to_string()),
                None => {
                    self.unusable("`after` returned a name that is not a string");
                    return;
                }
            }
        }
        *names = kept;
        self.carrier.record(&self.entry, ScriptOutcome::Applied);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::interface::script::{FnScriptEngine, ScriptEngine, ScriptLimits, ScriptSource};
    use base::prompt::BlockOrigin;
    use std::collections::HashSet;

    /// An engine that answers with a fixed value, whatever it is asked.
    struct Says(serde_json::Value);

    #[async_trait::async_trait]
    impl ScriptEngine for Says {
        async fn eval(
            &self,
            _s: &ScriptSource,
            _e: &str,
            _i: serde_json::Value,
            _l: &ScriptLimits,
        ) -> Result<serde_json::Value, base::interface::script::ScriptError> {
            Ok(self.0.clone())
        }
        fn eval_blocking(
            &self,
            _s: &ScriptSource,
            _e: &str,
            _i: serde_json::Value,
            _l: &ScriptLimits,
        ) -> Result<serde_json::Value, base::interface::script::ScriptError> {
            Ok(self.0.clone())
        }
    }

    fn hook(engine: Arc<dyn ScriptEngine>) -> RetrievalHookScript {
        RetrievalHookScript::new(
            Arc::new(ScriptCarrier::new(
                engine,
                ScriptSource {
                    id: "./recall.js".into(),
                    origin: BlockOrigin::Script("./recall.js".into()),
                    code: String::new(),
                },
                "memory.retrieval_hook",
                ScriptLimits::default(),
            )),
            "onRetrieval",
        )
    }

    fn request() -> RetrievalRequest {
        RetrievalRequest {
            query: "ship it".into(),
            limit: 5,
            already_surfaced: HashSet::from(["seen".to_string()]),
            recent_tools: vec!["Bash".into()],
            model_name: "test-model".into(),
            session_id: Some("s1".into()),
        }
    }

    #[test]
    fn a_script_rewrites_the_question_and_narrows_the_answer() {
        let h = hook(Arc::new(Says(
            serde_json::json!({"query": "deploy", "limit": 2}),
        )));
        let mut req = request();
        h.before_retrieve(&mut req);
        assert_eq!(req.query, "deploy");
        assert_eq!(req.limit, 2);

        let h = hook(Arc::new(Says(serde_json::json!(["b", "a"]))));
        let mut names = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        h.after_retrieve(&request(), &mut names);
        assert_eq!(names, ["b", "a"], "the script's order is the order");
    }

    /// The phase is the only thing that tells a script which half it is in,
    /// and `names` is only there for the half that has an answer to filter.
    #[test]
    fn each_phase_says_which_it_is() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = seen.clone();
        struct Recorder(Arc<std::sync::Mutex<Vec<serde_json::Value>>>);
        #[async_trait::async_trait]
        impl ScriptEngine for Recorder {
            async fn eval(
                &self,
                _s: &ScriptSource,
                _e: &str,
                i: serde_json::Value,
                _l: &ScriptLimits,
            ) -> Result<serde_json::Value, base::interface::script::ScriptError> {
                self.0.lock().unwrap().push(i);
                Ok(serde_json::Value::Null)
            }
            fn eval_blocking(
                &self,
                _s: &ScriptSource,
                _e: &str,
                i: serde_json::Value,
                _l: &ScriptLimits,
            ) -> Result<serde_json::Value, base::interface::script::ScriptError> {
                self.0.lock().unwrap().push(i);
                Ok(serde_json::Value::Null)
            }
        }

        let h = hook(Arc::new(Recorder(recorded)));
        let mut req = request();
        h.before_retrieve(&mut req);
        h.after_retrieve(&req, &mut vec!["a".to_string()]);

        let seen = seen.lock().unwrap();
        assert_eq!(seen[0]["phase"], "before");
        assert!(seen[0].get("names").is_none());
        assert_eq!(seen[1]["phase"], "after");
        assert_eq!(seen[1]["names"], serde_json::json!(["a"]));
        assert_eq!(seen[0]["alreadySurfaced"], serde_json::json!(["seen"]));
    }

    #[test]
    fn a_script_that_answers_with_nonsense_changes_nothing() {
        for answer in [
            serde_json::json!(42),
            serde_json::json!({"query": 7}),
            serde_json::json!({"limit": 0}),
            serde_json::json!({"query": "deploy", "limit": -1}),
            serde_json::json!(null),
        ] {
            let h = hook(Arc::new(Says(answer.clone())));
            let mut req = request();
            h.before_retrieve(&mut req);
            assert_eq!(req.query, "ship it", "answer was {answer}");
            assert_eq!(req.limit, 5, "answer was {answer}");
        }

        for answer in [
            serde_json::json!("a"),
            serde_json::json!({"names": ["a"]}),
            serde_json::json!(["a", 3]),
        ] {
            let h = hook(Arc::new(Says(answer.clone())));
            let mut names = vec!["a".to_string(), "b".to_string()];
            h.after_retrieve(&request(), &mut names);
            assert_eq!(names, ["a", "b"], "answer was {answer}");
        }
    }

    /// A script that dies mid-recall leaves the turn recalling what it would
    /// have recalled anyway.
    #[test]
    fn a_script_that_fails_leaves_recall_alone() {
        let engine: Arc<dyn ScriptEngine> =
            Arc::new(FnScriptEngine(|_: &ScriptSource, _: &str, _| async {
                Err(base::interface::script::ScriptError::Failed("boom".into()))
            }));
        // `FnScriptEngine` has no blocking half, so the synchronous path
        // fails before the closure is ever reached — which is the same
        // outcome from the point's side, and the one being pinned.
        let h = hook(engine);
        let mut req = request();
        h.before_retrieve(&mut req);
        assert_eq!(req.query, "ship it");

        let mut names = vec!["a".to_string()];
        h.after_retrieve(&req, &mut names);
        assert_eq!(names, ["a"]);
    }
}
