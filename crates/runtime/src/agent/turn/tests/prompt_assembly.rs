//! 提示词装配、工具定义与 MCP 适配器同步。

use crate::agent::sync_mcp_adapters;
use crate::agent::Agent;
use base::interface::model::ModelMessage;
use base::interface::model::ToolDef;
use base::prompt::PromptBlock;
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use base::error::ToolError;
use base::interface::scene::{ExecutionParams, FixedPrompt, SceneSpec, ToolSurface};
use base::tool::{
    InMemoryToolRegistry, PathStyle, ProgressSender, PromptContext, Tool, ToolContext, ToolResult,
};
use serde_json::{json, Value};

const GUIDE_BODY: &str = "# Sub-agent usage guide\n\nDispatch a sub-agent when \
                              the search space is wide and only the conclusion matters.";

/// A tool with a substantial `Tool::guide()` — the `AgentTool` /
/// `TeamCreateTool` shape, whose guides were unreachable.
struct GuidedTool;
#[async_trait::async_trait]
impl Tool for GuidedTool {
    fn name(&self) -> &str {
        "Guided"
    }
    fn spec(&self) -> base::tool::ToolSpec {
        base::tool::ToolSpec::new()
    }

    fn description(&self) -> &str {
        "Launch a sub-agent."
    }
    fn input_schema(&self, _: PathStyle) -> Value {
        json!({"type": "object", "properties": {"prompt": {"type": "string"}}})
    }
    async fn guide(&self, _: &PromptContext) -> Option<String> {
        Some(GUIDE_BODY.to_string())
    }
    async fn permission_facts(
        &self,
        _: &serde_json::Value,
        _: &base::tool::ToolContext,
    ) -> base::interface::permission::PermissionFacts {
        use base::interface::permission::{PermissionFacts, Reach, Subject};
        PermissionFacts::new(Subject::None, Reach::SessionOnly)
    }
    async fn call(
        &self,
        _: Value,
        _: ToolContext,
        _: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::text("ok"))
    }
}

/// A tool that never overrides `guide()` — must be left completely
/// alone, both in `build_tool_defs` and in `ToolSearch`.
struct PlainTool;
#[async_trait::async_trait]
impl Tool for PlainTool {
    fn name(&self) -> &str {
        "Plain"
    }
    fn spec(&self) -> base::tool::ToolSpec {
        base::tool::ToolSpec::new()
    }

    fn description(&self) -> &str {
        "Reads a file."
    }
    fn input_schema(&self, _: PathStyle) -> Value {
        json!({"type": "object"})
    }
    async fn call(
        &self,
        _: Value,
        _: ToolContext,
        _: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::text("ok"))
    }
}

struct DummyModel;
#[async_trait::async_trait]
impl base::interface::model::Model for DummyModel {
    fn api_type(&self) -> base::provider::ApiType {
        base::provider::ApiType::Anthropic
    }
    async fn stream(
        &self,
        _: Vec<PromptBlock>,
        _: Vec<ToolDef>,
        _: Vec<ModelMessage>,
        _: base::interface::model::StreamParams,
        _: CancellationToken,
    ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError> {
        unimplemented!("prompt assembly is exercised without issuing requests")
    }
}

/// A scene whose only distinguishing features are the tool whitelist and
/// the API-call ceiling, so tests can control whether `ToolSearch` is
/// reachable and what `execution` declares.
fn scene_with(allowed: Vec<String>, max_api_calls: u32) -> SceneSpec {
    let mut s = SceneSpec::new(
        "test",
        "Test",
        Arc::new(FixedPrompt(vec![PromptBlock::system("skeleton")])),
    );
    s.tools = ToolSurface::allowing(&allowed);
    s.execution = ExecutionParams::new(max_api_calls);
    s
}

fn test_scene(allowed: Vec<String>) -> Arc<SceneSpec> {
    Arc::new(scene_with(allowed, 200))
}

fn test_settings(root: &std::path::Path) -> base::settings::LoadedSettings {
    use base::settings::*;
    let mut values = Settings::defaults_for("test");
    values.model.max_tokens = 2000;
    values.model.thinking_mode = ThinkingMode::Auto;
    values.model.effort = None;
    values.memory_enabled = false;
    values.telemetry_enabled = false;
    let mut paths = PathSettings::default();
    paths.user_data_dir = root.to_path_buf();
    paths.global_data_dir = root.to_path_buf();
    paths.workspace = base::settings::Workspace::project_of(root);
    paths.local_data_dir = root.to_path_buf();
    base::settings::LoadedSettings::new(values, paths)
}

/// An `Agent` wired with `GuidedTool`, `PlainTool` and the real
/// `ToolSearch` and nothing else, so `build_tool_defs()` output is
/// exactly these three and easy to assert on.
fn agent_with(settings: base::settings::LoadedSettings, scene_allowed: Vec<String>) -> Agent {
    let registry = Arc::new(InMemoryToolRegistry::new());
    registry.register(Arc::new(GuidedTool));
    registry.register(Arc::new(PlainTool));
    registry.register(Arc::new(tools::tool_search::ToolSearchTool::new(
        registry.clone(),
    )));
    let (agent, _rx, _tx) = crate::agent::Builder::new()
        .scene(test_scene(scene_allowed))
        .model(Arc::new(DummyModel))
        .tools(registry)
        .settings(Arc::new(settings))
        .skip_warmup(true)
        .build()
        .expect("builder should succeed");
    agent
}

/// The whole point of the progressive-disclosure design: `build_tool_defs`
/// advertises that a guide exists without paying for its body. If this
/// ever inverts, every request grows by ~85 KB across the real tool set.
#[tokio::test]
async fn tool_defs_point_at_the_guide_instead_of_carrying_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut agent = agent_with(test_settings(dir.path()), vec![]);

