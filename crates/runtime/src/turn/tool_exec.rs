//! 一次工具调用的执行：权限、钩子、中间件、结果变换与遥测。

use super::process::hook_permission_mode;
use super::reporting::{record_permission_decision, telemetry_error_text};
use super::tool_env::tool_context_for;
use super::tool_progress::{CloseOnDrop, GateHandle, ProgressGate};
use super::tool_result::{render_tool_result_content, PendingToolImage};
use base::event::AgentEvent;
use std::sync::Arc;

/// Context bundle for tool execution — owned to survive async closures.
#[derive(Clone)]
pub(crate) struct ToolExecCtx {
    pub tool_use_id: String,
    pub tools: Arc<dyn base::tool::ToolRegistry>,
    pub cwd: std::path::PathBuf,
    /// Where a `PermitAlways { scope: Local }` answer persists its rule;
    /// `None` keeps it to this session. Carried from `settings.paths` rather
    /// than derived from `cwd` here — see `PathSettings::local_settings_file`
    /// for why the exact location matters.
    pub local_settings_path: Option<std::path::PathBuf>,
    pub session_id: String,
    pub turn_no: u32,
    /// Forwarded into every `ToolContext` this turn builds — see
    /// `Agent::agent_depth`.
    pub agent_depth: u32,
    pub telemetry_handle: telemetry::TelemetryHandle,
    pub turn_id: String,
    pub cancel: tokio_util::sync::CancellationToken,
    pub hooks: Arc<hooks::HookRunner>,
    /// Set by a `PostToolUse` hook that returns `continue: false` — checked
    /// by `process_turn` after `execute_stream` returns to end the turn
    /// early, same outcome as the `Stop` hook's discontinue path. Shared
    /// (not per-call) so any tool in this round's batch can request it.
    pub discontinued: Arc<std::sync::atomic::AtomicBool>,
    /// Real `EngineConfig` derived from `Settings` — see `Agent.config`'s doc
    /// comment. Cloned into `ToolContext.config` instead of the
    /// `defaults_for("unknown")` placeholder.
    pub config: Arc<base::context::EngineConfig>,
    /// The session's providers — see `Agent.exec`.
    pub exec: base::interface::exec::ExecProviders,
    /// Real permission checker — see `Agent.permission`. Consulted once per
    /// tool call in `execute_tool_inner`, after the `PreToolUse` hook step.
    pub permission: Arc<dyn base::interface::permission::Permission>,
    /// The session's one shared `SessionState` — see `Agent.session_state`.
    /// Cloned into every `ToolContext` so state a tool records (read cache,
    /// file snapshots, todos, plan mode) is visible to the next tool call
    /// instead of being dropped with a per-call throwaway.
    pub session_state: Arc<base::context::SessionState>,
    /// How this session asks a person to authorize a call. The turn decides
    /// what to do with the answer — hooks, telemetry, the deadline — and this
    /// decides how the question reaches a human at all.
    pub elicitation: Arc<dyn base::interface::elicitation::Elicitation>,
    /// Rings around every tool call — see
    /// [`base::interface::tool_middleware`]. Empty in every session that
    /// registered none, which is the case dispatch short-circuits.
    pub tool_middleware: Arc<Vec<Arc<dyn base::interface::tool_middleware::ToolMiddleware>>>,
    /// The last hands on a tool's output — see
    /// [`base::interface::tool_result`].
    pub result_transformers: Arc<Vec<Arc<dyn base::interface::tool_result::ToolResultTransformer>>>,
    /// Images returned by tools this round, collected out-of-band.
    ///
    /// The tool-dispatch closure returns `(String, Option<Vec<Value>>)` — text
    /// and deferred `new_messages` — and `streaming::buffer_result` turns that
    /// String into the `ToolResult` block. There is no room in that channel for
    /// binary content, and widening it would mean changing the executor
    /// signature and every call site in `streaming.rs`. Instead each call
    /// deposits its images here (shared across the round's whole batch), and
    /// `attach_tool_result_images` appends them to the tool-result message once
    /// `execute_stream` returns. Order within the message follows tool
    /// completion order, which is fine — images trail all `tool_result` blocks
    /// either way, and the text of each result names its own image.
    pub images: Arc<std::sync::Mutex<Vec<PendingToolImage>>>,
    /// Set on the calls a `RunCode` program makes — see
    /// `ToolContext::from_code`.
    pub from_code: bool,
    /// The session's event stream, for what dispatch reports on a tool's
    /// behalf (a `PushNotification`). `None` where nobody is listening.
    pub events: Option<crate::agent::EventSender>,
}

