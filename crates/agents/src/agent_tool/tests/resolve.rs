//! Tests: resolve.

use super::*;

pub(super) fn test_agent_tool() -> AgentTool {
    let tools = InMemoryToolRegistry::new();
    tools.register(Arc::new(NamedTool("Read")));
    tools.register(Arc::new(NamedTool("Bash")));
    let tools = Arc::new(tools);
    let model: Arc<dyn Model> = Arc::new(DummyModel);
    let config = Arc::new(EngineConfig::defaults_for("test-model"));
    AgentTool::with_parent_tools(model, config, tools.clone(), tools, &[], &[])
}

fn names_of(registry: &InMemoryToolRegistry) -> Vec<String> {
    registry
        .list()
        .iter()
        .map(|t| t.name().to_string())
        .collect()
}

/// A parent built from `Read`/`Bash`/`Write`, whose session then registered
/// `Skill`, `TaskStop` and `Agent` of its own and dropped `Write`.
fn agent_tool_over_a_built_session() -> AgentTool {
    let template = InMemoryToolRegistry::new();
    for name in ["Read", "Bash", "Write"] {
        template.register(Arc::new(NamedTool(name)));
    }
    let session = InMemoryToolRegistry::new();
    for name in ["Read", "Bash", "Skill", "TaskStop", AGENT_TOOL_NAME] {
        session.register(Arc::new(NamedTool(name)));
    }
    AgentTool::with_parent_tools(
        Arc::new(DummyModel),
        Arc::new(EngineConfig::defaults_for("test-model")),
        Arc::new(session),
        Arc::new(template),
        &[],
        &[],
    )
}

#[test]
fn a_delegate_inherits_only_template_tools_its_parent_still_has() {
    let names = names_of(&agent_tool_over_a_built_session().delegate_tools(None, &[], &[]));
    assert_eq!(names, ["Read", "Bash"]);
}

#[test]
fn a_callers_lists_narrow_the_agent_types_tools() {
    let agent_tool = agent_tool_over_a_built_session();
    let explore = agent_tool.delegate_tools(Some("explore"), &[], &[]);
    assert_eq!(names_of(&explore), ["Read"]);
    let none_left = agent_tool.delegate_tools(Some("explore"), &[], &["Read".into()]);
    assert!(names_of(&none_left).is_empty());
    let only_bash = agent_tool.delegate_tools(None, &["Bash".into()], &[]);
    assert_eq!(names_of(&only_bash), ["Bash"]);
}

#[test]
fn model_for_subagent_uses_parent_model_when_no_router_configured() {
    let agent_tool = test_agent_tool();
    let picked = agent_tool.inner.model_for_subagent();
    assert!(Arc::ptr_eq(&picked, &agent_tool.inner.model));
}

#[test]
fn model_for_subagent_routes_through_task_router_when_configured() {
    let routed_model: Arc<dyn Model> = Arc::new(DummyModel);
    let mut providers = std::collections::HashMap::new();
    providers.insert("other".to_string(), routed_model.clone());
    let mut resolved = std::collections::HashMap::new();
    resolved.insert(
        "subagent".to_string(),
        base::provider::ResolvedModel {
            provider_id: "other".into(),
            model: "x".into(),
        },
    );
    let default_model: Arc<dyn Model> = Arc::new(DummyModel);
    let router = Arc::new(base::provider::TaskRouter::new(
        providers,
        resolved,
        default_model,
    ));

    let agent_tool = test_agent_tool().with_host(runtime::agent::HostContracts {
        task_router: Some(router),
        ..host_contracts()
    });
    let picked = agent_tool.inner.model_for_subagent();
    assert!(Arc::ptr_eq(&picked, &routed_model));
    assert!(!Arc::ptr_eq(&picked, &agent_tool.inner.model));
}

#[test]
fn model_for_subagent_falls_back_to_default_when_no_subagent_override() {
    // No "subagent" entry in `resolved` — router's own `default`
    // (which may differ from `inner.model`) should win, proving the
    // router is actually consulted rather than short-circuited.
    let router_default: Arc<dyn Model> = Arc::new(DummyModel);
    let router = Arc::new(base::provider::TaskRouter::new(
        std::collections::HashMap::new(),
        std::collections::HashMap::new(),
        router_default.clone(),
    ));

    let agent_tool = test_agent_tool().with_host(runtime::agent::HostContracts {
        task_router: Some(router),
        ..host_contracts()
    });
    let picked = agent_tool.inner.model_for_subagent();
    assert!(Arc::ptr_eq(&picked, &router_default));
    assert!(!Arc::ptr_eq(&picked, &agent_tool.inner.model));
}

