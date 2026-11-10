//! Built-in tool implementations — all concrete tools the agent can invoke.

pub mod ask_user;
pub mod bash;
pub mod cancel;
pub mod cron;
mod external_content;
pub mod file_edit;
pub mod file_read;
pub mod file_write;
pub mod glob;
pub mod goal;
pub mod grep;
pub mod monitor;
pub mod notebook_edit;
pub mod ping;
pub mod plan_mode;
pub mod push_notification;
pub mod security;
pub mod sleep;
#[cfg(feature = "structured-output")]
pub mod structured_output;
pub mod task_message;
pub mod task_output;
pub mod task_stop;
pub mod task_wait;
pub mod todo_write;
pub mod tool_search;
#[cfg(feature = "web")]
pub mod web_fetch;
#[cfg(feature = "web")]
pub mod web_search;
pub mod worktree;
pub mod worktree_tools;

// From anthropic/ — tool-side logic
#[cfg(feature = "web")]
pub mod native_search;
#[cfg(feature = "web")]
pub mod page_extract;

// Agent tool

use base::tool::Tool;
use std::sync::Arc;

/// Register the standard set of self-contained built-in tools — no host-specific
/// wiring (bridges, external providers, registries) required to construct any of
/// them. Every embedder (daemon, test harness, ...) that wants a working
/// Bash/Read/Write/Edit/... agent needs to call this on the registry it hands to
/// `runtime::agent::Builder::tools()` — `Builder::build()` does NOT populate these
/// itself. It only auto-registers a handful of tools that need engine-internal
/// state to construct (Skill, TaskStop, TaskOutput, Import, the "Agent" spawner,
/// MCP adapters) — see `crates/runtime/src/agent/builder/build.rs::Builder::build()`.
///
/// Deliberately excluded, because they need per-session host state this
/// function has no access to. Each has its own registration helper below,
/// called from `runtime::agent::Builder::build()` where that state exists:
/// `WebSearchTool` ([`register_web_search`] — needs `Settings` to resolve the
/// search backend), the cron tools ([`register_cron_tools`] — need a shared
/// `CronStore` plus something to drain it), and
/// `EnterWorktreeTool`/`ExitWorktreeTool` ([`register_worktree_tools`] — need
/// a session-scoped `WorktreeRegistry`).
///
/// `WebFetchTool` is registered here without page extraction;
/// [`register_web_fetch`] swaps in the extracting one when a session routes
/// `task_models.web_fetch` to a model.
pub fn register_builtin_tools(reg: &std::sync::Arc<base::tool::InMemoryToolRegistry>) {
    let simple: Vec<Arc<dyn Tool>> = vec![
        Arc::new(crate::bash::BashTool),
        Arc::new(crate::file_read::FileReadTool),
        Arc::new(crate::file_write::FileWriteTool),
        Arc::new(crate::file_edit::FileEditTool),
        Arc::new(crate::grep::GrepTool),
        Arc::new(crate::glob::GlobTool),
        Arc::new(crate::notebook_edit::NotebookEditTool),
        Arc::new(crate::todo_write::TodoWriteTool),
        Arc::new(crate::monitor::MonitorTool),
        Arc::new(crate::sleep::SleepTool),
        Arc::new(crate::ping::PingTool),
        Arc::new(crate::plan_mode::EnterPlanModeTool),
        Arc::new(crate::plan_mode::ExitPlanModeTool),
        Arc::new(crate::push_notification::PushNotificationTool),
        Arc::new(crate::ask_user::AskUserQuestionTool),
        Arc::new(crate::goal::GoalCreateTool),
        Arc::new(crate::goal::GoalGetTool),
        Arc::new(crate::goal::GoalUpdateTool),
    ];
    for t in simple {
        reg.register(t);
    }
    #[cfg(feature = "web")]
    reg.register(Arc::new(crate::web_fetch::WebFetchTool::new()));
    #[cfg(feature = "structured-output")]
    reg.register(Arc::new(
        crate::structured_output::StructuredOutputTool::default(),
    ));
    // Needs the same registry it searches over — registered last so it sees
    // every tool registered above (it reads `reg.list()` at call time via the
    // `Arc`, so registration order doesn't actually matter for correctness,
    // but "last" documents the dependency clearly).
    reg.register(Arc::new(crate::tool_search::ToolSearchTool::new(
        reg.clone(),
    )));
}

