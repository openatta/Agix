//! What a session gets from being assembled with [`AgentsModule`]: the
//! `Agent` tool up to the depth limit, the team tools when the operator turns
//! them on, and both subject to the scene's tool policy like any built-in.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use agents::AgentsModule;
use async_trait::async_trait;
use base::interface::model::{Model, ModelMessage};
use base::interface::scene::Scene;
use base::settings::*;
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
        _prompt_blocks: Vec<base::prompt::PromptBlock>,
        _tools: Vec<base::interface::model::ToolDef>,
        _messages: Vec<ModelMessage>,
        _params: base::interface::model::StreamParams,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError> {
        Err(base::interface::model::ModelError::protocol(
            "DummyModel does not stream",
        ))
    }
}

#[allow(deprecated)]
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
            workspace: base::settings::Workspace::project_of(&dir),
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
        ignored_values: Vec::new(),
        managed: None,
        mcp_servers: Default::default(),
        mcp_server_dirs: Default::default(),
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
    assert!(
        agent.tools().get("Agent").is_some(),
        "the module registers Agent"
    );
    assert!(
        !agent.deferred_tools().contains("Agent"),
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

/// What one turn call carried: the mark that routed it, the system prompt
/// and conversation as text, and the tools it offered.
#[derive(Clone)]
struct Seen {
    mark: Option<String>,
    text: String,
    tools: Vec<String>,
}

/// Answers each turn call from the first queue whose mark the call carries;
/// "done" when none has a reply left.
#[derive(Default)]
struct RoutedModel {
    queues: std::sync::Mutex<Vec<(String, std::collections::VecDeque<ModelEventSet>)>>,
    seen: std::sync::Mutex<Vec<Seen>>,
}

type ModelEventSet = Vec<base::interface::model::ModelEvent>;

fn says(text: &str) -> ModelEventSet {
    vec![
        base::interface::model::ModelEvent::TextDelta { text: text.into() },
        base::interface::model::ModelEvent::EndTurn {
            stop_reason: "end_turn".into(),
            usage: Default::default(),
        },
    ]
}

fn calls(tool: &str, input: serde_json::Value) -> ModelEventSet {
    vec![
        base::interface::model::ModelEvent::ToolUse {
            id: format!("toolu_{tool}"),
            name: tool.into(),
            input,
        },
        base::interface::model::ModelEvent::EndTurn {
            stop_reason: "tool_use".into(),
            usage: Default::default(),
        },
    ]
}

impl RoutedModel {
    fn route(self, mark: &str, replies: impl IntoIterator<Item = ModelEventSet>) -> Self {
        self.queues
            .lock()
            .unwrap()
            .push((mark.into(), replies.into_iter().collect()));
        self
    }

    fn seen_with(&self, mark: &str) -> Vec<Seen> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.mark.as_deref() == Some(mark))
            .cloned()
            .collect()
    }
}