#[test]
fn delegate_tools_full_access_excludes_agent_itself() {
    let agent_tool = test_agent_tool();
    // Register "Agent" into the same registry the tool was built with,
    // simulating `Builder::build()`'s self-registration.
    let self_arc = Arc::new(AgentTool::with_parent_tools(
        agent_tool.inner.model.clone(),
        agent_tool.inner.config.clone(),
        agent_tool.inner.parent_tools.clone(),
        agent_tool.inner.fallback_tools.clone(),
        &[],
        &[],
    ));
    agent_tool.inner.parent_tools.register(self_arc);

    // "general-purpose" is a full-access type (empty allowed_tools).
    let resolved = agent_tool.delegate_tools(Some("general-purpose"), &[], &[]);
    let names: Vec<String> = resolved
        .list()
        .iter()
        .map(|t| t.name().to_string())
        .collect();
    assert!(names.iter().any(|n| n == "Read"));
    assert!(names.iter().any(|n| n == "Bash"));
    assert!(
        !names.iter().any(|n| n == AGENT_TOOL_NAME),
        "sub-agent must not be able to spawn further sub-agents"
    );
}

#[test]
fn delegate_tools_restricted_type_filters_to_allowed_list() {
    let agent_tool = test_agent_tool();
    let resolved = agent_tool.delegate_tools(Some("explore"), &[], &[]);
    let names: Vec<String> = resolved
        .list()
        .iter()
        .map(|t| t.name().to_string())
        .collect();
    assert!(names.iter().any(|n| n == "Read")); // explore allows Read
    assert!(!names.iter().any(|n| n == "Bash")); // explore doesn't allow Bash
}

type GrantsAsked = Arc<std::sync::Mutex<Vec<Option<Vec<String>>>>>;

/// Servers whose `granted` records what it was asked for and keeps the tools
/// named after those servers.
struct RecordingServers {
    tools: Vec<&'static str>,
    asked: GrantsAsked,
}

#[async_trait]
impl base::interface::mcp_servers::McpServers for RecordingServers {
    fn tools(&self) -> Vec<Arc<dyn base::tool::Tool>> {
        self.tools
            .iter()
            .map(|n| Arc::new(NamedTool(n)) as Arc<dyn base::tool::Tool>)
            .collect()
    }
    async fn refresh_tools(&mut self) {}
    async fn refresh_if_changed(&mut self) -> bool {
        false
    }
    fn instructions(&self) -> Vec<base::context::McpServerInstruction> {
        Vec::new()
    }
    async fn resolve_resource_refs(&self, _: &str) -> Option<String> {
        None
    }
    fn prompt_commands(&self) -> Vec<base::interface::mcp_servers::McpPromptCommand> {
        Vec::new()
    }
    fn has_prompt_command(&self, _: &str) -> bool {
        false
    }
    async fn invoke_prompt_command(
        &self,
        _: &str,
        _: &str,
    ) -> base::interface::mcp_servers::McpPromptInvocation {
        unreachable!()
    }
    fn granted(
        &self,
        servers: Option<&[String]>,
    ) -> Box<dyn base::interface::mcp_servers::McpServers> {
        self.asked
            .lock()
            .unwrap()
            .push(servers.map(<[String]>::to_vec));
        let keep = |tool: &&str| {
            servers.is_none_or(|s| s.iter().any(|n| tool.starts_with(&format!("mcp__{n}__"))))
        };
        Box::new(RecordingServers {
            tools: self.tools.iter().copied().filter(keep).collect(),
            asked: self.asked.clone(),
        })
    }
}

fn agent_tool_with_servers(types: &str) -> (AgentTool, GrantsAsked, tempfile::TempDir) {
    let tmp = agent_type_dir();
    write_agent_md(tmp.path(), "github-agent.md", types);
    let tools = Arc::new(InMemoryToolRegistry::new());
    let agent_tool = AgentTool::with_parent_tools(
        Arc::new(DummyModel),
        Arc::new(EngineConfig::defaults_for("parent-default-model")),
        tools.clone(),
        tools,
        &[(tmp.path().to_path_buf(), AgentTypeSource::User)],
        &[],
    );
    let asked = Arc::new(std::sync::Mutex::new(Vec::new()));
    agent_tool.set_mcp_servers(Arc::new(RecordingServers {
        tools: vec!["mcp__github__list_prs", "mcp__gitlab__list_mrs"],
        asked: asked.clone(),
    }));
    (agent_tool, asked, tmp)
}

const GITHUB_AGENT: &str =
    "---\ndescription: Uses only the github MCP server\nmcp_servers: github\n---\nbody";

