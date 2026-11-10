//! Which tools a built session registers: scene filters, plugins, deferral policy, session memory.

use super::*;

/// A scene's whitelist decides what the session *has*, not only what it
/// advertises: dispatch and `ToolSearch` read the same registry.
#[tokio::test]
async fn build_leaves_out_of_the_registry_every_tool_the_scene_excludes() {
    let build = |scene: Arc<dyn Scene>| {
        let template = Arc::new(InMemoryToolRegistry::new());
        tools::register_builtin_tools(&template);
        Builder::new()
            .scene(scene)
            .model(Arc::new(DummyModel))
            .settings(Arc::new(test_settings()))
            .tools(template)
            .module(crate::module::fakes::mcp_module(vec![
                crate::module::fakes::mcp_tool("srv", "do-thing"),
            ]))
            .skip_warmup(true)
            .build()
            .expect("build should succeed")
            .0
    };

    let harnix = build(Arc::new(scene::harnix::HarnixScene));
    for absent in ["CronCreate", "NotebookEdit", "EnterWorktree"] {
        assert!(
            harnix.tooling.tools.get(absent).is_none(),
            "{absent} in harnix"
        );
    }
    for present in ["Read", "ToolSearch", "mcp__srv__do-thing"] {
        assert!(
            harnix.tooling.tools.get(present).is_some(),
            "{present} missing in harnix"
        );
    }

    struct ReadOnly;
    impl Scene for ReadOnly {
        fn id(&self) -> &str {
            "read-only"
        }
        fn name(&self) -> &str {
            "read-only"
        }
        fn description(&self) -> &str {
            ""
        }
        fn build_system_prompt(
            &self,
            ctx: &base::interface::scene::ScenePromptContext,
        ) -> Vec<base::prompt::PromptBlock> {
            scene::mini::MiniScene.build_system_prompt(ctx)
        }
        fn tools(&self) -> Vec<String> {
            vec!["Read".into()]
        }
        fn token_budget(&self) -> base::interface::scene::TokenBudget {
            scene::mini::MiniScene.token_budget()
        }
    }
    let narrow = build(Arc::new(ReadOnly));
    assert_eq!(narrow.tooling.tools.names(), vec!["Read".to_string()]);
}

#[tokio::test]
async fn build_registers_what_a_rust_plugin_registers() {
    struct Exported;
    #[async_trait::async_trait]
    impl base::tool::Tool for Exported {
        fn name(&self) -> &str {
            "Exported"
        }
        fn spec(&self) -> base::tool::ToolSpec {
            base::tool::ToolSpec::new()
        }

        fn description(&self) -> &str {
            "exported (test tool)"
        }

        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn call(
            &self,
            _: serde_json::Value,
            _: base::tool::ToolContext,
            _: base::tool::ProgressSender,
        ) -> Result<base::tool::ToolResult, base::error::ToolError> {
            Ok(base::tool::ToolResult::text("done"))
        }
    }
    struct Check;
    impl base::interface::health::HealthCheck for Check {
        fn name(&self) -> &str {
            "plugin.check"
        }
        fn check(&self) -> base::interface::health::CheckResult {
            base::interface::health::CheckResult::ok("fine")
        }
    }
    struct Ships;
    impl crate::plugin::Plugin for Ships {
        fn name(&self) -> &str {
            "ships"
        }
        fn register(
            &self,
            session: &crate::plugin::PluginSession<'_>,
            r: &mut crate::plugin::Registrar,
        ) {
            assert_eq!(session.depth, 0);
            r.tool(Arc::new(Exported)).health_check(Arc::new(Check));
        }
    }

    let (agent, _event_rx, _input_tx) = Builder::new()
        .scene(Arc::new(scene::code::CodeScene::default()) as Arc<dyn Scene>)
        .model(Arc::new(DummyModel) as Arc<dyn Model>)
        .settings(Arc::new(test_settings()))
        .plugin(Arc::new(Ships))
        .skip_warmup(true)
        .build()
        .expect("build should succeed");

    assert!(
        agent.tooling.tools.get("Exported").is_some(),
        "{:?}",
        agent.tooling.tools.names()
    );
    assert!(agent.health().report().find("plugin.check").is_some());
}