    let defs = agent.build_tool_defs().await;
    let guided = defs.iter().find(|d| d.name == "Guided").unwrap();

    assert!(
        guided
            .description
            .contains(r#"ToolSearch{"query":"select:Guided"}"#),
        "documented tool must tell the model how to fetch its guide, got: {}",
        guided.description
    );
    assert!(
        !guided.description.contains("Dispatch a sub-agent when"),
        "the guide BODY must not be in the per-request tools payload: {}",
        guided.description
    );
    // Payload growth is one short pointer line, not the document.
    let overhead = guided.description.len() - GuidedTool.description().len();
    assert!(
        overhead < 80,
        "pointer should be a single short line, added {overhead} bytes"
    );
    assert!(
        !defs.iter().any(|d| d.description.contains(GUIDE_BODY)),
        "no tool def may carry a full usage guide"
    );
}

/// A tool without a `guide()` comes through byte-identical.
#[tokio::test]
async fn undocumented_tool_def_is_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let mut agent = agent_with(test_settings(dir.path()), vec![]);

    let defs = agent.build_tool_defs().await;
    let plain = defs.iter().find(|d| d.name == "Plain").unwrap();
    assert_eq!(plain.description, "Reads a file.");
}

/// Pointing at a fetcher the scene filtered out is a dead end: the model
/// would be told a guide exists with no way to reach it.
#[tokio::test]
async fn no_pointer_when_the_scene_excludes_tool_search() {
    let dir = tempfile::tempdir().unwrap();
    let mut agent = agent_with(
        test_settings(dir.path()),
        vec!["Guided".into(), "Plain".into()], // whitelist without ToolSearch
    );

    let defs = agent.build_tool_defs().await;
    assert!(!defs.iter().any(|d| d.name == "ToolSearch"));
    let guided = defs.iter().find(|d| d.name == "Guided").unwrap();
    assert_eq!(guided.description, "Launch a sub-agent.");
}

use crate::module::fakes::{mcp_module, mcp_tool, FakeMcp};

#[tokio::test]
async fn sync_mcp_adapters_adds_previously_unseen_tools() {
    let tools = base::tool::InMemoryToolRegistry::new();
    let mgr = FakeMcp(vec![mcp_tool("test-server", "new-tool")]);

    sync_mcp_adapters(&tools, &mgr, |_| true);
    assert!(tools.get("mcp__test-server__new-tool").is_some());
}

