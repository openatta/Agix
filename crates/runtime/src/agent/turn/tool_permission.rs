//! Whether a tool call runs: the `PreToolUse` hook's answer, the permission
//! policy's conclusion on the call's facts, the question put to whoever can
//! answer it, and the conclusion recorded.

use super::process::hook_permission_mode;
use super::reporting::record_permission_decision;
use super::tool_exec::{fire_ctx_lifecycle_hook, ToolExecCtx};
use std::sync::Arc;

/// `PreToolUse`: the input the call goes on with, and what the hook answered
/// about it in advance. A block keeps the input as the model wrote it.
pub(super) async fn pre_tool_use(
    ctx: &ToolExecCtx,
    name: &str,
    input: serde_json::Value,
) -> (serde_json::Value, base::interface::permission::HookAnswer) {
    use base::interface::permission::HookAnswer;
    if !ctx.hooks.has_hooks_for(hooks::HookEvent::PreToolUse) {
        return (input, HookAnswer::None);
    }
    let pre_input = hooks::HookInput {
        hook_event_name: "PreToolUse".into(),
        session_id: ctx.session_id.clone(),
        cwd: ctx.cwd.display().to_string(),
        permission_mode: hook_permission_mode(ctx.session_state.permission_mode()),
        tool_name: Some(name.to_string()),
        tool_input: Some(input.clone()),
        tool_use_id: Some(ctx.tool_use_id.clone()),
        tool_result: None,
        is_error: None,
        user_prompt: None,
        ..Default::default()
    };
    let hook_result = ctx
        .hooks
        .run(hooks::HookEvent::PreToolUse, &pre_input, &ctx.cancel)
        .await;
    if ctx.cancel.is_cancelled() {
        let answer = HookAnswer::Block {
            hook: "PreToolUse".into(),
            reason: "the turn was cancelled before its hooks answered".into(),
        };
        return (input, answer);
    }
    if let Some(response) = hook_result.blocked() {
        let answer = HookAnswer::Block {
            hook: "PreToolUse".into(),
            reason: response
                .message
                .clone()
                .unwrap_or_else(|| "no reason given".into()),
        };
        return (input, answer);
    }
    let input = hook_result.updated_input().cloned().unwrap_or(input);
    let answer = match hook_result.approved() {
        Some(_) => HookAnswer::Approve {
            hook: "PreToolUse".into(),
        },
        None => HookAnswer::None,
    };
    (input, answer)
}

/// Why a call does not run: what the `PermissionDenied` hook is told, and
/// what the model is.
struct Refusal {
    reason: String,
    error: String,
}

impl Refusal {
    fn denied(reason: impl Into<String>) -> Self {
        let reason = reason.into();
        Self {
            error: format!("Denied by permission: {reason}"),
            reason,
        }
    }
}

/// How a call was settled: by the policy, or by whoever its question was put
/// to.
enum Answer {
    Decided(base::interface::permission::PermissionOutcome),
    Refused(base::interface::permission::Reason, Refusal),
    Cancelled { waited_ms: u64 },
}

/// A call let through: what it runs on, what is owed once it has run, what
/// it reaches, and whether a person approved it.
pub(super) struct Authorized {
    pub approval: base::interface::permission::Approval,
    pub obligations: base::interface::permission::Obligations,
    /// What the tool said about the call, gathered once.
    pub facts: base::interface::permission::ToolFacts,
    pub by_person: bool,
}

/// Decide the call once, put a question to whoever can answer it, and record
/// the conclusion.
pub(super) async fn authorize(
    ctx: &ToolExecCtx,
    name: &str,
    tool: &Arc<dyn base::tool::Tool>,
    input: &serde_json::Value,
    tool_ctx: &base::tool::ToolContext,
    hook: &base::interface::permission::HookAnswer,
) -> Result<Authorized, String> {
    use base::interface::permission::{
        Approval, Basis, PermissionOutcome, PermissionRequest, Stage, ToolFacts,
    };
    let started = std::time::Instant::now();
    let facts = ToolFacts::gather(tool.as_ref(), input, tool_ctx).await;
    let request = PermissionRequest::new(
        name,
        Some(tool),
        input,
        &facts,
        tool_ctx,
        &ctx.session_state,
    )
    .with_hook(hook);
    let answer = match ctx.permission.policy.decide(&request).await {
        PermissionOutcome::Prompt {
            prompt_id,
            message,
            paths,
            reason,
            ..
        } => {
            let asked = Asked {
                prompt_id,
                message,
                paths,
                reason,
            };
            ask(ctx, name, input, &facts, tool_ctx, asked).await
        }
        decided => Answer::Decided(decided),
    };
    let concluded = match &answer {
        Answer::Decided(outcome) => outcome.clone(),
        Answer::Refused(reason, _) => PermissionOutcome::Deny {
            reason: reason.clone(),
        },
        Answer::Cancelled { .. } => {
            PermissionOutcome::deny(Stage::Attendance, "the session closed while asking")
        }
    };
    record_permission_decision(ctx, tool, name, input, &concluded, started);
    let refusal = match answer {
        Answer::Decided(PermissionOutcome::Permit {
            basis,
            profile,
            obligations,
        }) => {
            return Ok(Authorized {
                by_person: basis == Basis::Person,
                approval: Approval::new(basis, profile),
                obligations,
                facts,
            })
        }
        Answer::Decided(PermissionOutcome::Deny { reason }) if reason.stage == Stage::Hook => {
            Refusal {
                error: reason.message.clone(),
                reason: reason.message,
            }
        }
        Answer::Decided(PermissionOutcome::Deny { reason }) => Refusal::denied(reason.message),
        Answer::Decided(PermissionOutcome::Prompt { message, .. }) => Refusal::denied(message),
        Answer::Refused(_, refusal) => refusal,
        Answer::Cancelled { waited_ms } => {
            let _ = ctx
                .telemetry_handle
                .record(telemetry::TelemetryEvent::tool_cancelled(
                    &ctx.session_id,
                    ctx.turn_no,
                    Some(ctx.turn_id.clone()),
                    telemetry::ToolCancelledPayload {
                        tool_name: name.to_string(),
                        reason: "session_closing_during_permission_wait".into(),
                        elapsed_ms: waited_ms,
                    },
                ));
            return Err("permission request cancelled: session closing".into());
        }
    };
    fire_ctx_lifecycle_hook(ctx, hooks::HookEvent::PermissionDenied, |i| {
        i.with_tool(name, input.clone())
            .with_reason(refusal.reason.clone())
    })
    .await;
    Err(refusal.error)
}