#[test]
fn a_types_mcp_servers_are_granted_as_servers_not_as_tools() {
    let (agent_tool, asked, _tmp) = agent_tool_with_servers(GITHUB_AGENT);
    let def = agent_tool.inner.agent_type_def("github-agent");
    let granted = agent_tool
        .inner
        .mcp_granted_to(def.as_ref(), &SubRun::default())
        .expect("a grant");
    let names: Vec<String> = granted
        .tools()
        .iter()
        .map(|t| t.name().to_string())
        .collect();
    assert_eq!(names, ["mcp__github__list_prs"]);
    assert_eq!(*asked.lock().unwrap(), [Some(vec!["github".to_string()])]);
    let tools = agent_tool.delegate_tools(Some("github-agent"), &[], &[]);
    assert!(!names_of(&tools).iter().any(|n| n.starts_with("mcp__")));
}

#[test]
fn a_type_without_mcp_servers_is_granted_none() {
    let (agent_tool, asked, _tmp) = agent_tool_with_servers(GITHUB_AGENT);
    let def = agent_tool.inner.agent_type_def("general-purpose");
    assert!(agent_tool
        .inner
        .mcp_granted_to(def.as_ref(), &SubRun::default())
        .is_none());
    assert!(agent_tool
        .inner
        .mcp_granted_to(None, &SubRun::default())
        .is_none());
    assert!(asked.lock().unwrap().is_empty());
}

#[test]
fn a_hosts_tool_lists_hold_the_granted_servers_tools() {
    let (agent_tool, _, _tmp) = agent_tool_with_servers(GITHUB_AGENT);
    let def = agent_tool.inner.agent_type_def("github-agent");
    let run = SubRun {
        disallowed_tools: vec!["mcp__github__list_prs".into()],
        ..SubRun::default()
    };
    let granted = agent_tool.inner.mcp_granted_to(def.as_ref(), &run).unwrap();
    assert!(granted.tools().is_empty());
}

#[test]
fn delegate_tools_disallowed_tools_removes_from_full_access_pool() {
    let tmp = agent_type_dir();
    let dir = tmp.path();
    write_agent_md(
        dir,
        "no-writes.md",
        "---\ndescription: Inherits every tool except file writes\ndisallowed_tools: Write, Edit\n---\nbody",
    );
    let tools = InMemoryToolRegistry::new();
    tools.register(Arc::new(NamedTool("Read")));
    tools.register(Arc::new(NamedTool("Write")));
    tools.register(Arc::new(NamedTool("Edit")));
    let tools = Arc::new(tools);
    let model: Arc<dyn Model> = Arc::new(DummyModel);
    let config = Arc::new(EngineConfig::defaults_for("parent-default-model"));
    let agent_tool = AgentTool::with_parent_tools(
        model,
        config,
        tools.clone(),
        tools,
        &[(dir.to_path_buf(), AgentTypeSource::User)],
        &[],
    );

    let resolved = agent_tool.delegate_tools(Some("no-writes"), &[], &[]);
    let names: Vec<String> = resolved
        .list()
        .iter()
        .map(|t| t.name().to_string())
        .collect();
    assert!(!names.iter().any(|n| n == "Write"));
    assert!(!names.iter().any(|n| n == "Edit"));
    // Full-access convention (empty allowed_tools) still applies to
    // everything else — disallowed_tools only removes, doesn't restrict
    // to an allowlist.
    assert!(names.iter().any(|n| n == "Read"));
}

#[test]
fn delegate_tools_disallowed_tools_applied_before_allowed_tools() {
    let tmp = agent_type_dir();
    let dir = tmp.path();
    // A tool listed in both allowed_tools and disallowed_tools must end
    // up removed either way.
    write_agent_md(
        dir,
        "conflicting.md",
        "---\ndescription: Lists Bash in both allow and deny\nallowed_tools: Read, Bash\ndisallowed_tools: Bash\n---\nbody",
    );
    let tools = InMemoryToolRegistry::new();
    tools.register(Arc::new(NamedTool("Read")));
    tools.register(Arc::new(NamedTool("Bash")));
    let tools = Arc::new(tools);
    let model: Arc<dyn Model> = Arc::new(DummyModel);
    let config = Arc::new(EngineConfig::defaults_for("parent-default-model"));
    let agent_tool = AgentTool::with_parent_tools(
        model,
        config,
        tools.clone(),
        tools,
        &[(dir.to_path_buf(), AgentTypeSource::User)],
        &[],
    );

    let resolved = agent_tool.delegate_tools(Some("conflicting"), &[], &[]);
    let names: Vec<String> = resolved
        .list()
        .iter()
        .map(|t| t.name().to_string())
        .collect();
    assert!(names.iter().any(|n| n == "Read"));
    assert!(!names.iter().any(|n| n == "Bash"));
}