#[tokio::test]
async fn sync_mcp_adapters_registers_each_tool_once() {
    let tools = base::tool::InMemoryToolRegistry::new();
    let mgr = FakeMcp(vec![mcp_tool("test-server", "existing-tool")]);

    sync_mcp_adapters(&tools, &mgr, |_| true);
    sync_mcp_adapters(&tools, &mgr, |_| true);

    let count = tools
        .list()
        .iter()
        .filter(|t| t.name() == "mcp__test-server__existing-tool")
        .count();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn sync_mcp_adapters_takes_the_relisted_instance_and_drops_vanished_tools() {
    let tools = base::tool::InMemoryToolRegistry::new();
    let before = FakeMcp(vec![
        mcp_tool("test-server", "kept"),
        mcp_tool("test-server", "gone"),
    ]);
    sync_mcp_adapters(&tools, &before, |_| true);

    let after = FakeMcp(vec![mcp_tool("test-server", "kept")]);
    sync_mcp_adapters(&tools, &after, |_| true);

    assert!(
        tools.get("mcp__test-server__gone").is_none(),
        "a tool the server no longer lists must stop being callable"
    );
    let kept = tools.get("mcp__test-server__kept").unwrap();
    assert!(Arc::ptr_eq(&kept, &after.0[0]));
}

#[tokio::test]
async fn sync_mcp_adapters_leaves_builtins_alone() {
    let tools = base::tool::InMemoryToolRegistry::new();
    let builtin: Arc<dyn base::tool::Tool> = Arc::new(PlainTool);
    tools.register(builtin.clone());
    let mgr = FakeMcp(Vec::new());

    sync_mcp_adapters(&tools, &mgr, |_| true);
    assert!(Arc::ptr_eq(&tools.get("Plain").unwrap(), &builtin));
}

#[tokio::test]
async fn an_mcp_adapter_is_advertised_once_and_not_reported_as_shadowed() {
    let dir = tempfile::tempdir().unwrap();

    let (mut agent, _rx, _tx) = crate::agent::Builder::new()
        .scene(test_scene(vec![]))
        .model(Arc::new(DummyModel))
        .tools(Arc::new(InMemoryToolRegistry::new()))
        .settings(Arc::new(test_settings(dir.path())))
        .module(mcp_module(vec![mcp_tool("test-server", "lookup")]))
        .skip_warmup(true)
        .build()
        .unwrap();

    let defs = agent.build_tool_defs().await;
    let matching: Vec<_> = defs
        .iter()
        .filter(|d| d.name == "mcp__test-server__lookup")
        .collect();
    assert_eq!(matching.len(), 1);
    assert_eq!(matching[0].source.as_deref(), Some("mcp:test-server"));
    assert!(
        agent.tooling.shadow_warned().is_empty(),
        "the dispatch copy of an MCP adapter is not a builtin shadowing it"
    );
}

/// Deferral is the session's, not the tool's: an MCP tool a server starts
/// listing mid-session is advertised by name only when the scene defers it,
/// exactly like one that was there when the session was built.
#[tokio::test]
async fn an_mcp_tool_that_arrives_mid_session_is_deferred_by_the_scene() {
    let mut defers = scene_with(vec![], 200);
    defers.tools.deferred = vec!["mcp__desk__late".into()];

    let dir = tempfile::tempdir().unwrap();
    let (mut agent, _rx, _tx) = crate::agent::Builder::new()
        .scene(Arc::new(defers))
        .model(Arc::new(DummyModel))
        .tools(Arc::new(InMemoryToolRegistry::new()))
        .settings(Arc::new(test_settings(dir.path())))
        .module(mcp_module(vec![mcp_tool("desk", "early")]))
        .skip_warmup(true)
        .build()
        .unwrap();
    assert!(agent.tooling.tools().get("mcp__desk__late").is_none());

    let arrived = FakeMcp(vec![mcp_tool("desk", "early"), mcp_tool("desk", "late")]);
    sync_mcp_adapters(agent.tooling.tools().as_ref(), &arrived, |_| true);
    agent.tooling.set_mcp(Box::new(arrived));

    let defs = agent.build_tool_defs().await;
    let schema_of = |name: &str| {
        defs.iter()
            .find(|d| d.name == name)
            .unwrap_or_else(|| panic!("{name} advertised"))
            .input_schema
            .clone()
    };
    assert_eq!(
        schema_of("mcp__desk__late"),
        json!({"type": "object", "properties": {}})
    );
    assert_eq!(schema_of("mcp__desk__early"), json!({"type": "object"}));
}

/// A session rooted in a monorepo sub-package must still see the
/// repository-root conventions.
#[tokio::test]
async fn parent_agents_md_reaches_the_prompt_with_nearest_last() {
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("repo");
    let child = parent.join("packages").join("api");
    std::fs::create_dir_all(&child).unwrap();
    std::fs::write(parent.join("AGENTS.md"), "ROOT-CONVENTION: use tabs").unwrap();
    std::fs::write(child.join("AGENTS.md"), "PACKAGE-CONVENTION: use spaces").unwrap();

    let frozen = base::frozen::FrozenContext::collect(
        child.clone(),
        &base::paths::ConfigPaths::new(child.join(".state"), child.join(".agix"), "code"),
        &base::interface::environment::SystemEnvironment,
        &base::engine_exec::EngineExec::new(
            platform::local_providers(),
            Default::default(),
            Default::default(),
        ),
    )
    .await;
    let mut settings = test_settings(dir.path());
    settings.paths.local_data_dir = child.clone();
    settings.paths.workspace =
        base::settings::Workspace::project_of(&settings.paths.local_data_dir);
    let (agent, _rx, _tx) = crate::agent::Builder::new()
        .scene(test_scene(vec![]))
        .model(Arc::new(DummyModel))
        .tools(Arc::new(InMemoryToolRegistry::new()))
        .settings(Arc::new(settings))
        .frozen(frozen)
        .skip_warmup(true)
        .build()
        .unwrap();

    let text = agent.build_instruction_context();
    assert!(
        text.contains("ROOT-CONVENTION"),
        "parent AGENTS.md must reach the prompt: {text}"
    );
    assert!(text.contains("PACKAGE-CONVENTION"));

    // Documented precedence: outermost first, nearest last, nearest wins.
    let root_at = text.find("ROOT-CONVENTION").unwrap();
    let pkg_at = text.find("PACKAGE-CONVENTION").unwrap();
    assert!(
        root_at < pkg_at,
        "root must come first so the nearest file is read last: {text}"
    );
    assert!(
        text.contains("later") && text.contains("wins"),
        "precedence must be stated, not merely implied by order: {text}"
    );
}

/// The single-file (non-monorepo) case stays byte-identical to what was
/// injected before parent files were merged in — no headers, no
/// precedence preamble, no new tokens for the common case.
#[tokio::test]
async fn single_instruction_file_is_injected_verbatim() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("solo");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("AGENTS.md"), "ONLY-CONVENTION").unwrap();

    let frozen = base::frozen::FrozenContext::collect(
        root.clone(),
        &base::paths::ConfigPaths::new(root.join(".state"), root.join(".agix"), "code"),
        &base::interface::environment::SystemEnvironment,
        &base::engine_exec::EngineExec::new(
            platform::local_providers(),
            Default::default(),
            Default::default(),
        ),
    )
    .await;
    let mut settings = test_settings(dir.path());
    settings.paths.local_data_dir = root.clone();
    settings.paths.workspace =
        base::settings::Workspace::project_of(&settings.paths.local_data_dir);
    let (agent, _rx, _tx) = crate::agent::Builder::new()
        .scene(test_scene(vec![]))
        .model(Arc::new(DummyModel))
        .tools(Arc::new(InMemoryToolRegistry::new()))
        .settings(Arc::new(settings))
        .frozen(frozen)
        .skip_warmup(true)
        .build()
        .unwrap();

    assert_eq!(agent.build_instruction_context(), "ONLY-CONVENTION");
}