/// Deliver a host's answer to the `execute_tool_inner` call awaiting it.
///
/// A missing entry means the prompt already resolved (timed out, or the
/// session was cancelled) or this is a stale/duplicate answer — either way
/// there is nothing left to wake, so it is dropped rather than treated as an
/// error.
pub(crate) fn resolve_permission_response(
    pending: &Arc<
        std::sync::Mutex<
            std::collections::HashMap<
                String,
                tokio::sync::oneshot::Sender<crate::agent::PermissionDecision>,
            >,
        >,
    >,
    denial_count: &Arc<std::sync::atomic::AtomicU32>,
    prompt_id: String,
    decision: crate::agent::PermissionDecision,
) {
    if let crate::agent::PermissionDecision::Deny { .. } = decision {
        denial_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    if let Some(sender) = pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&prompt_id)
    {
        let _ = sender.send(decision);
    }
}

/// `Agent::fire_lifecycle_hook`'s counterpart for the free functions in the
/// tool-dispatch path, which hold a `ToolExecCtx` rather than `&self`.
///
/// Same contract: notification only, response discarded, guarded on
/// `has_hooks_for` because this sits on the per-tool-call hot path.
pub(super) async fn fire_ctx_lifecycle_hook(
    ctx: &ToolExecCtx,
    event: hooks::HookEvent,
    build: impl FnOnce(hooks::HookInput) -> hooks::HookInput,
) {
    if !ctx.hooks.has_hooks_for(event) {
        return;
    }
    let input = build(
        hooks::HookInput::lifecycle(
            format!("{event:?}"),
            ctx.session_id.clone(),
            ctx.cwd.display().to_string(),
            hook_permission_mode(ctx.session_state.permission_mode()),
        )
        .with_turn_id(ctx.turn_id.clone()),
    );
    let _ = ctx.hooks.run(event, &input).await;
}

/// A `PushNotification` reaches the host as a `notification` frame and the
/// `Notification` hook, which is the same hook a pending permission prompt
/// rings.
pub(super) async fn notify_host(ctx: &ToolExecCtx, input: &serde_json::Value) {
    let Some(message) = input.get("message").and_then(|m| m.as_str()).map(str::trim) else {
        return;
    };
    let status = input
        .get("status")
        .and_then(|s| s.as_str())
        .map(str::to_string);
    if let Some(events) = &ctx.events {
        let _ = events.send(AgentEvent::Notification {
            message: message.to_string(),
            status,
            turn_id: ctx.turn_id.clone(),
        });
    }
    fire_ctx_lifecycle_hook(ctx, hooks::HookEvent::Notification, |i| {
        i.with_tool("PushNotification", input.clone())
            .with_reason(message)
    })
    .await;
}

/// Execute a single tool and record telemetry. Free function for streaming executor.
pub(crate) async fn execute_tool_with_telemetry(
    ctx: &ToolExecCtx,
    name: &str,
    input: serde_json::Value,
) -> base::interface::tool_middleware::ToolOutcome {
    // No wrappers means no wrapping: the call goes straight through, with no
    // context clone and no future inside a future. A seam nobody uses should
    // cost nothing on the hot path.
    if ctx.tool_middleware.is_empty() {
        return execute_tool_inner(ctx, name, input).await;
    }
    let chain = Arc::clone(&ctx.tool_middleware);
    let call = base::interface::tool_middleware::ToolCall {
        name: name.to_string(),
        input: input.clone(),
    };
    base::interface::tool_middleware::dispatch_through(
        &chain,
        call,
        ctx.cancel.clone(),
        move |cancel| {
            // Each pass through gets the signal the wrappers settled on. The
            // rest of the context is the same one dispatch always had, which
            // is what keeps a retry a retry rather than a differently
            // configured call.
            let mut ctx = ctx.clone();
            ctx.cancel = cancel;
            let name = name.to_string();
            let input = input.clone();
            async move { execute_tool_inner(&ctx, &name, input).await }
        },
    )
    .await
}