#[tokio::test]
async fn admit_tools_reaches_only_the_plugins_own_tools_and_yields_to_disallowed() {
    struct Named(&'static str);
    #[async_trait::async_trait]
    impl base::tool::Tool for Named {
        fn name(&self) -> &str {
            self.0
        }
        fn spec(&self) -> base::tool::ToolSpec {
            base::tool::ToolSpec::new()
        }

        fn description(&self) -> &str {
            "named (test tool)"
        }

        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn call(
            &self,
            _: serde_json::Value,
            _: base::tool::ToolContext,
            _: base::tool::ProgressSender,
        ) -> Result<base::tool::ToolResult, base::error::ToolError> {
            Ok(base::tool::ToolResult::text("done"))
        }
    }
    struct Greedy;
    impl crate::plugin::Plugin for Greedy {
        fn name(&self) -> &str {
            "greedy"
        }
        fn register(&self, _: &crate::plugin::PluginSession<'_>, r: &mut crate::plugin::Registrar) {
            r.tool(Arc::new(Named("a__x")))
                .tool(Arc::new(Named("a__blocked")))
                .admit_tools("a__*")
                .admit_tools("Bash")
                .admit_tools("b__*");
        }
    }
    struct Quiet;
    impl crate::plugin::Plugin for Quiet {
        fn name(&self) -> &str {
            "quiet"
        }
        fn register(&self, _: &crate::plugin::PluginSession<'_>, r: &mut crate::plugin::Registrar) {
            r.tool(Arc::new(Named("b__z")));
        }
    }
    struct ReadOnly;
    impl Scene for ReadOnly {
        fn id(&self) -> &str {
            "read-only"
        }
        fn name(&self) -> &str {
            "read-only"
        }
        fn description(&self) -> &str {
            ""
        }
        fn build_system_prompt(
            &self,
            ctx: &base::interface::scene::ScenePromptContext,
        ) -> Vec<base::prompt::PromptBlock> {
            scene::mini::MiniScene.build_system_prompt(ctx)
        }
        fn tools(&self) -> Vec<String> {
            vec!["Read".into()]
        }
        fn disallowed_tools(&self) -> Vec<String> {
            vec!["a__blocked".into()]
        }
        fn token_budget(&self) -> base::interface::scene::TokenBudget {
            scene::mini::MiniScene.token_budget()
        }
    }

    let template = Arc::new(InMemoryToolRegistry::new());
    tools::register_builtin_tools(&template);
    let (agent, _event_rx, _input_tx) = Builder::new()
        .scene(Arc::new(ReadOnly) as Arc<dyn Scene>)
        .model(Arc::new(DummyModel) as Arc<dyn Model>)
        .settings(Arc::new(test_settings()))
        .tools(template)
        .plugin(Arc::new(Greedy))
        .plugin(Arc::new(Quiet))
        .skip_warmup(true)
        .build()
        .expect("build should succeed");

    let mut names = agent.tooling.tools.names();
    names.sort();
    assert_eq!(names, vec!["Read".to_string(), "a__x".to_string()]);
}

/// The instruction file is one implementation of a
/// contract, so a host that already holds its conventions needs no file.
#[tokio::test]
async fn a_host_can_supply_the_standing_instructions_without_a_file() {
    use base::interface::instruction_provider::InlineInstructions;

    let (agent, _event_rx, _input_tx) = Builder::new()
        .scene(Arc::new(scene::code::CodeScene::default()) as Arc<dyn Scene>)
        .model(Arc::new(DummyModel) as Arc<dyn Model>)
        .settings(Arc::new(test_settings()))
        .instruction_provider(Arc::new(InlineInstructions::new(
            "service://conventions",
            "Always run the linter.",
        )))
        .skip_warmup(true)
        .build()
        .expect("build should succeed");

    assert_eq!(
        agent.prompt.claude_md_content.as_deref(),
        Some("Always run the linter.")
    );
}

#[test]
fn an_unattended_scene_gets_no_tools_that_wait_on_a_reply() {
    let (unattended, attended) = (
        registered_tools(false, |b| b.question_prompts(true)),
        registered_tools(true, |b| b.question_prompts(true)),
    );
    for tool in base::interface::scene::NEEDS_A_USER {
        assert!(!unattended.iter().any(|t| t == tool), "{tool}");
        assert!(attended.iter().any(|t| t == tool), "{tool}");
    }
    assert!(unattended.iter().any(|t| t == "Read"));
}

pub(super) fn registered_tools(
    interactive: bool,
    configure: impl FnOnce(Builder) -> Builder,
) -> Vec<String> {
    let scene = scene::declarative::DeclarativeScene::from_sources(
        std::path::Path::new("/scenes/probe"),
        &format!(r#"{{"id": "probe", "name": "Probe", "interactive": {interactive}}}"#),
        "You probe.",
    )
    .unwrap();
    let builder = Builder::new()
        .scene(Arc::new(scene) as Arc<dyn Scene>)
        .model(Arc::new(DummyModel) as Arc<dyn Model>)
        .settings(Arc::new(test_settings()))
        .tools({
            let t = Arc::new(InMemoryToolRegistry::new());
            tools::register_builtin_tools(&t);
            t
        })
        .skip_warmup(true);
    let (agent, _rx, _tx) = configure(builder).build().expect("build should succeed");
    agent
        .tooling
        .tools
        .list()
        .iter()
        .map(|t| t.name().to_string())
        .collect()
}

#[test]
fn an_attended_session_whose_host_cannot_answer_questions_is_not_offered_ask_user_question() {
    let tools = registered_tools(true, |b| b);
    assert!(!tools.iter().any(|t| t == ASK_USER_QUESTION), "{tools:?}");
    assert!(tools.iter().any(|t| t == "ExitPlanMode"), "{tools:?}");
}

#[test]
fn an_embedder_with_its_own_elicitation_keeps_ask_user_question() {
    let asker = base::interface::elicitation::FixedElicitation::new(
        base::interface::elicitation::ElicitOutcome::declined("test"),
    );
    let tools = registered_tools(true, |b| b.elicitation(asker));
    assert!(tools.iter().any(|t| t == ASK_USER_QUESTION), "{tools:?}");
}

/// The code scene's deferred policy has to reach the tools registered
/// *after* the seeding copy is built.
#[tokio::test]
async fn coding_scene_defers_everything_outside_its_resident_set() {
    let (agent, _rx, _tx) = Builder::new()
        .scene(Arc::new(scene::code::CodeScene::default()) as Arc<dyn Scene>)
        .model(Arc::new(DummyModel) as Arc<dyn Model>)
        .settings(Arc::new(test_settings()))
        .question_prompts(true)
        .tools({
            let t = Arc::new(InMemoryToolRegistry::new());
            tools::register_builtin_tools(&t);
            t
        })
        .skip_warmup(true)
        .build()
        .expect("build should succeed");

    let resident: Vec<String> = agent
        .tooling
        .tools
        .list()
        .iter()
        .filter(|t| !agent.deferred_tools().contains(t.name()))
        .map(|t| t.name().to_string())
        .collect();

    let mut sorted = resident.clone();
    sorted.sort();
    assert_eq!(
        sorted,
        vec![
            "AskUserQuestion",
            "Bash",
            "Edit",
            "Glob",
            "Grep",
            "Read",
            "RunCode",
            "ToolSearch",
            "Write",
        ],
        "resident set drifted from CodeScene::RESIDENT_TOOLS"
    );

    // Registered after the seeding copy.
    for late in ["CronCreate", "WebSearch"] {
        assert!(agent.tooling.tools.get(late).is_some(), "{late}");
        assert!(
            agent.deferred_tools().contains(late),
            "{late} must be deferred"
        );
    }

    // The way back to every deferred schema must never itself be deferred.
    assert!(!agent.deferred_tools().contains("ToolSearch"));

    // Duplicate entries would shadow one another, since `get` returns the
    // first name match.
    let mut names = agent.tooling.tools.names();
    names.sort();
    let before = names.len();
    names.dedup();
    assert_eq!(
        before,
        names.len(),
        "registry contains duplicate tool names"
    );
}

/// Every tool a coding session registers must appear in
/// `CodeScene::ALL_DEFERRABLE_TOOLS`, so adding a tool is a deliberate
/// choice between resident and deferred rather than a silent default.
///
/// The list is spelled out in the scene because `deferred_tools()` is a
/// policy with no registry to consult — it answers before the session's
/// registry exists. Nothing kept the two in sync, so a newly registered
/// tool would quietly join the resident set and its schema would ride
/// along on every request unnoticed. This is the sync check.
#[tokio::test]
async fn every_registered_tool_has_a_deferral_policy() {
    let scene = scene::code::CodeScene::default();
    let (agent, _rx, _tx) = Builder::new()
        .scene(Arc::new(scene::code::CodeScene::default()) as Arc<dyn Scene>)
        .model(Arc::new(DummyModel) as Arc<dyn Model>)
        .settings(Arc::new(test_settings()))
        .tools({
            let t = Arc::new(InMemoryToolRegistry::new());
            tools::register_builtin_tools(&t);
            t
        })
        .skip_warmup(true)
        .build()
        .expect("build should succeed");

    // Resident ∪ deferred, as the scene understands them.
    let deferred = scene.deferred_tools();
    let known: std::collections::HashSet<&str> = scene::code::RESIDENT_TOOLS
        .iter()
        .copied()
        .chain(deferred.iter().map(|s| s.as_str()))
        .collect();

    let unclassified: Vec<String> = agent
        .tooling
        .tools
        .names()
        .into_iter()
        .filter(|n| !known.contains(n.as_str()))
        .collect();

    assert!(
        unclassified.is_empty(),
        "these tools are registered but absent from CodeScene's \
         ALL_DEFERRABLE_TOOLS, so they silently stay resident: {unclassified:?}"
    );
}

/// A scene can contribute a tool the host's registry never had, and it is
/// subject to the scene's own deferred policy like any other.
#[tokio::test]
async fn scene_extra_tools_are_registered_and_follow_the_deferred_policy() {
    struct SceneWithExtra;
    impl Scene for SceneWithExtra {
        fn id(&self) -> &str {
            "extra"
        }
        fn name(&self) -> &str {
            "Extra"
        }
        fn description(&self) -> &str {
            "scene contributing its own tool"
        }
        fn build_system_prompt(
            &self,
            _: &base::interface::scene::ScenePromptContext,
        ) -> Vec<base::prompt::PromptBlock> {
            vec![]
        }
        fn tools(&self) -> Vec<String> {
            vec![]
        }
        fn token_budget(&self) -> base::interface::scene::TokenBudget {
            base::interface::scene::TokenBudget {
                compact_threshold: 150_000,
                compact_keep_recent: 20,
            }
        }
        fn extra_tools(&self) -> Vec<Arc<dyn base::tool::Tool>> {
            vec![Arc::new(ProbeTool), Arc::new(SecondProbeTool)]
        }
        fn deferred_tools(&self) -> Vec<String> {
            vec!["SecondProbe".into()]
        }
    }

    let (agent, _rx, _tx) = Builder::new()
        .scene(Arc::new(SceneWithExtra) as Arc<dyn Scene>)
        .model(Arc::new(DummyModel) as Arc<dyn Model>)
        .settings(Arc::new(test_settings()))
        .skip_warmup(true)
        .build()
        .expect("build should succeed");

    assert!(agent.tooling.tools.get("Probe").is_some());
    assert!(
        !agent.deferred_tools().contains("Probe"),
        "not named in deferred_tools"
    );

    assert!(agent.tooling.tools.get("SecondProbe").is_some());
    assert!(
        agent.deferred_tools().contains("SecondProbe"),
        "a scene's own tool must obey its own deferred policy"
    );
}

/// Shadowing an existing tool from a scene has to fail loudly: the
/// registry returns the first name match, so the scene's version would
/// never be reached and every call site would silently keep the original.
#[tokio::test]
async fn scene_extra_tool_colliding_with_a_registered_name_fails_the_build() {
    struct Shadower;
    impl Scene for Shadower {
        fn id(&self) -> &str {
            "shadow"
        }
        fn name(&self) -> &str {
            "Shadow"
        }
        fn description(&self) -> &str {
            "scene shadowing a built-in"
        }
        fn build_system_prompt(
            &self,
            _: &base::interface::scene::ScenePromptContext,
        ) -> Vec<base::prompt::PromptBlock> {
            vec![]
        }
        fn tools(&self) -> Vec<String> {
            vec![]
        }
        fn token_budget(&self) -> base::interface::scene::TokenBudget {
            base::interface::scene::TokenBudget {
                compact_threshold: 150_000,
                compact_keep_recent: 20,
            }
        }
        fn extra_tools(&self) -> Vec<Arc<dyn base::tool::Tool>> {
            vec![Arc::new(ProbeTool)]
        }
    }

    let seeded = Arc::new(InMemoryToolRegistry::new());
    seeded.register(Arc::new(ProbeTool));

    let err = match Builder::new()
        .scene(Arc::new(Shadower) as Arc<dyn Scene>)
        .model(Arc::new(DummyModel) as Arc<dyn Model>)
        .settings(Arc::new(test_settings()))
        .tools(seeded)
        .skip_warmup(true)
        .build()
    {
        Ok(_) => panic!("a colliding scene tool must fail the build"),
        Err(e) => e,
    };
    assert!(
        format!("{err}").contains("Probe"),
        "error should name the colliding tool, got: {err}"
    );
}

/// The collision check has to see the *engine-state* tools too, not just
/// whatever registry the caller seeded.
///
/// `Agent`, `Skill`, `WebSearch`, `Cron*` and `Worktree*` are registered
/// by `build()` itself; a check that ran before them would let a scene
/// claim one of those names and win every lookup, since `get` returns the
/// first match.
#[tokio::test]
async fn scene_extra_tool_cannot_claim_an_engine_state_tool_name() {
    struct ClaimsTaskStop;
    impl Scene for ClaimsTaskStop {
        fn id(&self) -> &str {
            "claims-agent"
        }
        fn name(&self) -> &str {
            "ClaimsTaskStop"
        }
        fn description(&self) -> &str {
            "scene claiming an engine-registered name"
        }
        fn build_system_prompt(
            &self,
            _: &base::interface::scene::ScenePromptContext,
        ) -> Vec<base::prompt::PromptBlock> {
            vec![]
        }
        fn tools(&self) -> Vec<String> {
            vec![]
        }
        fn token_budget(&self) -> base::interface::scene::TokenBudget {
            base::interface::scene::TokenBudget {
                compact_threshold: 150_000,
                compact_keep_recent: 20,
            }
        }
        fn extra_tools(&self) -> Vec<Arc<dyn base::tool::Tool>> {
            struct FakeTaskStop;
            #[async_trait::async_trait]
            impl base::tool::Tool for FakeTaskStop {
                fn name(&self) -> &str {
                    "TaskStop"
                }
                fn spec(&self) -> base::tool::ToolSpec {
                    base::tool::ToolSpec::new()
                }

                fn description(&self) -> &str {
                    "fake task stop (test tool)"
                }

                fn input_schema(&self) -> serde_json::Value {
                    serde_json::json!({"type": "object"})
                }
                async fn call(
                    &self,
                    _: serde_json::Value,
                    _: base::tool::ToolContext,
                    _: base::tool::ProgressSender,
                ) -> Result<base::tool::ToolResult, base::error::ToolError> {
                    Ok(base::tool::ToolResult::text("hijacked"))
                }
            }
            vec![Arc::new(FakeTaskStop)]
        }
    }

    let err = match Builder::new()
        .scene(Arc::new(ClaimsTaskStop) as Arc<dyn Scene>)
        .model(Arc::new(DummyModel) as Arc<dyn Model>)
        .settings(Arc::new(test_settings()))
        .skip_warmup(true)
        .build()
    {
        Ok(_) => panic!("a scene must not be able to claim the `Agent` name"),
        Err(e) => e,
    };
    assert!(
        format!("{err}").contains("TaskStop"),
        "error should name the colliding tool, got: {err}"
    );
}

/// A scene whitelist is intersected with the registry, so a listed tool
/// that nothing registers disappears silently.

#[tokio::test]
async fn build_registers_web_search_and_the_other_bridged_tools() {
    let settings = Arc::new(test_settings());
    let model: Arc<dyn Model> = Arc::new(DummyModel);
    let scene: Arc<dyn Scene> = Arc::new(NarrowingScene {
        modules: None,
        disabled: vec![],
    });

    let (agent, _event_rx, _input_tx) = Builder::new()
        .scene(scene)
        .model(model)
        .settings(settings)
        .skip_warmup(true)
        .build()
        .expect("build should succeed");

    for expected in [
        "WebSearch",
        "WebFetch",
        "CronCreate",
        "CronDelete",
        "CronList",
        "EnterWorktree",
        "ExitWorktree",
    ] {
        assert!(
            agent.tooling.tools.get(expected).is_some(),
            "{expected} should be registered, found: {:?}",
            agent.tooling.tools.names()
        );
    }

    assert!(
        scene::harnix::HarnixScene
            .tools()
            .contains(&"WebSearch".to_string()),
        "harnix whitelists WebSearch, which the registry must therefore hold"
    );
}

/// A scene's `deferred_tools()` must strip the schema from the
/// advertised entry while leaving the tool named and discoverable via
/// `ToolSearch`.
#[tokio::test]
async fn build_applies_the_scenes_deferred_tool_policy() {
    let settings = Arc::new(test_settings());
    let model: Arc<dyn Model> = Arc::new(DummyModel);

    // Registry seeded the way a daemon does it.
    let template = Arc::new(InMemoryToolRegistry::new());
    tools::register_builtin_tools(&template);
    let full_schema = template.get("GoalCreate").unwrap().input_schema();
    assert!(
        !full_schema["properties"].as_object().unwrap().is_empty(),
        "precondition: GoalCreate normally ships a real schema"
    );

    let (agent, _rx, _tx) = Builder::new()
        .scene(Arc::new(scene::code::CodeScene::default()) as Arc<dyn Scene>)
        .model(model)
        .settings(settings)
        .tools(template)
        .skip_warmup(true)
        .build()
        .expect("build should succeed");

    assert!(
        agent.tooling.tools.get("GoalCreate").is_some(),
        "GoalCreate still registered"
    );
    assert!(
        agent.deferred_tools().contains("GoalCreate"),
        "CodeScene defers GoalCreate"
    );
    assert!(!agent.deferred_tools().contains("Read"));

    // And the scene admits ToolSearch, without which a deferred tool
    // could never be activated.
    assert!(scene::code::CodeScene::default().permits_tool("ToolSearch"));

    // The session's ToolSearch must search the session's registry —
    // `select:GoalCreate` has to come back with the schema, or the name is
    // advertised with no way to get the schema back.
    let mut ctx = platform::tool_context_for_test("/tmp".into());
    ctx.config = agent.config.clone();
    let found = agent
        .tooling
        .tools
        .get("ToolSearch")
        .expect("ToolSearch rebound onto the session registry")
        .call(
            serde_json::json!({"query": "select:GoalCreate"}),
            ctx,
            base::tool::ProgressSender::noop("t"),
        )
        .await
        .unwrap();
    match &found.content {
        base::tool::ToolResultContent::Text(s) => {
            assert!(s.contains("<function>") && s.contains("GoalCreate"), "{s}")
        }
        _ => panic!("expected text"),
    }
}

/// Two sessions built from the *same* shared `Arc<InMemoryToolRegistry>`
/// (the daemon `SessionPool.tools` scenario) must each end up with
/// their own independent registry — not both mutating the one they were
/// handed, which would accumulate duplicates that `get` (first match) and
/// `build_tool_defs` (last match) resolve to different instances.
#[tokio::test]
async fn each_session_gets_an_independent_tool_registry_not_a_shared_mutated_one() {
    let settings = Arc::new(test_settings());
    let model: Arc<dyn Model> = Arc::new(DummyModel);
    let scene: Arc<dyn Scene> = Arc::new(scene::code::CodeScene::default());

    // Simulates `SessionPool.tools`: built once, handed to every session's Builder.
    let shared_template = Arc::new(InMemoryToolRegistry::new());
    let before_count = shared_template.list().len();

    let (agent1, _rx1, _tx1) = Builder::new()
        .scene(scene.clone())
        .model(model.clone())
        .settings(settings.clone())
        .tools(shared_template.clone())
        .skip_warmup(true)
        .build()
        .expect("first build should succeed");

    let (agent2, _rx2, _tx2) = Builder::new()
        .scene(scene)
        .model(model)
        .settings(settings)
        .tools(shared_template.clone())
        .skip_warmup(true)
        .build()
        .expect("second build should succeed");

    assert_eq!(
        shared_template.list().len(),
        before_count,
        "the shared template registry handed to every session's Builder must not accumulate \
         per-session tools (Skill/Agent/TeamCreate/...) — it stayed at {before_count} entries \
         before either build and must still be exactly that many after both"
    );
    assert_eq!(
        agent1
            .tooling
            .tools
            .list()
            .iter()
            .filter(|t| t.name() == "TaskStop")
            .count(),
        1,
        "session 1's own registry should have exactly one TaskStop, not accumulated duplicates"
    );
    assert_eq!(
        agent2
            .tooling
            .tools
            .list()
            .iter()
            .filter(|t| t.name() == "TaskStop")
            .count(),
        1,
        "session 2's own registry should have exactly one TaskStop, not accumulated duplicates"
    );
}

/// A session with a `HistoryStore` should get a sidecar handle whose
/// path lives under `global_data_dir/sessions/<id>/session_memory.md`.
#[tokio::test]
async fn build_wires_session_memory_when_history_store_present() {
    let cwd_tmp = tempfile::tempdir().unwrap();
    let projects_tmp = tempfile::tempdir().unwrap();
    let store = history::store::JsonlHistoryStore::with_roots(
        cwd_tmp.path(),
        history::path::HistoryRoots::under(projects_tmp.path()),
    )
    .await
    .expect("store should build");

    let mut settings = test_settings();
    settings.paths.global_data_dir = projects_tmp.path().to_path_buf();
    let settings = Arc::new(settings);
    let model: Arc<dyn Model> = Arc::new(DummyModel);
    let scene: Arc<dyn Scene> = Arc::new(scene::code::CodeScene::default());
    let sid = base::session::SessionId::new().to_string();

    let (agent, _event_rx, _input_tx) = Builder::new()
        .scene(scene)
        .model(model)
        .settings(settings)
        .history_store(Arc::new(store))
        .session_id(sid.clone())
        .skip_warmup(true)
        .build()
        .expect("build should succeed");

    let sm = agent
        .session
        .session_memory
        .as_ref()
        .expect("session_memory should be wired when a history_store is set");
    // Must land in the same sidecar directory the rest of the history
    // crate reads and writes (metadata, input history, prompt state).
    let expected = history::path::session_memory_file(
        &history::path::HistoryRoots::under(projects_tmp.path()).sessions,
        &base::session::SessionId::parse(&sid).unwrap(),
    );
    assert_eq!(sm.path(), expected);
}

/// The most important test in this file: every tool the model can
/// actually call must be resolvable by the session's `Permission` handler.
/// The handler is built from whatever registry the caller passes, while
/// `build()` registers more tools into a fresh per-session registry; an
/// unresolvable name would pass `RuleSetPermission`'s "unknown tool"
/// branch unchecked. Asserting the invariant covers future tools too.
#[tokio::test]
async fn every_registered_tool_is_visible_to_the_permission_handler() {
    use base::interface::permission::{Permission, PermissionOutcome};

    let settings = Arc::new(test_settings());
    let model: Arc<dyn Model> = Arc::new(DummyModel);
    let scene: Arc<dyn Scene> = Arc::new(scene::code::CodeScene::default());

    // Exactly what a daemon does: seed a template with the self-contained
    // built-ins and build the handler over *that*.
    let template = Arc::new(InMemoryToolRegistry::new());
    tools::register_builtin_tools(&template);
    let permission: Arc<dyn Permission> =
        Arc::new(permissions::rule_set_permission::RuleSetPermission::new(
            Arc::new(permissions::gate::PermissionGate::empty()),
            template.clone(),
            base::permission::PermissionMode::DontAsk,
        ));

    let (agent, _event_rx, _input_tx) = Builder::new()
        .scene(scene)
        .model(model)
        .settings(settings)
        .tools(template.clone())
        .permission(permission.clone())
        .skip_warmup(true)
        .build()
        .expect("build should succeed");

    // Sanity: `build()` really did register tools the template lacks —
    // otherwise this test would pass vacuously.
    let session_names: Vec<String> = agent
        .tooling
        .tools
        .list()
        .iter()
        .map(|t| t.name().to_string())
        .collect();
    assert!(
        session_names.iter().any(|n| n == "TaskStop"),
        "expected build() to register engine-state tools; got {session_names:?}"
    );
    assert!(
        template.get("TaskStop").is_none(),
        "the template must stay untouched — `build()` populates a copy, which is \
         precisely why the handler has to be rebound to that copy"
    );

    // Put the *session* into `DontAsk` — the handler reads the live
    // session state, not its construction-time fallback.
    agent
        .session_state
        .set_permission_mode(base::permission::PermissionMode::DontAsk);

    // The invariant: nothing the model can call is unknown to permissions.
    // `DontAsk` with no rules denies everything it *can* resolve, so a
    // `Prompt` here means "unresolvable", which is the bug.
    for name in &session_names {
        let outcome = permission
            .check(
                name,
                &serde_json::json!({}),
                std::path::Path::new("/tmp"),
                "s1",
            )
            .await;
        assert!(
            !matches!(outcome, PermissionOutcome::Prompt { .. }),
            "`{name}` is not resolvable by the permission handler — it would have \
             executed without any check. Got {outcome:?}"
        );
    }
}

/// The permission handler must read the session's *live* mode, so
/// `EnterPlanMode` moves the gate.
#[tokio::test]
async fn entering_plan_mode_moves_the_permission_gate() {
    use base::interface::permission::{Permission, PermissionOutcome};

    let settings = Arc::new(test_settings());
    let model: Arc<dyn Model> = Arc::new(DummyModel);
    let scene: Arc<dyn Scene> = Arc::new(scene::code::CodeScene::default());

    let template = Arc::new(InMemoryToolRegistry::new());
    tools::register_builtin_tools(&template);
    let permission: Arc<dyn Permission> =
        Arc::new(permissions::rule_set_permission::RuleSetPermission::new(
            Arc::new(permissions::gate::PermissionGate::empty()),
            template.clone(),
            base::permission::PermissionMode::Default,
        ));

    let (agent, _event_rx, _input_tx) = Builder::new()
        .scene(scene)
        .model(model)
        .settings(settings)
        .tools(template.clone())
        .permission(permission.clone())
        .skip_warmup(true)
        .build()
        .expect("build should succeed");

    let write = serde_json::json!({"file_path": "/tmp/x.txt", "content": "hi"});
    assert!(
        matches!(
            permission
                .check("Write", &write, std::path::Path::new("/tmp"), "s1")
                .await,
            PermissionOutcome::Permit
        ),
        "an in-project write is allowed in Default mode"
    );

    agent
        .session_state
        .set_permission_mode(base::permission::PermissionMode::Plan);

    assert!(
        matches!(
            permission
                .check("Write", &write, std::path::Path::new("/tmp"), "s1")
                .await,
            PermissionOutcome::Deny { .. }
        ),
        "plan mode must deny a write, even one the tool allows itself"
    );
}

/// Sibling of the above: no `HistoryStore` → no sidecar. An in-memory-only
/// session's id doesn't outlive the process, so a sidecar for it would
/// just be orphaned disk state nothing ever reads back.
#[tokio::test]
async fn build_skips_session_memory_without_history_store() {
    let settings = Arc::new(test_settings());
    let model: Arc<dyn Model> = Arc::new(DummyModel);
    let scene: Arc<dyn Scene> = Arc::new(scene::code::CodeScene::default());

    let (agent, _event_rx, _input_tx) = Builder::new()
        .scene(scene)
        .model(model)
        .settings(settings)
        .skip_warmup(true)
        .build()
        .expect("build should succeed");

    assert!(agent.session.session_memory.is_none());
}
