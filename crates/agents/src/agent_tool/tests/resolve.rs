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
        .all()
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

    let agent_tool = test_agent_tool().with_task_router(router);
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

    let agent_tool = test_agent_tool().with_task_router(router);
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
        .all()
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
        .all()
        .iter()
        .map(|t| t.name().to_string())
        .collect();
    assert!(names.iter().any(|n| n == "Read")); // explore allows Read
    assert!(!names.iter().any(|n| n == "Bash")); // explore doesn't allow Bash
}

#[test]
fn delegate_tools_mcp_servers_grants_matching_server_tools_only() {
    let tmp = agent_type_dir();
    let dir = tmp.path();
    write_agent_md(
        dir,
        "github-agent.md",
        "---\ndescription: Uses only the github MCP server\nmcp_servers: github\n---\nbody",
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

    // Two servers' worth of MCP tools attached to the parent — only
    // "github"'s should make it into the resolved registry.
    let adapters: Vec<Arc<dyn base::tool::Tool>> = vec![
        Arc::new(NamedTool("mcp__github__list_prs")),
        Arc::new(NamedTool("mcp__gitlab__list_mrs")),
    ];
    agent_tool.set_mcp_tool_adapters(adapters);

    let resolved = agent_tool.delegate_tools(Some("github-agent"), &[], &[]);
    let names: Vec<String> = resolved
        .all()
        .iter()
        .map(|t| t.name().to_string())
        .collect();
    assert!(names.iter().any(|n| n == "mcp__github__list_prs"));
    assert!(!names.iter().any(|n| n == "mcp__gitlab__list_mrs"));
}

#[test]
fn delegate_tools_without_mcp_servers_field_grants_no_mcp_tools() {
    // Confirms MCP access is opt-in, not inherited by default — even
    // the "full access" (empty allowed_tools) convention doesn't pull
    // in MCP tools unless mcp_servers explicitly names a server.
    let agent_tool = test_agent_tool();
    agent_tool.set_mcp_tool_adapters(vec![Arc::new(NamedTool("mcp__github__list_prs"))]);

    let resolved = agent_tool.delegate_tools(Some("general-purpose"), &[], &[]);
    let names: Vec<String> = resolved
        .all()
        .iter()
        .map(|t| t.name().to_string())
        .collect();
    assert!(!names.iter().any(|n| n.starts_with("mcp__")));
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
        .all()
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
        .all()
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
        permission_mode: Some(base::interface::settings::PermissionMode::Plan),
        effort: Some("max".into()),
        max_turns: Some(5),
        ..Default::default()
    };
    let mut settings = Settings::defaults_for("test-model");
    let original_max_calls = settings.execution.max_api_calls_per_turn;
    apply_agent_type_overrides(&mut settings, &def);
    assert_eq!(
        settings.permission_mode,
        base::interface::settings::PermissionMode::Plan
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
    use base::interface::settings::PermissionMode as M;
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
    use base::interface::settings::PermissionMode as M;
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
    use base::interface::settings::PermissionMode as M;
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
    use base::interface::settings::PermissionMode as M;
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
        base::interface::settings::PermissionMode::Plan
    );
    assert_eq!(settings.execution.max_api_calls_per_turn, 3);
}
