//! Builder validation, module ordering and scene narrowing, session end and failed-turn persistence.

use super::*;

#[test]
fn builder_requires_scene() {
    assert!(Builder::new().build().is_err());
}

pub(super) struct Probe {
    pub(super) id: &'static str,
    pub(super) requires: &'static [&'static str],
    pub(super) saw: Arc<std::sync::Mutex<Vec<String>>>,
}

impl crate::module::Module for Probe {
    fn id(&self) -> &'static str {
        self.id
    }
    fn requires(&self) -> &'static [&'static str] {
        self.requires
    }
    fn on_session(
        &self,
        cx: &mut crate::module::SessionContext<'_>,
    ) -> Result<(), crate::module::ModuleError> {
        self.saw.lock().unwrap().push(format!(
            "{} depth={} spawner={}",
            self.id,
            cx.depth,
            cx.agent_spawner().is_some()
        ));
        Ok(())
    }
}

pub(super) fn probe_builder(modules: Vec<Arc<dyn crate::module::Module>>) -> Builder {
    Builder::new()
        .scene(Arc::new(scene::code::CodeScene::default()) as Arc<dyn Scene>)
        .model(Arc::new(DummyModel))
        .settings(Arc::new(test_settings()))
        .skip_warmup(true)
        .modules(modules)
}

#[test]
fn modules_run_in_order_once_the_session_exists() {
    let saw = Arc::new(std::sync::Mutex::new(Vec::new()));
    let probe = |id, requires| {
        Arc::new(Probe {
            id,
            requires,
            saw: saw.clone(),
        }) as Arc<dyn crate::module::Module>
    };
    probe_builder(vec![probe("a", &[]), probe("b", &["a"])])
        .build()
        .expect("build should succeed");
    assert_eq!(
        *saw.lock().unwrap(),
        ["a depth=0 spawner=false", "b depth=0 spawner=false"],
        "no module here provides a spawner, so neither sees one"
    );
}

pub(super) struct SeesPrompt(pub(super) Arc<std::sync::Mutex<Option<Option<String>>>>);

impl crate::module::Module for SeesPrompt {
    fn id(&self) -> &'static str {
        "sees-prompt"
    }
    fn on_session(
        &self,
        cx: &mut crate::module::SessionContext<'_>,
    ) -> Result<(), crate::module::ModuleError> {
        *self.0.lock().unwrap() = Some(cx.settings.prompt_append.clone());
        Ok(())
    }
}

#[test]
fn a_session_prompt_reaches_the_agent_and_not_its_modules() {
    let saw = Arc::new(std::sync::Mutex::new(None));
    let (agent, _rx, _tx) = probe_builder(vec![Arc::new(SeesPrompt(saw.clone()))])
        .session_prompt(SessionPrompt {
            append: Some("SESSION-ONLY".into()),
            override_text: None,
        })
        .build()
        .expect("build should succeed");
    assert_eq!(
        agent.settings().prompt_append.as_deref(),
        Some("SESSION-ONLY")
    );
    assert_eq!(
        *saw.lock().unwrap(),
        Some(None),
        "modules — and the delegates they spawn — must not inherit a session's own prompt"
    );
}

#[test]
fn a_module_whose_requirement_comes_later_is_refused() {
    let saw = Arc::new(std::sync::Mutex::new(Vec::new()));
    let probe = |id, requires| {
        Arc::new(Probe {
            id,
            requires,
            saw: saw.clone(),
        }) as Arc<dyn crate::module::Module>
    };
    let err = match probe_builder(vec![probe("b", &["a"]), probe("a", &[])]).build() {
        Ok(_) => panic!("a module list out of order was accepted"),
        Err(e) => e.to_string(),
    };
    assert!(err.contains("requires `a`"), "{err}");
    assert!(
        saw.lock().unwrap().is_empty(),
        "a refused list still ran modules"
    );
    let err = match probe_builder(vec![probe("a", &[]), probe("a", &[])]).build() {
        Ok(_) => panic!("a module assembled twice was accepted"),
        Err(e) => e.to_string(),
    };
    assert!(err.contains("assembled twice"), "{err}");
}

pub(super) struct NarrowingScene {
    pub(super) modules: Option<Vec<String>>,
    pub(super) disabled: Vec<String>,
}

