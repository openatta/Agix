//! The same conversation, run twice, must leave the same log.
//!
//! Every entry's timestamp and id come from `Environment`, not straight from
//! the machine; otherwise a replay would agree about everything a person cares
//! about while disagreeing on every line at the byte level — which is exactly
//! the level a regression net compares at. This is the check that they do.

use base::id::Id;
use base::interface::environment::FixedEnvironment;
use base::interface::model::Model;
use base::interface::scene::Scene;
use base::interface::tool::InMemoryToolRegistry;
use base::settings::{PathSettings, PermissionMode, Settings, ThinkingMode};
use runtime::agent::{Builder, InputMessage};
use std::sync::Arc;
use test_runner::scripted_model::{Reply, ScriptedModel};
use tokio_util::sync::CancellationToken;

/// Run one scripted session and return the raw bytes of the log it left.
async fn run_and_read_log(root: &std::path::Path, fixed: bool) -> String {
    std::fs::create_dir_all(root.join("workdir")).expect("workdir");

    let model: Arc<dyn Model> = ScriptedModel::new(vec![
        Reply::Tool {
            id: "c1",
            name: "GoldenEcho",
            input: serde_json::json!({"say": "hello"}),
        },
        Reply::Text("done"),
    ]);

    let mut settings = base::settings::LoadedSettings::new(
        Settings::defaults_for("golden-model"),
        base::settings::PathSettings::default(),
    );
    settings.values.model.model_name = "golden-model".into();
    settings.values.model.max_tokens = 1024;
    settings.values.model.thinking_mode = ThinkingMode::Off;
    settings.paths = PathSettings {
        user_data_dir: root.join("user"),
        global_data_dir: root.join("global"),
        workspace: base::settings::Workspace::project_of(&root.join("local")),
        local_data_dir: root.join("local"),
        scope: "code".into(),
    };
    settings.values.memory_enabled = false;
    settings.values.permission_mode = PermissionMode::BypassPermissions;
    let settings = Arc::new(settings);

    let registry = Arc::new(InMemoryToolRegistry::new());
    registry.register(Arc::new(Echo));

    let mut store = history::store::JsonlHistoryStore::with_roots(
        &root.join("workdir"),
        history::path::HistoryRoots::under(&root.join("global")),
    )
    .await
    .expect("history store");
    if fixed {
        store = store.with_environment(Arc::new(FixedEnvironment::epoch()));
    }
    let store = Arc::new(store);

    // A fixed session id as well: it names the file and appears in every
    // line, and it is generated the same way entry ids were.
    let session_id = Id::from_bytes([7u8; 16]);

    let mut builder = Builder::new()
        .scene(Arc::new(scene::harnix::HarnixScene) as Arc<dyn Scene>)
        .model(model)
        .tools(registry)
        .settings(settings)
        .session_id(session_id.to_string())
        .history_store(store.clone())
        .skip_warmup(true);
    if fixed {
        builder = builder.environment(Arc::new(FixedEnvironment::epoch()));
    }
    let (mut agent, mut event_rx, input_tx) = builder.build().expect("agent builds");

    let engine = tokio::spawn(async move { agent.run(CancellationToken::new()).await });
    input_tx
        .send(InputMessage::User {
            content: "say hello".into(),
            attachments: vec![],
            turn_id: "t0".into(),
        })
        .expect("send turn");

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let ev = tokio::time::timeout_at(deadline, event_rx.recv())
            .await
            .expect("no TurnComplete within 30s")
            .expect("event channel closed early");
        if matches!(ev, base::event::AgentEvent::TurnComplete { .. }) {
            break;
        }
    }
    drop(input_tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), engine).await;

    let mut logs: Vec<std::path::PathBuf> = Vec::new();
    collect_jsonl(&root.join("global"), &mut logs);
    assert_eq!(
        logs.len(),
        1,
        "expected exactly one transcript, got {logs:?}"
    );
    std::fs::read_to_string(&logs[0]).expect("read log")
}

fn collect_jsonl(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_jsonl(&p, out);
        } else if p.extension().is_some_and(|x| x == "jsonl") {
            out.push(p);
        }
    }
}

/// The acceptance: two runs, one log.
#[tokio::test(flavor = "multi_thread")]
async fn the_same_session_replayed_leaves_the_same_log() {
    let a = tempfile::tempdir().expect("tempdir");
    let b = tempfile::tempdir().expect("tempdir");
    let first = run_and_read_log(a.path(), true)
        .await
        .replace(&a.path().display().to_string(), "<root>");
    let second = run_and_read_log(b.path(), true)
        .await
        .replace(&b.path().display().to_string(), "<root>");

    assert!(
        !first.trim().is_empty(),
        "the run must have logged something"
    );
    assert_eq!(
        first, second,
        "the same conversation under a fixed environment must produce the same log"
    );
}

/// And the check is not vacuous: on the machine's own clock and randomness the
/// two runs disagree.
#[tokio::test(flavor = "multi_thread")]
async fn on_the_machines_own_clock_the_two_runs_disagree() {
    let a = tempfile::tempdir().expect("tempdir");
    let b = tempfile::tempdir().expect("tempdir");
    let first = run_and_read_log(a.path(), false).await;
    let second = run_and_read_log(b.path(), false).await;
    assert_ne!(first, second);
}

#[derive(Debug)]
struct Echo;

#[async_trait::async_trait]
impl base::interface::tool::Tool for Echo {
    fn name(&self) -> &str {
        "GoldenEcho"
    }
    fn spec(&self) -> base::tool::ToolSpec {
        base::tool::ToolSpec::new()
            .concurrency_safe(true)
            .read_only(true)
    }

    fn description(&self) -> &str {
        "Echo the `say` argument back."
    }
    fn input_schema(&self, _: base::tool::PathStyle) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {"say": {"type": "string"}}})
    }
    async fn check_permissions(
        &self,
        _: &serde_json::Value,
        _: &base::interface::tool::ToolContext,
    ) -> base::interface::tool::PermissionDecision {
        base::interface::tool::PermissionDecision::allow()
    }
    async fn call(
        &self,
        input: serde_json::Value,
        _: base::interface::tool::ToolContext,
        _: base::interface::tool::ProgressSender,
    ) -> Result<base::interface::tool::ToolResult, base::error::ToolError> {
        let say = input.get("say").and_then(|v| v.as_str()).unwrap_or("");
        Ok(base::interface::tool::ToolResult::text(format!(
            "echo: {say}"
        )))
    }
}
