//! Built-in tool implementations — all concrete tools the agent can invoke.

pub mod aliases;
pub mod ask_user;
pub mod bash;
pub mod cancel;
pub mod config;
pub mod cron;
pub mod deferred;
mod exec_capture;
pub mod file_edit;
pub mod file_read;
pub mod file_write;
pub mod glob;
pub mod grep;
pub mod monitor;
pub mod notebook_edit;
pub mod ping;
pub mod plan_mode;
pub mod plan_verify;
pub mod push_notification;
pub mod saas_stubs;
pub mod schedule_wakeup;
pub mod security;
pub mod skill_tool;
pub mod sleep;
pub mod structured_output;
pub mod task_output;
pub mod task_stop;
pub mod tasks;
pub mod todo_write;
pub mod tool_search;
pub mod web_fetch;
pub mod web_search;
pub mod worktree;
pub mod worktree_tools;

// From anthropic/ — tool-side logic
pub mod native_search;
pub mod secondary_llm;

// Agent tool

use base::tool::Tool;
use std::sync::Arc;

/// Assemble the final tool pool: deduplicate built-in and MCP tools by name.
/// Built-in tools take priority on name conflict.
pub fn assemble_tool_pool(
    builtin: Vec<Arc<dyn Tool>>,
    mcp: Vec<Arc<dyn Tool>>,
) -> Vec<Arc<dyn Tool>> {
    use std::collections::BTreeMap;
    let mut pool: BTreeMap<String, Arc<dyn Tool>> = BTreeMap::new();
    for t in mcp {
        pool.insert(t.name().to_string(), t);
    }
    for t in builtin {
        pool.insert(t.name().to_string(), t);
    }
    pool.into_values().collect()
}

/// Reference-based variant — used in `build_tool_defs()` where tools are behind Arc.
pub fn assemble_tool_pool_refs<'a>(
    builtin: Vec<&'a dyn Tool>,
    mcp: Vec<&'a dyn Tool>,
) -> Vec<&'a dyn Tool> {
    use std::collections::BTreeMap;
    let mut pool: BTreeMap<String, &dyn Tool> = BTreeMap::new();
    for t in mcp {
        pool.insert(t.name().to_string(), t);
    }
    for t in builtin {
        pool.insert(t.name().to_string(), t);
    }
    pool.into_values().collect()
}

pub fn register_skill_tool(
    r: &base::tool::InMemoryToolRegistry,
    m: std::sync::Arc<skills::manager::SkillManager>,
    spawner: Option<std::sync::Arc<dyn base::interface::agent_spawner::AgentSpawner>>,
    permission: std::sync::Arc<dyn base::interface::permission::Permission>,
) {
    let mut tool = crate::skill_tool::SkillTool::new(m).with_permission(permission);
    if let Some(spawner) = spawner {
        tool = tool.with_spawner(spawner);
    }
    r.register(std::sync::Arc::new(tool));
}

/// Register the standard set of self-contained built-in tools — no host-specific
/// wiring (bridges, external providers, registries) required to construct any of
/// them. Every embedder (daemon, test harness, ...) that wants a working
/// Bash/Read/Write/Edit/... agent needs to call this on the registry it hands to
/// `runtime::agent::Builder::tools()` — `Builder::build()` does NOT populate these
/// itself. It only auto-registers a handful of tools that need engine-internal
/// state to construct (Skill, TaskStop, TaskOutput, Import, the "Agent" spawner,
/// MCP adapters) — see `crates/runtime/src/agent.rs::Builder::build()`.
///
/// Found via: `daemon` never called `Builder::tools()` at all (confirmed by its
/// own startup log — `"startup: tools registered (noop)"`,
/// `daemon/src/main.rs`), so every daemon session ran with only that
/// engine-internal handful and nothing else — no Bash, no file edits, no search.
/// `tests/runner/src/api_runner.rs` had independently hand-rolled a near-identical
/// list for the same reason; this is now the single source of truth both use.
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
/// Still excluded with no helper: `saas_stubs::McpAuthTool` (needs SaaS auth
/// infrastructure that does not exist here).
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
        Arc::new(crate::tasks::TaskCreateTool),
        Arc::new(crate::tasks::TaskGetTool),
        Arc::new(crate::tasks::TaskListTool),
        Arc::new(crate::tasks::TaskUpdateTool),
        Arc::new(crate::web_fetch::WebFetchTool::new()),
        Arc::new(crate::monitor::MonitorTool),
        Arc::new(crate::sleep::SleepTool),
        Arc::new(crate::ping::PingTool),
        Arc::new(crate::plan_mode::EnterPlanModeTool),
        Arc::new(crate::plan_mode::ExitPlanModeTool),
        Arc::new(crate::plan_verify::VerifyPlanExecutionTool),
        Arc::new(crate::schedule_wakeup::ScheduleWakeupTool),
        Arc::new(crate::push_notification::PushNotificationTool),
        Arc::new(crate::ask_user::AskUserQuestionTool),
        Arc::new(crate::structured_output::StructuredOutputTool::default()),
    ];
    for t in simple {
        reg.register(t);
    }
    // Needs the same registry it searches over — registered last so it sees
    // every tool registered above (it reads `reg.list()` at call time via the
    // `Arc`, so registration order doesn't actually matter for correctness,
    // but "last" documents the dependency clearly).
    reg.register(Arc::new(crate::tool_search::ToolSearchTool::new(
        reg.clone(),
    )));
}

