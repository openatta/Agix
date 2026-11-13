//! The answering half: what settles a question without a person, and what
//! refuses it for want of one.

use super::*;
use base::error::ToolError;
use base::interface::exec::{ConfinementReport, Enforcement, ExecProviders, NetworkMode};
use base::interface::permission::{
    Attendance, ExecProfile, HookAnswer, PermissionFacts, Reach, Runs, Subject, ToolFacts,
};
use base::tool::{Tool, ToolContext};
use serde_json::json;
use std::path::PathBuf;

struct Stated(PermissionFacts);

#[async_trait::async_trait]
impl Tool for Stated {
    fn name(&self) -> &str {
        "Stated"
    }
    fn description(&self) -> &str {
        "states its facts"
    }
    fn spec(&self) -> base::tool::ToolSpec {
        base::tool::ToolSpec::new()
    }
    fn input_schema(&self, _: base::tool::PathStyle) -> Value {
        json!({"type": "object"})
    }
    async fn permission_facts(&self, _: &Value, _: &ToolContext) -> PermissionFacts {
        self.0.clone()
    }
    async fn call(
        &self,
        _: Value,
        _: ToolContext,
        _: base::tool::ProgressSender,
    ) -> Result<base::tool::ToolResult, ToolError> {
        Ok(base::tool::ToolResult::text("ok"))
    }
}

fn holding() -> ConfinementReport {
    ConfinementReport::probe(
        &Holds,
        base::interface::exec::ProcessSpec::new("bash", "/tmp"),
        &base::interface::exec::SandboxPolicy {
            network_mode: NetworkMode::DenyAll,
            ..Default::default()
        },
        false,
        true,
    )
}

struct Holds;

impl base::interface::exec::Sandbox for Holds {
    fn confine(
        &self,
        spec: base::interface::exec::ProcessSpec,
        _: &base::interface::exec::SandboxPolicy,
    ) -> base::interface::exec::Confined {
        base::interface::exec::Confined {
            spec,
            mode: base::interface::exec::SandboxMode::LinuxBwrap,
            enforcement: Enforcement::Full,
            unmet: Vec::new(),
        }
    }
}

async fn outcome(
    facts: PermissionFacts,
    confinement: ConfinementReport,
    hook: HookAnswer,
    attendance: Attendance,
) -> PermissionOutcome {
    let tool: Arc<dyn Tool> = Arc::new(Stated(facts));
    let mut ctx = ToolContext::for_test_with(PathBuf::from("/tmp"), ExecProviders::in_process());
    ctx.confinement = confinement;
    let input = json!({});
    let facts = ToolFacts::gather(tool.as_ref(), &input, &ctx).await;
    Policy::empty()
        .decide(
            &PermissionRequest::new("Stated", Some(&tool), &input, &facts, &ctx, &ctx.session)
                .with_hook(&hook)
                .with_attendance(attendance),
        )
        .await
}

fn command(reach: Reach) -> PermissionFacts {
    PermissionFacts::new(Subject::Shell("make".into()), reach).with_runs(Runs::foreground())
}

#[tokio::test]
async fn an_unattended_command_the_sandbox_holds_runs_and_nothing_else_does() {
    let unattended =
        |facts, confinement| outcome(facts, confinement, HookAnswer::None, Attendance::Unattended);
    let watch = PermissionFacts::new(Subject::Shell("tail -f log".into()), Reach::Inside)
        .with_runs(Runs::background());
    for facts in [command(Reach::Unknown), watch] {
        let d = unattended(facts.clone(), holding()).await;
        assert!(
            matches!(
                d,
                PermissionOutcome::Permit {
                    basis: Basis::UnattendedConfined,
                    profile: ExecProfile::Unattended,
                    ..
                }
            ),
            "{facts:?}: {d:?}"
        );
    }
    for (facts, confinement) in [
        (command(Reach::Unknown), ConfinementReport::default()),
        (command(Reach::Host { listed: true }), holding()),
        (command(Reach::Outside), holding()),
        (
            PermissionFacts::new(Subject::Opaque("x".into()), Reach::Unknown),
            holding(),
        ),
        (
            PermissionFacts::new(
                Subject::Shell("touch .agix/settings.json".into()),
                Reach::Unknown,
            )
            .with_runs(Runs::foreground()),
            holding(),
        ),
    ] {
        let d = unattended(facts.clone(), confinement).await;
        assert!(
            matches!(&d, PermissionOutcome::Deny { reason } if reason.stage == Stage::Attendance),
            "{facts:?}: {d:?}"
        );
    }
    let attended = outcome(
        command(Reach::Unknown),
        holding(),
        HookAnswer::None,
        Attendance::Attended,
    )
    .await;
    assert!(
        matches!(attended, PermissionOutcome::Prompt { .. }),
        "{attended:?}"
    );
}

#[tokio::test]
async fn a_hooks_approve_answers_before_anyone_is_found_missing() {
    let approve = HookAnswer::Approve {
        hook: "audit".into(),
    };
    for attendance in [
        Attendance::Attended,
        Attendance::Unattended,
        Attendance::NoOneToAsk("a program's call"),
    ] {
        let d = outcome(
            command(Reach::Unknown),
            ConfinementReport::default(),
            approve.clone(),
            attendance,
        )
        .await;
        assert!(
            matches!(&d, PermissionOutcome::Permit { basis: Basis::HookApproved(h), .. } if h == "audit"),
            "{attendance:?}: {d:?}"
        );
    }
}

#[tokio::test]
async fn nobody_to_ask_refuses_with_why() {
    let d = outcome(
        command(Reach::Unknown),
        holding(),
        HookAnswer::None,
        Attendance::NoOneToAsk("a program cannot ask for it"),
    )
    .await;
    assert!(
        matches!(&d, PermissionOutcome::Deny { reason }
            if reason.stage == Stage::Attendance
                && reason.message.ends_with("a program cannot ask for it")),
        "{d:?}"
    );
}

#[tokio::test]
async fn a_hook_that_blocks_refuses_even_under_bypass() {
    let tool: Arc<dyn Tool> = Arc::new(Stated(command(Reach::Inside)));
    let ctx = ToolContext::for_test_with(PathBuf::from("/tmp"), ExecProviders::in_process());
    ctx.session
        .set_permission_mode(base::permission::PermissionMode::BypassPermissions);
    let block = HookAnswer::Block {
        hook: "guard".into(),
        reason: "not here".into(),
    };
    let input = json!({});
    let facts = ToolFacts::gather(tool.as_ref(), &input, &ctx).await;
    let d = Policy::empty()
        .decide(
            &PermissionRequest::new("Stated", Some(&tool), &input, &facts, &ctx, &ctx.session)
                .with_hook(&block),
        )
        .await;
    assert!(
        matches!(&d, PermissionOutcome::Deny { reason }
            if reason.stage == Stage::Hook && reason.message.contains("not here")),
        "{d:?}"
    );
}

#[tokio::test]
async fn what_a_command_let_through_on_its_own_word_runs_strictly() {
    let d = outcome(
        command(Reach::Inside),
        ConfinementReport::default(),
        HookAnswer::None,
        Attendance::Attended,
    )
    .await;
    assert!(
        matches!(
            d,
            PermissionOutcome::Permit {
                basis: Basis::Contained,
                profile: ExecProfile::Strict,
                obligations,
            } if !obligations.scan_nested_repos
        ),
        "{d:?}"
    );
}
