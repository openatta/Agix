//! `prompt.block`, `prompt.context`, `prompt.variable` — what a script adds to
//! the system prompt.
//!
//! These three are contributions, not interceptions: the script does not get
//! the prompt and hand back a different one, it registers something the
//! assembler merges in. Nothing here can change or remove what anyone else
//! contributed, which is why none of it takes an
//! [`Authority`](crate::interface::prompt_assembly::Authority) — the block
//! still carries [`ScriptCarrier::origin`] so a later pass can see whose it
//! is.
//!
//! # Where the name and the order come from
//!
//! A registration needs a name and an order *before* any prompt exists, and a
//! `ScriptBinding` carries only a path, a point and an entry point. So the
//! script is asked: it is called once, when the binding is bound, with `null`
//! instead of a context, and answers with its own identity.
//!
//! ```js
//! function onContext(ctx) {
//!   if (ctx === null) return { name: "project.status", order: 250 };
//!   return "branch: " + ctx.gitBranch;
//! }
//! ```
//!
//! The alternative was two more fields on `ScriptBinding`, meaningless at the
//! ten points that are not prompt registrations. This way one file's identity
//! travels with the file rather than with the line of configuration that
//! mentions it, and a binding stays the same three fields everywhere.
//!
//! `order` is optional and defaults to
//! [`orders::CONFIG_PROMPT_APPEND`](crate::interface::prompt_registry::orders::CONFIG_PROMPT_APPEND),
//! which puts the block after every stage the kernel contributes. See
//! [`orders`](crate::interface::prompt_registry::orders) for the rest.
//!
//! A script that fails this call registers nothing at all. That is the only
//! honest outcome — a block with no name is not addressable, and a block at no
//! particular order is not placeable.
//!
//! # A name a block already has is refused
//!
//! Registering under a kernel block's name is not "adding". Blocks sort by
//! order and a later pass addresses one by name, taking the *first* match — so
//! a contribution called `rules` sitting ahead of the kernel's `rules` would
//! quietly absorb an edit meant for it, and the real block would go to the
//! model untouched. A contribution that may only add must not be able to take
//! over an address that already means something else.
//!
//! # What the script is given
//!
//! Where the session is running, and nothing that is already a prompt block:
//!
//! ```json
//! {
//!   "cwd": "/home/u/proj", "os": "linux", "shell": "bash",
//!   "homeDir": "/home/u", "date": "2026-06-10", "modelName": "…",
//!   "isGit": true, "gitBranch": "main", "isWorktree": false,
//!   "language": null, "scratchpadDir": null, "availableTools": "Read,Bash"
//! }
//! ```
//!
//! The skills inventory, the MCP instructions, the session memory and the
//! output style are left out on purpose. They are each already a block of the
//! prompt, so a script that wants to read or rewrite one binds to
//! `prompt.assemble`, which is the point that hands over the assembled blocks;
//! offering them here as well would be a second name for the same thing and a
//! second answer when the two disagree. They are also the four largest strings
//! in the context — tens of kilobytes serialized, parsed into a fresh QuickJS
//! runtime once per turn per binding, for a script that wanted `cwd`.
//!
//! `git_status` is left out for its size too, and because a diff of the user's
//! uncommitted work is the one thing here that a script arriving with a
//! downloaded plugin has no business reading. `tool_results_ever_cleared`
//! gates one scene's own prompt section and means nothing outside the kernel.

use std::sync::Arc;

use crate::interface::prompt_registry::{
    orders, ContextProvider, PromptContent, RegisteredBlock, VariableProvider,
};
use crate::interface::scene::ScenePromptContext;
use crate::interface::script::{ScriptCarrier, ScriptOutcome};
use crate::prompt::{names, BlockRole};