/// Register `WebSearchTool` ("WebSearch"), the Research/Chat scenes' only
/// search capability.
///
/// Not part of [`register_builtin_tools`] because picking a `SearchProvider`
/// needs resolved `Settings` (endpoint + credentials), which only the session
/// builder has. Both scenes list `"WebSearch"` in their `tools()` whitelist,
/// and whitelists are *intersected* with the registry — so while this was
/// unregistered the effect was silent absence: no error, the model simply
/// never saw the tool and Research had no way to search.
///
/// The provider is [`native_search::NativeSearchProvider`] (a sub-call using
/// the provider's server-side `web_search_20250305` tool) when settings
/// resolve to an Anthropic-compatible endpoint, and
/// [`web_search::UnavailableSearchProvider`] otherwise. Note that on the
/// Anthropic-direct path the provider is never consulted at all — see
/// `UnavailableSearchProvider`'s docs for why registration alone is what
/// switches search on there.
pub fn register_web_search(
    reg: &std::sync::Arc<base::tool::InMemoryToolRegistry>,
    settings: &base::interface::settings::Settings,
) {
    let provider: Box<dyn crate::web_search::SearchProvider> =
        match crate::native_search::NativeSearchProvider::from_settings(settings) {
            Some(p) => Box::new(p),
            None => Box::new(crate::web_search::UnavailableSearchProvider::new(
                "no Anthropic-compatible endpoint or credentials resolved from settings/env",
            )),
        };
    reg.register(Arc::new(crate::web_search::WebSearchTool::new(provider)));
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

#[cfg(test)]
mod register_builtin_tools_tests {
    use super::*;

    /// Regression: `daemon` used to never call `Builder::tools()` at all, so
    /// every daemon session ran with zero of these — only the handful
    /// `Builder::build()` registers internally (Skill/TaskStop/TaskOutput/
    /// Import/Agent/MCP). Pin down that the tools every built-in scene
    /// actually lists (CodingScene/ChatScene/ResearchScene) end up
    /// registered by this single function.
    #[test]
    fn registers_the_tools_every_built_in_scene_lists() {
        let reg = Arc::new(base::tool::InMemoryToolRegistry::new());
        register_builtin_tools(&reg);
        let names: std::collections::HashSet<String> = reg
            .list()
            .into_iter()
            .map(|t| t.name().to_string())
            .collect();

        // CodingScene's tools() is an empty allow-list ("everything
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

    /// Settings that deterministically resolve to *no* client-side search
    /// backend. `api_type: OpenAICompatible` short-circuits
    /// `NativeSearchProvider::from_settings` before it consults
    /// `ANTHROPIC_*` environment variables, so this test doesn't depend on
    /// whether the developer running it happens to have a key exported.
    fn settings_without_search_backend() -> base::interface::settings::Settings {
        let mut s = base::interface::settings::Settings::defaults_for("test-model");
        s.model.api_type = base::provider::ApiType::OpenAICompatible;
        s.model.auth_token = String::new();
        s.model.base_url = String::new();
        s.providers.clear();
        s.default_provider = None;
        s
    }

    /// W-1: `WebSearch` must end up in the registry, because both
    /// `ChatScene::tools()` and `ResearchScene::tools()` list it and a
    /// whitelist is intersected with the registry — an unregistered name is
    /// silently dropped, leaving Research with no search capability at all.
    #[test]
    fn register_web_search_puts_websearch_in_the_registry() {
        let reg = Arc::new(base::tool::InMemoryToolRegistry::new());
        register_builtin_tools(&reg);
        assert!(
            reg.get("WebSearch").is_none(),
            "register_builtin_tools must stay settings-free; WebSearch comes from register_web_search"
        );
        register_web_search(&reg, &settings_without_search_backend());
        let t = reg.get("WebSearch").expect("WebSearch registered");
        assert!(t.is_read_only(&serde_json::Value::Null));
    }

    /// Invocable, not merely present: with no credentials the tool still
    /// validates input and returns an actionable error instead of silently
    /// producing nothing.
    #[tokio::test]
    async fn registered_web_search_is_invocable() {
        let reg = Arc::new(base::tool::InMemoryToolRegistry::new());
        register_web_search(&reg, &settings_without_search_backend());
        let t = reg.get("WebSearch").unwrap();

        let bad = t
            .validate_input(
                &serde_json::json!({"query": "   "}),
                &base::tool::ToolContext::for_test("/tmp".into()),
            )
            .await;
        assert!(bad.is_err(), "empty query must fail validation");

        let r = t
            .call(
                serde_json::json!({"query": "rust release notes"}),
                base::tool::ToolContext::for_test("/tmp".into()),
                base::tool::ProgressSender::noop("t"),
            )
            .await;
        match r {
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains("no client-side web search backend is configured"),
                    "unhelpful error: {msg}"
                );
            }
            Ok(ok) => panic!("expected a descriptive error, got {:?}", ok.content),
        }
    }

    /// W-2 bridge 1: cron tools registered against a shared store actually
    /// mutate that store — create → list → delete round-trips.
    #[tokio::test]
    async fn registered_cron_tools_round_trip_through_the_store() {
        let reg = Arc::new(base::tool::InMemoryToolRegistry::new());
        let store = Arc::new(crate::cron::CronStore::new());
        register_cron_tools(&reg, store.clone());

        let ctx = || base::tool::ToolContext::for_test("/tmp".into());
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

    /// W-2 bridge 2: worktree tools share one registry, so `ExitWorktree`
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
                base::tool::ToolContext::for_test("/tmp".into()),
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
                &base::tool::ToolContext::for_test("/tmp".into()),
            )
            .await;
        assert!(bad.is_err());
    }

    #[test]
    fn tool_search_tool_sees_the_other_registered_tools() {
        // ToolSearchTool needs the *same* registry Arc it searches over —
        // regression for constructing it with a registry snapshotted before
        // the rest were registered (would make it permanently blind).
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