/// The question the policy left open.
struct Asked {
    prompt_id: String,
    message: String,
    paths: Vec<std::path::PathBuf>,
    reason: base::interface::permission::Reason,
}

/// Put the policy's question to the host and wait for it. Only the session
/// going away interrupts the wait; an unanswered prompt within the deadline is
/// not consent.
async fn ask(
    ctx: &ToolExecCtx,
    name: &str,
    input: &serde_json::Value,
    facts: &base::interface::permission::ToolFacts,
    tool_ctx: &base::tool::ToolContext,
    asked: Asked,
) -> Answer {
    use base::interface::permission::{Basis, ExecProfile, Obligations, PermissionOutcome, Reason};
    let Asked {
        prompt_id,
        message,
        paths,
        reason: asked,
    } = asked;
    let refused =
        |refusal: Refusal| Answer::Refused(Reason::new(asked.stage, &refusal.reason), refusal);
    let waited = std::time::Instant::now();
    fire_ctx_lifecycle_hook(ctx, hooks::HookEvent::PermissionRequested, |i| {
        i.with_tool(name, input.clone())
            .with_reason(message.clone())
    })
    .await;
    // `Notification` rings when a person's attention is needed.
    fire_ctx_lifecycle_hook(ctx, hooks::HookEvent::Notification, |i| {
        i.with_tool(name, input.clone())
            .with_reason(format!("waiting for a permission decision on `{name}`"))
    })
    .await;
    let prompt_timeout = ctx.config.permission_prompt_timeout_secs;
    let ask = ctx.elicitation.ask_within(
        base::interface::elicitation::ElicitRequest {
            id: prompt_id.clone(),
            kind: base::interface::elicitation::ElicitKind::Authorization {
                tool_name: name.to_string(),
                paths,
                tool_use_id: Some(ctx.tool_use_id.clone()),
            },
            message,
            options: crate::elicitation::ChannelElicitation::authorization_options(
                ctx.permission.grants.is_some(),
            ),
        },
        (prompt_timeout > 0).then(|| std::time::Duration::from_secs(prompt_timeout)),
    );
    // An answer that already took the prompt is the answer, cancelled or not:
    // the host was told it was answered, and an "always allow" is kept.
    let outcome = tokio::select! {
        biased;
        outcome = ask => match outcome {
            Some(outcome) => outcome,
            None => {
                return refused(Refusal {
                    reason: "permission prompt timed out".into(),
                    error: format!(
                        "Denied by permission: no answer to the permission prompt within \
                         {prompt_timeout}s"
                    ),
                });
            }
        },
        // Dropping the ask is what discards a late answer for this id — see
        // `ChannelElicitation`'s registration guard.
        _ = ctx.cancel.cancelled() => {
            return Answer::Cancelled { waited_ms: waited.elapsed().as_millis() as u64 };
        }
    };
    let profile = ExecProfile::for_permit(&Basis::Person, facts.runs, tool_ctx.attendance);
    let permitted = Answer::Decided(PermissionOutcome::Permit {
        basis: Basis::Person,
        profile,
        obligations: Obligations::after(facts.runs, profile),
    });
    match outcome.answer_as::<crate::agent::PermissionDecision>() {
        Some(crate::agent::PermissionDecision::Permit) => permitted,
        Some(crate::agent::PermissionDecision::PermitAlways { scope }) => {
            grant_always(ctx, name, facts.subject.content(), scope).await;
            permitted
        }
        Some(crate::agent::PermissionDecision::Deny { reason }) => refused(Refusal::denied(reason)),
        // Declined, or answered with something that is not a decision: the
        // decliner's own words are what the model is told.
        None => {
            let reason = outcome
                .decline_reason()
                .unwrap_or("the answer to the permission prompt was not a decision")
                .to_string();
            refused(Refusal {
                error: reason.clone(),
                reason,
            })
        }
    }
}

/// Keep a person's "always allow", for exactly what they were asked about.
async fn grant_always(
    ctx: &ToolExecCtx,
    name: &str,
    content: Option<&str>,
    scope: crate::agent::PersistScope,
) {
    use base::interface::permission::{Grant, GrantScope};
    let Some(grants) = &ctx.permission.grants else {
        tracing::warn!(
            tool_name = %name,
            "answered permit_always, which this session did not offer: it keeps no grants, so \
             the call is allowed once"
        );
        return;
    };
    let scope = match scope {
        crate::agent::PersistScope::Session => GrantScope::Session,
        crate::agent::PersistScope::Local => GrantScope::Project,
    };
    let grant = Grant::new(name, content.map(str::to_string), scope);
    if let Err(e) = grants.grant(grant).await {
        tracing::warn!(tool_name = %name, error = %e, "permit-always not kept as asked");
    }
}