#[async_trait]
impl Model for RoutedModel {
    fn api_type(&self) -> base::provider::ApiType {
        base::provider::ApiType::Anthropic
    }
    async fn stream(
        &self,
        prompt_blocks: Vec<base::prompt::PromptBlock>,
        tools: Vec<base::interface::model::ToolDef>,
        messages: Vec<ModelMessage>,
        params: base::interface::model::StreamParams,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError> {
        let turn_call = params
            .origin
            .as_ref()
            .is_some_and(|o| matches!(o.kind, base::interface::model::CallKind::Turn { .. }));
        if !turn_call {
            return Ok(Box::new(futures::stream::iter(
                says("[]").into_iter().map(Ok),
            )));
        }
        let text = format!(
            "{}\n{}",
            prompt_blocks
                .iter()
                .map(|b| b.content.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
            serde_json::to_string(&messages).unwrap_or_default()
        );
        let routed = self
            .queues
            .lock()
            .unwrap()
            .iter_mut()
            .find(|(mark, replies)| !replies.is_empty() && text.contains(mark.as_str()))
            .map(|(mark, replies)| (mark.clone(), replies.pop_front().unwrap_or_default()));
        let (mark, reply) = match routed {
            Some((mark, reply)) => (Some(mark), reply),
            None => (None, says("done")),
        };
        self.seen.lock().unwrap().push(Seen {
            mark,
            text,
            tools: tools.into_iter().map(|t| t.name).collect(),
        });
        Ok(Box::new(futures::stream::iter(reply.into_iter().map(Ok))))
    }
}

/// A root session that delegates once, `delegation` being the `Agent` call's
/// input, and whose delegate makes one `Glob` call; `adjust` sets up the
/// root's builder.
async fn delegate_once(
    delegation: serde_json::Value,
    adjust: impl FnOnce(Builder) -> Builder,
) -> Arc<RoutedModel> {
    let model = Arc::new(
        RoutedModel::default()
            .route("ROOT-MARK", [calls("Agent", delegation), says("root done")])
            .route(
                "CHILD-MARK",
                [
                    calls("Glob", serde_json::json!({ "pattern": "*.nothing" })),
                    says("child done"),
                ],
            ),
    );
    let mut s = settings();
    s.memory_enabled = false;
    let builder = Builder::new()
        .scene(Arc::new(scene::code::CodeScene::default()) as Arc<dyn Scene>)
        .model(model.clone())
        .settings(Arc::new(s))
        .tools({
            let t = Arc::new(InMemoryToolRegistry::new());
            tools::register_builtin_tools(&t);
            t
        })
        .skip_warmup(true)
        .module(Arc::new(AgentsModule::new()));
    let (mut agent, _events, _input) = adjust(builder).build().expect("the root builds");
    agent
        .run_turn(
            "ROOT-MARK delegate it".into(),
            "t1".into(),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect("the turn runs");
    model
}

fn plain_delegation() -> serde_json::Value {
    serde_json::json!({ "prompt": "CHILD-MARK look around", "description": "look" })
}

/// The date the host fixed for a session is the date its delegates are told.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "D2: a delegate does not inherit the host's environment (R1)"]
async fn a_delegate_is_told_the_date_its_host_fixed() {
    let start = time::OffsetDateTime::from_unix_timestamp(978_307_200).expect("2001-01-01");
    let model = delegate_once(plain_delegation(), |b| {
        b.environment(Arc::new(
            base::interface::environment::FixedEnvironment::new(start, time::Duration::seconds(1)),
        ))
    })
    .await;
    let root = model.seen_with("ROOT-MARK");
    assert!(
        root[0].text.contains("2001-01-01"),
        "the root itself is not told the fixed date, so this case proves nothing"
    );
    let child = model.seen_with("CHILD-MARK");
    assert!(!child.is_empty(), "the delegate never ran");
    assert!(
        child[0].text.contains("2001-01-01"),
        "the delegate was told another date"
    );
}

struct CountsCalls(Arc<std::sync::Mutex<Vec<String>>>);

#[async_trait]
impl base::interface::tool_middleware::ToolMiddleware for CountsCalls {
    async fn around(
        &self,
        call: &base::interface::tool_middleware::ToolCall,
        exec: &mut base::interface::tool_middleware::ToolExec,
        next: base::interface::tool_middleware::NextDispatch<'_>,
    ) -> base::interface::tool_middleware::ToolOutcome {
        self.0.lock().unwrap().push(call.name.clone());
        next.run(call, exec).await
    }
}

/// A host's tool middleware wraps its delegates' tool calls too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "D2: a delegate does not inherit the host's tool middleware (R1)"]
async fn a_hosts_tool_middleware_wraps_its_delegates_calls() {
    let wrapped = Arc::new(std::sync::Mutex::new(Vec::new()));
    let model = delegate_once(plain_delegation(), |b| {
        b.tool_middleware(Arc::new(CountsCalls(wrapped.clone())))
    })
    .await;
    assert!(
        !model.seen_with("CHILD-MARK").is_empty(),
        "the delegate never ran"
    );
    let wrapped = wrapped.lock().unwrap().clone();
    assert!(
        wrapped.iter().any(|n| n == "Agent"),
        "the root's own call is not wrapped, so this case proves nothing: {wrapped:?}"
    );
    assert!(
        wrapped.iter().any(|n| n == "Glob"),
        "the delegate's call went around the host's middleware: {wrapped:?}"
    );
}

/// Scheduled jobs belong to the session its host keeps: a delegate is not
/// given the tools to schedule its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "D2 §7.7: a delegate registers the Cron tools and keeps its own store"]
async fn a_delegate_is_offered_no_cron_tools() {
    let model = delegate_once(plain_delegation(), |b| b).await;
    let root = model.seen_with("ROOT-MARK");
    assert!(
        root[0].tools.iter().any(|t| t == "CronCreate"),
        "the root has no CronCreate, so this case proves nothing: {:?}",
        root[0].tools
    );
    let child = model.seen_with("CHILD-MARK");
    let cron: Vec<&String> = child[0]
        .tools
        .iter()
        .filter(|t| t.starts_with("Cron"))
        .collect();
    assert!(cron.is_empty(), "the delegate was offered {cron:?}");
}

/// A type's tool list holds whatever the delegate's own engine and modules
/// register, not only what it inherits: a read-only type gets none of them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "D2: a type's tool list does not reach engine and module tools (T1)"]
async fn an_explore_delegate_is_offered_only_its_types_tools() {
    let model = delegate_once(
        serde_json::json!({
            "prompt": "CHILD-MARK look around", "description": "look",
            "subagent_type": "explore"
        }),
        |b| b,
    )
    .await;
    let child = model.seen_with("CHILD-MARK");
    assert!(!child.is_empty(), "the delegate never ran");
    let beyond: Vec<&String> = child[0]
        .tools
        .iter()
        .filter(|t| ["EnterWorktree", "CronCreate", "Skill", "Agent"].contains(&t.as_str()))
        .collect();
    assert!(beyond.is_empty(), "a read-only type was offered {beyond:?}");
}