/// A script bound to `prompt.block`: one fixed block of the system prompt.
///
/// The identity call is the only call. It answers with the block, text
/// included:
///
/// ```json
/// { "name": "team.conventions", "order": 250, "content": "…" }
/// ```
///
/// `content` must be a string, and a script that leaves it out has bound
/// itself to the wrong point — text that depends on the session belongs at
/// `prompt.context`, which is called with one.
pub fn prompt_block_from_script(
    carrier: Arc<ScriptCarrier>,
    entry: &str,
) -> Option<RegisteredBlock> {
    let identity = identity(&carrier, entry)?;
    let Some(content) = identity.content else {
        tracing::warn!(
            script = %carrier.script().id,
            block = %identity.name,
            "a prompt.block script must answer with `content`; for text that depends on the \
             session, bind to prompt.context"
        );
        carrier.record(
            entry,
            ScriptOutcome::NoChange {
                detail: Some("answered without `content`; nothing was registered".into()),
            },
        );
        return None;
    };
    carrier.record(entry, ScriptOutcome::Applied);
    Some(RegisteredBlock {
        name: identity.name,
        role: BlockRole::System,
        order: identity.order,
        content: PromptContent::Static(content),
        cache_strategy: None,
        origin: carrier.origin().clone(),
    })
}