/// Both recovery paths (overloaded → fallback model, prompt-too-long →
/// compact and retry) share `build_prompt_blocks` with the normal path —
/// and that function is the only `assemble_prompt` call site — so
/// asserting the shared builder carries skills asserts all three paths.
#[tokio::test]
async fn rebuilt_prompt_still_carries_skills_and_scene_skeleton() {
    let dir = tempfile::tempdir().unwrap();
    let mut agent = agent_with(test_settings(dir.path()), vec![]);

    struct OneSkill(base::frozen::SkillEntry);
    impl base::interface::skill_catalog::SkillCatalog for OneSkill {
        fn list(&self) -> Vec<base::frozen::SkillEntry> {
            vec![self.0.clone()]
        }
        fn get(&self, name: &str) -> Option<base::frozen::SkillEntry> {
            (self.0.name == name).then(|| self.0.clone())
        }
        fn body(&self, _: &str) -> Option<String> {
            None
        }
        fn reload_changed(&self) -> usize {
            0
        }
        fn discover_for_paths(&self, _: &[PathBuf]) -> Vec<base::frozen::SkillEntry> {
            Vec::new()
        }
        fn activate_for_paths(&self, _: &[PathBuf]) -> Vec<base::frozen::SkillEntry> {
            Vec::new()
        }
        fn invocation_count(&self, _: &str) -> u32 {
            0
        }
        fn last_invoked_seq(&self, _: &str) -> u64 {
            0
        }
    }
    agent
        .tooling
        .set_skills(Arc::new(OneSkill(base::frozen::SkillEntry {
            name: "release-checklist".into(),
            description: "Runs the pre-release checklist".into(),
            user_invocable: true,
            ..Default::default()
        })));

    let joined = agent
        .build_prompt_blocks("test")
        .await
        .iter()
        .map(|b| b.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        joined.contains("release-checklist"),
        "the shared prompt builder — used by the normal path and both \
             recovery paths — must carry the skill inventory: {joined}"
    );
    assert!(joined.contains("skeleton"), "scene skeleton missing");
}