/// Register `WebSearchTool` ("WebSearch"), searching through `model`'s
/// provider as `model_name` ([`native_search::NativeSearchProvider`]).
///
/// Not part of [`register_builtin_tools`] because the search goes through a
/// model, which only the session builder has. Both the Research and Chat
/// scenes list `"WebSearch"` in their `tools()` whitelist, and whitelists are
/// *intersected* with the registry — an unregistered name is not an error, it
/// silently disappears.
///
/// Registers nothing in a build without the `web` tools.
pub fn register_web_search(
    reg: &std::sync::Arc<base::tool::InMemoryToolRegistry>,
    model: Arc<dyn base::interface::model::Model>,
    model_name: String,
) {
    #[cfg(not(feature = "web"))]
    let _ = (reg, model, model_name);
    #[cfg(feature = "web")]
    reg.register(Arc::new(crate::web_search::WebSearchTool::new(Box::new(
        crate::native_search::NativeSearchProvider::new(model, model_name),
    ))));
}

/// Make the registry's `WebFetch` answer its `prompt` through `extraction`
/// (a model and the model name to call it with), replacing the one already
/// there. Without `extraction` an existing `WebFetch` is kept as it is — a
/// delegate's registry carries its parent's — and a plain one is registered
/// only when there is none. Never leaves two entries under the name.
///
/// Registers nothing in a build without the `web` tools.
pub fn register_web_fetch(
    reg: &base::tool::InMemoryToolRegistry,
    extraction: Option<(Arc<dyn base::interface::model::Model>, String)>,
) {
    #[cfg(not(feature = "web"))]
    let _ = (reg, extraction);
    #[cfg(feature = "web")]
    match extraction {
        Some((model, model_name)) => {
            reg.replace(Arc::new(crate::web_fetch::WebFetchTool::with_extractor(
                crate::page_extract::PageExtractor::new(model, model_name),
            )));
        }
        None if reg.get("WebFetch").is_none() => {
            reg.register(Arc::new(crate::web_fetch::WebFetchTool::new()));
        }
        None => {}
    }
}

/// Registers `StructuredOutput` bound to `schema`, the host's output contract,
/// in place of any unbound one. A session without a schema has no use for the
/// tool, so this is the only way it gets one. Does nothing in a build without
/// the `structured-output` tool.
pub fn bind_output_schema(reg: &base::tool::InMemoryToolRegistry, schema: &serde_json::Value) {
    #[cfg(not(feature = "structured-output"))]
    let _ = (reg, schema);
    #[cfg(feature = "structured-output")]
    reg.replace(Arc::new(
        crate::structured_output::StructuredOutputTool::with_schema(schema.clone()),
    ));
}

/// Register `CronCreate` / `CronDelete` / `CronList` against a shared store.
///
/// The "scheduler bridge" these needed is just the store plus something that
/// drains it: `CronStore::pop_due()` returns the jobs whose expression matches
/// the current minute (updating `last_fired_ms` for recurring jobs and
/// dropping one-shots), and the caller is responsible for feeding those
/// prompts back into the engine — see `Builder::build()`, which spawns a
/// once-a-minute ticker doing exactly that.
pub fn register_cron_tools(
    reg: &std::sync::Arc<base::tool::InMemoryToolRegistry>,
    store: Arc<crate::cron::CronStore>,
) {
    reg.register(Arc::new(crate::cron::CronCreateTool::new(store.clone())));
    reg.register(Arc::new(crate::cron::CronDeleteTool::new(store.clone())));
    reg.register(Arc::new(crate::cron::CronListTool::new(store)));
}

/// Register `EnterWorktree` / `ExitWorktree` against a session-scoped registry.
///
/// `WorktreeRegistry` holds at most one active worktree and is deliberately
/// per-session, not global: `EnterWorktree` refuses to create a second one and
/// `ExitWorktree` cleans up whatever the same session created.
pub fn register_worktree_tools(
    reg: &std::sync::Arc<base::tool::InMemoryToolRegistry>,
    registry: Arc<crate::worktree_tools::WorktreeRegistry>,
) {
    reg.register(Arc::new(crate::worktree_tools::EnterWorktreeTool::new(
        registry.clone(),
    )));
    reg.register(Arc::new(crate::worktree_tools::ExitWorktreeTool::new(
        registry,
    )));
}