#[test]
fn custom_agent_type_model_override_is_applied_to_sub_settings() {
    let tmp = agent_type_dir();
    let dir = tmp.path();
    write_agent_md(
        dir,
        "opus-reviewer.md",
        "---\ndescription: Reviewer pinned to a specific model\nmodel: claude-opus-4-8\n---\nbody",
    );
    let tools = Arc::new(InMemoryToolRegistry::new());
    let model: Arc<dyn Model> = Arc::new(DummyModel);
    let config = Arc::new(EngineConfig::defaults_for("parent-default-model"));
    let agent_tool = AgentTool::with_parent_tools(
        model,
        config,
        tools.clone(),
        tools,
        &[(dir.to_path_buf(), AgentTypeSource::User)],
        &[],
    );

    // Same lookup `run_sub`/`run_sub_inner` perform: resolve the type's
    // `model` override, then thread it into `sub_settings()`.
    let model_override = agent_tool
        .inner
        .agent_type_def("opus-reviewer")
        .and_then(|d| d.model);
    assert_eq!(model_override.as_deref(), Some("claude-opus-4-8"));

    let settings = agent_tool.sub_settings(
        model_override.as_deref(),
        std::path::PathBuf::from("/tmp/cwd"),
    );
    assert_eq!(settings.model.model_name, "claude-opus-4-8");

    // A call with no override (or an unknown type) still falls back to
    // the parent's configured model — this must not regress.
    let settings_default = agent_tool.sub_settings(None, std::path::PathBuf::from("/tmp/cwd"));
    assert_eq!(settings_default.model.model_name, "parent-default-model");
}

#[test]
fn effort_names_parse_to_their_levels() {
    assert_eq!(parse_effort("low"), Some(Effort::Low));
    assert_eq!(parse_effort("medium"), Some(Effort::Medium));
    assert_eq!(parse_effort("high"), Some(Effort::High));
    assert_eq!(parse_effort("xhigh"), Some(Effort::Xhigh));
    assert_eq!(parse_effort("max"), Some(Effort::Max));
    assert_eq!(parse_effort("not-a-level"), None);
}

#[test]
fn apply_agent_type_overrides_sets_permission_mode_effort_and_max_turns() {
    let def = AgentTypeDefinition {
        name: "db-reader".into(),
        description: "d".into(),
        permission_mode: Some(base::settings::PermissionMode::Plan),
        effort: Some("max".into()),
        max_turns: Some(5),
        ..Default::default()
    };
    let mut settings = Settings::defaults_for("test-model");
    let original_max_calls = settings.execution.max_api_calls_per_turn;
    apply_agent_type_overrides(&mut settings, &def);
    assert_eq!(
        settings.permission_mode,
        base::settings::PermissionMode::Plan
    );
    assert_eq!(settings.model.effort, Some(Effort::Max));
    assert_eq!(settings.execution.max_api_calls_per_turn, 5);
    assert_ne!(
        settings.execution.max_api_calls_per_turn,
        original_max_calls
    );
}

/// A plugin ships over the network. Letting its agent type name
/// `bypassPermissions` would switch the permission gate off for
/// everything that sub-agent does, and a huge `max_turns` would spend the
/// user's tokens at a rate they never agreed to.
#[test]
fn plugin_agent_type_cannot_loosen_permissions_or_raise_the_call_cap() {
    use base::settings::PermissionMode as M;
    let def = AgentTypeDefinition {
        name: "greedy".into(),
        source: AgentTypeSource::Plugin,
        description: "d".into(),
        permission_mode: Some(M::BypassPermissions),
        max_turns: Some(10_000),
        ..Default::default()
    };
    let mut settings = Settings::defaults_for("test-model");
    settings.permission_mode = M::Default;
    settings.execution.max_api_calls_per_turn = 25;

    apply_agent_type_overrides(&mut settings, &def);

    assert_eq!(
        settings.permission_mode,
        M::Default,
        "a plugin must not be able to switch the permission gate off"
    );
    assert_eq!(
        settings.execution.max_api_calls_per_turn, 25,
        "a plugin must not be able to raise the API-call cap"
    );
}

