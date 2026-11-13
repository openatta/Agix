//! 主循环的端到端行为：预算、召回、斜杠命令、技能目录。

use super::support::{recall_test_settings, FixedUsageModel};
use crate::agent::InputMessage;
use crate::turn::prompt::build_prompt_context;
use base::event::AgentEvent;
use base::interface::model::{ModelContentBlock, ModelMessage};
use base::memory::{DurableMemory, MemoryStore, MemoryType};
use base::settings::ThinkingMode;
use base::tool::ToolContext;
use std::path::PathBuf;
use std::sync::Arc;

#[test]
fn prompt_context_includes_os() {
    let settings = base::settings::LoadedSettings::new(
        base::settings::Settings {
            model: base::settings::ModelSettings {
                api_type: base::provider::ApiType::Anthropic,
                base_url: "https://api.example.com".into(),
                auth_token: "test".into(),
                model_name: "test-model".into(),
                max_tokens: 4096,
                thinking_mode: ThinkingMode::Auto,
                effort: None,
                fallback_model: None,
            },
            execution: Default::default(),
            compaction: Default::default(),
            sandbox: Default::default(),
            plugins: Default::default(),
            instruction_file: None,
            prompt_append: None,
            prompt_override: None,
            recorder: None,
            telemetry_url: None,
            memory_enabled: true,
            disable_skill_shell_execution: false,
            path_style: Default::default(),
            permission_mode: base::settings::PermissionMode::Default,
            allow_client_permission_override: false,
            telemetry_enabled: false,
            hooks_config: None,
            project_trust: Default::default(),
            mcp_servers: Default::default(),
            providers: Default::default(),
            default_provider: None,
            task_models: Default::default(),
            language: None,
            scripts: Vec::new(),
        },
        base::settings::PathSettings {
            user_data_dir: "/tmp/user".into(),
            global_data_dir: "/tmp/global".into(),
            workspace: base::settings::Workspace::project_of(std::path::Path::new("/tmp/local")),
            local_data_dir: "/tmp/local".into(),
            scope: "code".into(),
        },
    );
    let ctx = build_prompt_context(
        &settings,
        None,
        None,
        None,
        "test-model",
        false,
        &base::interface::environment::SystemEnvironment,
    );
    assert_eq!(ctx.os, std::env::consts::OS);

    // `ctx.shell` comes from `FrozenContext.shell`, and `ctx.model_name`
    // from the caller-supplied effective model, so after an
    // overloaded-fallback switch the prompt describes the fallback model.
    let frozen = base::frozen::FrozenContext {
        shell: Some("/usr/bin/zsh".into()),
        ..Default::default()
    };
    let ctx_with_frozen = build_prompt_context(
        &settings,
        Some(&frozen),
        None,
        None,
        "fallback-model",
        false,
        &base::interface::environment::SystemEnvironment,
    );
    assert_eq!(ctx_with_frozen.shell, "/usr/bin/zsh");
    assert_eq!(ctx_with_frozen.model_name, "fallback-model");
    assert_ne!(ctx_with_frozen.model_name, settings.values.model.model_name);
}