pub fn input_schema<T: schemars::JsonSchema>() -> serde_json::Value {
    #[allow(clippy::expect_used)]
    serde_json::to_value(schemars::schema_for!(T)).expect("a schemars schema is always valid JSON")
}

#[cfg(test)]
mod register_builtin_tools_tests {
    use super::*;

    /// The tools the built-in scenes list are registered by this single
    /// function, not by `Builder::build()`.
    #[test]
    fn registers_the_tools_every_built_in_scene_lists() {
        let reg = Arc::new(base::tool::InMemoryToolRegistry::new());
        register_builtin_tools(&reg);
        let names: std::collections::HashSet<String> = reg
            .list()
            .into_iter()
            .map(|t| t.name().to_string())
            .collect();

        // CodeScene's tools() is an empty allow-list ("everything
        // registered is allowed") — it depends on these actually being
        // registered, not just declared in a scene's tools() list.
        for expected in [
            "Bash",
            "Read",
            "Write",
            "Edit",
            "Grep",
            "Glob",
            "TodoWrite",
            "WebFetch",
        ] {
            assert!(
                names.contains(expected),
                "missing built-in tool: {expected}"
            );
        }
    }

    /// A model whose provider has no search of its own.
    struct NoSearch;

    #[async_trait::async_trait]
    impl base::interface::model::Model for NoSearch {
        fn api_type(&self) -> base::provider::ApiType {
            base::provider::ApiType::OpenAICompatible
        }
        async fn stream(
            &self,
            _: Vec<base::prompt::PromptBlock>,
            _: Vec<base::interface::model::ToolDef>,
            _: Vec<base::interface::model::ModelMessage>,
            _: base::interface::model::StreamParams,
            _: tokio_util::sync::CancellationToken,
        ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError>
        {
            Err(base::interface::model::ModelError::Unsupported(
                "this provider has no web search of its own".into(),
            ))
        }
    }

    /// `WebSearch` must end up in the registry: the `harnix` scene lists it,
    /// and a whitelist is intersected with the registry, so an unregistered
    /// name is silently dropped.
    #[test]
    fn register_web_search_puts_websearch_in_the_registry() {
        let reg = Arc::new(base::tool::InMemoryToolRegistry::new());
        register_builtin_tools(&reg);
        assert!(
            reg.get("WebSearch").is_none(),
            "register_builtin_tools must stay model-free; WebSearch comes from register_web_search"
        );
        register_web_search(&reg, Arc::new(NoSearch), "m".into());
        let t = reg.get("WebSearch").expect("WebSearch registered");
        assert!(t.call_traits(&serde_json::Value::Null).read_only);
    }