/// The clamp is one-directional: a plugin restraining its own sub-agent
/// more than the session does is exactly what a well-behaved plugin
/// should be able to declare.
#[test]
fn plugin_agent_type_may_still_tighten() {
    use base::settings::PermissionMode as M;
    let def = AgentTypeDefinition {
        name: "careful".into(),
        source: AgentTypeSource::Plugin,
        description: "d".into(),
        permission_mode: Some(M::Plan),
        max_turns: Some(5),
        ..Default::default()
    };
    let mut settings = Settings::defaults_for("test-model");
    settings.permission_mode = M::AcceptEdits;
    settings.execution.max_api_calls_per_turn = 25;

    apply_agent_type_overrides(&mut settings, &def);

    assert_eq!(settings.permission_mode, M::Plan);
    assert_eq!(settings.execution.max_api_calls_per_turn, 5);
}

#[test]
fn a_users_agent_type_may_raise_its_cap_and_loosen_its_mode() {
    use base::settings::PermissionMode as M;
    let def = AgentTypeDefinition {
        name: "mine".into(),
        source: AgentTypeSource::User,
        description: "d".into(),
        permission_mode: Some(M::BypassPermissions),
        max_turns: Some(200),
        ..Default::default()
    };
    let mut settings = Settings::defaults_for("test-model");
    settings.permission_mode = M::Default;
    settings.execution.max_api_calls_per_turn = 25;

    apply_agent_type_overrides(&mut settings, &def);

    assert_eq!(settings.permission_mode, M::BypassPermissions);
    assert_eq!(settings.execution.max_api_calls_per_turn, 200);
}

#[test]
fn a_planning_parent_holds_a_type_to_plan_and_a_bypassing_one_takes_it_along() {
    use base::settings::PermissionMode as M;
    let typed = |mode| AgentTypeDefinition {
        name: "mine".into(),
        source: AgentTypeSource::User,
        description: "d".into(),
        permission_mode: Some(mode),
        ..Default::default()
    };
    for (parent, asked, applied) in [
        (M::Plan, M::AcceptEdits, M::Plan),
        (M::Plan, M::DontAsk, M::DontAsk),
        (M::BypassPermissions, M::Plan, M::BypassPermissions),
        (M::AcceptEdits, M::Default, M::Default),
    ] {
        let mut settings = Settings::defaults_for("test-model");
        settings.permission_mode = parent;
        apply_agent_type_overrides(&mut settings, &typed(asked));
        assert_eq!(settings.permission_mode, applied, "{parent:?} + {asked:?}");
    }
    let mut settings = Settings::defaults_for("test-model");
    settings.permission_mode = M::Default;
    let plugin = AgentTypeDefinition {
        source: AgentTypeSource::Plugin,
        ..typed(M::BypassPermissions)
    };
    apply_agent_type_overrides(&mut settings, &plugin);
    assert_eq!(settings.permission_mode, M::Default);
}

#[test]
fn apply_agent_type_overrides_leaves_settings_untouched_when_fields_absent() {
    let def = AgentTypeDefinition {
        name: "plain".into(),
        description: "d".into(),
        ..Default::default()
    };
    let mut settings = Settings::defaults_for("test-model");
    let before = (
        settings.permission_mode,
        settings.model.thinking_mode.clone(),
        settings.model.effort,
        settings.execution.max_api_calls_per_turn,
    );
    apply_agent_type_overrides(&mut settings, &def);
    assert_eq!(
        (
            settings.permission_mode,
            settings.model.thinking_mode,
            settings.model.effort,
            settings.execution.max_api_calls_per_turn
        ),
        before
    );
}

#[test]
fn run_sub_applies_permission_mode_and_max_turns_override_end_to_end() {
    let tmp = agent_type_dir();
    let dir = tmp.path();
    write_agent_md(
        dir,
        "cautious.md",
        "---\ndescription: A cautious read-only researcher\npermission_mode: plan\nmax_turns: 3\n---\nbody",
    );
    let tools = Arc::new(InMemoryToolRegistry::new());
    let model: Arc<dyn Model> = Arc::new(DummyModel);
    let config = Arc::new(EngineConfig::defaults_for("parent-default-model"));
    let agent_tool = AgentTool::with_parent_tools(
        model,
        config,
        tools.clone(),
        tools,
        &[(dir.to_path_buf(), AgentTypeSource::User)],
        &[],
    );

    let def = agent_tool.inner.agent_type_def("cautious").unwrap();
    let mut settings = agent_tool.sub_settings(None, std::path::PathBuf::from("/tmp/cwd"));
    apply_agent_type_overrides(&mut settings, &def);
    assert_eq!(
        settings.permission_mode,
        base::settings::PermissionMode::Plan
    );
    assert_eq!(settings.execution.max_api_calls_per_turn, 3);
}
