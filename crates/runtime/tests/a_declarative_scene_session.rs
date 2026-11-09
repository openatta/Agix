//! A session built from a scene that exists only as files: the prompt the
//! model is sent carries `prompt.md`, and the tool surface is the one
//! `scene.json` describes — whitelist, disallowed list and deferred list
//! applied to the registry the session dispatches from.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use base::interface::model::{
    Model, ModelError, ModelEvent, ModelMessage, ModelStream, StreamParams, ToolDef,
};
use base::interface::prompt::PromptBlock;
use base::interface::prompt_registry::{InMemoryPromptRegistry, PromptRegistry};
use base::interface::scene::Scene;
use base::interface::settings::*;
use base::tool::InMemoryToolRegistry;
use runtime::agent::{Agent, Builder};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Captured {
    prompt: Vec<PromptBlock>,
    tools: Vec<ToolDef>,
}

struct CapturingModel(Arc<Mutex<Option<Captured>>>);

#[async_trait]
impl Model for CapturingModel {
    fn api_type(&self) -> base::provider::ApiType {
        base::provider::ApiType::Anthropic
    }
    async fn stream(
        &self,
        prompt_blocks: Vec<PromptBlock>,
        tools: Vec<ToolDef>,
        _messages: Vec<ModelMessage>,
        _params: StreamParams,
        _cancel: CancellationToken,
    ) -> Result<ModelStream, ModelError> {
        self.0.lock().unwrap().get_or_insert(Captured {
            prompt: prompt_blocks,
            tools,
        });
        let events: Vec<Result<ModelEvent, ModelError>> = vec![
            Ok(ModelEvent::TextDelta { text: "ok".into() }),
            Ok(ModelEvent::EndTurn {
                stop_reason: "end_turn".into(),
                usage: Default::default(),
            }),
        ];
        Ok(Box::new(futures::stream::iter(events)))
    }
}

fn settings(dir: &Path) -> Settings {
    Settings {
        model: ModelSettings {
            api_type: base::provider::ApiType::Anthropic,
            base_url: String::new(),
            auth_token: String::new(),
            model_name: "test-model".into(),
            max_tokens: 2000,
            thinking_mode: ThinkingMode::Auto,
            effort: None,
            fallback_model: None,
        },
        paths: PathSettings {
            user_data_dir: dir.to_path_buf(),
            global_data_dir: dir.to_path_buf(),
            local_data_dir: dir.to_path_buf(),
            scope: "review".into(),
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
        ignored_values: Vec::new(),
        mcp_servers: Default::default(),
        providers: Default::default(),
        default_provider: None,
        task_models: Default::default(),
        language: None,
        scripts: Vec::new(),
    }
}

fn write_scene(root: &Path, id: &str, scene_json: &str, prompt_md: &str) {
    let dir = root.join(id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("scene.json"), scene_json).unwrap();
    std::fs::write(dir.join("prompt.md"), prompt_md).unwrap();
}

fn registry_with(root: &Path) -> scene::SceneRegistry {
    let mut registry = scene::SceneRegistry::new();
    registry.register_builtin();
    let refused = registry.register_declarative_dir(root);
    assert!(refused.is_empty(), "{refused:?}");
    registry
}

fn build(scene: Arc<dyn Scene>, settings: Settings, model: Arc<dyn Model>) -> Agent {
    build_with(Builder::new(), scene, settings, model)
}

fn build_with(
    builder: Builder,
    scene: Arc<dyn Scene>,
    settings: Settings,
    model: Arc<dyn Model>,
) -> Agent {
    builder
        .scene(scene)
        .model(model)
        .settings(Arc::new(settings))
        .tools({
            let t = Arc::new(InMemoryToolRegistry::new());
            tools::register_builtin_tools(&t);
            t
        })
        .skip_warmup(true)
        .build()
        .expect("build should succeed")
        .0
}

const REVIEW_SCENE: &str = r#"{
    "id": "review",
    "name": "Review",
    "description": "Reads code and reports; never edits",
    "tools": ["Read", "Glob", "Grep", "ToolSearch", "Web*"],
    "disallowed_tools": ["WebSearch"],
    "deferred_tools": ["Grep"]
}"#;

const REVIEW_PROMPT: &str = "You are a code reviewer. You read; you never edit.\n\n\
                             You are running as {{model}} for {{team}}.\n";

#[tokio::test(flavor = "multi_thread")]
async fn the_session_runs_on_the_files_prompt_and_tool_surface() {
    let root = tempfile::tempdir().unwrap();
    write_scene(root.path(), "review", REVIEW_SCENE, REVIEW_PROMPT);
    let scene = registry_with(root.path())
        .resolve("review")
        .expect("the declarative scene is registered");

    let variables = InMemoryPromptRegistry::new();
    let _team = variables.register_variable("team", Arc::new(|_| Some("the platform team".into())));

    let captured = Arc::new(Mutex::new(None));
    let mut agent = build_with(
        Builder::new().prompt_registry(variables.clone()),
        scene,
        settings(root.path()),
        Arc::new(CapturingModel(captured.clone())),
    );

    let tools = agent.tools();
    for kept in ["Read", "Glob", "Grep", "ToolSearch", "WebFetch"] {
        assert!(tools.get(kept).is_some(), "{kept} is on the whitelist");
    }
    for dropped in ["Bash", "Write", "Edit", "WebSearch"] {
        assert!(
            tools.get(dropped).is_none(),
            "{dropped} is off the whitelist or disallowed"
        );
    }
    assert!(tools.get("Grep").unwrap().is_deferred());
    assert!(!tools.get("Read").unwrap().is_deferred());

    agent
        .run_turn("look at this".into(), "t1".into(), CancellationToken::new())
        .await
        .expect("the turn completes");

    let captured = captured
        .lock()
        .unwrap()
        .take()
        .expect("the model was called");
    let prompt: Vec<&str> = captured.prompt.iter().map(|b| b.content.as_str()).collect();
    let fixed = captured
        .prompt
        .iter()
        .find(|b| b.content == "You are a code reviewer. You read; you never edit.")
        .unwrap_or_else(|| panic!("prompt.md's fixed part is a block of its own: {prompt:?}"));
    assert!(fixed.cache_strategy.is_some());
    assert!(
        prompt
            .iter()
            .any(|b| b.contains("You are running as test-model for the platform team.")),
        "the scene fills `model` per call and the registered variable fills `team`: {prompt:?}"
    );
    assert!(!prompt.iter().any(|b| b.contains("{{")), "{prompt:?}");

    let advertised: Vec<&str> = captured.tools.iter().map(|t| t.name.as_str()).collect();
    assert!(advertised.contains(&"Read"), "{advertised:?}");
    assert!(!advertised.contains(&"Bash"), "{advertised:?}");
    assert!(!advertised.contains(&"WebSearch"), "{advertised:?}");
}

#[tokio::test]
async fn a_disallowed_list_alone_leaves_everything_else_in() {
    let root = tempfile::tempdir().unwrap();
    write_scene(
        root.path(),
        "careful",
        r#"{"id": "careful", "name": "Careful", "disallowed_tools": ["Bash", "Write"]}"#,
        "Be careful.",
    );
    let scene = registry_with(root.path()).resolve("careful").unwrap();
    let agent = build(
        scene,
        settings(root.path()),
        Arc::new(CapturingModel(Arc::default())),
    );
    let tools = agent.tools();
    assert!(tools.get("Bash").is_none());
    assert!(tools.get("Write").is_none());
    for kept in ["Read", "Edit", "Glob", "Grep"] {
        assert!(tools.get(kept).is_some(), "{kept} is not disallowed");
    }
}