    /// Invocable, not merely present: on a provider that cannot search the
    /// tool still validates its input, and says why it found nothing.
    #[tokio::test]
    async fn registered_web_search_is_invocable() {
        let reg = Arc::new(base::tool::InMemoryToolRegistry::new());
        register_web_search(&reg, Arc::new(NoSearch), "m".into());
        let t = reg.get("WebSearch").unwrap();

        let bad = t
            .validate_input(
                &serde_json::json!({"query": "   "}),
                &platform::tool_context_for_test("/tmp".into()),
            )
            .await;
        assert!(bad.is_err(), "empty query must fail validation");

        let r = t
            .call(
                serde_json::json!({"query": "rust release notes"}),
                platform::tool_context_for_test("/tmp".into()),
                base::tool::ProgressSender::noop("t"),
            )
            .await;
        match r {
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains("no web search of its own"),
                    "unhelpful error: {msg}"
                );
            }
            Ok(ok) => panic!("expected a descriptive error, got {:?}", ok.content),
        }
    }

    fn web_fetches(reg: &base::tool::InMemoryToolRegistry) -> Vec<Arc<dyn Tool>> {
        reg.list()
            .into_iter()
            .filter(|t| t.name() == "WebFetch")
            .collect()
    }

    #[test]
    fn register_web_fetch_leaves_one_web_fetch_either_way() {
        let empty = base::tool::InMemoryToolRegistry::new();
        register_web_fetch(&empty, None);
        assert_eq!(web_fetches(&empty).len(), 1, "registered when absent");

        let reg = Arc::new(base::tool::InMemoryToolRegistry::new());
        register_builtin_tools(&reg);
        register_web_fetch(&reg, None);
        assert_eq!(web_fetches(&reg).len(), 1);
        assert!(web_fetches(&reg)[0].description().contains("plain text"));

        register_web_fetch(&reg, Some((Arc::new(NoSearch), "fetch-model".into())));
        let fetches = web_fetches(&reg);
        assert_eq!(fetches.len(), 1, "replaced, not appended");
        assert!(fetches[0].description().contains("answer `prompt`"));
    }

    #[test]
    fn without_a_routed_model_an_extracting_web_fetch_is_kept() {
        let reg = Arc::new(base::tool::InMemoryToolRegistry::new());
        register_web_fetch(&reg, Some((Arc::new(NoSearch), "fetch-model".into())));
        register_web_fetch(&reg, None);
        let fetches = web_fetches(&reg);
        assert_eq!(fetches.len(), 1);
        assert!(fetches[0].description().contains("answer `prompt`"));
    }

    /// Cron tools registered against a shared store actually
    /// mutate that store — create → list → delete round-trips.
    #[tokio::test]
    async fn registered_cron_tools_round_trip_through_the_store() {
        let reg = Arc::new(base::tool::InMemoryToolRegistry::new());
        let store = Arc::new(crate::cron::CronStore::new());
        register_cron_tools(&reg, store.clone());

        let ctx = || platform::tool_context_for_test("/tmp".into());
        let created = reg
            .get("CronCreate")
            .unwrap()
            .call(
                serde_json::json!({"cron": "*/5 * * * *", "prompt": "check CI"}),
                ctx(),
                base::tool::ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        assert!(!created.is_error);
        let id = created.structured_content.as_ref().unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(store.list().len(), 1, "CronCreate must reach the store");

        let listed = reg
            .get("CronList")
            .unwrap()
            .call(
                serde_json::json!({}),
                ctx(),
                base::tool::ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        match &listed.content {
            base::tool::ToolResultContent::Text(s) => assert!(s.contains("check CI"), "{s}"),
            _ => panic!("expected text"),
        }

        let deleted = reg
            .get("CronDelete")
            .unwrap()
            .call(
                serde_json::json!({"id": id}),
                ctx(),
                base::tool::ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        assert!(!deleted.is_error);
        assert!(store.list().is_empty(), "CronDelete must reach the store");
    }

    /// Worktree tools share one registry, so `ExitWorktree`
    /// sees what `EnterWorktree` created. Nothing is active here, so the
    /// observable contract is the "no active worktree" path — the one that
    /// proves the two tools were wired to the *same* registry rather than
    /// each getting its own.
    #[tokio::test]
    async fn registered_worktree_tools_share_one_registry() {
        let reg = Arc::new(base::tool::InMemoryToolRegistry::new());
        let wt = Arc::new(crate::worktree_tools::WorktreeRegistry::new());
        register_worktree_tools(&reg, wt.clone());

        assert!(reg.get("EnterWorktree").is_some());
        let exited = reg
            .get("ExitWorktree")
            .unwrap()
            .call(
                serde_json::json!({}),
                platform::tool_context_for_test("/tmp".into()),
                base::tool::ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        assert!(exited.is_error);
        match &exited.content {
            base::tool::ToolResultContent::Text(s) => {
                assert!(s.contains("No active worktree"), "{s}")
            }
            _ => panic!("expected text"),
        }
        assert!(wt.current().is_none());

        // A traversal slug is rejected before any git command runs.
        let bad = reg
            .get("EnterWorktree")
            .unwrap()
            .validate_input(
                &serde_json::json!({"slug": "../escape"}),
                &platform::tool_context_for_test("/tmp".into()),
            )
            .await;
        assert!(bad.is_err());
    }

    #[test]
    fn tool_search_tool_sees_the_other_registered_tools() {
        // ToolSearchTool needs the *same* registry Arc it searches over; a
        // registry snapshotted before the rest were registered would leave it
        // permanently blind.
        let reg = Arc::new(base::tool::InMemoryToolRegistry::new());
        register_builtin_tools(&reg);
        let names: Vec<String> = reg
            .list()
            .into_iter()
            .map(|t| t.name().to_string())
            .collect();
        assert!(names.contains(&"ToolSearch".to_string()));
        assert!(names.contains(&"Bash".to_string()));
    }
}
