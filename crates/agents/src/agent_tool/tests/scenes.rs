//! Tests: scenes.

use super::*;

/// Without wiring, a sub-agent's permission is allow-all.
#[test]
fn permission_handler_falls_back_to_always_permit_when_no_parent_is_wired() {
    let agent_tool = test_agent_tool();
    let perm = agent_tool.sub_permission();
    let outcome = futures::executor::block_on(perm.check(
        "Bash",
        &serde_json::json!({}),
        Path::new("/tmp"),
        "s",
    ));
    assert!(matches!(
        outcome,
        base::interface::permission::PermissionOutcome::Permit
    ));
}

/// The sub-agent inherits the parent's scene rather than always
/// being handed `CodeScene`, and an agent type's own `scene:` wins.
#[test]
fn subagent_scene_inherits_parent_and_agent_type_override_wins() {
    let agent_tool = test_agent_tool();
    let harnix: Arc<dyn Scene> = Arc::new(scene::harnix::HarnixScene);
    agent_tool.set_scene(harnix);
    let settings = Settings::defaults_for("test-model");

    assert_eq!(
        agent_tool.inner.scene_for_subagent(None, &settings).id(),
        "harnix",
        "a harnix-scene parent must not hand its sub-agent the code scene"
    );

    // An agent type may *narrow* the scene, never widen it.
    // `.agix/agents/*.md` is repository content; letting it name
    // `scene: code` (whose empty allow-list means "every registered
    // tool") would let a file in the repo hand a harnix session tools its
    // scene leaves out.
    let widening = AgentTypeDefinition {
        name: "coder".into(),
        description: "d".into(),
        scene: Some("code".into()),
        ..Default::default()
    };
    assert_eq!(
        agent_tool
            .inner
            .scene_for_subagent(Some(&widening), &settings)
            .id(),
        "harnix",
        "an agent type must not be able to widen the parent scene's tool surface"
    );

    // Narrowing is fine: mini's allow-list is a strict subset of harnix's.
    let narrowing = AgentTypeDefinition {
        name: "minimal".into(),
        description: "d".into(),
        scene: Some("mini".into()),
        ..Default::default()
    };
    assert_eq!(
        agent_tool
            .inner
            .scene_for_subagent(Some(&narrowing), &settings)
            .id(),
        "mini",
        "an agent type narrowing the parent scene must still be honored"
    );

    let bogus = AgentTypeDefinition {
        name: "bogus".into(),
        description: "d".into(),
        scene: Some("nonexistent".into()),
        ..Default::default()
    };
    assert_eq!(
        agent_tool
            .inner
            .scene_for_subagent(Some(&bogus), &settings)
            .id(),
        "harnix",
        "an unknown scene id is ignored, not fatal — inherited scene is kept"
    );
}

/// A scene id that exists only in the host's registry — which is what
/// every `plugin:<name>` scene is — must resolve. Before the registry was
/// wired in, `scene_for_subagent` matched against a hardcoded list of the
/// four built-ins, so such an id hit the "unrecognized scene" branch and
/// the sub-agent silently inherited its parent's scene instead.
#[test]
fn agent_type_can_name_a_scene_that_only_the_registry_knows() {
    let agent_tool = test_agent_tool();
    let settings = Settings::defaults_for("test-model");
    let named = AgentTypeDefinition {
        name: "specialist".into(),
        description: "d".into(),
        scene: Some("plugin:demo".into()),
        ..Default::default()
    };

    // Reusing the parent's scene under a plugin-style id keeps this test
    // about *resolution*, not about the narrowing rule above.
    let mini: Arc<dyn Scene> = Arc::new(scene::mini::MiniScene);
    agent_tool.set_scene(mini);

    assert_eq!(
        agent_tool
            .inner
            .scene_for_subagent(Some(&named), &settings)
            .id(),
        "mini",
        "without a registry, a non-builtin id is unresolvable and the parent scene is kept"
    );

    let mut registry = scene::SceneRegistry::new();
    registry.register_builtin();
    registry.register(Arc::new(AliasScene {
        id: "plugin:demo",
        inner: Arc::new(scene::mini::MiniScene),
    }));
    agent_tool.set_scene_registry(Arc::new(registry));

    assert_eq!(
        agent_tool
            .inner
            .scene_for_subagent(Some(&named), &settings)
            .id(),
        "plugin:demo",
        "a registry-only scene id must resolve once the registry is wired in"
    );
}

/// A scene that borrows another's behavior under a different id — stands
/// in for a plugin scene without depending on the plugin crate.
pub(super) struct AliasScene {
    pub(super) id: &'static str,
    pub(super) inner: Arc<dyn Scene>,
}

impl Scene for AliasScene {
    fn id(&self) -> &str {
        self.id
    }
    fn name(&self) -> &str {
        self.id
    }
    fn description(&self) -> &str {
        "alias"
    }
    fn build_system_prompt(
        &self,
        ctx: &base::interface::scene::ScenePromptContext,
    ) -> Vec<base::prompt::PromptBlock> {
        self.inner.build_system_prompt(ctx)
    }
    fn tools(&self) -> Vec<String> {
        self.inner.tools()
    }
    fn token_budget(&self) -> base::interface::scene::TokenBudget {
        self.inner.token_budget()
    }
    fn disallowed_tools(&self) -> Vec<String> {
        self.inner.disallowed_tools()
    }
}