impl Scene for NarrowingScene {
    fn id(&self) -> &str {
        "narrowing"
    }
    fn name(&self) -> &str {
        "Narrowing"
    }
    fn description(&self) -> &str {
        ""
    }
    fn build_system_prompt(
        &self,
        _: &base::interface::scene::ScenePromptContext,
    ) -> Vec<base::prompt::PromptBlock> {
        Vec::new()
    }
    fn tools(&self) -> Vec<String> {
        Vec::new()
    }
    fn token_budget(&self) -> base::interface::scene::TokenBudget {
        base::interface::scene::TokenBudget {
            compact_threshold: 0,
            compact_keep_recent: 10,
        }
    }
    fn modules(&self) -> Option<Vec<String>> {
        self.modules.clone()
    }
    fn disabled_modules(&self) -> Vec<String> {
        self.disabled.clone()
    }
}

pub(super) struct ListsModules {
    pub(super) saw: Arc<std::sync::Mutex<Vec<String>>>,
}

impl crate::module::Module for ListsModules {
    fn id(&self) -> &'static str {
        "lister"
    }
    fn on_session(
        &self,
        cx: &mut crate::module::SessionContext<'_>,
    ) -> Result<(), crate::module::ModuleError> {
        let mut saw = self.saw.lock().unwrap();
        saw.extend(cx.modules.iter().map(|m| format!("handed {}", m.id())));
        Ok(())
    }
}

