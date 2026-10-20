//! The script carrier's engine: QuickJS, behind
//! [`base::interface::script::ScriptEngine`].
//!
//! This is the cheapest tier of hook-point backend — the operator's own code,
//! in this process, in microseconds, between "recompile the engine" and "spawn
//! a subprocess". The contract, the per-turn quota and the wall-clock budget
//! live in `base::interface::script`; this is only the interpreter behind them.
//!
//! # A fresh runtime per call
//!
//! Every call builds a QuickJS runtime, evaluates the script into it, calls the
//! entry point and throws the runtime away. That costs more than keeping one
//! warm, and it buys something worth more at this stage: no state survives a
//! call, so one session's script cannot stash anything another session's script
//! can see, and a script cannot accumulate memory across a turn. Pooling
//! runtimes is an optimization to make once there is a reason to; sharing one
//! between sessions is a decision that would need arguing for.
//!
//! # Interruption is the engine's job
//!
//! `ScriptCarrier` puts a timeout around the future, and a timeout abandons a
//! future without stopping a busy loop — `while(true){}` would keep a thread
//! spinning after the caller gave up. So the runtime carries its own deadline
//! through QuickJS's interrupt handler, which fires between bytecode
//! instructions and can stop code that never yields. The carrier's budget is
//! the second line; this is the first.
//!
//! # What a script can reach
//!
//! Nothing. No filesystem, no network, no clock beyond `Date`, no host
//! bindings at all. A script gets its input and returns its output, both as
//! JSON. Capabilities are declared and resolved through
//! `base::interface::capabilities` like every other carrier's, and until
//! something is wired to grant one, the honest answer is that this carrier
//! grants none.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use base::interface::script::{ScriptError, ScriptLimits, ScriptSource};

/// QuickJS.
pub struct QuickJsEngine {
    /// Ceiling on a single runtime's heap. Independent of the per-call
    /// timeout: a script can exhaust memory quickly or slowly.
    memory_limit_bytes: usize,
}

impl Default for QuickJsEngine {
    fn default() -> Self {
        Self {
            // Generous for string work, small enough that a script cannot
            // starve the process it is a guest in.
            memory_limit_bytes: 16 * 1024 * 1024,
        }
    }
}

impl QuickJsEngine {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_memory_limit(mut self, bytes: usize) -> Self {
        self.memory_limit_bytes = bytes;
        self
    }
}

/// Wrap the operator's code so the whole exchange is one call with strings on
/// both sides.
///
/// JSON in and JSON out rather than converting Rust values through QuickJS's
/// type system: the contract is already JSON-shaped, and going through
/// `JSON.parse` / `JSON.stringify` means a script sees exactly what a command
/// hook or a wasm component would see for the same point.
///
/// The user's code goes inside the function body, so `function onAssemble(…)`
/// hoists and is in scope by the time it is called.
fn wrap(code: &str, entry: &str) -> String {
    format!(
        "(function (__atta_input) {{\n\
         {code}\n\
         if (typeof {entry} !== 'function') {{\n\
           throw new Error('script does not export a function named {entry}');\n\
         }}\n\
         var __atta_out = {entry}(JSON.parse(__atta_input));\n\
         return __atta_out === undefined ? 'null' : JSON.stringify(__atta_out);\n\
         }})"
    )
}

fn run_blocking(
    code: String,
    entry: String,
    input: String,
    deadline: Instant,
    memory_limit_bytes: usize,
) -> Result<String, ScriptError> {
    let runtime = rquickjs::Runtime::new().map_err(|e| ScriptError::Failed(e.to_string()))?;
    runtime.set_memory_limit(memory_limit_bytes);
    // Fires between instructions, which is what makes a script that never
    // yields stoppable. Returning true unwinds the interpreter.
    runtime.set_interrupt_handler(Some(Box::new(move || Instant::now() >= deadline)));

    let context =
        rquickjs::Context::full(&runtime).map_err(|e| ScriptError::Failed(e.to_string()))?;

    context.with(|ctx| {
        let source = wrap(&code, &entry);
        let func: rquickjs::Function = ctx
            .eval(source.as_bytes())
            .map_err(|e| script_error(&ctx, e, deadline))?;
        let out: String = func
            .call((input,))
            .map_err(|e| script_error(&ctx, e, deadline))?;
        Ok(out)
    })
}

/// Turn a QuickJS error into something a caller can act on.
///
/// An interrupted script surfaces as an ordinary exception, so "did it run out
/// of time" is answered by the clock rather than by the message — QuickJS does
/// not distinguish them and a string match on its wording would break on the
/// next release.
fn script_error(ctx: &rquickjs::Ctx<'_>, e: rquickjs::Error, deadline: Instant) -> ScriptError {
    if Instant::now() >= deadline {
        return ScriptError::TimedOut {
            after: Duration::ZERO,
        };
    }
    if let rquickjs::Error::Exception = e {
        let caught = ctx.catch();
        if let Some(exception) = caught.as_exception() {
            let message = exception
                .message()
                .unwrap_or_else(|| "uncaught exception".to_string());
            return match exception.stack() {
                Some(stack) if !stack.trim().is_empty() => {
                    ScriptError::Failed(format!("{message}\n{stack}"))
                }
                _ => ScriptError::Failed(message),
            };
        }
    }
    ScriptError::Failed(e.to_string())
}

#[async_trait]
impl base::interface::script::ScriptEngine for QuickJsEngine {
    async fn eval(
        &self,
        script: &ScriptSource,
        entry: &str,
        input: serde_json::Value,
        limits: &ScriptLimits,
    ) -> Result<serde_json::Value, ScriptError> {
        let input = serde_json::to_string(&input)
            .map_err(|e| ScriptError::Failed(format!("input is not serializable: {e}")))?;
        let code = script.code.clone();
        let entry = entry.to_string();
        let id = script.id.clone();
        // The deadline is the engine's copy of the carrier's budget, because
        // the carrier's `timeout` cannot reach inside a running interpreter.
        let deadline = Instant::now() + limits.timeout;
        let memory_limit_bytes = self.memory_limit_bytes;

        // JavaScript is synchronous and CPU-bound. On an ordinary task it
        // would hold a runtime worker for its whole execution; a script that
        // spins would hold one until its deadline.
        let joined = tokio::task::spawn_blocking(move || {
            run_blocking(code, entry, input, deadline, memory_limit_bytes)
        })
        .await;

        let out = match joined {
            Ok(result) => result?,
            Err(e) => {
                tracing::warn!(script = %id, error = %e, "script task did not finish");
                return Err(ScriptError::Failed(format!("script task failed: {e}")));
            }
        };

        serde_json::from_str(&out).map_err(|e| {
            ScriptError::Failed(format!("script returned something that is not JSON: {e}"))
        })
    }