/// A script bound to `prompt.context`: a block whose text is computed each
/// time the prompt is assembled.
///
/// Called once with `null` for its identity — `{ "name": …, "order": … }` —
/// and again at every assembly with the context object, where it answers with
/// the block's text as a string. Anything else it answers, including `null`,
/// contributes no block that turn, which is also what a script that failed or
/// ran out of time contributes.
///
/// Registered through `register_block` rather than the `register_context`
/// sugar, which stamps every block it makes as the kernel's.
pub fn prompt_context_from_script(
    carrier: Arc<ScriptCarrier>,
    entry: &str,
) -> Option<RegisteredBlock> {
    let identity = identity(&carrier, entry)?;
    carrier.record(entry, ScriptOutcome::Applied);
    let origin = carrier.origin().clone();
    let entry = entry.to_string();
    let provider: ContextProvider =
        Arc::new(move |ctx: &ScenePromptContext<'_>| answer(&carrier, &entry, ctx, "context"));
    Some(RegisteredBlock {
        name: identity.name,
        role: BlockRole::System,
        order: identity.order,
        content: PromptContent::Provider(provider),
        cache_strategy: None,
        origin,
    })
}

/// A script bound to `prompt.variable`: what `{{name}}` expands to.
///
/// Called once with `null` for its identity — `{ "name": "reviewer" }`, the
/// `{{reviewer}}` it answers for; `order` and `content` mean nothing here —
/// and again at every assembly with the context object, where it answers with
/// the value as a string.
///
/// **Only a string is a value.** `null`, a number, an object, a thrown error,
/// a timeout and an exhausted quota all leave the placeholder in the prompt
/// exactly as written, because a variable that could not be resolved is a bug
/// to see rather than a hole to hide — and a script that blanked its own
/// placeholder on the way down would make its failure look like a successful
/// empty answer. A script that does want the placeholder to disappear says so
/// by returning `""`.
pub fn prompt_variable_from_script(
    carrier: Arc<ScriptCarrier>,
    entry: &str,
) -> Option<(String, VariableProvider)> {
    let identity = identity(&carrier, entry)?;
    carrier.record(entry, ScriptOutcome::Applied);
    let entry = entry.to_string();
    let provider: VariableProvider =
        Arc::new(move |ctx: &ScenePromptContext<'_>| answer(&carrier, &entry, ctx, "variable"));
    Some((identity.name, provider))
}

/// What a script says about itself on its first call.
struct Identity {
    name: String,
    order: i32,
    content: Option<String>,
}

fn identity(carrier: &ScriptCarrier, entry: &str) -> Option<Identity> {
    let returned = match carrier.call_blocking(entry, serde_json::Value::Null) {
        Ok(v) => v,
        Err(error) => {
            tracing::warn!(
                script = %carrier.script().id,
                error = %error,
                "script did not say what it contributes; nothing was registered"
            );
            carrier.record(entry, ScriptOutcome::Failed { error });
            return None;
        }
    };

    let name = returned.get("name").and_then(|n| n.as_str()).unwrap_or("");
    if name.is_empty() {
        tracing::warn!(
            script = %carrier.script().id,
            "script did not answer its first call with a `name`; nothing was registered"
        );
        carrier.record(
            entry,
            ScriptOutcome::NoChange {
                detail: Some(
                    "answered its first call without a `name`; nothing was registered".into(),
                ),
            },
        );
        return None;
    }
    if is_taken(name) {
        tracing::warn!(
            script = %carrier.script().id,
            name = %name,
            "that name already belongs to a prompt block the engine contributes; \
             nothing was registered"
        );
        carrier.record(
            entry,
            ScriptOutcome::NoChange {
                detail: Some(
                    "asked for a name the engine already contributes; nothing was registered"
                        .into(),
                ),
            },
        );
        return None;
    }

    let order = match returned.get("order") {
        None | Some(serde_json::Value::Null) => orders::CONFIG_PROMPT_APPEND,
        Some(v) => match v.as_i64().and_then(|n| i32::try_from(n).ok()) {
            Some(order) => order,
            None => {
                tracing::warn!(
                    script = %carrier.script().id,
                    name = %name,
                    "`order` is not a number the prompt can be sorted by; nothing was registered"
                );
                carrier.record(
            entry,
            ScriptOutcome::NoChange {
                detail: Some("answered with an `order` the prompt cannot be sorted by; nothing was registered".into()),
            },
        );
                return None;
            }
        },
    };

    Some(Identity {
        name: name.to_string(),
        order,
        content: returned
            .get("content")
            .and_then(|c| c.as_str())
            .map(str::to_string),
    })
}

/// One per-assembly call, for the two points that have them.
fn answer(
    carrier: &ScriptCarrier,
    entry: &str,
    ctx: &ScenePromptContext<'_>,
    kind: &'static str,
) -> Option<String> {
    match carrier.call_blocking(entry, encode(ctx)) {
        Ok(v) => match v.as_str() {
            Some(text) => {
                carrier.record(entry, ScriptOutcome::Applied);
                Some(text.to_string())
            }
            None => {
                carrier.record(
                    entry,
                    ScriptOutcome::NoChange {
                        detail: Some("answered with something that is not a string".into()),
                    },
                );
                None
            }
        },
        Err(error) => {
            tracing::warn!(
                script = %carrier.script().id,
                kind,
                error = %error,
                "script did not run; it contributes nothing to this prompt"
            );
            carrier.record(entry, ScriptOutcome::Failed { error });
            None
        }
    }
}

fn encode(ctx: &ScenePromptContext<'_>) -> serde_json::Value {
    serde_json::json!({
        "cwd": ctx.cwd,
        "os": ctx.os,
        "shell": ctx.shell,
        "homeDir": ctx.home_dir,
        "date": ctx.date,
        "modelName": ctx.model_name,
        "isGit": ctx.is_git,
        "gitBranch": ctx.git_branch,
        "isWorktree": ctx.is_worktree,
        "language": ctx.language,
        "scratchpadDir": ctx.scratchpad_dir,
        "availableTools": ctx.available_tools,
    })
}

fn is_taken(name: &str) -> bool {
    const KERNEL: [&str; 7] = [
        names::SCENE_SKELETON,
        names::SKILLS_CATALOG,
        names::MEMORY_SESSION,
        names::RULES,
        names::MCP_INSTRUCTIONS,
        names::CONFIG_PROMPT_APPEND,
        names::CONFIG_PROMPT_OVERRIDE,
    ];
    name.starts_with(names::SCENE_PREFIX) || KERNEL.contains(&name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interface::prompt_registry::interpolate;
    use crate::interface::script::{ScriptEngine, ScriptError, ScriptLimits, ScriptSource};
    use crate::prompt::BlockOrigin;

    fn ctx() -> ScenePromptContext<'static> {
        use std::borrow::Cow;
        ScenePromptContext {
            cwd: Cow::Borrowed("/tmp/proj"),
            os: Cow::Borrowed("linux"),
            shell: Cow::Borrowed("bash"),
            home_dir: Cow::Borrowed("/home/user"),
            date: Cow::Borrowed("2026-06-10"),
            model_name: Cow::Borrowed("test-model"),
            skills_text: Some(Cow::Borrowed("the whole skills inventory")),
            mcp_instructions: Some(Cow::Borrowed("the whole mcp instructions")),
            session_memory: Some(Cow::Borrowed("the whole session memory")),
            is_git: true,
            git_branch: Some(Cow::Borrowed("lane-s1")),
            is_worktree: true,
            git_status: Some(Cow::Borrowed("M secret-work-in-progress.rs")),
            language: None,
            scratchpad_dir: None,
            output_style_content: Some(Cow::Borrowed("the whole output style")),
            available_tools: Some(Cow::Borrowed("Read,Bash")),
            tool_results_ever_cleared: true,
        }
    }

    /// Stands in for the interpreter: the closure sees exactly the JSON a
    /// script would see and answers exactly what one would answer.
    fn carrier_of(
        f: impl Fn(serde_json::Value) -> Result<serde_json::Value, ScriptError>
            + Send
            + Sync
            + Clone
            + 'static,
        origin: BlockOrigin,
    ) -> Arc<ScriptCarrier> {
        struct Blocking<F>(F);
        #[async_trait::async_trait]
        impl<F> ScriptEngine for Blocking<F>
        where
            F: Fn(serde_json::Value) -> Result<serde_json::Value, ScriptError> + Send + Sync,
        {
            async fn eval(
                &self,
                _s: &ScriptSource,
                _e: &str,
                input: serde_json::Value,
                _l: &ScriptLimits,
            ) -> Result<serde_json::Value, ScriptError> {
                (self.0)(input)
            }
            fn eval_blocking(
                &self,
                _s: &ScriptSource,
                _e: &str,
                input: serde_json::Value,
                _l: &ScriptLimits,
            ) -> Result<serde_json::Value, ScriptError> {
                (self.0)(input)
            }
        }
        Arc::new(ScriptCarrier::new(
            Arc::new(Blocking(f)),
            ScriptSource {
                id: "./.agix/scripts/contribute.js".into(),
                origin,
                code: String::new(),
            },
            "prompt.block",
            ScriptLimits::default(),
        ))
    }

    fn local(
        f: impl Fn(serde_json::Value) -> Result<serde_json::Value, ScriptError>
            + Send
            + Sync
            + Clone
            + 'static,
    ) -> Arc<ScriptCarrier> {
        carrier_of(
            f,
            BlockOrigin::Script("./.agix/scripts/contribute.js".into()),
        )
    }

    fn text_of(block: &RegisteredBlock, ctx: &ScenePromptContext<'_>) -> Option<String> {
        match &block.content {
            PromptContent::Static(s) => Some(s.clone()),
            PromptContent::Provider(p) => p(ctx),
        }
    }

    #[test]
    fn a_block_script_names_orders_and_writes_itself() {
        let block = prompt_block_from_script(
            local(|_| Ok(serde_json::json!({"name": "team.conventions", "order": 250, "content": "be brief"}))),
            "onBlock",
        )
        .expect("it registers");
        assert_eq!(block.name, "team.conventions");
        assert_eq!(block.order, 250);
        assert_eq!(text_of(&block, &ctx()).as_deref(), Some("be brief"));
        assert_eq!(
            block.origin,
            BlockOrigin::Script("./.agix/scripts/contribute.js".into()),
            "the block must carry whose it is, not the kernel's name"
        );
    }

    #[test]
    fn a_block_script_that_omits_an_order_lands_after_the_kernel() {
        let block = prompt_block_from_script(
            local(|_| Ok(serde_json::json!({"name": "mine", "content": "x"}))),
            "onBlock",
        )
        .expect("it registers");
        assert_eq!(block.order, orders::CONFIG_PROMPT_APPEND);
    }

    #[test]
    fn a_context_script_is_called_again_with_the_session_it_is_writing_for() {
        let block = prompt_context_from_script(
            local(|input| {
                Ok(match input.as_object() {
                    None => serde_json::json!({"name": "project.status", "order": 250}),
                    Some(ctx) => serde_json::json!(format!(
                        "on {} in {}",
                        ctx["gitBranch"].as_str().unwrap_or("?"),
                        ctx["cwd"].as_str().unwrap_or("?")
                    )),
                })
            }),
            "onContext",
        )
        .expect("it registers");
        assert_eq!(block.name, "project.status");
        assert_eq!(
            text_of(&block, &ctx()).as_deref(),
            Some("on lane-s1 in /tmp/proj"),
            "the provider must run per assembly, against the real context"
        );
    }

    /// Every field a script is given, and every field it is not. Written as
    /// the whole key set rather than a spot check, so adding a field to
    /// `ScenePromptContext` cannot widen what a script sees by default.
    #[test]
    fn a_script_sees_the_environment_and_none_of_the_prompt() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(serde_json::Value::Null));
        let recorded = seen.clone();
        let block = prompt_context_from_script(
            local(move |input| {
                if input.is_object() {
                    *recorded.lock().unwrap() = input;
                    return Ok(serde_json::json!("ok"));
                }
                Ok(serde_json::json!({"name": "mine"}))
            }),
            "onContext",
        )
        .expect("it registers");
        text_of(&block, &ctx());

        let seen = seen.lock().unwrap();
        let mut keys: Vec<&str> = seen
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "availableTools",
                "cwd",
                "date",
                "gitBranch",
                "homeDir",
                "isGit",
                "isWorktree",
                "language",
                "modelName",
                "os",
                "scratchpadDir",
                "shell",
            ]
        );
        let json = seen.to_string();
        for withheld in [
            "the whole skills inventory",
            "the whole mcp instructions",
            "the whole session memory",
            "the whole output style",
            "secret-work-in-progress",
        ] {
            assert!(!json.contains(withheld), "`{withheld}` reached the script");
        }
    }

    #[test]
    fn a_variable_script_expands_its_own_placeholder() {
        let (name, provider) = prompt_variable_from_script(
            local(|input| {
                Ok(match input.as_object() {
                    None => serde_json::json!({"name": "branch"}),
                    Some(ctx) => ctx["gitBranch"].clone(),
                })
            }),
            "onVariable",
        )
        .expect("it registers");
        assert_eq!(name, "branch");
        assert_eq!(
            interpolate("on {{branch}} now", &[(name, provider)], &ctx()),
            "on lane-s1 now"
        );
    }

    /// The distinction the point's own contract rests on: `""` blanks the
    /// placeholder, anything that is not a string leaves it standing.
    #[test]
    fn only_a_string_is_a_variable_value() {
        for (answer, expected) in [
            (serde_json::json!(""), "a  b"),
            (serde_json::json!("v"), "a v b"),
            (serde_json::json!(null), "a {{x}} b"),
            (serde_json::json!(7), "a {{x}} b"),
            (serde_json::json!({"value": "v"}), "a {{x}} b"),
            (serde_json::json!(["v"]), "a {{x}} b"),
        ] {
            let value = answer.clone();
            let (name, provider) = prompt_variable_from_script(
                local(move |input| {
                    Ok(if input.is_object() {
                        value.clone()
                    } else {
                        serde_json::json!({"name": "x"})
                    })
                }),
                "onVariable",
            )
            .expect("it registers");
            assert_eq!(
                interpolate("a {{x}} b", &[(name, provider)], &ctx()),
                expected,
                "answering {answer:?}"
            );
        }
    }

    #[test]
    fn a_variable_script_that_fails_leaves_its_placeholder_alone() {
        let (name, provider) = prompt_variable_from_script(
            local(|input| {
                if input.is_object() {
                    return Err(ScriptError::Failed("deliberate".into()));
                }
                Ok(serde_json::json!({"name": "x"}))
            }),
            "onVariable",
        )
        .expect("it registers");
        assert_eq!(
            interpolate("a {{x}} b", &[(name, provider)], &ctx()),
            "a {{x}} b"
        );
    }

    #[test]
    fn a_context_script_that_fails_contributes_no_block() {
        let block = prompt_context_from_script(
            local(|input| {
                if input.is_object() {
                    return Err(ScriptError::TimedOut {
                        after: std::time::Duration::from_millis(1),
                    });
                }
                Ok(serde_json::json!({"name": "mine"}))
            }),
            "onContext",
        )
        .expect("it registers");
        assert_eq!(text_of(&block, &ctx()), None);
    }

    /// Every way the first call can go wrong ends in the same place: no
    /// registration, so the prompt is the one that would have been assembled
    /// with no script bound at all.
    #[test]
    fn nothing_is_registered_when_the_script_cannot_say_what_it_is() {
        let answers = [
            Err(ScriptError::Failed("threw".into())),
            Err(ScriptError::NoEngine),
            Ok(serde_json::json!(null)),
            Ok(serde_json::json!("just a string")),
            Ok(serde_json::json!({})),
            Ok(serde_json::json!({"name": ""})),
            Ok(serde_json::json!({"name": 7, "content": "x"})),
            Ok(serde_json::json!({"name": "mine", "order": "late", "content": "x"})),
            Ok(serde_json::json!({"name": "mine", "order": 4e18, "content": "x"})),
        ];
        for answer in answers {
            let value = answer.clone();
            let carrier = local(move |_| value.clone());
            assert!(
                prompt_block_from_script(carrier.clone(), "f").is_none(),
                "{answer:?}"
            );
            assert!(prompt_context_from_script(carrier.clone(), "f").is_none());
            assert!(prompt_variable_from_script(carrier, "f").is_none());
        }
    }

    #[test]
    fn a_block_script_that_answers_no_content_registers_nothing() {
        assert!(prompt_block_from_script(
            local(|_| Ok(serde_json::json!({"name": "mine", "order": 10}))),
            "f"
        )
        .is_none());
    }

    /// A contribution may add. Taking the name of a block that already exists
    /// is not adding: it would sit ahead of the real one and absorb an edit
    /// addressed to it.
    #[test]
    fn a_contribution_cannot_take_a_name_the_engine_already_uses() {
        for taken in [
            names::RULES,
            names::SKILLS_CATALOG,
            names::MEMORY_SESSION,
            names::MCP_INSTRUCTIONS,
            names::CONFIG_PROMPT_APPEND,
            names::CONFIG_PROMPT_OVERRIDE,
            names::SCENE_SKELETON,
            "scene.anything",
        ] {
            let carrier = local(move |_| {
                Ok(serde_json::json!({"name": taken, "order": 1, "content": "mine"}))
            });
            assert!(
                prompt_block_from_script(carrier.clone(), "f").is_none(),
                "`{taken}` was allowed"
            );
            assert!(
                prompt_context_from_script(carrier, "f").is_none(),
                "`{taken}`"
            );
        }
    }

    /// A downloaded script contributes under its own provenance, so a later
    /// pass can tell it apart from the operator's own.
    #[test]
    fn a_block_from_outside_the_project_says_so() {
        let block = prompt_block_from_script(
            carrier_of(
                |_| Ok(serde_json::json!({"name": "example.note", "content": "hi"})),
                BlockOrigin::Plugin("example".into()),
            ),
            "f",
        )
        .expect("it registers");
        assert_eq!(block.origin, BlockOrigin::Plugin("example".into()));
        assert!(!block.origin.is_local());
    }
}
