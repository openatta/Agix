//! What a turn reports having spent.
//!
//! A turn is several model calls, and a provider reports usage per call. Which
//! makes "how much did this turn cost" a sum, and every number the engine hands
//! a host something that has to be that sum — otherwise a host budgeting on it
//! is budgeting on a fraction, and a budget that undercounts is worse than none
//! because nobody knows it is not holding.
//!
//! The two numbers were both wrong in different ways: `TurnOutcome.usage` was
//! whatever the last call happened to report, and `AgentEvent::TurnComplete`
//! carried a hardcoded zero on every path — which the daemon forwards, so every
//! JSON-RPC client saw a turn that spent nothing.
//!
//! The usages here are deliberately different per call. Equal ones would let a
//! sum and a last-value pass the same assertion.

use base::interface::model::{Model, Usage};
use base::interface::tool::{
    InMemoryToolRegistry, PathStyle, ProgressSender, Tool, ToolContext, ToolResult,
};
use base::settings::{PathSettings, PermissionMode, Settings, ThinkingMode};
use runtime::agent::Builder;
use std::sync::Arc;
use test_runner::scripted_model::{Reply, ScriptedModel};
use tokio_util::sync::CancellationToken;

/// One turn, two model calls: the model asks for a tool and then answers.
const PER_CALL: &[(u32, u32)] = &[(100, 20), (7, 3)];

fn expected_total() -> Usage {
    Usage {
        input_tokens: PER_CALL.iter().map(|(i, _)| i).sum(),
        output_tokens: PER_CALL.iter().map(|(_, o)| o).sum(),
        ..Default::default()
    }
}

struct Ran {
    outcome: runtime::turn::TurnOutcome,
    reported: Vec<Usage>,
    telemetry: Vec<telemetry::TelemetryEvent>,
}

async fn one_turn_of_two_calls() -> Ran {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    let model_arc = ScriptedModel::new(vec![
        Reply::Tool {
            id: "call-1",
            name: "Echo",
            input: serde_json::json!({"say": "anything"}),
        },
        Reply::Text("done"),
    ])
    .reporting_usage(PER_CALL);
    let model: Arc<dyn Model> = model_arc.clone();

    let mut settings = base::settings::LoadedSettings::new(
        Settings::defaults_for("accounting-model"),
        base::settings::PathSettings::default(),
    );
    settings.values.model.model_name = "accounting-model".into();
    settings.values.model.max_tokens = 1024;
    settings.values.model.thinking_mode = ThinkingMode::Off;
    settings.values.memory_enabled = false;
    settings.values.permission_mode = PermissionMode::BypassPermissions;
    settings.paths = PathSettings {
        user_data_dir: root.join("user"),
        global_data_dir: root.join("global"),
        workspace: base::settings::Workspace::project_of(&root.join("local")),
        local_data_dir: root.join("local"),
        scope: "code".into(),
    };

    let registry = Arc::new(InMemoryToolRegistry::new());
    registry.register(Arc::new(Echo) as Arc<dyn Tool>);

    let (telemetry_tx, mut telemetry_rx) = tokio::sync::mpsc::channel(64);
    let (mut agent, mut event_rx, input_tx) = Builder::new()
        .scene(Arc::new(scene::harnix::spec()))
        .model(model)
        .tools(registry)
        .settings(Arc::new(settings))
        .telemetry_handle(telemetry::TelemetryHandle::new(telemetry_tx))
        .skip_warmup(true)
        .build()
        .expect("agent builds");

    // `run_turn` rather than the event loop: it is the entry the daemon uses
    // and the only one that hands back a `TurnOutcome`, which is half of what
    // this case is about.
    let outcome = agent
        .run_turn("use the tool".into(), "t0".into(), CancellationToken::new())
        .await
        .expect("the turn ran");

    let mut reported = Vec::new();
    while let Ok(ev) = event_rx.try_recv() {
        if let base::event::AgentEvent::TurnComplete { usage, .. } = ev {
            reported.push(usage);
        }
    }
    drop(input_tx);

    let mut telemetry = Vec::new();
    while let Ok(ev) = telemetry_rx.try_recv() {
        telemetry.push(ev);
    }

    Ran {
        outcome,
        reported,
        telemetry,
    }
}