    /// QuickJS is synchronous; the asynchronous path above exists only to keep
    /// a CPU-bound interpreter off a runtime worker. From a synchronous hook
    /// point there is no worker to protect and nothing to hand off to, so this
    /// is the same interpreter run on the calling thread.
    fn eval_blocking(
        &self,
        script: &ScriptSource,
        entry: &str,
        input: serde_json::Value,
        limits: &ScriptLimits,
    ) -> Result<serde_json::Value, ScriptError> {
        let input = serde_json::to_string(&input)
            .map_err(|e| ScriptError::Failed(format!("input is not serializable: {e}")))?;
        let out = run_blocking(
            script.code.clone(),
            entry.to_string(),
            input,
            Instant::now() + limits.timeout,
            self.memory_limit_bytes,
        )?;
        serde_json::from_str(&out).map_err(|e| {
            ScriptError::Failed(format!("script returned something that is not JSON: {e}"))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::interface::script::{ScriptCarrier, ScriptEngine};
    use base::prompt::BlockOrigin;
    use std::sync::Arc;

    fn script(code: &str) -> ScriptSource {
        ScriptSource {
            id: "./.atta/scripts/test.js".into(),
            origin: BlockOrigin::Script("./.atta/scripts/test.js".into()),
            code: code.to_string(),
        }
    }

    async fn call(
        code: &str,
        entry: &str,
        input: serde_json::Value,
    ) -> Result<serde_json::Value, ScriptError> {
        QuickJsEngine::new()
            .eval(&script(code), entry, input, &ScriptLimits::default())
            .await
    }

    #[tokio::test]
    async fn a_script_transforms_its_input() {
        let out = call(
            "function onAssemble(blocks) { return blocks.map(b => b.name); }",
            "onAssemble",
            serde_json::json!([{"name": "a"}, {"name": "b"}]),
        )
        .await
        .expect("the script runs");
        assert_eq!(out, serde_json::json!(["a", "b"]));
    }

    #[tokio::test]
    async fn a_script_returning_nothing_returns_null_rather_than_failing() {
        let out = call("function f() {}", "f", serde_json::json!({}))
            .await
            .expect("the script runs");
        assert_eq!(out, serde_json::Value::Null);
    }

    #[tokio::test]
    async fn a_missing_entry_point_says_which_one() {
        let err = call("function other() {}", "onAssemble", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, ScriptError::Failed(m) if m.contains("onAssemble")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn a_thrown_error_comes_back_as_a_failure_with_its_message() {
        let err = call(
            "function f() { throw new Error('deliberate'); }",
            "f",
            serde_json::json!({}),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&err, ScriptError::Failed(m) if m.contains("deliberate")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn a_syntax_error_is_a_failure_not_a_panic() {
        let err = call("function f( {", "f", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, ScriptError::Failed(_)), "{err:?}");
    }

    /// The property the carrier's timeout alone cannot deliver: a script that
    /// never yields is stopped, not merely abandoned.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_infinite_loop_is_interrupted_rather_than_left_spinning() {
        let engine = QuickJsEngine::new();
        let started = Instant::now();
        let err = engine
            .eval(
                &script("function f() { while (true) {} }"),
                "f",
                serde_json::json!({}),
                &ScriptLimits {
                    timeout: Duration::from_millis(50),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        let elapsed = started.elapsed();

        assert!(matches!(err, ScriptError::TimedOut { .. }), "{err:?}");
        assert!(
            elapsed < Duration::from_secs(5),
            "the interpreter must have been stopped, not waited out: {elapsed:?}"
        );
    }

    /// Through the carrier, which is how the engine is actually reached: the
    /// budget and the quota are enforced outside it and the interruption
    /// inside it, and both hold.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_stuck_script_costs_its_own_call_and_no_other() {
        let engine: Arc<dyn ScriptEngine> = Arc::new(QuickJsEngine::new());
        let stuck = ScriptCarrier::new(
            engine.clone(),
            script("function f() { while (true) {} }"),
            "prompt.assemble",
            ScriptLimits {
                timeout: Duration::from_millis(50),
                ..Default::default()
            },
        );
        let fine = ScriptCarrier::new(
            engine,
            script("function f(x) { return x; }"),
            "prompt.assemble",
            ScriptLimits::default(),
        );

        let (stuck_out, fine_out) = tokio::join!(
            stuck.call("f", serde_json::json!({})),
            fine.call("f", serde_json::json!({"ok": true}))
        );
        assert!(
            matches!(stuck_out, Err(ScriptError::TimedOut { .. })),
            "{stuck_out:?}"
        );
        assert_eq!(fine_out, Ok(serde_json::json!({"ok": true})));
    }

    /// Nothing survives a call. Two calls to the same carrier cannot see each
    /// other's globals, which is what makes one session's script unable to
    /// leave anything for another's.
    #[tokio::test]
    async fn no_state_survives_between_calls() {
        let engine = QuickJsEngine::new();
        let code = "function f() { \
                      if (typeof globalThis.__seen === 'undefined') { globalThis.__seen = 0; } \
                      globalThis.__seen += 1; \
                      return globalThis.__seen; \
                    }";
        for _ in 0..3 {
            let out = engine
                .eval(
                    &script(code),
                    "f",
                    serde_json::json!({}),
                    &ScriptLimits::default(),
                )
                .await
                .expect("runs");
            assert_eq!(
                out,
                serde_json::json!(1),
                "a fresh runtime starts from nothing"
            );
        }
    }

    /// A script cannot reach the host: no `require`, no `process`, no `fetch`.
    #[tokio::test]
    async fn a_script_has_no_way_out() {
        for probe in ["require", "process", "fetch", "XMLHttpRequest", "Deno"] {
            let out = call(
                &format!("function f() {{ return typeof {probe}; }}"),
                "f",
                serde_json::json!({}),
            )
            .await
            .expect("the probe itself runs");
            assert_eq!(
                out,
                serde_json::json!("undefined"),
                "`{probe}` is reachable from a script"
            );
        }
    }
}

/// Turning `settings.scripts` into registrations on a prompt registry.
///
/// Kept here rather than in the composition root so that the whole path from
/// "a line in settings.json" to "a script running at a point" lives with the
/// carrier that runs it, and so a build without this crate has no half of it.
pub mod bindings {
    use std::path::Path;
    use std::sync::Arc;

    use base::interface::prompt_assembly::{AssemblyCapabilities, Authority};
    use base::interface::script::{
        PromptAssemblyScript, ScriptCarrier, ScriptEngine, ScriptLimits, ScriptSource,
    };
    use base::prompt::BlockOrigin;
    use base::settings::ScriptBinding;

    /// Points a script may be bound to today.
    ///
    /// A short list on purpose. Every entry is a place where a script's cost
    /// is bounded and its authority is defined; adding one means answering
    /// both questions for the new place, not appending a string here.
    /// The points a script can be bound to today.
    ///
    /// The catalog's `script` column says what the *contract* allows; this
    /// says what the carrier has an adapter for. They are not the same list
    /// and saying so is the point — a binding to a point that is open in
    /// principle and unimplemented here is refused at startup with a message
    /// naming what is available, rather than accepted and silently never run.
    pub const BINDABLE_POINTS: &[&str] = &[
        "prompt.assemble",
        "prompt.block",
        "prompt.context",
        "prompt.variable",
        "tool.around",
        "tool.result",
        "memory.retrieval_hook",
        "model.request",
        "model.message",
    ];

    pub use base::interface::script_adapters::BoundScripts;

    /// Why a binding could not be honored.
    ///
    /// Every variant is a startup failure rather than a warning. A script that
    /// silently never runs is worse than one that refuses to load: the author
    /// changes their prompt, sees no difference, and concludes the engine
    /// ignored them.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum BindingError {
        Unreadable { path: String, reason: String },
        UnknownPoint { point: String },
        UnbindablePoint { point: String },
    }

    impl std::fmt::Display for BindingError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::Unreadable { path, reason } => {
                    write!(f, "script `{path}` could not be read: {reason}")
                }
                Self::UnknownPoint { point } => write!(
                    f,
                    "no extension point is called `{point}` — see docs/extension_points.md"
                ),
                Self::UnbindablePoint { point } => write!(
                    f,
                    "`{point}` exists but scripts cannot be bound to it; today that is {}",
                    BINDABLE_POINTS.join(", ")
                ),
            }
        }
    }

    /// Where a set of bindings came from, which is what decides what they
    /// may do.
    ///
    /// The two differ in more than provenance. A project's bindings resolve
    /// against the project root and are judged by where the file landed —
    /// the operator's own tree means the operator's authority. A package's
    /// resolve against the package's own directory and carry the package's
    /// name, stated rather than inferred: inferring it from the path is only
    /// correct while the plugin cache happens to sit outside every project
    /// root, which is a fact about default path layout and not something
    /// anything enforces. A deployment that put its scene root inside a
    /// checkout would have handed a downloaded package the authority to
    /// rewrite the prompt.
    pub enum BindingSource<'a> {
        Project { root: &'a Path },
        Package { name: &'a str, root: &'a Path },
    }

    /// Where a project script came from, which is what decides what it may do.
    ///
    /// Inside the project root means the operator wrote it, and it gets their
    /// authority. Anywhere else — a shared location, an absolute path out of
    /// tree — means it arrived from outside, and it may add and no more. The
    /// check is on the resolved path rather than on anything the binding
    /// declares, because a declaration is exactly what an outside script
    /// would lie in.
    fn origin_of(path: &Path, project_root: &Path) -> BlockOrigin {
        let inside = path
            .canonicalize()
            .ok()
            .zip(project_root.canonicalize().ok())
            .map(|(p, root)| p.starts_with(root))
            .unwrap_or(false);
        if inside {
            BlockOrigin::Script(path.display().to_string())
        } else {
            BlockOrigin::Plugin(path.display().to_string())
        }
    }

    fn authority_for(origin: &BlockOrigin) -> Authority {
        if origin.is_local() {
            Authority::local(origin.clone())
        } else {
            // Nothing declared anything, so nothing beyond adding.
            Authority::plugin(
                match origin {
                    BlockOrigin::Plugin(name) => name.clone(),
                    _ => "script".to_string(),
                },
                AssemblyCapabilities::default(),
            )
        }
    }

    /// The engine behind the script carrier, as the trait the rest of the
    /// engine talks to.
    ///
    /// Exists so a host binding its own extension points gets this carrier's
    /// quota, wall clock, provenance and ledger rather than linking a second
    /// interpreter and growing a second answer to "what may an extension do".
    pub fn engine() -> Arc<dyn ScriptEngine> {
        Arc::new(crate::QuickJsEngine::new())
    }

    /// Everything a host does to honor a `scripts` section: pick the engine,
    /// register the bindings, hand back the answer.
    ///
    /// `Ok(None)` means nothing was configured — the overwhelming case, and
    /// the one where a session should pay for neither an interpreter nor a
    /// prompt registry of its own.
    ///
    /// One place rather than one per host, because the copies are where a
    /// harness and the daemon drift apart: a test binding a point production
    /// does not (or the reverse) is a test that stays green while saying
    /// nothing about the thing it names.
    pub fn bind_quickjs(
        bindings: &[ScriptBinding],
        project_root: &Path,
    ) -> Result<Option<BoundScripts>, BindingError> {
        if bindings.is_empty() {
            return Ok(None);
        }
        bind(engine(), bindings, project_root).map(Some)
    }

    /// Register every binding, or say which one is wrong.
    ///
    /// All-or-nothing: a partially applied script configuration is a
    /// configuration nobody wrote.
    pub fn bind(
        engine: Arc<dyn ScriptEngine>,
        bindings: &[ScriptBinding],
        project_root: &Path,
    ) -> Result<BoundScripts, BindingError> {
        let mut out = BoundScripts::default();
        bind_into(
            &mut out,
            engine,
            bindings,
            &BindingSource::Project { root: project_root },
        )?;
        Ok(out)
    }

    /// Add a whole set of bindings to `out`, or add none of them.
    ///
    /// `out` rather than a fresh value because the ledger lives on it: two
    /// sets bound into separate values would keep separate records and count
    /// their per-turn quotas apart, so a turn resetting one would leave the
    /// other running on a budget nobody reset.
    ///
    /// Nothing is installed until every binding has been read, so a failure
    /// leaves `out` exactly as it was.
    pub fn bind_into(
        out: &mut BoundScripts,
        engine: Arc<dyn ScriptEngine>,
        bindings: &[ScriptBinding],
        source: &BindingSource<'_>,
    ) -> Result<(), BindingError> {
        let mut prepared = Vec::with_capacity(bindings.len());
        for binding in bindings {
            prepared.push(prepare(binding, source)?);
        }
        install(out, &engine, prepared)
    }

    /// Add what can be added, and hand back what could not with the reason.
    ///
    /// The lenient half of the pair, and the difference is whose mistake it
    /// is. An operator's `scripts` section is one document by one author, so
    /// half of it is a document nobody wrote — [`bind_into`] refuses the lot.
    /// A set assembled from several installed packages has no such author:
    /// dropping it whole would let one bad package silently take away every
    /// other package's contributions. So a package's bindings fail one at a
    /// time, and the caller reports each failure against the package it came
    /// from.
    pub fn bind_lenient(
        out: &mut BoundScripts,
        engine: Arc<dyn ScriptEngine>,
        bindings: &[ScriptBinding],
        source: &BindingSource<'_>,
    ) -> Vec<BindingError> {
        let mut errors = Vec::new();
        for binding in bindings {
            match prepare(binding, source) {
                // One at a time, so a later binding cannot be lost to an
                // earlier one's failure.
                Ok(one) => {
                    if let Err(e) = install(out, &engine, vec![one]) {
                        errors.push(e);
                    }
                }
                Err(e) => errors.push(e),
            }
        }
        errors
    }

    /// One binding, read and judged: the code, its budget, and the authority
    /// its provenance earns it.
    type Prepared = (ScriptBinding, ScriptSource, ScriptLimits, Authority);

    fn prepare(
        binding: &ScriptBinding,
        source: &BindingSource<'_>,
    ) -> Result<Prepared, BindingError> {
        if base::interface::catalog::find(&binding.point).is_none() {
            return Err(BindingError::UnknownPoint {
                point: binding.point.clone(),
            });
        }
        if !BINDABLE_POINTS.contains(&binding.point.as_str()) {
            return Err(BindingError::UnbindablePoint {
                point: binding.point.clone(),
            });
        }

        let (path, origin) = match source {
            BindingSource::Project { root } => {
                let path = if binding.path.is_absolute() {
                    binding.path.clone()
                } else {
                    root.join(&binding.path)
                };
                let origin = origin_of(&path, root);
                (path, origin)
            }
            BindingSource::Package { name, root } => {
                let path = root.join(&binding.path);
                // A package's file has to be the package's. The manifest
                // layer refuses a path that could leave the directory, and
                // this refuses one that left it anyway — through a symlink,
                // which no amount of reading the manifest can see.
                if !path
                    .canonicalize()
                    .ok()
                    .zip(root.canonicalize().ok())
                    .map(|(p, r)| p.starts_with(r))
                    .unwrap_or(false)
                {
                    return Err(BindingError::Unreadable {
                        path: path.display().to_string(),
                        reason: "it is not inside the package that declared it".to_string(),
                    });
                }
                (path, BlockOrigin::Plugin((*name).to_string()))
            }
        };

        let code = std::fs::read_to_string(&path).map_err(|e| BindingError::Unreadable {
            path: path.display().to_string(),
            reason: e.to_string(),
        })?;
        let defaults = ScriptLimits::default();
        Ok((
            binding.clone(),
            ScriptSource {
                id: path.display().to_string(),
                origin: origin.clone(),
                code,
            },
            ScriptLimits {
                timeout: binding
                    .timeout_ms
                    .map(std::time::Duration::from_millis)
                    .unwrap_or(defaults.timeout),
                calls_per_turn: binding.calls_per_turn.unwrap_or(defaults.calls_per_turn),
            },
            authority_for(&origin),
        ))
    }

    fn install(
        out: &mut BoundScripts,
        engine: &Arc<dyn ScriptEngine>,
        prepared: Vec<Prepared>,
    ) -> Result<(), BindingError> {
        for (binding, source, limits, authority) in prepared {
            let carrier = Arc::new(
                ScriptCarrier::new(engine.clone(), source, &binding.point, limits)
                    .with_ledger(out.ledger.clone()),
            );
            out.carriers.push(carrier.clone());
            match binding.point.as_str() {
                "prompt.assemble" => out.assembly_hooks.push((
                    Arc::new(PromptAssemblyScript::new(carrier, &binding.entry)),
                    authority,
                )),
                // A contribution that could not say what it contributes is
                // not registered, and that is the whole failure: the prompt is
                // the one that would have been assembled with nothing bound.
                "prompt.block" => out.prompt_blocks.extend(
                    base::interface::script_adapters::prompt_block_from_script(
                        carrier,
                        &binding.entry,
                    ),
                ),
                "prompt.context" => out.prompt_blocks.extend(
                    base::interface::script_adapters::prompt_context_from_script(
                        carrier,
                        &binding.entry,
                    ),
                ),
                "prompt.variable" => out.prompt_variables.extend(
                    base::interface::script_adapters::prompt_variable_from_script(
                        carrier,
                        &binding.entry,
                    ),
                ),
                "tool.around" => out.tool_middleware.push(Arc::new(
                    base::interface::script_adapters::ToolAroundScript::new(
                        carrier,
                        &binding.entry,
                    ),
                )),
                "model.request" => out.model_interceptors.push(Arc::new(
                    base::interface::script_adapters::ModelRequestScript::new(
                        carrier,
                        &binding.entry,
                    ),
                )),
                "model.message" => out.model_interceptors.push(Arc::new(
                    base::interface::script_adapters::ModelMessageScript::new(
                        carrier,
                        &binding.entry,
                    ),
                )),
                "memory.retrieval_hook" => out.retrieval_hooks.push(Arc::new(
                    base::interface::script_adapters::RetrievalHookScript::new(
                        carrier,
                        &binding.entry,
                    ),
                )),
                "tool.result" => out.tool_results.push(Arc::new(
                    base::interface::script_adapters::ToolResultScript::new(
                        carrier,
                        &binding.entry,
                    ),
                )),
                // Unreachable: `BINDABLE_POINTS` gates this above, and the two
                // are meant to be read together. A point added to that list
                // and not to this match is a script bound to nothing, so it
                // fails loudly rather than joining the set silently.
                other => {
                    return Err(BindingError::UnbindablePoint {
                        point: other.to_string(),
                    })
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod binding_tests {
    use super::bindings::*;
    use super::QuickJsEngine;
    use base::interface::prompt_registry::{InMemoryPromptRegistry, PromptRegistry};
    use base::interface::script::ScriptEngine;
    use base::prompt::{names, PromptBlock};
    use base::settings::ScriptBinding;
    use std::sync::Arc;

    fn ctx() -> base::interface::scene::ScenePromptContext<'static> {
        use std::borrow::Cow;
        base::interface::scene::ScenePromptContext {
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

    fn binding(path: &str) -> ScriptBinding {
        ScriptBinding {
            path: path.into(),
            point: "prompt.assemble".into(),
            entry: "onAssemble".into(),
            timeout_ms: None,
            calls_per_turn: None,
        }
    }

    /// The whole point of the ticket, end to end: a file on disk plus a line
    /// of configuration changes the prompt, with nothing recompiled.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_script_file_and_a_config_line_rewrite_the_prompt() {
        let project = tempdir();
        let script = project.join("prompt.js");
        std::fs::write(
            &script,
            "function onAssemble(blocks) {\n\
               return blocks.map(function (b) {\n\
                 if (b.name === 'skills.catalog') { b.content = 'skills: (curated)'; }\n\
                 return b;\n\
               });\n\
             }",
        )
        .unwrap();

        let registry = InMemoryPromptRegistry::new();
        let engine: Arc<dyn ScriptEngine> = Arc::new(QuickJsEngine::new());
        let bound = bind(engine, &[binding("prompt.js")], &project).expect("the binding is valid");
        assert_eq!(bound.assembly_hooks.len(), 1);
        bound.apply_to_registry(registry.as_ref());

        let out = base::interface::prompt_assembly::run_async_assembly_hooks(
            prompt(),
            &registry.async_assembly_hooks(),
            &ctx(),
        )
        .await;
        let skills = out
            .iter()
            .find(|b| b.name.as_deref() == Some(names::SKILLS_CATALOG))
            .expect("still there");
        assert_eq!(skills.content, "skills: (curated)");
    }

    /// A script outside the project did not come from the operator, so it may
    /// add and no more — the same rule any downloaded extension is held to,
    /// decided by where the file is rather than by what the binding claims.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_script_from_outside_the_project_may_only_add() {
        let project = tempdir();
        let elsewhere = tempdir();
        let script = elsewhere.join("prompt.js");
        std::fs::write(
            &script,
            "function onAssemble(blocks) {\n\
               var kept = blocks.filter(function (b) { return b.name !== 'rules'; });\n\
               kept.push({ name: 'outside.note', content: 'hello' });\n\
               return kept;\n\
             }",
        )
        .unwrap();

        let registry = InMemoryPromptRegistry::new();
        let engine: Arc<dyn ScriptEngine> = Arc::new(QuickJsEngine::new());
        bind(engine, &[binding(script.to_str().unwrap())], &project)
            .expect("the binding is valid")
            .apply_to_registry(registry.as_ref());

        let out = base::interface::prompt_assembly::run_async_assembly_hooks(
            prompt(),
            &registry.async_assembly_hooks(),
            &ctx(),
        )
        .await;
        let named: Vec<&str> = out.iter().filter_map(|b| b.name.as_deref()).collect();
        assert!(
            named.contains(&names::RULES),
            "an outside script must not be able to delete a kernel block: {named:?}"
        );
        assert!(
            named.contains(&"outside.note"),
            "its addition still stands: {named:?}"
        );
    }

    #[test]
    fn a_binding_naming_a_point_that_does_not_exist_is_refused() {
        let engine: Arc<dyn ScriptEngine> = Arc::new(QuickJsEngine::new());
        let mut b = binding("prompt.js");
        b.point = "prompt.nonexistent".into();
        let err = bind(engine, &[b], &tempdir()).unwrap_err();
        assert_eq!(
            err,
            BindingError::UnknownPoint {
                point: "prompt.nonexistent".into()
            }
        );
        assert!(err.to_string().contains("extension_points.md"), "{err}");
    }

    #[test]
    fn a_binding_to_a_point_scripts_may_not_use_says_which_they_may() {
        let engine: Arc<dyn ScriptEngine> = Arc::new(QuickJsEngine::new());
        let mut b = binding("prompt.js");
        // A real point, and one a script has no business in.
        b.point = "event.sink".into();
        let err = bind(engine, &[b], &tempdir()).unwrap_err();
        assert!(
            matches!(err, BindingError::UnbindablePoint { .. }),
            "{err:?}"
        );
        assert!(err.to_string().contains("prompt.assemble"), "{err}");
    }

    /// A synchronous point, end to end. `tool.result` hands the script the
    /// call and the draft and takes back the text — and it is a `&self`
    /// method with nowhere to await, so this also exercises the blocking path
    /// through the interpreter.
    #[test]
    fn a_script_rewrites_a_tool_result() {
        use base::interface::tool_result::ToolResultDraft;

        let project = tempdir();
        std::fs::write(
            project.join("result.js"),
            "function onResult(r) { return `[${r.tool}] ` + r.text.toUpperCase(); }",
        )
        .unwrap();

        let mut b = binding("result.js");
        b.point = "tool.result".into();
        b.entry = "onResult".into();

        let engine: Arc<dyn ScriptEngine> = Arc::new(QuickJsEngine::new());
        let bound = bind(engine, &[b], &project).expect("the binding is valid");
        assert_eq!(bound.tool_results.len(), 1);

        let call = base::interface::tool_middleware::ToolCall {
            name: "Read".into(),
            input: serde_json::json!({"path": "a.txt"}),
        };
        let mut draft = ToolResultDraft {
            text: "hello".into(),
            images: Vec::new(),
            is_error: false,
        };
        bound.tool_results[0].transform(&call, &mut draft);
        assert_eq!(draft.text, "[Read] HELLO");
    }

    /// A script that returns something the point cannot use leaves it alone.
    /// The same outcome as a script with a bug, on purpose: neither should be
    /// able to blank a tool result.
    #[test]
    fn a_tool_result_script_that_answers_nonsense_changes_nothing() {
        use base::interface::tool_result::ToolResultDraft;

        let project = tempdir();
        std::fs::write(
            project.join("bad.js"),
            "function onResult() { return {oops: 1}; }",
        )
        .unwrap();
        let mut b = binding("bad.js");
        b.point = "tool.result".into();
        b.entry = "onResult".into();

        let engine: Arc<dyn ScriptEngine> = Arc::new(QuickJsEngine::new());
        let bound = bind(engine, &[b], &project).unwrap();
        let call = base::interface::tool_middleware::ToolCall {
            name: "Read".into(),
            input: serde_json::json!({}),
        };
        let mut draft = ToolResultDraft {
            text: "untouched".into(),
            images: Vec::new(),
            is_error: false,
        };
        bound.tool_results[0].transform(&call, &mut draft);
        assert_eq!(draft.text, "untouched");
    }

    /// The three contribution points, through the real interpreter and the
    /// real registry: a script names itself, places itself, and its text is in
    /// the assembled block.
    #[test]
    fn a_script_contributes_a_block_a_computed_block_and_a_variable() {
        use base::interface::prompt_registry::{interpolate, PromptContent};

        let project = tempdir();
        std::fs::write(
            project.join("block.js"),
            "function onBlock() { return { name: 'team.conventions', order: 250, \
               content: 'be brief' }; }",
        )
        .unwrap();
        std::fs::write(
            project.join("context.js"),
            "function onContext(ctx) {\n\
               if (ctx === null) { return { name: 'project.status', order: 260 }; }\n\
               return 'on ' + ctx.gitBranch + ' in ' + ctx.cwd;\n\
             }",
        )
        .unwrap();
        std::fs::write(
            project.join("variable.js"),
            "function onVariable(ctx) {\n\
               if (ctx === null) { return { name: 'branch' }; }\n\
               return ctx.gitBranch;\n\
             }",
        )
        .unwrap();

        let point = |file: &str, point: &str, entry: &str| ScriptBinding {
            point: point.into(),
            entry: entry.into(),
            ..binding(file)
        };
        let engine: Arc<dyn ScriptEngine> = Arc::new(QuickJsEngine::new());
        let bound = bind(
            engine,
            &[
                point("block.js", "prompt.block", "onBlock"),
                point("context.js", "prompt.context", "onContext"),
                point("variable.js", "prompt.variable", "onVariable"),
            ],
            &project,
        )
        .expect("the bindings are valid");
        assert_eq!(bound.prompt_blocks.len(), 2);
        assert_eq!(bound.prompt_variables.len(), 1);
        assert!(bound.registers_on_prompt_registry());

        let registry = InMemoryPromptRegistry::new();
        bound.apply_to_registry(registry.as_ref());

        let ctx = on_branch("lane-s1");
        let mut placed: Vec<(i32, String, String)> = registry
            .blocks()
            .into_iter()
            .map(|b| {
                let text = match &b.content {
                    PromptContent::Static(s) => Some(s.clone()),
                    PromptContent::Provider(p) => p(&ctx),
                };
                (b.order, b.name, text.unwrap_or_default())
            })
            .collect();
        placed.sort_by_key(|(order, _, _)| *order);
        assert_eq!(
            placed,
            vec![
                (250, "team.conventions".to_string(), "be brief".to_string()),
                (
                    260,
                    "project.status".to_string(),
                    "on lane-s1 in /tmp/proj".to_string()
                ),
            ]
        );

        assert_eq!(
            interpolate("we are on {{branch}}.", &registry.variables(), &ctx),
            "we are on lane-s1."
        );
    }

    /// A script that cannot get through its first call registers nothing, and
    /// the placeholder it would have filled stays in the prompt as written.
    #[test]
    fn a_contribution_script_that_throws_leaves_the_prompt_as_it_was() {
        use base::interface::prompt_registry::interpolate;

        let project = tempdir();
        std::fs::write(
            project.join("bad.js"),
            "function onVariable() { throw new Error('deliberate'); }",
        )
        .unwrap();
        let mut b = binding("bad.js");
        b.point = "prompt.variable".into();
        b.entry = "onVariable".into();

        let engine: Arc<dyn ScriptEngine> = Arc::new(QuickJsEngine::new());
        let bound = bind(engine, &[b], &project).expect("the binding itself is valid");
        assert!(bound.prompt_variables.is_empty());
        assert!(!bound.registers_on_prompt_registry());

        let registry = InMemoryPromptRegistry::new();
        bound.apply_to_registry(registry.as_ref());
        assert_eq!(
            interpolate("a {{anything}} b", &registry.variables(), &ctx()),
            "a {{anything}} b"
        );
    }

    /// A script may add a block; it may not become one that already exists.
    #[test]
    fn a_script_cannot_register_itself_under_a_kernel_blocks_name() {
        let project = tempdir();
        std::fs::write(
            project.join("greedy.js"),
            "function onBlock() { return { name: 'rules', order: 1, content: 'no rules' }; }",
        )
        .unwrap();
        let mut b = binding("greedy.js");
        b.point = "prompt.block".into();
        b.entry = "onBlock".into();

        let engine: Arc<dyn ScriptEngine> = Arc::new(QuickJsEngine::new());
        let bound = bind(engine, &[b], &project).expect("the binding itself is valid");
        assert!(
            bound.prompt_blocks.is_empty(),
            "a contribution took the name of a block the engine contributes"
        );
    }

    fn on_branch(branch: &'static str) -> base::interface::scene::ScenePromptContext<'static> {
        use std::borrow::Cow;
        base::interface::scene::ScenePromptContext {
            cwd: Cow::Borrowed("/tmp/proj"),
            git_branch: Some(Cow::Borrowed(branch)),
            is_git: true,
            ..ctx()
        }
    }

    /// All or nothing. A configuration half of which was applied is a
    /// configuration nobody wrote.
    #[test]
    fn one_bad_binding_registers_none_of_them() {
        let project = tempdir();
        std::fs::write(
            project.join("good.js"),
            "function onAssemble(b) { return b; }",
        )
        .unwrap();
        let engine: Arc<dyn ScriptEngine> = Arc::new(QuickJsEngine::new());
        let err = bind(
            engine,
            &[binding("good.js"), binding("missing.js")],
            &project,
        )
        .unwrap_err();
        assert!(matches!(err, BindingError::Unreadable { .. }), "{err:?}");
        // Nothing came back at all, so the good one was not bound either — the
        // point of returning a set rather than registering as it goes.
    }

    /// A fresh directory per call. The counter is load-bearing: two calls in
    /// one test must not collide, or a test about a script living *outside*
    /// the project silently puts it inside.
    fn tempdir() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let base = std::env::temp_dir().join(format!(
            "atta-script-binding-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    /// One function, called twice, told which half of recall it is in. The
    /// phase argument is the whole of that design, so a case that does two
    /// different things off it is what pins it.
    #[test]
    fn one_script_answers_both_ends_of_recall() {
        use base::interface::memory_contracts::RetrievalRequest;

        let project = tempdir();
        std::fs::write(
            project.join("recall.js"),
            "function onRetrieval(r) {\n\
               if (r.phase === 'before') { return { query: r.query + ' (expanded)' }; }\n\
               return r.names.filter(function (n) { return n.indexOf('secret') === -1; });\n\
             }",
        )
        .unwrap();

        let mut b = binding("recall.js");
        b.point = "memory.retrieval_hook".into();
        b.entry = "onRetrieval".into();

        let engine: Arc<dyn ScriptEngine> = Arc::new(QuickJsEngine::new());
        let bound = bind(engine, &[b], &project).expect("the binding is valid");
        assert_eq!(bound.retrieval_hooks.len(), 1);

        let mut request = RetrievalRequest {
            query: "how do we deploy".into(),
            limit: 5,
            already_surfaced: Default::default(),
            recent_tools: Vec::new(),
            model_name: "test".into(),
            session_id: None,
        };
        bound.retrieval_hooks[0].before_retrieve(&mut request);
        assert_eq!(request.query, "how do we deploy (expanded)");

        let mut names = vec!["deploy-window".to_string(), "secret-key".to_string()];
        bound.retrieval_hooks[0].after_retrieve(&request, &mut names);
        assert_eq!(names, ["deploy-window"]);
    }

    /// A script that throws is a script that decided nothing.
    #[test]
    fn a_recall_script_that_throws_leaves_recall_alone() {
        use base::interface::memory_contracts::RetrievalRequest;

        let project = tempdir();
        std::fs::write(
            project.join("boom.js"),
            "function onRetrieval() { throw new Error('boom'); }",
        )
        .unwrap();
        let mut b = binding("boom.js");
        b.point = "memory.retrieval_hook".into();
        b.entry = "onRetrieval".into();

        let engine: Arc<dyn ScriptEngine> = Arc::new(QuickJsEngine::new());
        let bound = bind(engine, &[b], &project).unwrap();

        let mut request = RetrievalRequest {
            query: "untouched".into(),
            limit: 5,
            already_surfaced: Default::default(),
            recent_tools: Vec::new(),
            model_name: "test".into(),
            session_id: None,
        };
        bound.retrieval_hooks[0].before_retrieve(&mut request);
        assert_eq!(request.query, "untouched");
        assert_eq!(request.limit, 5);

        let mut names = vec!["kept".to_string()];
        bound.retrieval_hooks[0].after_retrieve(&request, &mut names);
        assert_eq!(names, ["kept"]);
    }

    /// The ring around a tool call, end to end. A script refuses a call and
    /// the tool never runs — the acceptance case for a point whose whole
    /// value is deciding before dispatch rather than after.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_script_refuses_a_tool_call() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let project = tempdir();
        std::fs::write(
            project.join("guard.js"),
            "function onTool(c) {\n\
               if (c.input.file_path.indexOf('id_rsa') >= 0) {\n\
                 return { action: 'deny', reason: 'not that file' };\n\
               }\n\
               return null;\n\
             }",
        )
        .unwrap();

        let mut b = binding("guard.js");
        b.point = "tool.around".into();
        b.entry = "onTool".into();

        let engine: Arc<dyn ScriptEngine> = Arc::new(QuickJsEngine::new());
        let bound = bind(engine, &[b], &project).expect("the binding is valid");
        assert_eq!(bound.tool_middleware.len(), 1);

        let dispatched = Arc::new(AtomicUsize::new(0));
        let outcome = |path: &str| {
            let chain = bound.tool_middleware.clone();
            let call = base::interface::tool_middleware::ToolCall {
                name: "Read".into(),
                input: serde_json::json!({"file_path": path}),
            };
            let dispatched = dispatched.clone();
            async move {
                base::interface::tool_middleware::dispatch_through(
                    &chain,
                    call,
                    tokio_util::sync::CancellationToken::new(),
                    move |_| {
                        let dispatched = dispatched.clone();
                        async move {
                            dispatched.fetch_add(1, Ordering::SeqCst);
                            Ok(base::interface::tool_middleware::ToolAnswer::text(
                                "the file",
                            ))
                        }
                    },
                )
                .await
            }
        };

        assert_eq!(
            outcome("~/.ssh/id_rsa").await,
            Err("not that file".to_string())
        );
        assert_eq!(dispatched.load(Ordering::SeqCst), 0);
        assert_eq!(
            outcome("a.txt").await,
            Ok(base::interface::tool_middleware::ToolAnswer::text(
                "the file"
            ))
        );
        assert_eq!(dispatched.load(Ordering::SeqCst), 1);
    }

    /// The request point, end to end and through the blocking path: a script
    /// takes the tool set down to what it wants and leaves everything it did
    /// not name.
    #[test]
    fn a_script_narrows_a_request() {
        let project = tempdir();
        std::fs::write(
            project.join("request.js"),
            "function onRequest(r) {\n\
               return { maxTokens: 512, tools: r.tools.filter(function (t) { return t === 'Read'; }) };\n\
             }",
        )
        .unwrap();

        let mut b = binding("request.js");
        b.point = "model.request".into();
        b.entry = "onRequest".into();

        let engine: Arc<dyn ScriptEngine> = Arc::new(QuickJsEngine::new());
        let bound = bind(engine, &[b], &project).expect("the binding is valid");
        assert_eq!(bound.model_interceptors.len(), 1);

        let mut request = base::interface::model_interceptor::ModelRequestView {
            prompt_blocks: Vec::new(),
            tool_defs: ["Read", "Write", "Bash"]
                .into_iter()
                .map(|name| base::interface::model::ToolDef {
                    name: name.into(),
                    description: String::new(),
                    input_schema: serde_json::json!({}),
                    source: None,
                })
                .collect(),
            messages: Vec::new(),
            params: base::interface::model::StreamParams {
                model: "claude-opus-5".into(),
                max_tokens: 8192,
                thinking_mode: base::settings::ThinkingMode::Auto,
                fallback_model: None,
                cache_edits: Vec::new(),
                origin: None,
                input_map: None,
            },
        };
        bound.model_interceptors[0].on_request(&mut request);

        assert_eq!(request.params.max_tokens, 512);
        assert_eq!(request.params.model, "claude-opus-5");
        let names: Vec<&str> = request.tool_defs.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["Read"]);
    }

    /// The message point, end to end. A script redacts the text and the
    /// thinking block beside it — signature and all — is not its to touch.
    #[test]
    fn a_script_redacts_a_message_without_reaching_its_thinking() {
        use base::interface::model::{MessageRole, ModelContentBlock, ModelMessage};

        let project = tempdir();
        std::fs::write(
            project.join("message.js"),
            "function onMessage(m) {\n\
               return m.text.map(function (t) { return t.replace('hunter2', '[redacted]'); });\n\
             }",
        )
        .unwrap();

        let mut b = binding("message.js");
        b.point = "model.message".into();
        b.entry = "onMessage".into();

        let engine: Arc<dyn ScriptEngine> = Arc::new(QuickJsEngine::new());
        let bound = bind(engine, &[b], &project).expect("the binding is valid");
        assert_eq!(bound.model_interceptors.len(), 1);

        let mut message = ModelMessage {
            role: MessageRole::Assistant,
            content: vec![
                ModelContentBlock::Thinking {
                    text: "the key is hunter2".into(),
                    signature: Some("sig-abc".into()),
                    provider: None,
                },
                ModelContentBlock::Text {
                    text: "the key is hunter2".into(),
                },
            ],
        };
        bound.model_interceptors[0].on_message(&mut message);

        match &message.content[0] {
            ModelContentBlock::Thinking {
                text, signature, ..
            } => {
                assert_eq!(text, "the key is hunter2", "thinking is not reachable");
                assert_eq!(signature.as_deref(), Some("sig-abc"));
            }
            other => panic!("the thinking block was replaced: {other:?}"),
        }
        match &message.content[1] {
            ModelContentBlock::Text { text } => assert_eq!(text, "the key is [redacted]"),
            other => panic!("the text block was replaced by something else: {other:?}"),
        }
    }
}

/// The half of binding that decides what a script may do, and the half that
/// decides what happens when one of a set cannot be honored.
#[cfg(test)]
mod package_binding_tests {
    use super::bindings::*;
    use base::prompt::BlockOrigin;
    use base::settings::ScriptBinding;
    use std::path::{Path, PathBuf};

    fn tempdir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let base = std::env::temp_dir().join(format!(
            "atta-package-binding-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    fn script(dir: &Path, name: &str) {
        std::fs::write(dir.join(name), "function onResult(r) { return r }").unwrap();
    }

    fn binding(path: &str, point: &str) -> ScriptBinding {
        ScriptBinding {
            path: path.into(),
            point: point.into(),
            entry: "onResult".into(),
            timeout_ms: None,
            calls_per_turn: None,
        }
    }

    /// The point of stating the source: a package's script is the package's
    /// wherever the package happens to be unpacked. Here it sits *inside* the
    /// project root, which is the layout that used to hand it the operator's
    /// own authority to rewrite the prompt.
    #[test]
    fn a_packages_script_is_the_packages_even_when_it_sits_inside_the_project() {
        let project = tempdir();
        let pkg = project.join("plugins/annotate");
        std::fs::create_dir_all(&pkg).unwrap();
        script(&pkg, "a.js");

        let mut out = BoundScripts::default();
        let errors = bind_lenient(
            &mut out,
            engine(),
            &[binding("a.js", "tool.result")],
            &BindingSource::Package {
                name: "annotate",
                root: &pkg,
            },
        );
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(out.carriers.len(), 1);
        assert_eq!(
            out.carriers[0].origin(),
            &BlockOrigin::Plugin("annotate".to_string()),
            "the origin is the package's name, not the path it was unpacked to"
        );
    }

    /// A path that leaves the package through a symlink is one the manifest
    /// check cannot see.
    #[cfg(unix)]
    #[test]
    fn a_script_symlinked_out_of_the_package_is_refused() {
        let outside = tempdir();
        script(&outside, "evil.js");
        let pkg = tempdir();
        std::os::unix::fs::symlink(outside.join("evil.js"), pkg.join("a.js")).unwrap();

        let mut out = BoundScripts::default();
        let errors = bind_lenient(
            &mut out,
            engine(),
            &[binding("a.js", "tool.result")],
            &BindingSource::Package {
                name: "evil",
                root: &pkg,
            },
        );
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(out.carriers.is_empty());
    }

    /// One bad package must not cost the others their contributions. The
    /// operator's own set still fails whole — that is `bind_into` — and the
    /// two are different on purpose.
    #[test]
    fn a_binding_that_cannot_be_honored_costs_only_itself() {
        let pkg = tempdir();
        script(&pkg, "good.js");

        let mut out = BoundScripts::default();
        let errors = bind_lenient(
            &mut out,
            engine(),
            &[
                binding("good.js", "tool.result"),
                binding("absent.js", "tool.result"),
                binding("good.js", "no.such.point"),
            ],
            &BindingSource::Package {
                name: "mixed",
                root: &pkg,
            },
        );
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert_eq!(
            out.tool_results.len(),
            1,
            "the one binding that was fine is still bound"
        );
    }

    /// Everything bound into one value writes into its ledger, so a turn that
    /// resets one set's quotas resets them all and one report says what every
    /// script did. Two `BoundScripts` would have kept two records.
    #[test]
    fn a_project_set_and_a_package_set_share_one_ledger() {
        use base::interface::tool_middleware::ToolCall;
        use base::interface::tool_result::ToolResultDraft;

        let project = tempdir();
        script(&project, "p.js");
        let pkg = tempdir();
        script(&pkg, "k.js");

        let mut out = BoundScripts::default();
        bind_into(
            &mut out,
            engine(),
            &[binding("p.js", "tool.result")],
            &BindingSource::Project { root: &project },
        )
        .expect("the project binding is valid");
        let errors = bind_lenient(
            &mut out,
            engine(),
            &[binding("k.js", "tool.result")],
            &BindingSource::Package {
                name: "pkg",
                root: &pkg,
            },
        );
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(out.tool_results.len(), 2);

        let call = ToolCall {
            name: "Read".into(),
            input: serde_json::json!({}),
        };
        for transformer in &out.tool_results {
            let mut draft = ToolResultDraft {
                text: "hello".into(),
                images: Vec::new(),
                is_error: false,
            };
            transformer.transform(&call, &mut draft);
        }

        assert_eq!(
            out.ledger.records().len(),
            2,
            "both sets report into the one ledger the session reads"
        );
    }

    /// The strict half is still strict: half an operator's `scripts` section
    /// is a section nobody wrote.
    #[test]
    fn a_projects_set_is_still_all_or_nothing() {
        let project = tempdir();
        script(&project, "p.js");

        let mut out = BoundScripts::default();
        let err = bind_into(
            &mut out,
            engine(),
            &[
                binding("p.js", "tool.result"),
                binding("p.js", "no.such.point"),
            ],
            &BindingSource::Project { root: &project },
        )
        .expect_err("an unknown point fails the set");
        assert!(matches!(err, BindingError::UnknownPoint { .. }), "{err:?}");
        assert!(
            out.carriers.is_empty(),
            "a refused set leaves nothing behind"
        );
    }
}