pub(super) fn narrowed_build(
    modules: Option<&[&str]>,
    disabled: &[&str],
    assembled: Vec<(&'static str, &'static [&'static str])>,
) -> (Result<(), String>, Vec<String>) {
    let saw = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut list: Vec<Arc<dyn crate::module::Module>> = assembled
        .into_iter()
        .map(|(id, requires)| {
            Arc::new(Probe {
                id,
                requires,
                saw: saw.clone(),
            }) as Arc<dyn crate::module::Module>
        })
        .collect();
    list.push(Arc::new(ListsModules { saw: saw.clone() }));
    let scene = NarrowingScene {
        modules: modules.map(|m| m.iter().map(|s| s.to_string()).collect()),
        disabled: disabled.iter().map(|s| s.to_string()).collect(),
    };
    let result = Builder::new()
        .scene(Arc::new(scene) as Arc<dyn Scene>)
        .model(Arc::new(DummyModel))
        .settings(Arc::new(test_settings()))
        .skip_warmup(true)
        .modules(list)
        .build()
        .map(|_| ())
        .map_err(|e| e.to_string());
    let saw = saw.lock().unwrap().clone();
    (result, saw)
}

#[test]
fn a_scene_that_names_no_modules_runs_every_assembled_one() {
    let (result, saw) = narrowed_build(None, &[], vec![("a", &[]), ("b", &[])]);
    result.unwrap();
    assert_eq!(
        saw,
        [
            "a depth=0 spawner=false",
            "b depth=0 spawner=false",
            "handed a",
            "handed b",
            "handed lister",
        ]
    );
}

#[test]
fn a_module_the_scene_does_not_permit_does_not_run_and_is_not_handed_on() {
    let (result, saw) = narrowed_build(
        Some(&["a", "c", "lister"]),
        &["c"],
        vec![("a", &[]), ("b", &[]), ("c", &[])],
    );
    result.unwrap();
    assert_eq!(
        saw,
        ["a depth=0 spawner=false", "handed a", "handed lister"],
        "`b` is outside the whitelist and `c` is disabled; delegates must not get them either"
    );
}

#[test]
fn disabling_a_module_leaves_the_rest_running() {
    let (result, saw) = narrowed_build(None, &["a"], vec![("a", &[]), ("b", &[])]);
    result.unwrap();
    assert_eq!(
        saw,
        ["b depth=0 spawner=false", "handed b", "handed lister"]
    );
}

#[test]
fn a_kept_module_whose_requirement_the_scene_excludes_is_refused() {
    let (result, saw) =
        narrowed_build(Some(&["b", "lister"]), &[], vec![("a", &[]), ("b", &["a"])]);
    let err = result.expect_err("`b` cannot run without `a`");
    assert!(
        err.contains("module `b`") && err.contains("requires `a`") && err.contains("narrowing"),
        "{err}"
    );
    assert!(saw.is_empty(), "a refused list still ran modules");

    let (result, _) = narrowed_build(None, &["a"], vec![("a", &[]), ("b", &["a"])]);
    assert!(result.unwrap_err().contains("requires `a`"));
}

#[tokio::test]
async fn run_emits_session_end_on_exit() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    let (mut agent, _event_rx, _input_tx) = Builder::new()
        .scene(Arc::new(scene::code::CodeScene::default()) as Arc<dyn Scene>)
        .model(Arc::new(DummyModel))
        .settings(Arc::new(test_settings()))
        .tools(Arc::new(base::tool::InMemoryToolRegistry::new()))
        .permission(Arc::new(PromptingPermission))
        .telemetry_handle(telemetry::TelemetryHandle::new(tx))
        .skip_warmup(true)
        .build()
        .expect("build should succeed");

    // Already-cancelled token: `run()` exits on the loop's first
    // iteration without ever needing the model or a real turn.
    let cancel = CancellationToken::new();
    cancel.cancel();
    agent.run(cancel).await;

    let mut kinds = Vec::new();
    while let Ok(event) = rx.try_recv() {
        kinds.push(event.kind().to_string());
    }
    assert!(kinds.contains(&"session_end".to_string()), "got: {kinds:?}");
}

pub(super) struct DummyModel;
#[async_trait::async_trait]
impl Model for DummyModel {
    fn api_type(&self) -> base::provider::ApiType {
        base::provider::ApiType::Anthropic
    }
    async fn stream(
        &self,
        _prompt_blocks: Vec<base::prompt::PromptBlock>,
        _tools: Vec<base::interface::model::ToolDef>,
        _messages: Vec<base::interface::model::ModelMessage>,
        _params: base::interface::model::StreamParams,
        _cancel: CancellationToken,
    ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError> {
        unimplemented!("not exercised by these tests")
    }
}

/// Fails every call, the way an unreachable endpoint or a 5xx does.
/// `DummyModel` can't stand in here — it panics rather than returning
/// `Err`, so it never reaches the turn's model-error return path.
pub(super) struct FailingModel;
#[async_trait::async_trait]
impl Model for FailingModel {
    fn api_type(&self) -> base::provider::ApiType {
        base::provider::ApiType::Anthropic
    }
    async fn stream(
        &self,
        _prompt_blocks: Vec<base::prompt::PromptBlock>,
        _tools: Vec<base::interface::model::ToolDef>,
        _messages: Vec<base::interface::model::ModelMessage>,
        _params: base::interface::model::StreamParams,
        _cancel: CancellationToken,
    ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError> {
        Err(base::interface::model::ModelError::Api {
            status: 503,
            message: "simulated endpoint failure".into(),
        })
    }
}

/// A turn that fails still has to leave the user's message on disk:
/// `HistoryStore::append` creates the file lazily, and the session whose
/// turns all failed is exactly the one a user wants `--continue` to find.
#[tokio::test]
async fn a_failed_turn_still_persists_the_users_message() {
    let cwd_tmp = tempfile::tempdir().unwrap();
    let projects_tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(
        history::store::JsonlHistoryStore::with_roots(
            cwd_tmp.path(),
            history::path::HistoryRoots::under(projects_tmp.path()),
        )
        .await
        .expect("store should build"),
    );

    let settings = Arc::new(test_settings());
    let model: Arc<dyn Model> = Arc::new(FailingModel);
    let scene: Arc<dyn Scene> = Arc::new(scene::code::CodeScene::default());
    let sid = base::session::SessionId::new().to_string();

    let (mut agent, _event_rx, _input_tx) = Builder::new()
        .scene(scene)
        .model(model)
        .settings(settings)
        .history_store(store.clone())
        .session_id(sid.clone())
        .skip_warmup(true)
        .build()
        .expect("build should succeed");

    let outcome = agent
        .run_turn(
            "remember this".into(),
            "turn-1".into(),
            CancellationToken::new(),
        )
        .await;
    assert!(outcome.is_err(), "FailingModel should fail the turn");

    let parsed = base::session::SessionId::parse(&sid).unwrap();
    let entries = history::store::HistoryStore::load(&*store, parsed)
        .await
        .expect("the failed turn must have created a transcript on disk");
    assert!(
        entries
            .iter()
            .any(|e| format!("{:?}", e.entry).contains("remember this")),
        "the user's message must survive a failed turn, got: {entries:?}"
    );
}

/// `/model` has to reach the engine. `process_turn`'s command match is
/// exhaustive, so a new `EngineCommand` fails the build instead of being
/// swallowed.
#[tokio::test]
async fn update_model_command_changes_the_model_the_next_turn_uses() {
    let (mut agent, _event_rx, _input_tx) = Builder::new()
        .scene(Arc::new(scene::code::CodeScene::default()) as Arc<dyn Scene>)
        .model(Arc::new(DummyModel) as Arc<dyn Model>)
        .settings(Arc::new(test_settings()))
        .skip_warmup(true)
        .build()
        .expect("build should succeed");

    assert_eq!(agent.settings.model.model_name, "test");

    agent
        .process_turn(
            InputMessage::System {
                kind: EngineCommand::UpdateModel,
                content: "claude-opus-5".into(),
            },
            CancellationToken::new(),
        )
        .await
        .expect("UpdateModel should be handled");

    assert_eq!(agent.settings.model.model_name, "claude-opus-5");
}

/// The counter a host builds the session with is what its size — the number
/// compaction triggers on — is judged by, request overhead included.
#[tokio::test]
async fn a_session_is_judged_by_the_counter_it_was_built_with() {
    let counter: Arc<dyn base::interface::token_counter::TokenCounter> = Arc::new(
        base::interface::token_counter::FixedTokenCounter::new(0, 1_000),
    );
    let (mut agent, _event_rx, _input_tx) = Builder::new()
        .scene(Arc::new(scene::code::CodeScene::default()) as Arc<dyn Scene>)
        .model(Arc::new(super::events_and_steering::PlainReplyModel))
        .settings(Arc::new(test_settings()))
        .token_counter(counter.clone())
        .skip_warmup(true)
        .build()
        .expect("build should succeed");
    assert!(Arc::ptr_eq(agent.session.token_counter(), &counter));

    agent
        .run_turn("go".into(), "t1".into(), CancellationToken::new())
        .await
        .expect("the turn runs");
    let size = agent.session.token_count();
    assert!(size > 0 && size % 1_000 == 0, "{size}");
    let overhead = agent.compaction.request_overhead_tokens;
    assert!(overhead > 0 && overhead % 1_000 == 0, "{overhead}");
}

/// Answers at once, keeping the system prompt each call carried.
struct KeepsPrompt(Arc<std::sync::Mutex<Vec<String>>>);

#[async_trait::async_trait]
impl Model for KeepsPrompt {
    fn api_type(&self) -> base::provider::ApiType {
        base::provider::ApiType::Anthropic
    }
    async fn stream(
        &self,
        prompt_blocks: Vec<base::prompt::PromptBlock>,
        tools: Vec<base::interface::model::ToolDef>,
        messages: Vec<base::interface::model::ModelMessage>,
        params: base::interface::model::StreamParams,
        cancel: CancellationToken,
    ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError> {
        self.0.lock().unwrap().extend(
            prompt_blocks
                .iter()
                .filter(|b| b.name.as_deref() == Some(base::prompt::names::RULES))
                .map(|b| b.content.clone()),
        );
        super::events_and_steering::PlainReplyModel
            .stream(prompt_blocks, tools, messages, params, cancel)
            .await
    }
}

/// The rule index the model is shown is the one the host's provider gives,
/// not the three tiers on disk.
#[tokio::test]
async fn the_rule_index_comes_from_the_provider_the_session_was_built_with() {
    let rules = Arc::new(base::interface::instruction_provider::StaticRules::new(
        "service",
        vec![base::rules::RuleEntry {
            name: "deploys".into(),
            description: "how a deploy goes out".into(),
            path: "/srv/rules/deploys.md".into(),
        }],
    ));
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (mut agent, _event_rx, _input_tx) = Builder::new()
        .scene(Arc::new(scene::code::CodeScene::default()) as Arc<dyn Scene>)
        .model(Arc::new(KeepsPrompt(seen.clone())))
        .settings(Arc::new(test_settings()))
        .rule_provider(rules)
        .skip_warmup(true)
        .build()
        .expect("build should succeed");
    agent
        .run_turn("go".into(), "t1".into(), CancellationToken::new())
        .await
        .expect("the turn runs");
    let seen = seen.lock().unwrap();
    assert!(
        seen.iter()
            .any(|block| block.contains("`/srv/rules/deploys.md`: how a deploy goes out")),
        "{seen:?}"
    );
}