/// With no explicit scene wired in, `Settings.paths.scope` (which the
/// daemon sets to `scene.id()`) is the fallback, and an unknown scope
/// degrades to `CodeScene`.
#[test]
fn subagent_scene_falls_back_to_settings_scope_then_code() {
    let agent_tool = test_agent_tool();
    let mut settings = Settings::defaults_for("test-model");

    settings.paths.scope = "mini".into();
    assert_eq!(
        agent_tool.inner.scene_for_subagent(None, &settings).id(),
        "mini"
    );

    settings.paths.scope = "nonexistent".into();
    assert_eq!(
        agent_tool.inner.scene_for_subagent(None, &settings).id(),
        "code"
    );
}

/// A sub-agent must be able to see AGENTS.md / CLAUDE.md, both when
/// the parent got it from settings.json and when it came from
/// `Builder::instruction_file(..)` (invisible in `Settings`).
#[test]
fn subagent_inherits_the_parent_instruction_file() {
    let tools = Arc::new(InMemoryToolRegistry::new());
    let model: Arc<dyn Model> = Arc::new(DummyModel);
    let config = Arc::new(EngineConfig::defaults_for("test-model"));

    let mut parent = Settings::defaults_for("test-model");
    parent.instruction_file = Some(std::path::PathBuf::from("/proj/AGENTS.md"));
    let agent_tool = AgentTool::with_parent_tools(
        model.clone(),
        config.clone(),
        tools.clone(),
        tools.clone(),
        &[],
        &[],
    )
    .with_settings(Arc::new(parent));
    assert_eq!(
        agent_tool
            .sub_settings(None, std::path::PathBuf::from("/tmp"))
            .instruction_file,
        Some(std::path::PathBuf::from("/proj/AGENTS.md")),
        "sub-agents must inherit the project's instruction file from settings"
    );

    // Builder-level instruction file: not present in `Settings` at all.
    let bare = AgentTool::with_parent_tools(model, config, tools.clone(), tools, &[], &[]);
    assert_eq!(
        bare.sub_settings(None, std::path::PathBuf::from("/tmp"))
            .instruction_file,
        None
    );
    bare.set_instruction_file(Some(std::path::PathBuf::from("/proj/CLAUDE.md")));
    assert_eq!(
        bare.sub_settings(None, std::path::PathBuf::from("/tmp"))
            .instruction_file,
        Some(std::path::PathBuf::from("/proj/CLAUDE.md"))
    );
}

#[test]
fn agent_type_frontmatter_parses_scene_override() {
    let dir = std::env::temp_dir().join(format!(
        "agix-agent-scene-front-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    write_agent_md(
        &dir,
        "digger.md",
        "---\nname: digger\ndescription: Source digger\nscene: harnix\n---\nBody.",
    );
    let types = load_agent_types_from_dir(&dir, AgentTypeSource::User, &Default::default());
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(types[0].scene.as_deref(), Some("harnix"));
}

/// The daemon relays the forwarded event by serializing it — a nested
/// `Box<AgentEvent>` inside an internally-tagged enum must round-trip.
#[test]
fn subagent_progress_event_round_trips_through_json() {
    let ev = AgentEvent::SubagentProgress {
        agent_label: "explore#abc12345".into(),
        agent_session_id: "sub-1".into(),
        agent_type: Some("explore".into()),
        parent_session_id: "parent-1".into(),
        parent_turn: 3,
        parent_tool_use_id: Some("toolu_0".into()),
        task_id: Some("ag-1".into()),
        event: Box::new(AgentEvent::ToolUse {
            id: "toolu_1".into(),
            name: "Read".into(),
            input: serde_json::json!({"file_path": "/a"}),
            turn_id: "t".into(),
        }),
    };
    let json = serde_json::to_value(&ev).expect("serialize");
    assert_eq!(json["kind"], "subagent_progress");
    assert_eq!(json["event"]["kind"], "tool_use");
    assert_eq!(json["event"]["name"], "Read");
    let back: AgentEvent = serde_json::from_value(json).expect("deserialize");
    match back {
        AgentEvent::SubagentProgress {
            agent_label, event, ..
        } => {
            assert_eq!(agent_label, "explore#abc12345");
            assert!(matches!(*event, AgentEvent::ToolUse { .. }));
        }
        other => panic!("wrong variant: {other:?}"),
    }
}

#[test]
fn subagent_label_is_stable_and_names_the_agent_type() {
    let sid = "3f2a1b7c-dead-beef-0000-111122223333";
    assert_eq!(subagent_label(Some("explore"), sid), "explore#3f2a1b7c");
    assert_eq!(subagent_label(None, sid), "agent#3f2a1b7c");
    assert_eq!(
        subagent_label(Some("explore"), sid),
        subagent_label(Some("explore"), sid),
        "the label must not change between events of the same run"
    );
}

#[test]
fn a_background_agents_answer_is_its_whole_output_by_offset() {
    let session = base::context::SessionState::new(std::env::temp_dir());
    let task = session.register_task(
        "ag-answer",
        base::context::TaskType::Agent,
        tokio_util::sync::CancellationToken::new(),
        serde_json::Value::Null,
    );
    let answer = "the answer, \u{4e2d}".to_string();
    super::conclude_background_task(&session, &task, &Ok(answer.clone()), false, false);
    let read = task.output_from(0).unwrap();
    assert_eq!(read.text, answer);
    assert_eq!(read.next_byte, answer.len() as u64);
    assert!(read.closed);
}