/// A scene's declared ceiling binds: the turn's ceiling is `min(settings,
/// scene)`, whichever is stricter. The rule lives in `LimitsPolicy::new`,
/// which the agent builder feeds these two values.
#[test]
fn scene_execution_params_tighten_the_api_call_ceiling() {
    let dir = tempfile::tempdir().unwrap();
    let settings = test_settings(dir.path());
    assert_eq!(settings.values.execution.max_api_calls_per_turn, 25);

    let resolve = |scene_limit: u32| {
        let scene = scene_with(vec![], scene_limit);
        settings
            .values
            .execution
            .max_api_calls_per_turn
            .min(scene.execution.max_api_calls_per_turn)
    };

    assert_eq!(resolve(3), 3, "a scene tighter than settings must win");
    assert_eq!(
        resolve(5_000),
        25,
        "a scene cannot widen past the deployment-wide setting"
    );
}

mod input_map {
    use crate::turn::prompt::{resolve_input_map, user_text_fingerprint};
    use base::interface::model::{InputSpan, MessageRole, ModelContentBlock, ModelMessage};

    fn user(text: &str) -> ModelMessage {
        ModelMessage {
            role: MessageRole::User,
            content: vec![ModelContentBlock::Text { text: text.into() }],
        }
    }

    fn assistant(text: &str) -> ModelMessage {
        ModelMessage {
            role: MessageRole::Assistant,
            content: vec![ModelContentBlock::Text { text: text.into() }],
        }
    }

    fn pending(text: &str) -> (u64, Vec<InputSpan>) {
        (
            user_text_fingerprint(&user(text)).unwrap(),
            vec![InputSpan {
                source: base::interface::model::input_source::USER_PROMPT.into(),
                block: 0,
                range: Some((0, text.len())),
            }],
        )
    }

    #[test]
    fn points_at_the_message_it_was_built_from() {
        let messages = vec![user("older"), assistant("reply"), user("this turn")];
        let map = resolve_input_map(Some(&pending("this turn")), &messages).unwrap();
        assert_eq!(map.user_message, 2);
        assert_eq!(map.spans.len(), 1);
    }

    /// Compaction rewrites the message list between the push and the
    /// request, so the index has to be found, not remembered. Here the
    /// same message sits two slots earlier than when it was pushed.
    #[test]
    fn survives_the_message_list_being_rewritten() {
        let pending = pending("this turn");
        let before = vec![user("a"), assistant("b"), user("this turn")];
        let after_compaction = vec![user("summary"), user("this turn")];
        assert_eq!(
            resolve_input_map(Some(&pending), &before)
                .unwrap()
                .user_message,
            2
        );
        assert_eq!(
            resolve_input_map(Some(&pending), &after_compaction)
                .unwrap()
                .user_message,
            1
        );
    }

    /// A turn's later steps append tool-result user messages after the
    /// prompt, so the search runs from the end and must not stop on those.
    #[test]
    fn is_not_confused_by_tool_result_messages_appended_after_it() {
        let messages = vec![
            user("this turn"),
            assistant("calling a tool"),
            user("<tool_result>…</tool_result>"),
        ];
        assert_eq!(
            resolve_input_map(Some(&pending("this turn")), &messages)
                .unwrap()
                .user_message,
            0
        );
    }

    /// Compaction folding the message away means there is nothing to point
    /// at. Absent is the honest answer; an index would name another message.
    #[test]
    fn a_compacted_away_message_yields_no_map() {
        let messages = vec![user("summary of everything"), assistant("ok")];
        assert!(resolve_input_map(Some(&pending("this turn")), &messages).is_none());
    }

    #[test]
    fn no_pending_input_yields_no_map() {
        assert!(resolve_input_map(None, &[user("anything")]).is_none());
    }

    /// An image-first message has no text to fingerprint, so it can never
    /// be mistaken for the prompt.
    #[test]
    fn a_message_with_no_leading_text_has_no_fingerprint() {
        let image_only = ModelMessage {
            role: MessageRole::User,
            content: vec![ModelContentBlock::Image {
                media_type: "image/png".into(),
                data: "AAAA".into(),
            }],
        };
        assert!(user_text_fingerprint(&image_only).is_none());
    }
}