/// Answers directly (no tool calls) and records every message it was sent,
/// so the test can check what the model actually saw.
struct RecordingAnswerModel {
    seen: Arc<std::sync::Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl base::interface::model::Model for RecordingAnswerModel {
    fn api_type(&self) -> base::provider::ApiType {
        base::provider::ApiType::Anthropic
    }
    async fn stream(
        &self,
        _prompt_blocks: Vec<base::prompt::PromptBlock>,
        _tools: Vec<base::interface::model::ToolDef>,
        messages: Vec<ModelMessage>,
        _params: base::interface::model::StreamParams,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError> {
        let mut seen = self.seen.lock().unwrap();
        for m in &messages {
            for b in &m.content {
                if let ModelContentBlock::Text { text } = b {
                    seen.push(text.clone());
                }
            }
        }
        let events = vec![
            Ok(base::interface::model::ModelEvent::TextDelta {
                text: "answering directly".into(),
            }),
            Ok(base::interface::model::ModelEvent::EndTurn {
                stop_reason: "end_turn".into(),
                usage: Default::default(),
            }),
        ];
        Ok(Box::new(futures::stream::iter(events)))
    }
}

/// A policy is handed every figure the provider reported, cache included.
///
/// What counts as spent is the policy's judgement, and it cannot make one
/// on figures it never receives — and on a provider with the cache warm,
/// the cache fields are most of what the turn read.
#[tokio::test]
async fn a_policy_is_told_what_the_cache_cost() {
    use std::sync::Mutex;

    #[derive(Default)]
    struct Recording(Mutex<Vec<base::interface::budget_policy::Spend>>);

    impl base::interface::budget_policy::BudgetPolicy for Recording {
        fn on_usage(
            &self,
            spend: &base::interface::budget_policy::Spend,
        ) -> base::interface::budget_policy::Spending {
            self.0.lock().unwrap().push(*spend);
            base::interface::budget_policy::Spending::WithinBudget
        }
    }

    let tmp = tempfile::tempdir().unwrap();
    let reported = base::interface::model::Usage {
        input_tokens: 400,
        output_tokens: 300,
        cache_creation_input_tokens: 2_000,
        cache_read_input_tokens: 30_000,
    };
    let model: Arc<dyn base::interface::model::Model> =
        Arc::new(FixedUsageModel { usage: reported });
    let scene: Arc<base::interface::scene::SceneSpec> =
        Arc::new(scene::code::spec(scene::code::Interaction::Interactive));
    let policy = Arc::new(Recording::default());

    let (telemetry_tx, _telemetry_rx) = tokio::sync::mpsc::channel(16);
    let (mut agent, _event_rx, _input_tx) = crate::agent::Builder::new()
        .scene(scene)
        .model(model)
        .settings(Arc::new(recall_test_settings(tmp.path())))
        .telemetry_handle(telemetry::TelemetryHandle::new(telemetry_tx))
        .budget_policy(policy.clone())
        .skip_warmup(true)
        .build()
        .expect("build should succeed");

    agent
        .process_turn(
            InputMessage::User {
                content: "hello".into(),
                attachments: vec![],
                turn_id: "t1".into(),
            },
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect("process_turn should succeed");

    let seen = policy.0.lock().unwrap();
    let spend = seen
        .first()
        .expect("the policy was never asked about this turn");
    assert_eq!(
        *spend,
        base::interface::budget_policy::Spend::from(reported),
        "the policy was given a different turn than the one the provider reported"
    );
    assert_eq!(
        spend.total_tokens(),
        700,
        "the older reading has to keep meaning what it meant"
    );
    assert_eq!(
        spend.all_tokens(),
        32_700,
        "reading everything is what the four fields are for"
    );
}

/// `max_budget_tokens` must stop the turn on real reported usage — no
/// dollar conversion, no per-model price table to keep in sync.
#[tokio::test]
async fn max_budget_tokens_stops_the_turn_on_reported_usage() {
    let tmp = tempfile::tempdir().unwrap();
    let model: Arc<dyn base::interface::model::Model> = Arc::new(FixedUsageModel {
        usage: base::interface::model::Usage {
            input_tokens: 800,
            output_tokens: 300,
            ..Default::default()
        },
    });
    let scene: Arc<base::interface::scene::SceneSpec> =
        Arc::new(scene::code::spec(scene::code::Interaction::Interactive));

    let mut settings = recall_test_settings(tmp.path());
    settings.values.execution = base::settings::ExecutionSettings {
        max_budget_tokens: Some(1000),
        ..Default::default()
    };

    let (telemetry_tx, mut telemetry_rx) = tokio::sync::mpsc::channel(16);
    let (mut agent, _event_rx, _input_tx) = crate::agent::Builder::new()
        .scene(scene)
        .model(model)
        .settings(Arc::new(settings))
        .telemetry_handle(telemetry::TelemetryHandle::new(telemetry_tx))
        .skip_warmup(true)
        .build()
        .expect("build should succeed");

    let outcome = agent
        .process_turn(
            InputMessage::User {
                content: "hello".into(),
                attachments: vec![],
                turn_id: "t1".into(),
            },
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect("process_turn should succeed");

    assert_eq!(
        outcome.stop_reason, "budget_exceeded",
        "800 + 300 = 1100 reported tokens should trip a 1000-token budget"
    );

    // A budget-stopped turn must not be a telemetry blind spot: both a
    // `budget_enforced(TurnStopped)` and the turn's own `turn_complete`
    // have to land, or this exit path silently disappears from any
    // downstream dashboard/alert built on the event stream.
    let mut kinds = Vec::new();
    while let Ok(event) = telemetry_rx.try_recv() {
        kinds.push(event.kind().to_string());
    }
    assert!(
        kinds.contains(&"budget_enforced".to_string()),
        "got: {kinds:?}"
    );
    assert!(
        kinds.contains(&"turn_complete".to_string()),
        "got: {kinds:?}"
    );
}

/// A selection call still running when recall's budget runs out is
/// stopped, not left spending in the background.
#[tokio::test]
async fn a_recall_past_its_budget_is_stopped() {
    struct Probe(Arc<tokio::sync::Notify>);
    impl Drop for Probe {
        fn drop(&mut self) {
            self.0.notify_one();
        }
    }
    struct SlowSelection {
        dropped: Arc<tokio::sync::Notify>,
        answer: RecordingAnswerModel,
    }
    #[async_trait::async_trait]
    impl base::interface::model::Model for SlowSelection {
        fn api_type(&self) -> base::provider::ApiType {
            base::provider::ApiType::Anthropic
        }
        async fn stream(
            &self,
            prompt_blocks: Vec<base::prompt::PromptBlock>,
            tools: Vec<base::interface::model::ToolDef>,
            messages: Vec<ModelMessage>,
            params: base::interface::model::StreamParams,
            cancel: tokio_util::sync::CancellationToken,
        ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError>
        {
            let selecting = params.origin.as_ref().and_then(|o| o.purpose())
                == Some(base::interface::model::call_purpose::MEMORY);
            if selecting {
                let _probe = Probe(self.dropped.clone());
                futures::future::pending::<()>().await;
            }
            self.answer
                .stream(prompt_blocks, tools, messages, params, cancel)
                .await
        }
    }

    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(MemoryStore::new(
        tmp.path().join("user"),
        tmp.path().join("local"),
    ));
    store
        .persist_batch(
            (0..6)
                .map(|i| DurableMemory {
                    name: format!("m{i}"),
                    description: format!("memory {i}"),
                    memory_type: MemoryType::Project,
                    content: String::new(),
                    source_session_id: String::new(),
                    confidence: 0.9,
                    last_seen: "2026-08-01T00:00:00Z".into(),
                    recall_count: 0,
                })
                .collect(),
        )
        .unwrap();
    let dropped = Arc::new(tokio::sync::Notify::new());
    let model = SlowSelection {
        dropped: dropped.clone(),
        answer: RecordingAnswerModel {
            seen: Default::default(),
        },
    };
    let (mut agent, _event_rx, _input_tx) = crate::agent::Builder::new()
        .scene(Arc::new(scene::code::spec(
            scene::code::Interaction::Interactive,
        )))
        .model(Arc::new(model))
        .settings(Arc::new(recall_test_settings(tmp.path())))
        .memory_store(store)
        .skip_warmup(true)
        .build()
        .expect("build should succeed");

    let outcome = agent
        .process_turn(
            InputMessage::User {
                content: "anything".into(),
                attachments: vec![],
                turn_id: "t1".into(),
            },
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(outcome.stop_reason, "end_turn");
    tokio::time::timeout(std::time::Duration::from_secs(30), dropped.notified())
        .await
        .expect("the selection call outlived its turn's recall budget");
}

/// Short conversational turns without tool calls are most turns. The
/// recalled memories must reach the model in the *current* turn, and must carry their content, not
/// just the one-line description that exists to rank them.
#[tokio::test]
async fn recall_reaches_the_model_on_a_turn_with_no_tool_calls() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(MemoryStore::new(
        tmp.path().join("user"),
        tmp.path().join("local"),
    ));
    // One entry: `select_memories_from` short-circuits below its
    // max_results and selects it without a model call.
    store
        .persist_batch(vec![DurableMemory {
            name: "deploy-window".into(),
            description: "when deploys are allowed".into(),
            memory_type: MemoryType::Project,
            content: "Deploys only on Tuesdays, never on a Friday.".into(),
            source_session_id: String::new(),
            confidence: 0.9,
            last_seen: "2026-08-01T00:00:00Z".into(),
            recall_count: 0,
        }])
        .unwrap();

    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let model: Arc<dyn base::interface::model::Model> =
        Arc::new(RecordingAnswerModel { seen: seen.clone() });
    let scene: Arc<base::interface::scene::SceneSpec> =
        Arc::new(scene::code::spec(scene::code::Interaction::Interactive));

    let (mut agent, _event_rx, _input_tx) = crate::agent::Builder::new()
        .scene(scene)
        .model(model)
        .settings(Arc::new(recall_test_settings(tmp.path())))
        .memory_store(store)
        .skip_warmup(true)
        .build()
        .expect("build should succeed");

    let outcome = agent
        .process_turn(
            InputMessage::User {
                content: "can I ship this today?".into(),
                attachments: vec![],
                turn_id: "t1".into(),
            },
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect("process_turn should succeed");
    assert_eq!(outcome.stop_reason, "end_turn");

    let seen = seen.lock().unwrap();
    let transcript = seen.join("\n");
    assert!(
        transcript.contains("deploy-window"),
        "recalled memory never reached the model: {transcript}"
    );
    assert!(
        transcript.contains("Deploys only on Tuesdays"),
        "recall carried only the description, not the memory content: {transcript}"
    );
}

/// A catalog another owner swaps in place: the next turn's
/// `reload_changed` finds nothing to do, and the change is announced
/// anyway because this session was shown the old list.
#[tokio::test]
async fn a_skill_swapped_into_a_shared_catalog_is_announced() {
    struct Swappable(std::sync::Mutex<Vec<&'static str>>);
    impl base::interface::skill_catalog::SkillCatalog for Swappable {
        fn list(&self) -> Vec<base::frozen::SkillEntry> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .map(|n| base::frozen::SkillEntry {
                    name: (*n).into(),
                    description: "d".into(),
                    user_invocable: true,
                    ..Default::default()
                })
                .collect()
        }
        fn get(&self, name: &str) -> Option<base::frozen::SkillEntry> {
            self.list().into_iter().find(|e| e.name == name)
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

    let tmp = tempfile::tempdir().unwrap();
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (mut agent, mut event_rx, _input_tx) = crate::agent::Builder::new()
        .scene(Arc::new(scene::code::spec(
            scene::code::Interaction::Interactive,
        )))
        .model(Arc::new(RecordingAnswerModel { seen: seen.clone() }))
        .settings(Arc::new(recall_test_settings(tmp.path())))
        .skip_warmup(true)
        .build()
        .expect("build should succeed");
    let catalog = Arc::new(Swappable(std::sync::Mutex::new(vec!["early", "gone"])));
    agent.tooling.skills = catalog.clone();

    let turn = |id: &str| InputMessage::User {
        content: "hello".into(),
        attachments: vec![],
        turn_id: id.into(),
    };
    let changes = |rx: &mut crate::agent::EventReceiver| {
        let mut out = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let AgentEvent::SkillsChanged { added, removed, .. } = event {
                out.push((added, removed));
            }
        }
        out
    };
    let cancel = tokio_util::sync::CancellationToken::new;
    agent.process_turn(turn("t1"), cancel()).await.unwrap();
    assert!(
        changes(&mut event_rx).is_empty(),
        "the first turn lists them all"
    );

    *catalog.0.lock().unwrap() = vec!["early", "latecomer"];
    agent.process_turn(turn("t2"), cancel()).await.unwrap();
    assert_eq!(
        changes(&mut event_rx),
        vec![(vec!["latecomer".to_string()], vec!["gone".to_string()])]
    );
    assert!(seen
        .lock()
        .unwrap()
        .iter()
        .any(|t| t.contains("\"latecomer\" was just added")));

    agent.process_turn(turn("t3"), cancel()).await.unwrap();
    assert!(changes(&mut event_rx).is_empty(), "announced once");
}

/// `/name` expands through the catalog — the expansion the `Skill` tool
/// uses — and its grants hold for the turn it starts, not just until
/// that turn clears the previous one's.
#[tokio::test]
async fn a_slash_command_expands_through_the_catalog_and_grants_for_its_turn() {
    struct Expanding;
    impl base::interface::skill_catalog::SkillCatalog for Expanding {
        fn list(&self) -> Vec<base::frozen::SkillEntry> {
            vec![base::frozen::SkillEntry {
                name: "build".into(),
                description: "d".into(),
                user_invocable: true,
                ..Default::default()
            }]
        }
        fn get(&self, name: &str) -> Option<base::frozen::SkillEntry> {
            self.list().into_iter().find(|e| e.name == name)
        }
        fn body(&self, _: &str) -> Option<String> {
            Some("the raw body".into())
        }
        fn expand<'a>(
            &'a self,
            _: &'a str,
            args: &'a str,
            _: &'a ToolContext,
            _: &'a dyn base::interface::permission::PermissionPolicy,
        ) -> futures::future::BoxFuture<
            'a,
            Option<Result<base::interface::skill_catalog::SkillExpansion, String>>,
        > {
            Box::pin(async move {
                Some(Ok(base::interface::skill_catalog::SkillExpansion {
                    text: format!("EXPANDED for {args}"),
                    grants: vec![("Bash".into(), Some("make".into()))],
                }))
            })
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

    #[derive(Default)]
    struct Grants(std::sync::Mutex<Vec<String>>);
    #[async_trait::async_trait]
    impl base::interface::permission::PermissionGrants for Grants {
        async fn grant(
            &self,
            grant: base::interface::permission::Grant,
        ) -> Result<(), base::interface::permission::GrantError> {
            self.0.lock().unwrap().push(format!(
                "{}({:?}) {:?}",
                grant.tool_name, grant.content, grant.scope
            ));
            Ok(())
        }
        fn clear(&self, scope: base::interface::permission::GrantScope) {
            self.0.lock().unwrap().push(format!("clear {scope:?}"));
        }
    }

    let tmp = tempfile::tempdir().unwrap();
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (mut agent, _event_rx, _input_tx) = crate::agent::Builder::new()
        .scene(Arc::new(scene::code::spec(
            scene::code::Interaction::Interactive,
        )))
        .model(Arc::new(RecordingAnswerModel { seen: seen.clone() }))
        .settings(Arc::new(recall_test_settings(tmp.path())))
        .skip_warmup(true)
        .build()
        .expect("build should succeed");
    let catalog: Arc<dyn base::interface::skill_catalog::SkillCatalog> = Arc::new(Expanding);
    agent.tooling.skills = catalog.clone();
    agent.tooling.commands = Arc::new(crate::commands::CommandRegistry::from_skill_catalog(
        catalog,
    ));
    let grants = Arc::new(Grants::default());
    agent.gate.permission.grants = Some(grants.clone());

    agent
        .process_turn(
            InputMessage::User {
                content: "/build release".into(),
                attachments: vec![],
                turn_id: "t1".into(),
            },
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();

    assert!(seen
        .lock()
        .unwrap()
        .iter()
        .any(|t| t.contains("EXPANDED for release") && !t.contains("the raw body")));
    assert_eq!(
        *grants.0.lock().unwrap(),
        vec![
            "clear Turn".to_string(),
            "Bash(Some(\"make\")) Turn".to_string()
        ]
    );
}

/// Refuses every write while `down` is set.
#[derive(Default)]
struct StoreThatGoesDown {
    inner: history::store::InMemoryHistoryStore,
    down: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl history::store::HistoryStore for StoreThatGoesDown {
    async fn append(
        &self,
        session: base::session::SessionId,
        entry: history::entry::LogEntry,
    ) -> Result<(), history::error::HistoryError> {
        if self.down.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(history::error::HistoryError::Io(std::io::Error::other(
                "the disk is full",
            )));
        }
        self.inner.append(session, entry).await
    }

    async fn load(
        &self,
        session: base::session::SessionId,
    ) -> Result<Vec<history::entry::EnvelopedEntry>, history::error::HistoryError> {
        self.inner.load(session).await
    }

    async fn list_sessions(
        &self,
    ) -> Result<Vec<base::session::SessionId>, history::error::HistoryError> {
        self.inner.list_sessions().await
    }

    async fn delete(
        &self,
        session: base::session::SessionId,
    ) -> Result<(), history::error::HistoryError> {
        self.inner.delete(session).await
    }
}

fn completed_persisted(events: &mut tokio::sync::mpsc::UnboundedReceiver<AgentEvent>) -> bool {
    std::iter::from_fn(|| events.try_recv().ok())
        .find_map(|event| match event {
            AgentEvent::TurnComplete { persisted, .. } => Some(persisted),
            _ => None,
        })
        .expect("the turn completed")
}

/// A turn whose log writes fail still completes, saying its log lacks part of
/// it; a flush once the store is back writes the rest — the transcript and
/// the state the failed write left out.
#[tokio::test]
async fn a_turn_the_log_did_not_take_completes_unpersisted_and_a_flush_catches_the_log_up() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(StoreThatGoesDown::default());
    let sid = base::session::SessionId::new();
    let (mut agent, mut events, _input_tx) = crate::agent::Builder::new()
        .scene(Arc::new(scene::code::spec(
            scene::code::Interaction::Interactive,
        )))
        .model(Arc::new(FixedUsageModel {
            usage: Default::default(),
        }))
        .settings(Arc::new(recall_test_settings(tmp.path())))
        .history_store(store.clone())
        .session_id(sid.to_string())
        .skip_warmup(true)
        .build()
        .expect("build should succeed");
    let user = |turn_id: &str| InputMessage::User {
        content: "hello".into(),
        attachments: vec![],
        turn_id: turn_id.into(),
    };
    let flush = || {
        let (reply, flushed) = tokio::sync::oneshot::channel();
        (InputMessage::Flush { reply }, flushed)
    };
    let cancel = tokio_util::sync::CancellationToken::new;

    store.down.store(true, std::sync::atomic::Ordering::SeqCst);
    agent.session_state().enter_plan_mode();
    agent.process_turn(user("t1"), cancel()).await.unwrap();
    assert!(!completed_persisted(&mut events));

    let (asked, flushed) = flush();
    agent.process_turn(asked, cancel()).await.unwrap();
    assert!(flushed.await.unwrap().is_err(), "the store is still down");

    store.down.store(false, std::sync::atomic::Ordering::SeqCst);
    let (asked, flushed) = flush();
    agent.process_turn(asked, cancel()).await.unwrap();
    assert_eq!(flushed.await.unwrap(), Ok(()));
    let logged = history::store::HistoryStore::load(&*store, sid)
        .await
        .unwrap();
    assert!(
        logged
            .iter()
            .any(|e| format!("{:?}", e.entry).contains("hello")),
        "{logged:?}"
    );
    assert!(
        logged.iter().any(|e| matches!(&e.entry,
            history::entry::LogEntry::Extension { ns, payload, .. }
                if ns == base::context::SESSION_STATE_NS && payload["plan"].is_object())),
        "{logged:?}"
    );

    agent.process_turn(user("t2"), cancel()).await.unwrap();
    assert!(completed_persisted(&mut events));
}
