//! Tests: catalog.

use super::*;

/// A directory of agent-type `.md` files that outlives the assertions
/// reading them.
///
/// An `AgentTool` built over a directory keeps a `notify` watcher on it,
/// and a `.md` file disappearing re-runs the merge with that agent type
/// gone. Deleting the directory partway through a test therefore races the
/// watcher against itself: the unlink of the file is dispatched, removing
/// the directory tears the watch down, and whichever lands first decides
/// whether the catalog under assertion still holds anything. A `TempDir`
/// is cleaned up when the test ends, once nothing is going to read it
/// again.
pub(super) fn agent_type_dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("temp dir for agent-type files")
}

pub(super) fn write_agent_md(dir: &Path, filename: &str, content: &str) {
    std::fs::create_dir_all(dir).unwrap();
    let mut f = std::fs::File::create(dir.join(filename)).unwrap();
    f.write_all(content.as_bytes()).unwrap();
}

#[test]
fn merge_agent_types_with_no_dirs_returns_only_builtins() {
    let merged = merge_agent_types(&[], &[], &Default::default());
    assert_eq!(merged.len(), builtin_agent_types().len());
    assert!(merged.contains_key("explore"));
    assert!(merged.contains_key("code-reviewer"));
    assert!(merged.contains_key("worker"));
}