/// The outcome reports the whole turn, not its last call.
#[tokio::test(flavor = "multi_thread")]
async fn a_turn_reports_what_all_of_its_calls_spent() {
    let ran = one_turn_of_two_calls().await;

    assert_eq!(
        ran.outcome.api_calls, 2,
        "the case needs a turn of more than one call to mean anything"
    );
    assert_eq!(
        ran.outcome.total_usage,
        expected_total(),
        "the outcome reported {:?}, which is the last call rather than the turn",
        ran.outcome.total_usage
    );
}

/// And so does the event, which is the only number a daemon client ever sees.
#[tokio::test(flavor = "multi_thread")]
async fn the_turn_complete_event_carries_the_same_total() {
    let ran = one_turn_of_two_calls().await;

    assert_eq!(
        ran.reported.len(),
        1,
        "one turn, one TurnComplete: {:?}",
        ran.reported
    );
    assert_eq!(
        ran.reported[0],
        expected_total(),
        "the event reported {:?}. A daemon forwards this verbatim, so a zero \
         here is a zero in every client.",
        ran.reported[0]
    );
}

/// Echoes its argument, so a turn has a reason to make a second call.
#[derive(Debug)]
struct Echo;

#[async_trait::async_trait]
impl Tool for Echo {
    fn name(&self) -> &str {
        "Echo"
    }
    fn spec(&self) -> base::tool::ToolSpec {
        base::tool::ToolSpec::new().read_only(true)
    }

    fn description(&self) -> &str {
        "Echo the `say` argument back."
    }
    fn input_schema(&self, _: PathStyle) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"say": {"type": "string"}},
            "required": ["say"]
        })
    }
    async fn permission_facts(
        &self,
        _: &serde_json::Value,
        _: &base::tool::ToolContext,
    ) -> base::interface::permission::PermissionFacts {
        use base::interface::permission::{PermissionFacts, Reach, Subject};
        PermissionFacts::new(Subject::None, Reach::SessionOnly)
    }
    async fn call(
        &self,
        input: serde_json::Value,
        _ctx: ToolContext,
        _p: ProgressSender,
    ) -> Result<ToolResult, base::error::ToolError> {
        let say = input.get("say").and_then(|v| v.as_str()).unwrap_or("");
        Ok(ToolResult::text(format!("echo: {say}")))
    }
}

/// Every call is accounted for on its own, which is what a cost is computed
/// from.
///
/// The payload existed, the OTLP exporter had a branch for it, and nothing in
/// the engine ever constructed one — so the per-call numbers a cost needs were
/// measured, used for the budget, and thrown away. A turn-level total cannot
/// replace them: it cannot say which model the spending went to when a turn
/// switches models partway through.
#[tokio::test(flavor = "multi_thread")]
async fn every_model_call_is_reported_on_its_own() {
    let ran = one_turn_of_two_calls().await;

    let calls: Vec<&telemetry::ApiRequestPayload> = ran
        .telemetry
        .iter()
        .filter_map(|e| match &e.payload {
            telemetry::EventPayload::ApiRequest(p) => Some(p),
            _ => None,
        })
        .collect();

    assert_eq!(
        calls.len(),
        2,
        "one event per model call, and this turn made two: {:?}",
        ran.telemetry.iter().map(|e| e.kind()).collect::<Vec<_>>()
    );
    assert_eq!(
        (calls[0].input_tokens, calls[0].output_tokens),
        (PER_CALL[0].0 as u64, PER_CALL[0].1 as u64),
        "the first event carries the first call's usage, not the turn's total"
    );
    assert_eq!(
        (calls[1].input_tokens, calls[1].output_tokens),
        (PER_CALL[1].0 as u64, PER_CALL[1].1 as u64),
    );
    assert!(
        calls
            .iter()
            .all(|c| c.model == "accounting-model" && c.default_model),
        "a call has to name the model it went to, or a cost cannot be priced"
    );
    assert_eq!(
        calls.iter().map(|c| c.input_tokens).sum::<u64>(),
        ran.outcome.total_usage.input_tokens as u64,
        "the per-call events and the turn total must not disagree"
    );
}
