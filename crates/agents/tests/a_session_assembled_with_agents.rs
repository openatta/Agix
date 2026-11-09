//! What a session gets from being assembled with [`AgentsModule`]: the
//! `Agent` tool up to the depth limit, the team tools when the operator turns
//! them on, and both subject to the scene's tool policy like any built-in.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use agents::AgentsModule;
use async_trait::async_trait;
use base::interface::model::{Model, ModelMessage};
use base::interface::scene::Scene;
use base::interface::settings::*;
use base::tool::InMemoryToolRegistry;
use runtime::agent::{Agent, Builder};

struct DummyModel;

#[async_trait]
impl Model for DummyModel {
    fn api_type(&self) -> base::provider::ApiType {
        base::provider::ApiType::Anthropic
    }
    async fn stream(
        &self,
        _prompt_blocks: Vec<base::interface::prompt::PromptBlock>,
        _tools: Vec<base::interface::model::ToolDef>,
        _messages: Vec<ModelMessage>,
        _params: base::interface::model::StreamParams,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError> {
        Err(base::interface::model::ModelError::Internal(
            "DummyModel does not stream".into(),
        ))
    }
}

fn settings() -> Settings {
    let dir = std::env::temp_dir().join("agix-agents-module-tests");
    Settings {
        model: ModelSettings {
            api_type: base::provider::ApiType::Anthropic,
            base_url: String::new(),
            auth_token: String::new(),
            model_name: "test".into(),
            max_tokens: 2000,
            thinking_mode: ThinkingMode::Auto,
            effort: None,
            fallback_model: None,
        },
        paths: PathSettings {
            user_data_dir: dir.clone(),
            global_data_dir: dir.clone(),
            local_data_dir: dir,
            scope: "code".into(),
        },
        execution: ExecutionSettings::default(),
        compaction: CompactionConfig::default(),
        sandbox: SandboxConfig::default(),
        plugins: Default::default(),
        instruction_file: None,
        prompt_append: None,
        prompt_override: None,
        recorder: None,
        telemetry_url: None,
        session_dir: None,
        memory_enabled: false,
        disable_skill_shell_execution: true,
        path_style: Default::default(),
        permission_mode: PermissionMode::default(),
        permission_rules: Vec::new(),
        local_permission_rules: Vec::new(),
        allow_client_permission_override: false,
        telemetry_enabled: false,
        hooks_config: None,
        project_trust: Default::default(),
        project_layer: Default::default(),
        mcp_servers: Default::default(),
        providers: Default::default(),
        default_provider: None,
        task_models: Default::default(),
        language: None,
        scripts: Vec::new(),
    }
}

fn build(settings: Settings, depth: u32, with_agents: bool) -> Agent {
    let mut builder = Builder::new()
        .scene(Arc::new(scene::code::CodeScene::default()) as Arc<dyn Scene>)
        .model(Arc::new(DummyModel))
        .settings(Arc::new(settings))
        .tools({
            let t = Arc::new(InMemoryToolRegistry::new());
            tools::register_builtin_tools(&t);
            t
        })
        .agent_depth(depth)
        .skip_warmup(true);
    if with_agents {
        builder = builder.module(Arc::new(AgentsModule::new()));
    }
    builder.build().expect("build should succeed").0
}

#[tokio::test]
async fn the_agent_tool_is_resident_and_needs_the_module() {
    let agent = build(settings(), 0, true);
    let tool = agent
        .tools()
        .get("Agent")
        .expect("the module registers Agent");
    assert!(
        !tool.is_deferred(),
        "Agent is in CodeScene's resident set and must ship its full schema"
    );
    assert!(build(settings(), 0, false).tools().get("Agent").is_none());
}

/// The depth bound has to hold on the registry the delegate actually runs
/// with. A filter on what the parent hands down had no effect while `build()`
/// re-registered `Agent` regardless, and delegation chains recursed until the
/// process overflowed its stack.
#[tokio::test]
async fn the_agent_tool_is_withheld_at_the_delegation_depth_limit() {
    let max_depth = base::context::EngineConfig::defaults_for("test".to_string()).max_agent_depth;
    for depth in 0..max_depth {
        assert!(
            build(settings(), depth, true)
                .tools()
                .get("Agent")
                .is_some(),
            "depth {depth} is below the limit {max_depth} and should still be able to delegate"
        );
    }
    let at_limit = build(settings(), max_depth, true);
    assert!(
        at_limit.tools().get("Agent").is_none(),
        "an agent at the depth limit ({max_depth}) must not be handed the Agent tool"
    );
}

#[tokio::test]
async fn team_tools_are_registered_only_when_the_operator_turns_them_on() {
    let mut on = settings();
    on.execution.team_enabled = true;
    let agent = build(on, 0, true);
    for name in ["TeamCreate", "TeamList", "TeamDelete"] {
        assert!(
            agent.tools().get(name).is_some(),
            "{name} should be registered"
        );
    }

    let agent = build(settings(), 0, true);
    for name in ["TeamCreate", "TeamList", "TeamDelete"] {
        assert!(
            agent.tools().get(name).is_none(),
            "{name} must not be registered when team_enabled is false"
        );
    }
}

/// Nothing the module wires into a session may keep that session's `Agent`
/// tool alive once the session is gone.
#[tokio::test]
async fn the_agent_tool_goes_when_its_session_does() {
    let mut with_teams = settings();
    with_teams.execution.team_enabled = true;
    with_teams.hooks_config = Some(serde_json::json!({
        "Stop": [{ "type": "agent", "prompt": "check" }]
    }));
    let agent = build(with_teams, 0, true);
    let tool = Arc::downgrade(&agent.tools().get("Agent").expect("registered"));
    drop(agent);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while tool.upgrade().is_some() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the Agent tool outlived its session"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}