#[test]
fn load_agent_types_from_dir_parses_frontmatter() {
    let dir = std::env::temp_dir().join(format!(
        "agix-agent-types-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    write_agent_md(
        &dir,
        "security-auditor.md",
        "---\nname: security-auditor\ndescription: Finds security issues\nallowed_tools: [Read, Grep, Glob]\nmodel: claude-opus-4-8\n---\nYou are a security auditor.",
    );
    let types = load_agent_types_from_dir(&dir, AgentTypeSource::User, &Default::default());
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(types.len(), 1);
    let t = &types[0];
    assert_eq!(t.name, "security-auditor");
    assert_eq!(t.description, "Finds security issues");
    assert_eq!(t.allowed_tools, vec!["Read", "Grep", "Glob"]);
    assert_eq!(t.model.as_deref(), Some("claude-opus-4-8"));
    assert_eq!(t.system_prompt, "You are a security auditor.");
}

#[test]
fn load_agent_types_from_dir_parses_new_fields() {
    let dir = std::env::temp_dir().join(format!(
        "agix-agent-types-test-newfields-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    write_agent_md(
        &dir,
        "db-reader.md",
        "---\nname: db-reader\ndescription: Read-only DB queries\ndisallowed_tools: AskUserQuestion\npermission_mode: plan\neffort: high\nmax_turns: 10\nskills: api-conventions, error-handling\nmcp_servers: github, sentry\n---\nBody.",
    );
    let types = load_agent_types_from_dir(&dir, AgentTypeSource::User, &Default::default());
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(types.len(), 1);
    let t = &types[0];
    assert_eq!(t.disallowed_tools, vec!["AskUserQuestion"]);
    assert_eq!(
        t.permission_mode,
        Some(base::interface::settings::PermissionMode::Plan)
    );
    assert_eq!(t.effort.as_deref(), Some("high"));
    assert_eq!(t.max_turns, Some(10));
    assert_eq!(t.skills, vec!["api-conventions", "error-handling"]);
    assert_eq!(t.mcp_servers, vec!["github", "sentry"]);
}

#[test]
fn load_agent_types_from_dir_falls_back_to_first_paragraph_description() {
    let dir = std::env::temp_dir().join(format!(
        "agix-agent-types-test-para-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    write_agent_md(
        &dir,
        "helper.md",
        "---\nname: helper\n---\nThis agent helps\nwith multi-line stuff.\n\nMore body.",
    );
    let types = load_agent_types_from_dir(&dir, AgentTypeSource::User, &Default::default());
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(types.len(), 1);
    assert_eq!(
        types[0].description,
        "This agent helps with multi-line stuff."
    );
}

#[test]
fn preload_skills_text_includes_full_body_and_skips_missing_and_disabled() {
    let dir = std::env::temp_dir().join(format!(
        "agix-skills-preload-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(dir.join("api-conventions")).unwrap();
    std::fs::write(
        dir.join("api-conventions").join("SKILL.md"),
        "---\ndescription: API conventions\n---\nUse RESTful naming.",
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("manual-only")).unwrap();
    std::fs::write(
        dir.join("manual-only").join("SKILL.md"),
        "---\ndescription: Manual only\ndisable_model_invocation: true\n---\nSecret steps.",
    )
    .unwrap();

    let mgr = skills::manager::SkillManager::new();
    mgr.load_dir_subdirs(&dir, skills::manager::SkillSource::Project)
        .unwrap();

    // `get_skill_content` reads a disk skill's body lazily at lookup
    // time (not cached at load time) — the fixture directory must still
    // exist when `preload_skills_text` runs, so cleanup happens after.
    let text = preload_skills_text(
        &mgr,
        &[
            "api-conventions".to_string(),
            "manual-only".to_string(),
            "does-not-exist".to_string(),
        ],
    );
    let _ = std::fs::remove_dir_all(&dir);
    assert!(text.contains("api-conventions"));
    assert!(text.contains("Use RESTful naming."));
    // disable_model_invocation and unresolved names are skipped, not
    // included and not fatal to the rest of the preload.
    assert!(!text.contains("Secret steps."));
}

#[test]
fn preload_skills_text_is_empty_when_nothing_resolves() {
    let mgr = skills::manager::SkillManager::new();
    let text = preload_skills_text(&mgr, &["nope".to_string()]);
    assert_eq!(text, "");
}

#[test]
fn set_skill_catalog_makes_it_available_via_inner() {
    let tools = Arc::new(InMemoryToolRegistry::new());
    let model: Arc<dyn Model> = Arc::new(DummyModel);
    let config = Arc::new(EngineConfig::defaults_for("test-model"));
    let agent_tool = AgentTool::with_parent_tools(model, config, tools.clone(), tools, &[], &[]);
    assert!(agent_tool.inner.skill_manager().is_none());
    agent_tool.set_skill_catalog(Arc::new(skills::manager::SkillManager::new()));
    assert!(agent_tool.inner.skill_manager().is_some());
}

#[test]
fn tool_name_matches_exact_and_mcp_server_pattern() {
    assert!(AgentTool::tool_name_matches("Read", "Read"));
    assert!(!AgentTool::tool_name_matches("Read", "Write"));
    assert!(AgentTool::tool_name_matches(
        "mcp__github__list_prs",
        "mcp__github"
    ));
    assert!(AgentTool::tool_name_matches(
        "mcp__github__list_prs",
        "mcp__github__*"
    ));
    assert!(!AgentTool::tool_name_matches(
        "mcp__gitlab__list_prs",
        "mcp__github"
    ));
}

#[test]
fn merge_agent_types_disk_definition_overrides_builtin_by_name() {
    let dir = std::env::temp_dir().join(format!(
        "agix-agent-types-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    write_agent_md(
        &dir,
        "explore.md",
        "---\ndescription: Custom explore override\n---\nCustom body.",
    );
    let merged = merge_agent_types(
        &[(dir.clone(), AgentTypeSource::User)],
        &[],
        &Default::default(),
    );
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(merged["explore"].description, "Custom explore override");
    assert_eq!(merged["explore"].system_prompt, "Custom body.");
    // Untouched built-ins survive.
    assert!(merged.contains_key("code-reviewer"));
}

#[test]
fn merge_agent_types_precedence_is_low_to_high_across_dirs() {
    let base = std::env::temp_dir().join(format!(
        "agix-agent-types-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let global_dir = base.join("global");
    let scene_dir = base.join("scene");
    let project_dir = base.join("project");
    write_agent_md(
        &global_dir,
        "reviewer.md",
        "---\ndescription: from global\n---\nbody",
    );
    write_agent_md(
        &scene_dir,
        "reviewer.md",
        "---\ndescription: from scene\n---\nbody",
    );
    write_agent_md(
        &project_dir,
        "reviewer.md",
        "---\ndescription: from project\n---\nbody",
    );

    let merged = merge_agent_types(
        &[
            (global_dir, AgentTypeSource::User),
            (scene_dir, AgentTypeSource::User),
            (project_dir, AgentTypeSource::Project { trusted: true }),
        ],
        &[],
        &Default::default(),
    );
    let _ = std::fs::remove_dir_all(&base);
    assert_eq!(merged["reviewer"].description, "from project");
}

#[test]
fn a_project_agent_type_cannot_loosen_until_trusted() {
    use base::interface::settings::PermissionMode as M;
    let tmp = tempfile::tempdir().unwrap();
    let (user_dir, project_dir) = (tmp.path().join("user"), tmp.path().join("project"));
    write_agent_md(
        &user_dir,
        "reviewer.md",
        "---\ndescription: the user's\n---\nbody",
    );
    for name in ["explore", "reviewer", "fresh"] {
        write_agent_md(
            &project_dir,
            &format!("{name}.md"),
            "---\ndescription: the project's\npermission_mode: bypassPermissions\nmax_turns: 500\n---\nbody",
        );
    }
    let merged_as = |trusted: bool| {
        merge_agent_types(
            &[
                (user_dir.clone(), AgentTypeSource::User),
                (project_dir.clone(), AgentTypeSource::Project { trusted }),
            ],
            &[],
            &Default::default(),
        )
    };
    let loosened = |def: &AgentTypeDefinition| {
        let mut settings = Settings::defaults_for("test-model");
        settings.permission_mode = M::Default;
        settings.execution.max_api_calls_per_turn = 25;
        apply_agent_type_overrides(&mut settings, def);
        (
            settings.permission_mode,
            settings.execution.max_api_calls_per_turn,
        )
    };

    let untrusted = merged_as(false);
    assert_eq!(untrusted["explore"].source, AgentTypeSource::Builtin);
    assert_eq!(untrusted["reviewer"].description, "the user's");
    assert_eq!(untrusted["fresh"].description, "the project's");
    assert_eq!(loosened(&untrusted["fresh"]), (M::Default, 25));

    let trusted = merged_as(true);
    assert_eq!(trusted["explore"].description, "the project's");
    assert_eq!(trusted["reviewer"].description, "the project's");
    assert_eq!(loosened(&trusted["fresh"]), (M::BypassPermissions, 500));
}

#[test]
fn merge_agent_types_plugin_definition_overrides_builtin_by_name() {
    let plugin_types = vec![AgentTypeDefinition {
        name: "explore".into(),
        description: "Plugin-provided explore override".into(),
        allowed_tools: vec!["Read".into()],
        model: None,
        system_prompt: "Plugin explore body.".into(),
        ..Default::default()
    }];
    let merged = merge_agent_types(&[], &plugin_types, &Default::default());
    assert_eq!(
        merged["explore"].description,
        "Plugin-provided explore override"
    );
    assert_eq!(merged["explore"].system_prompt, "Plugin explore body.");
    // Untouched built-ins survive.
    assert!(merged.contains_key("code-reviewer"));
}

#[test]
fn merge_agent_types_dirs_override_plugin_definition_of_same_name() {
    let dir = std::env::temp_dir().join(format!(
        "agix-agent-types-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    write_agent_md(
        &dir,
        "explore.md",
        "---\ndescription: from disk dir\n---\nDisk body.",
    );
    let plugin_types = vec![AgentTypeDefinition {
        name: "explore".into(),
        description: "from plugin".into(),
        allowed_tools: vec![],
        model: None,
        system_prompt: "Plugin body.".into(),
        ..Default::default()
    }];
    // Confirm priority order: built-in < plugin < dirs.
    let merged_plugin_only = merge_agent_types(&[], &plugin_types, &Default::default());
    assert_eq!(merged_plugin_only["explore"].description, "from plugin");

    let merged = merge_agent_types(
        &[(dir.clone(), AgentTypeSource::User)],
        &plugin_types,
        &Default::default(),
    );
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(merged["explore"].description, "from disk dir");
    assert_eq!(merged["explore"].system_prompt, "Disk body.");
}

#[test]
fn describe_agent_types_lists_every_type_sorted_by_name() {
    let merged = merge_agent_types(&[], &[], &Default::default());
    let text = describe_agent_types(&merged);
    assert!(text.contains("- code-reviewer: "));
    assert!(text.contains("- explore: "));
    // Sorted: "claude" sorts before "code-reviewer" before "explore".
    let claude_pos = text.find("- claude:").unwrap();
    let explore_pos = text.find("- explore:").unwrap();
    assert!(claude_pos < explore_pos);
}

/// `AgentTool` implements `Tool::prompt()` with the whole of
/// `agent_tool.prompt.md`. The real tool is classified as documented (so
/// `build_tool_defs` points at it) and what `ToolSearch` would hand back
/// is the actual file, not a paraphrase of its description.
#[tokio::test]
async fn the_real_agent_tools_guide_is_reachable() {
    use base::tool::Tool;
    let agent_tool = AgentTool::new(
        Arc::new(DummyModel),
        Arc::new(base::context::EngineConfig::defaults_for("test")),
        Arc::new(InMemoryToolRegistry::new()),
    );
    let guide = agent_tool
        .detailed_prompt(&base::tool::PromptContext::default())
        .await
        .expect("AgentTool ships agent_tool.prompt.md and must count as documented");
    assert_eq!(guide, include_str!("../agent_tool.prompt.md"));
    assert_ne!(
        guide.trim(),
        agent_tool.description().trim(),
        "the guide must be distinguishable from the short description — \
         that difference is exactly what the default derivation keys on"
    );
}

#[test]
fn the_guide_says_what_the_tool_does() {
    let guide = include_str!("../agent_tool.prompt.md");
    assert!(
        guide.contains("you receive a `<task-notification>`"),
        "a finished background agent notifies its parent"
    );
    for stale in [
        "no automatic push",
        "no push notification",
        "FleetView",
        "code-reviewer:",
        "\"bypassPermissions\"` to let it",
    ] {
        assert!(!guide.contains(stale), "{stale}");
    }
    assert!(guide.contains("listed in this tool's description"));
    assert!(guide.contains("max_turns"));

    let schema = serde_json::to_value(schemars::schema_for!(AgentInput)).unwrap();
    assert!(
        schema["properties"]
            .get("auto_background_after_secs")
            .is_none(),
        "{schema}"
    );
}

#[test]
fn a_sub_agent_cut_off_by_a_limit_says_so() {
    assert_eq!(with_stop_note("done".into(), "end_turn"), "done");
    assert!(with_stop_note(String::new(), "max_turns").contains("gave no answer"));
    let partial = with_stop_note("half".into(), "max_turns");
    assert!(partial.starts_with("half") && partial.contains("may be incomplete"));
}

pub(super) struct DummyModel;
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
        // Most tests using `DummyModel` only exercise settings/catalog
        // derivation and never actually call `.stream()`. The
        // background-spawn test below is the exception — it needs
        // `run_sub_inner`'s `agent.run_turn(...)` to return promptly
        // (through the real error path, not a hang) so it can observe
        // the task transition to `Failed` without a full mock model.
        Err(base::interface::model::ModelError::Internal(
            "DummyModel does not implement streaming".into(),
        ))
    }
}

pub(super) struct NamedTool(pub(super) &'static str);
#[async_trait]
impl base::tool::Tool for NamedTool {
    fn name(&self) -> &str {
        self.0
    }
    fn input_schema(&self) -> Value {
        serde_json::json!({})
    }
    fn is_read_only(&self, _: &Value) -> bool {
        true
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }
    async fn call(
        &self,
        _input: Value,
        _ctx: ToolContext,
        _progress: ProgressSender,
    ) -> Result<base::tool::ToolResult, base::error::ToolError> {
        Ok(base::tool::ToolResult {
            content: ToolResultContent::Text("ok".into()),
            is_error: false,
            structured_content: None,
            mcp_meta: None,
            new_messages: None,
        })
    }
}

#[test]
fn a_session_plugins_agent_types_sit_between_the_shared_catalog_and_the_disk() {
    let def = |name: &str, source: AgentTypeSource, prompt: &str| AgentTypeDefinition {
        name: name.into(),
        source,
        system_prompt: prompt.into(),
        ..Default::default()
    };
    let shared: std::collections::HashMap<String, AgentTypeDefinition> = [
        def("builtin", AgentTypeSource::Builtin, "shared"),
        def("on-disk", AgentTypeSource::User, "disk"),
    ]
    .into_iter()
    .map(|d| (d.name.clone(), d))
    .collect();
    let tools = Arc::new(InMemoryToolRegistry::new());
    let tool = AgentTool::with_shared_agent_types(
        Arc::new(DummyModel),
        Arc::new(EngineConfig::defaults_for("test-model")),
        tools.clone(),
        tools,
        Arc::new(std::sync::RwLock::new(Arc::new(shared))),
    )
    .with_session_agent_types(&[
        def("builtin", AgentTypeSource::Plugin, "plugin"),
        def("on-disk", AgentTypeSource::Plugin, "plugin"),
        def("new", AgentTypeSource::Plugin, "plugin"),
    ]);

    let prompt = |name: &str| tool.inner.agent_type_def(name).map(|d| d.system_prompt);
    assert_eq!(prompt("builtin").as_deref(), Some("plugin"));
    assert_eq!(prompt("on-disk").as_deref(), Some("disk"));
    assert_eq!(prompt("new").as_deref(), Some("plugin"));
    assert!(base::tool::Tool::description(&tool).contains("new"));
}