/// Under `panic = "unwind"` a tool that panics fails its own call instead of
/// unwinding through the round; under `panic = "abort"` (the `release`
/// profile) the process ends before this ever runs.
pub(super) async fn run_catching_panics(
    call: impl std::future::Future<Output = Result<base::tool::ToolResult, base::error::ToolError>>,
) -> Result<base::tool::ToolResult, base::error::ToolError> {
    use futures::FutureExt as _;
    match std::panic::AssertUnwindSafe(call).catch_unwind().await {
        Ok(result) => result,
        Err(payload) => Err(base::error::ToolError::exec(format!(
            "tool panicked: {}",
            panic_message(payload.as_ref())
        ))),
    }
}

pub(super) fn panic_message(payload: &(dyn std::any::Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("(non-string panic payload)")
}

pub(super) async fn execute_tool_inner(
    ctx: &ToolExecCtx,
    name: &str,
    input: serde_json::Value,
) -> base::interface::tool_middleware::ToolOutcome {
    let tool = ctx
        .tools
        .get(name)
        .ok_or_else(|| format!("Tool not found: {name}"))?;

    // PreToolUse: can block the call outright, rewrite its input, or answer
    // the permission question in advance.
    let mut input = input;
    let mut hook_approved = false;
    if ctx.hooks.has_hooks_for(hooks::HookEvent::PreToolUse) {
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
            .run(hooks::HookEvent::PreToolUse, &pre_input)
            .await;
        if let Some(response) = hook_result.blocked() {
            return Err(format!(
                "Blocked by PreToolUse hook: {}",
                response.message.as_deref().unwrap_or("no reason given")
            ));
        }
        if let Some(updated) = hook_result.updated_input() {
            input = updated.clone();
        }
        hook_approved = hook_result.approved().is_some();
    }

    // Permission check. `Permit` proceeds unchanged; `Deny` blocks the call outright; `Prompt`
    // asks the host and genuinely blocks until it answers — matching
    // conventional confirmation-prompt UX (a CLI `y/n`, `sudo`, ...), not a
    // race against an arbitrary timeout. The one thing that can still
    // interrupt the wait is the *session* going away: `ctx.cancel` is the
    // same token `daemon::SessionPool::shutdown_session` fires on
    // `session.close`/client-disconnect, threaded down through
    // `Agent::run(cancel)` → `process_turn` → `run_user_turn`. If nobody explicitly approved the call before the
    // session tore down, the safe default is to *not* run it.
    // Whatever permission handler the host installed, a call whose input the
    // tool rejects never reaches it.
    if let base::tool::ValidationResult::Err(message, _) = tool
        .validate_input(&input, &tool_context_for(ctx, &tool))
        .await
    {
        return Err(message);
    }

    use base::interface::permission::PermissionOutcome;
    let check_started = std::time::Instant::now();
    let checked = ctx
        .permission
        .check(name, &input, &ctx.cwd, &ctx.session_id)
        .await;
    record_permission_decision(ctx, &tool, name, &input, &checked, check_started);
    // Before the unattended refusal below: a hook's approve is an answer given
    // in advance, so nobody needs to be attending for it to count.
    let checked = match checked {
        PermissionOutcome::Prompt { managed: false, .. } if hook_approved => {
            PermissionOutcome::Permit
        }
        other => other,
    };
    let user_approved = matches!(checked, PermissionOutcome::Prompt { .. });
    match checked {
        PermissionOutcome::Permit => {}
        PermissionOutcome::Deny { reason } => {
            fire_ctx_lifecycle_hook(ctx, hooks::HookEvent::PermissionDenied, |i| {
                i.with_tool(name, input.clone()).with_reason(reason.clone())
            })
            .await;
            return Err(format!("Denied by permission: {reason}"));
        }
        PermissionOutcome::Prompt { message, .. } if ctx.session_state.unattended => {
            let reason = format!(
                "{message} — and nobody is attending this session to approve it. Work within \
                 what is allowed; if the task needs this, report it: the instance's settings \
                 grant it (`sandbox.additional_directories`, `sandbox.read_directories`, or a \
                 `permission_rules` allow rule)."
            );
            fire_ctx_lifecycle_hook(ctx, hooks::HookEvent::PermissionDenied, |i| {
                i.with_tool(name, input.clone()).with_reason(reason.clone())
            })
            .await;
            return Err(format!("Denied by permission: {reason}"));
        }
        PermissionOutcome::Prompt {
            prompt_id,
            message,
            paths,
            ..
        } => {
            let permission_wait_start = std::time::Instant::now();
            fire_ctx_lifecycle_hook(ctx, hooks::HookEvent::PermissionRequested, |i| {
                i.with_tool(name, input.clone())
                    .with_reason(message.clone())
            })
            .await;
            // `Notification` is "fired when user attention is required
            // (typically a permission prompt is pending)". A host that wants to
            // ring a bell, raise a desktop notification or ping a channel
            // when the agent is blocked on a human hooks it here.
            fire_ctx_lifecycle_hook(ctx, hooks::HookEvent::Notification, |i| {
                i.with_tool(name, input.clone())
                    .with_reason(format!("waiting for a permission decision on `{name}`"))
            })
            .await;
            let ask = ctx
                .elicitation
                .ask(base::interface::elicitation::ElicitRequest {
                    id: prompt_id.clone(),
                    kind: base::interface::elicitation::ElicitKind::Authorization {
                        tool_name: name.to_string(),
                        paths,
                        tool_use_id: Some(ctx.tool_use_id.clone()),
                    },
                    message,
                    options: crate::elicitation::ChannelElicitation::authorization_options(),
                });
            tokio::pin!(ask);
            // `0` means wait indefinitely; `tokio::select!` needs a future
            // either way, so an unbounded wait becomes one that never
            // resolves rather than a branch that isn't there.
            let prompt_timeout = ctx.config.permission_prompt_timeout_secs;
            let timeout_fut = async move {
                if prompt_timeout == 0 {
                    std::future::pending::<()>().await
                } else {
                    tokio::time::sleep(std::time::Duration::from_secs(prompt_timeout)).await
                }
            };
            tokio::pin!(timeout_fut);
            tokio::select! {
                _ = &mut timeout_fut => {
                    // Fail closed. An unanswered prompt is not consent, and a
                    // host that never answers must not be able to hold a tool
                    // call open forever. Dropping `ask` here is what discards
                    // a late answer for this id — see `ChannelElicitation`'s
                    // registration guard.
                    fire_ctx_lifecycle_hook(ctx, hooks::HookEvent::PermissionDenied, |i| {
                        i.with_tool(name, input.clone())
                            .with_reason("permission prompt timed out")
                    })
                    .await;
                    return Err(format!(
                        "Denied by permission: no answer to the permission prompt within {prompt_timeout}s"
                    ));
                }
                outcome = &mut ask => {
                    match outcome.answer_as::<crate::agent::PermissionDecision>() {
                        Some(crate::agent::PermissionDecision::Permit) => {}
                        Some(crate::agent::PermissionDecision::PermitAlways { scope }) => {
                            // Same content derivation the rule engine itself
                            // uses to match a tool call against a rule (see
                            // `PermissionGate::check` step 3 — `RuleHit`
                            // evaluation) — this is what makes the persisted
                            // pattern actually match future calls the same
                            // way the one the user just answered did.
                            let match_content = tool.permission_match_content(&input);
                            ctx.permission
                                .add_persistent_allow(name, match_content.as_deref());
                            if let (crate::agent::PersistScope::Local, Some(local_settings_path)) =
                                (scope, &ctx.local_settings_path)
                            {
                                match permissions::settings_patch::build_rule_string(
                                    name,
                                    match_content.as_deref(),
                                ) {
                                    Some(rule_string) => {
                                        if let Err(e) = permissions::settings_patch::append_permission_rule(
                                            local_settings_path,
                                            permissions::settings_patch::AppendTarget::Allow,
                                            &rule_string,
                                        ) {
                                            tracing::warn!(
                                                path = %local_settings_path.display(),
                                                error = %e,
                                                "failed to persist permit-always rule to settings.local.json; \
                                                 in-memory allow for this session still applies"
                                            );
                                        }
                                    }
                                    None => {
                                        tracing::warn!(
                                            tool_name = %name,
                                            "permit-always with scope=local but no rule content could be \
                                             derived — nothing written to settings.local.json; \
                                             in-memory allow for this session still applies"
                                        );
                                    }
                                }
                            }
                        }
                        Some(crate::agent::PermissionDecision::Deny { reason }) => {
                            // Denied by the *host's* answer to the prompt, as
                            // opposed to the rule-engine `Deny` above. Same
                            // event either way — a hook watching for denials
                            // cares that the call was refused, not by whom.
                            fire_ctx_lifecycle_hook(
                                ctx,
                                hooks::HookEvent::PermissionDenied,
                                |i| i.with_tool(name, input.clone()).with_reason(reason.clone()),
                            )
                            .await;
                            return Err(format!("Denied by permission: {reason}"));
                        }
                        // Declined, or answered with something that is not a
                        // decision. Either way nobody authorized this call, and
                        // the reason is the decliner's own words — a host that
                        // cannot ask says so, and the model is told what it was
                        // told rather than a phrase invented here.
                        None => {
                            let reason = outcome
                                .decline_reason()
                                .unwrap_or("the answer to the permission prompt was not a decision")
                                .to_string();
                            fire_ctx_lifecycle_hook(
                                ctx,
                                hooks::HookEvent::PermissionDenied,
                                |i| i.with_tool(name, input.clone()).with_reason(reason.clone()),
                            )
                            .await;
                            return Err(reason);
                        }
                    }
                }
                _ = ctx.cancel.cancelled() => {
                    let _ = ctx.telemetry_handle.record(telemetry::TelemetryEvent::tool_cancelled(
                        &ctx.session_id,
                        ctx.turn_no,
                        Some(ctx.turn_id.clone()),
                        telemetry::ToolCancelledPayload {
                            tool_name: name.to_string(),
                            reason: "session_closing_during_permission_wait".into(),
                            elapsed_ms: permission_wait_start.elapsed().as_millis() as u64,
                        },
                    ));
                    return Err("permission request cancelled: session closing".into());
                }
            }
        }
    }

    let tool_ctx = tool_context_for(ctx, &tool);
    let input_for_post_hook = input.clone();
    let input_json_size = serde_json::to_string(&input)
        .map(|s| s.len() as u64)
        .unwrap_or(0);
    let _ = ctx
        .telemetry_handle
        .record(telemetry::TelemetryEvent::tool_start(
            &ctx.session_id,
            ctx.turn_no,
            Some(ctx.turn_id.clone()),
            telemetry::ToolStartPayload {
                tool_name: name.to_string(),
                tool_use_id: ctx.tool_use_id.clone(),
                input_json_size,
                destructive: tool.call_traits(&input).destructive,
                deferred: tool.is_deferred(),
            },
        ));
    let tool_start = std::time::Instant::now();
    let (progress, gate) = match &ctx.events {
        Some(events) => {
            let gate = ProgressGate::new(&ctx.tool_use_id, name, &ctx.turn_id, events.clone());
            let sender = base::tool::ProgressSender::with_callback(
                ctx.tool_use_id.clone(),
                Arc::new(GateHandle(gate.clone())),
            );
            (sender, Some(CloseOnDrop(gate)))
        }
        None => (
            base::tool::ProgressSender::noop(ctx.tool_use_id.clone()),
            None,
        ),
    };
    let result = run_catching_panics(tool.call(input, tool_ctx, progress)).await;
    if let Some(CloseOnDrop(gate)) = &gate {
        gate.finish();
    }
    drop(gate);
    let latency_ms = tool_start.elapsed().as_millis() as u64;

    let mut outcome = match result {
        Ok(r) => {
            let (text, images) = render_tool_result_content(r.content);
            if !images.is_empty() {
                ctx.images
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .extend(images);
            }
            Ok(base::interface::tool_middleware::ToolAnswer {
                text,
                new_messages: r.new_messages,
                reported_failure: r.is_error,
                structured: r.structured_content,
            })
        }
        Err(e) => Err(e.to_string()),
    };
    // Both ways a call can go wrong: the engine could not run it, or it ran
    // and called its own result a failure. Reading only `is_err` reported a
    // shell command that exited non-zero as a success.
    let (is_error, error_message, result_content_size) = match &outcome {
        Ok(answer) if answer.reported_failure => (
            true,
            Some(telemetry_error_text(&answer.text)),
            answer.text.len() as u64,
        ),
        Ok(answer) => (false, None, answer.text.len() as u64),
        Err(message) => (true, Some(telemetry_error_text(message)), 0),
    };
    let _ = ctx
        .telemetry_handle
        .record(telemetry::TelemetryEvent::tool_execution(
            &ctx.session_id,
            ctx.turn_no,
            Some(ctx.turn_id.clone()),
            telemetry::ToolExecutionPayload {
                tool_name: name.to_string(),
                tool_use_id: ctx.tool_use_id.clone(),
                outcome: if is_error {
                    telemetry::ToolOutcome::Failed
                } else {
                    telemetry::ToolOutcome::Succeeded
                },
                is_error,
                error_message,
                latency_ms,
                input_json_size,
                result_content_size,
                user_approved,
            },
        ));

    // PostToolUse: the tool already ran, but this can still act two ways.
    //
    // 1. `continue: false` requests the whole turn end. That's a turn-level
    //    decision this function can't act on directly (it only reports this
    //    one call's outcome) — surfaced via `ctx.discontinued`, a shared
    //    flag `process_turn` checks once `execute_stream` returns.
    //
    // 2. `decision: "block"` (the same field `PreToolUse` uses, and the same
    //    `HookRunResult::blocked()` accessor — `blocked()` isn't tied to a
    //    specific event, it just checks for that decision) denies *this*
    //    call's result: `outcome` gets overwritten with an `Err`, which
    //    becomes this tool_use's tool_result content (an error/denial
    //    message instead of whatever the tool actually returned). Turning it
    //    into an `Err` also — for free, not via new plumbing — triggers
    //    `execute_stream`'s "any tool error cancels all siblings in the
    //    same concurrent batch" mechanism (`streaming.rs`'s `batch_abort`),
    //    so in-flight/not-yet-started sibling calls in this batch get
    //    cancelled too.
    //
    // PostToolUseFailure — the error-only sibling of PostToolUse, so a hook
    // that only cares about failures doesn't have to subscribe to every tool
    // call and filter on `is_error`. Fires before PostToolUse, which still
    // sees the failure too (with `is_error: true`) and can still rewrite it.
    if let Err(ref e) = outcome {
        let err = e.clone();
        let inp = input_for_post_hook.clone();
        fire_ctx_lifecycle_hook(ctx, hooks::HookEvent::PostToolUseFailure, |i| {
            i.with_tool(name, inp).with_reason(err)
        })
        .await;
    }

    if ctx.hooks.has_hooks_for(hooks::HookEvent::PostToolUse) {
        let (result_json, is_error_flag) = match &outcome {
            Ok(a) => (
                serde_json::Value::String(a.text.clone()),
                a.reported_failure,
            ),
            Err(e) => (serde_json::Value::String(e.clone()), true),
        };
        let post_input = hooks::HookInput {
            hook_event_name: "PostToolUse".into(),
            session_id: ctx.session_id.clone(),
            cwd: ctx.cwd.display().to_string(),
            permission_mode: hook_permission_mode(ctx.session_state.permission_mode()),
            tool_name: Some(name.to_string()),
            tool_input: Some(input_for_post_hook.clone()),
            tool_use_id: Some(ctx.tool_use_id.clone()),
            tool_result: Some(result_json),
            is_error: Some(is_error_flag),
            user_prompt: None,
            ..Default::default()
        };
        let hook_result = ctx
            .hooks
            .run(hooks::HookEvent::PostToolUse, &post_input)
            .await;
        if let Some(response) = hook_result.blocked() {
            outcome = Err(format!(
                "Denied by PostToolUse hook: {}",
                response.message.as_deref().unwrap_or("no reason given")
            ));
        }
        if hook_result.discontinued() {
            ctx.discontinued
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    // Tool-shaped lifecycle events. `WorktreeCreate` and `WorktreeRemove`
    // describe things a *tool* does, but tools have no access to the `HookRunner` — `ToolContext` deliberately
    // carries no hooks handle, and threading one through every tool for this
    // would be a large change for a small feature. Dispatch is the natural
    // single site: it knows the tool name, the input, and whether the call
    // succeeded, which is exactly what these events carry.
    //
    // Fired only on success — a `WorktreeCreate` hook should not run for a
    // worktree that was never created.
    if outcome.is_ok() {
        let lifecycle_event = match name {
            "EnterWorktree" => Some(hooks::HookEvent::WorktreeCreate),
            "ExitWorktree" => Some(hooks::HookEvent::WorktreeRemove),
            _ => None,
        };
        if let Some(event) = lifecycle_event {
            // `input_for_post_hook` is the pre-dispatch clone the PostToolUse
            // block above already keeps for exactly this reason — `input`
            // itself was moved into `Tool::call`.
            let inp = input_for_post_hook.clone();
            fire_ctx_lifecycle_hook(ctx, event, |i| i.with_tool(name, inp)).await;
        }
        if name == "PushNotification" && !outcome.as_ref().is_ok_and(|a| a.reported_failure) {
            notify_host(ctx, &input_for_post_hook).await;
        }
    }

    apply_result_transformers(ctx, name, &input_for_post_hook, outcome)
}

/// Run the registered transformers over this call's outcome.
///
/// Deliberately the last thing that touches it — after every hook, after the
/// lifecycle events, immediately before the caller gets it. That ordering is
/// what makes a redacting transformer a guarantee: nothing downstream can put
/// back what it removed.
///
/// The images a tool returned were deposited in `ctx.images` on the way
/// through, so a transformer that drops one has to reach in there rather than
/// hand back a value. They are pulled out, offered, and put back — the
/// side-channel exists because the dispatch signature has no room for binary
/// content, not because images are supposed to be untouchable.
pub(super) fn apply_result_transformers(
    ctx: &ToolExecCtx,
    name: &str,
    input: &serde_json::Value,
    outcome: base::interface::tool_middleware::ToolOutcome,
) -> base::interface::tool_middleware::ToolOutcome {
    if ctx.result_transformers.is_empty() {
        return outcome;
    }
    let call = base::interface::tool_middleware::ToolCall {
        name: name.to_string(),
        input: input.clone(),
    };
    // A transformer that turns a plain answer into a failure gets the
    // engine-level treatment that has always meant: the call is an `Err`,
    // which cancels its concurrent siblings. One handed a result the tool
    // itself already called a failure hands back the same shape, because
    // nothing about it changed.
    let was_reported_failure = matches!(&outcome, Ok(a) if a.reported_failure);
    let (text, is_error, new_messages, structured) = match outcome {
        Ok(a) => (a.text, a.reported_failure, a.new_messages, a.structured),
        Err(e) => (e, true, None, None),
    };
    let original = (text.clone(), is_error);
    let images = std::mem::take(&mut *ctx.images.lock().unwrap_or_else(|e| e.into_inner()));
    let mut draft = base::interface::tool_result::ToolResultDraft {
        text,
        images: images
            .into_iter()
            .map(|i| base::interface::tool_result::ResultImage {
                media_type: i.media_type,
                data: i.data,
            })
            .collect(),
        is_error,
    };
    base::interface::tool_result::apply(&ctx.result_transformers, &call, &mut draft);
    // The structured form is the same answer as the text; once a transformer
    // has rewritten the text (a redaction, say), handing a program the
    // untouched structure would undo it.
    let structured =
        structured.filter(|_| (&draft.text, draft.is_error) == (&original.0, original.1));

    *ctx.images.lock().unwrap_or_else(|e| e.into_inner()) = draft
        .images
        .into_iter()
        .map(|i| PendingToolImage {
            media_type: i.media_type,
            data: i.data,
        })
        .collect();

    if draft.is_error && !was_reported_failure {
        Err(draft.text)
    } else {
        Ok(base::interface::tool_middleware::ToolAnswer {
            text: draft.text,
            new_messages,
            reported_failure: draft.is_error,
            structured,
        })
    }
}
