//! A session's tool calls and the permission judging them see one set of
//! execution providers.

use super::*;
use base::interface::exec::{FileSystem, Sandbox};

#[derive(Default)]
struct Seen {
    judged: Vec<(Arc<dyn FileSystem>, Arc<dyn Sandbox>)>,
    ran: Vec<(Arc<dyn FileSystem>, Arc<dyn Sandbox>)>,
}

struct ExecProbe(Arc<std::sync::Mutex<Seen>>);

#[async_trait::async_trait]
impl base::tool::Tool for ExecProbe {
    fn name(&self) -> &str {
        "ExecProbe"
    }
    fn spec(&self) -> base::tool::ToolSpec {
        base::tool::ToolSpec::new()
    }

    fn description(&self) -> &str {
        "exec probe (test tool)"
    }

    fn input_schema(&self, _: base::tool::PathStyle) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn check_permissions(
        &self,
        _input: &serde_json::Value,
        ctx: &base::tool::ToolContext,
    ) -> base::tool::PermissionDecision {
        self.0
            .lock()
            .unwrap()
            .judged
            .push((ctx.exec.filesystem.clone(), ctx.exec.sandbox.clone()));
        base::tool::PermissionDecision::allow()
    }
    async fn call(
        &self,
        _input: serde_json::Value,
        ctx: base::tool::ToolContext,
        _progress: base::tool::ProgressSender,
    ) -> Result<base::tool::ToolResult, base::error::ToolError> {
        self.0
            .lock()
            .unwrap()
            .ran
            .push((ctx.exec.filesystem.clone(), ctx.exec.sandbox.clone()));
        Ok(base::tool::ToolResult::text("probed"))
    }
}

#[tokio::test]
async fn the_permission_judges_a_call_on_the_providers_it_runs_on() {
    let seen = Arc::new(std::sync::Mutex::new(Seen::default()));
    let tools = Arc::new(InMemoryToolRegistry::new());
    tools.register(Arc::new(ExecProbe(seen.clone())));
    let settings = test_settings();
    let permission = base::interface::permission::PermissionSet::of(Arc::new(
        permissions::policy::Policy::from_settings(&settings, []),
    ));
    let (mut agent, _event_rx, _input_tx) = Builder::new()
        .scene(Arc::new(scene::code::spec(
            scene::code::Interaction::Interactive,
        )))
        .model(Arc::new(ToolThenStopModel {
            calls: Default::default(),
            tool: "ExecProbe",
        }))
        .settings(Arc::new(settings))
        .tools(tools)
        .permission(permission)
        .skip_warmup(true)
        .build()
        .expect("build should succeed");

    agent
        .process_turn(
            InputMessage::User {
                content: "go".into(),
                attachments: vec![],
                turn_id: "t1".into(),
            },
            CancellationToken::new(),
        )
        .await
        .expect("the turn runs");

    let seen = seen.lock().unwrap();
    assert!(!seen.judged.is_empty(), "the permission consulted the tool");
    assert_eq!(seen.ran.len(), 1, "the call ran");
    for (filesystem, sandbox) in seen.judged.iter().chain(&seen.ran) {
        assert!(Arc::ptr_eq(filesystem, &agent.exec.filesystem));
        assert!(Arc::ptr_eq(sandbox, &agent.exec.sandbox));
    }
}

#[tokio::test]
async fn a_session_built_with_providers_runs_on_them() {
    let seen = Arc::new(std::sync::Mutex::new(Seen::default()));
    let tools = Arc::new(InMemoryToolRegistry::new());
    tools.register(Arc::new(ExecProbe(seen.clone())));
    let providers = base::interface::exec::ExecProviders::in_process();
    let (mut agent, _event_rx, _input_tx) = Builder::new()
        .scene(Arc::new(scene::code::spec(
            scene::code::Interaction::Interactive,
        )))
        .model(Arc::new(ToolThenStopModel {
            calls: Default::default(),
            tool: "ExecProbe",
        }))
        .settings(Arc::new(test_settings()))
        .tools(tools)
        .exec(providers.clone())
        .skip_warmup(true)
        .build()
        .expect("build should succeed");
    assert!(Arc::ptr_eq(&agent.exec.process, &providers.process));
    let bound = agent.session_state.exec().expect("the session's providers");
    assert!(Arc::ptr_eq(&bound.filesystem, &providers.filesystem));

    agent
        .process_turn(
            InputMessage::User {
                content: "go".into(),
                attachments: vec![],
                turn_id: "t1".into(),
            },
            CancellationToken::new(),
        )
        .await
        .expect("the turn runs");
    let seen = seen.lock().unwrap();
    assert_eq!(seen.ran.len(), 1, "the call ran");
    assert!(Arc::ptr_eq(&seen.ran[0].0, &providers.filesystem));
    assert!(Arc::ptr_eq(&seen.ran[0].1, &providers.sandbox));
}

/// Keeping either one silently would drop the host's providers or the
/// session's process cleanup.
#[test]
fn providers_and_a_process_ledger_are_not_given_together() {
    let built = Builder::new()
        .scene(Arc::new(scene::code::spec(
            scene::code::Interaction::Interactive,
        )))
        .model(Arc::new(ToolThenStopModel {
            calls: Default::default(),
            tool: "ExecProbe",
        }))
        .settings(Arc::new(test_settings()))
        .exec(base::interface::exec::ExecProviders::in_process())
        .process_groups(platform::GroupLedger::new())
        .skip_warmup(true)
        .build();
    let Err(e) = built else {
        panic!("both were accepted");
    };
    assert!(e.to_string().contains("Builder::process_groups"), "{e}");
}
