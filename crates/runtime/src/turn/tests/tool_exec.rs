//! 工具执行：钩子、权限、中间件、结果变换。

use crate::turn::reporting::telemetry_error_text;
use crate::turn::tool_exec::{apply_result_transformers, execute_tool_with_telemetry, ToolExecCtx};
use base::event::AgentEvent;
use std::sync::Arc;

/// A probe tool that records the input it was actually called with.
struct ProbeTool {
    called: std::sync::Arc<std::sync::Mutex<Option<serde_json::Value>>>,
}
#[async_trait::async_trait]
impl base::tool::Tool for ProbeTool {
    fn name(&self) -> &str {
        "Probe"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn call(
        &self,
        input: serde_json::Value,
        _ctx: base::tool::ToolContext,
        _progress: base::tool::ProgressSender,
    ) -> Result<base::tool::ToolResult, base::error::ToolError> {
        *self.called.lock().unwrap() = Some(input);
        Ok(base::tool::ToolResult::text("probe-ran"))
    }
}

/// Rejects every input, and records whether it ran anyway.
struct PickyTool {
    ran: Arc<std::sync::atomic::AtomicBool>,
}
#[async_trait::async_trait]
impl base::tool::Tool for PickyTool {
    fn name(&self) -> &str {
        "Picky"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn validate_input(
        &self,
        _input: &serde_json::Value,
        _ctx: &base::tool::ToolContext,
    ) -> base::tool::ValidationResult {
        base::tool::ValidationResult::err("picky refuses this input", 1)
    }
    async fn call(
        &self,
        _input: serde_json::Value,
        _ctx: base::tool::ToolContext,
        _progress: base::tool::ProgressSender,
    ) -> Result<base::tool::ToolResult, base::error::ToolError> {
        self.ran.store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(base::tool::ToolResult::text("picky-ran"))
    }
}

/// Always errors, so the failure-only lifecycle events have something to
/// fire on.
struct FailingTool;
#[async_trait::async_trait]
impl base::tool::Tool for FailingTool {
    fn name(&self) -> &str {
        "Failing"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn call(
        &self,
        _input: serde_json::Value,
        _ctx: base::tool::ToolContext,
        _progress: base::tool::ProgressSender,
    ) -> Result<base::tool::ToolResult, base::error::ToolError> {
        Err(base::error::ToolError::exec("probe failed on purpose"))
    }
}

struct DenyingPermission;
#[async_trait::async_trait]
impl base::interface::permission::Permission for DenyingPermission {
    async fn check(
        &self,
        _tool_name: &str,
        _tool_input: &serde_json::Value,
        _cwd: &std::path::Path,
        _session_id: &str,
    ) -> base::interface::permission::PermissionOutcome {
        base::interface::permission::PermissionOutcome::Deny {
            reason: "denied by test rule engine".into(),
        }
    }
}

struct AllowAllTestPermission;
#[async_trait::async_trait]
impl base::interface::permission::Permission for AllowAllTestPermission {
    async fn check(
        &self,
        _tool_name: &str,
        _tool_input: &serde_json::Value,
        _cwd: &std::path::Path,
        _session_id: &str,
    ) -> base::interface::permission::PermissionOutcome {
        base::interface::permission::PermissionOutcome::Permit
    }
}

fn test_exec_ctx(
    tools: Arc<base::tool::InMemoryToolRegistry>,
    hooks: Arc<hooks::HookRunner>,
) -> ToolExecCtx {
    test_exec_ctx_with_permission(tools, hooks, Arc::new(AllowAllTestPermission))
}

fn test_exec_ctx_with_permission(
    tools: Arc<base::tool::InMemoryToolRegistry>,
    hooks: Arc<hooks::HookRunner>,
    permission: Arc<dyn base::interface::permission::Permission>,
) -> ToolExecCtx {
    ToolExecCtx {
        tool_use_id: "toolu_test".into(),
        tools,
        cwd: std::env::temp_dir(),
        local_settings_path: Some(
            std::env::temp_dir()
                .join(".agix")
                .join(base::settings::SETTINGS_LOCAL_FILE),
        ),
        session_id: "test-session".into(),
        turn_no: 1,
        agent_depth: 0,
        telemetry_handle: telemetry::TelemetryHandle::noop(),
        turn_id: "test-turn".into(),
        cancel: tokio_util::sync::CancellationToken::new(),
        hooks,
        discontinued: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        config: Arc::new(base::context::EngineConfig::defaults_for("test")),
        exec: crate::turn::exec_providers_from(
            &base::context::EngineConfig::defaults_for("test"),
            None,
        ),
        permission,
        session_state: Arc::new(base::context::SessionState::new(std::env::temp_dir())),
        elicitation: Arc::new(base::interface::elicitation::DeclineAll),
        tool_middleware: Arc::new(Vec::new()),
        result_transformers: Arc::new(Vec::new()),
        images: Arc::new(std::sync::Mutex::new(Vec::new())),
        from_code: false,
        events: None,
    }
}

#[tokio::test]
async fn tool_start_fires_before_tool_execution_completes() {
    let called = std::sync::Arc::new(std::sync::Mutex::new(None));
    let tools = Arc::new(base::tool::InMemoryToolRegistry::new());
    tools.register(std::sync::Arc::new(ProbeTool {
        called: called.clone(),
    }));
    let hooks_runner = Arc::new(hooks::HookRunner::new(std::collections::HashMap::new()));

    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    let mut ctx = test_exec_ctx(tools, hooks_runner);
    ctx.telemetry_handle = telemetry::TelemetryHandle::new(tx);

    let result = execute_tool_with_telemetry(&ctx, "Probe", serde_json::json!({"x": 1})).await;
    assert!(result.is_ok());
    drop(ctx);

    let decided = rx
        .recv()
        .await
        .expect("permission_decision should have been recorded");
    assert_eq!(decided.kind(), "permission_decision");
    let first = rx
        .recv()
        .await
        .expect("tool_start should have been recorded");
    assert_eq!(first.kind(), "tool_start");
    let second = rx
        .recv()
        .await
        .expect("tool_execution should have been recorded");
    assert_eq!(second.kind(), "tool_execution");
}

async fn recorded_tool_execution(
    ctx: &mut ToolExecCtx,
    tool: &str,
    input: serde_json::Value,
) -> (
    base::interface::tool_middleware::ToolOutcome,
    serde_json::Value,
) {
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    ctx.telemetry_handle = telemetry::TelemetryHandle::new(tx);
    let result = execute_tool_with_telemetry(ctx, tool, input).await;
    ctx.telemetry_handle = telemetry::TelemetryHandle::noop();
    let mut execution = None;
    while let Some(event) = rx.recv().await {
        if event.kind() == "tool_execution" {
            execution = Some(serde_json::to_value(event).unwrap());
        }
    }
    (
        result,
        execution.expect("tool_execution should have been recorded"),
    )
}

#[tokio::test]
async fn tool_execution_reports_the_sizes_and_the_failure_it_saw() {
    let tools = Arc::new(base::tool::InMemoryToolRegistry::new());
    tools.register(Arc::new(ProbeTool {
        called: Default::default(),
    }));
    tools.register(Arc::new(FailingTool));
    let hooks_runner = Arc::new(hooks::HookRunner::new(std::collections::HashMap::new()));
    let mut ctx = test_exec_ctx(tools, hooks_runner);

    let input = serde_json::json!({"x": 1});
    let (result, ran) = recorded_tool_execution(&mut ctx, "Probe", input.clone()).await;
    assert!(result.is_ok());
    assert_eq!(ran["input_json_size"], input.to_string().len());
    assert_eq!(ran["result_content_size"], "probe-ran".len());
    assert_eq!(ran["is_error"], false);
    assert!(ran.get("error_message").is_none(), "{ran}");
    assert_eq!(
        ran["user_approved"], false,
        "the rule engine allowed it; nobody was asked"
    );

    let (result, failed) =
        recorded_tool_execution(&mut ctx, "Failing", serde_json::json!({})).await;
    assert!(result.is_err());
    assert_eq!(failed["is_error"], true);
    assert_eq!(failed["outcome"], "failed");
    let message = failed["error_message"].as_str().unwrap();
    assert!(message.contains("probe failed on purpose"), "{failed}");
}

#[tokio::test]
async fn tool_execution_says_a_user_approved_the_call_they_were_asked_about() {
    let tools = Arc::new(base::tool::InMemoryToolRegistry::new());
    tools.register(Arc::new(ProbeTool {
        called: Default::default(),
    }));
    let hooks_runner = Arc::new(hooks::HookRunner::new(std::collections::HashMap::new()));
    let mut ctx = test_exec_ctx_with_permission(
        tools,
        hooks_runner,
        Arc::new(PromptTestPermission { prompt_id: "asked" }),
    );
    let pending: Arc<
        std::sync::Mutex<
            std::collections::HashMap<
                String,
                tokio::sync::oneshot::Sender<crate::agent::PermissionDecision>,
            >,
        >,
    > = Default::default();
    let (events, _events_rx) = tokio::sync::mpsc::unbounded_channel();
    ctx.elicitation = Arc::new(crate::elicitation::ChannelElicitation::new(
        crate::event_bus::EventBus::new(events),
        pending.clone(),
    ));
    let answer = tokio::spawn(async move {
        loop {
            if let Some(tx) = pending.lock().unwrap().remove("asked") {
                let _ = tx.send(crate::agent::PermissionDecision::Permit);
                return;
            }
            tokio::task::yield_now().await;
        }
    });

    let (result, ran) = recorded_tool_execution(&mut ctx, "Probe", serde_json::json!({})).await;
    answer.await.unwrap();
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(ran["user_approved"], true);
}

#[test]
fn a_long_failure_text_is_cut_on_a_character_boundary() {
    let kept = telemetry_error_text(&"é".repeat(2000));
    assert_eq!(kept.chars().count(), 513);
    assert!(kept.ends_with('…'));
    assert_eq!(telemetry_error_text("short"), "short");
}

#[tokio::test]
async fn a_pre_tool_use_approve_answers_the_permission_question_but_not_a_denial() {
    let called = std::sync::Arc::new(std::sync::Mutex::new(None));
    let tools = Arc::new(base::tool::InMemoryToolRegistry::new());
    tools.register(Arc::new(ProbeTool {
        called: called.clone(),
    }));
    let approving = || {
        let mut settings: hooks::config::HooksSettings = std::collections::HashMap::new();
        settings.insert(
            hooks::HookEvent::PreToolUse,
            vec![hooks::config::HookConfig::Command {
                command: r#"echo '{"decision":"approve"}'"#.into(),
                shell: None,
                timeout: None,
                if_pattern: None,
                only_on_error: None,
                once: None,
                async_rewake: None,
                on_error: hooks::OnError::Allow,
            }],
        );
        Arc::new(hooks::HookRunner::new(settings))
    };

    let unanswered = test_exec_ctx_with_permission(
        tools.clone(),
        Arc::new(hooks::HookRunner::new(std::collections::HashMap::new())),
        Arc::new(PromptTestPermission { prompt_id: "p" }),
    );
    let declined = execute_tool_with_telemetry(&unanswered, "Probe", serde_json::json!({})).await;
    assert!(declined.is_err(), "nobody answered, so it does not run");
    assert!(called.lock().unwrap().is_none());

    let mut approved = test_exec_ctx_with_permission(
        tools.clone(),
        approving(),
        Arc::new(PromptTestPermission { prompt_id: "p" }),
    );
    let (result, ran) =
        recorded_tool_execution(&mut approved, "Probe", serde_json::json!({})).await;
    assert!(result.is_ok(), "{result:?}");
    assert!(
        called.lock().unwrap().is_some(),
        "the hook's approval let it run"
    );
    assert_eq!(ran["user_approved"], false, "a hook is not a user");

    *called.lock().unwrap() = None;
    let denied = test_exec_ctx_with_permission(tools, approving(), Arc::new(DenyAllTestPermission));
    let refused = execute_tool_with_telemetry(&denied, "Probe", serde_json::json!({})).await;
    assert!(
        refused.is_err(),
        "approve answers a question, it does not lift a rule"
    );
    assert!(called.lock().unwrap().is_none());
}

#[tokio::test]
async fn input_the_tool_rejects_never_runs_whatever_the_permission_handler() {
    let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let tools = Arc::new(base::tool::InMemoryToolRegistry::new());
    tools.register(Arc::new(PickyTool { ran: ran.clone() }));
    let hooks_runner = Arc::new(hooks::HookRunner::new(std::collections::HashMap::new()));
    let ctx = test_exec_ctx(tools, hooks_runner);

    let result = execute_tool_with_telemetry(&ctx, "Picky", serde_json::json!({})).await;
    let error = result.expect_err("an allow-all permission does not let invalid input through");
    assert!(error.contains("picky refuses this input"), "{error}");
    assert!(!ran.load(std::sync::atomic::Ordering::SeqCst));
}

#[tokio::test]
async fn pre_tool_use_hook_blocks_call() {
    let called = std::sync::Arc::new(std::sync::Mutex::new(None));
    let tools = Arc::new(base::tool::InMemoryToolRegistry::new());
    tools.register(std::sync::Arc::new(ProbeTool {
        called: called.clone(),
    }));

    let mut settings: hooks::config::HooksSettings = std::collections::HashMap::new();
    settings.insert(
        hooks::HookEvent::PreToolUse,
        vec![hooks::config::HookConfig::Command {
            command: r#"echo '{"decision":"block","message":"nope"}'"#.into(),
            shell: None,
            timeout: None,
            if_pattern: None,
            only_on_error: None,
            once: None,
            async_rewake: None,
            on_error: hooks::OnError::Allow,
        }],
    );
    let hooks_runner = Arc::new(hooks::HookRunner::new(settings));
    let ctx = test_exec_ctx(tools, hooks_runner);

    let result = execute_tool_with_telemetry(&ctx, "Probe", serde_json::json!({"x": 1})).await;
    assert!(result.is_err(), "expected the tool call to be blocked");
    assert!(result.unwrap_err().contains("nope"));
    assert!(
        called.lock().unwrap().is_none(),
        "tool must not have actually run"
    );
}

#[tokio::test]
async fn pre_tool_use_hook_rewrites_input() {
    let called = std::sync::Arc::new(std::sync::Mutex::new(None));
    let tools = Arc::new(base::tool::InMemoryToolRegistry::new());
    tools.register(std::sync::Arc::new(ProbeTool {
        called: called.clone(),
    }));

    let mut settings: hooks::config::HooksSettings = std::collections::HashMap::new();
    settings.insert(
        hooks::HookEvent::PreToolUse,
        vec![hooks::config::HookConfig::Command {
            command: r#"echo '{"updated_input":{"x":99}}'"#.into(),
            shell: None,
            timeout: None,
            if_pattern: None,
            only_on_error: None,
            once: None,
            async_rewake: None,
            on_error: hooks::OnError::Allow,
        }],
    );
    let hooks_runner = Arc::new(hooks::HookRunner::new(settings));
    let ctx = test_exec_ctx(tools, hooks_runner);

    let result = execute_tool_with_telemetry(&ctx, "Probe", serde_json::json!({"x": 1})).await;
    assert!(
        result.is_ok(),
        "expected the (rewritten) call to succeed: {result:?}"
    );
    assert_eq!(*called.lock().unwrap(), Some(serde_json::json!({"x": 99})));
}

struct FakePushNotification;
#[async_trait::async_trait]
impl base::tool::Tool for FakePushNotification {
    fn name(&self) -> &str {
        "PushNotification"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn call(
        &self,
        _input: serde_json::Value,
        _ctx: base::tool::ToolContext,
        _progress: base::tool::ProgressSender,
    ) -> Result<base::tool::ToolResult, base::error::ToolError> {
        Ok(base::tool::ToolResult::text("Notification sent"))
    }
}

#[tokio::test]
async fn a_push_notification_reaches_the_event_stream_and_the_notification_hook() {
    let tools = Arc::new(base::tool::InMemoryToolRegistry::new());
    tools.register(Arc::new(FakePushNotification));
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("hook.json");
    let mut settings: hooks::config::HooksSettings = std::collections::HashMap::new();
    settings.insert(
        hooks::HookEvent::Notification,
        vec![hooks::config::HookConfig::Command {
            command: format!("cat > '{}'", log.display()),
            shell: None,
            timeout: None,
            if_pattern: None,
            only_on_error: None,
            once: None,
            async_rewake: None,
            on_error: hooks::OnError::Allow,
        }],
    );
    let mut ctx = test_exec_ctx(tools, Arc::new(hooks::HookRunner::new(settings)));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    ctx.events = Some(crate::event_bus::EventBus::new(tx));

    let result = execute_tool_with_telemetry(
        &ctx,
        "PushNotification",
        serde_json::json!({"message": " build done ", "status": "proactive"}),
    )
    .await;
    assert!(result.is_ok(), "{result:?}");

    let mut notified = None;
    while let Ok(event) = rx.try_recv() {
        if let AgentEvent::Notification {
            message, status, ..
        } = event
        {
            notified = Some((message, status));
        }
    }
    assert_eq!(
        notified,
        Some(("build done".to_string(), Some("proactive".to_string())))
    );
    let hooked: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&log).unwrap()).unwrap();
    assert_eq!(hooked["hook_event_name"], "Notification");
    assert_eq!(hooked["reason"], "build done");
}

#[tokio::test]
async fn post_tool_use_hook_fires_after_execution() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("post-hook-ran");
    let tools = Arc::new(base::tool::InMemoryToolRegistry::new());
    tools.register(std::sync::Arc::new(ProbeTool {
        called: std::sync::Arc::new(std::sync::Mutex::new(None)),
    }));

    let mut settings: hooks::config::HooksSettings = std::collections::HashMap::new();
    settings.insert(
        hooks::HookEvent::PostToolUse,
        vec![hooks::config::HookConfig::Command {
            command: format!("touch {}", marker.display()),
            shell: None,
            timeout: None,
            if_pattern: None,
            only_on_error: None,
            once: None,
            async_rewake: None,
            on_error: hooks::OnError::Allow,
        }],
    );
    let hooks_runner = Arc::new(hooks::HookRunner::new(settings));
    let ctx = test_exec_ctx(tools, hooks_runner);

    let result = execute_tool_with_telemetry(&ctx, "Probe", serde_json::json!({})).await;
    assert!(result.is_ok());
    assert!(
        marker.exists(),
        "PostToolUse hook should have run and created the marker file"
    );
}

/// Builds a `ToolExecCtx` whose runner touches `marker` when `event` fires.
fn ctx_with_marker_hook(
    event: hooks::HookEvent,
    marker: &std::path::Path,
    tools: Arc<base::tool::InMemoryToolRegistry>,
) -> ToolExecCtx {
    let mut settings: hooks::config::HooksSettings = std::collections::HashMap::new();
    settings.insert(
        event,
        vec![hooks::config::HookConfig::Command {
            command: format!("touch {}", marker.display()),
            shell: None,
            timeout: None,
            if_pattern: None,
            only_on_error: None,
            once: None,
            async_rewake: None,
            on_error: hooks::OnError::Allow,
        }],
    );
    test_exec_ctx(tools, Arc::new(hooks::HookRunner::new(settings)))
}

/// `PostToolUseFailure` is the error-only sibling of `PostToolUse`, so a
/// hook that only cares about failures does not have to subscribe to
/// every tool call and filter on `is_error`.
#[tokio::test]
async fn post_tool_use_failure_fires_only_when_the_tool_errors() {
    let dir = tempfile::tempdir().unwrap();
    let tools = Arc::new(base::tool::InMemoryToolRegistry::new());
    tools.register(std::sync::Arc::new(FailingTool));
    tools.register(std::sync::Arc::new(ProbeTool {
        called: std::sync::Arc::new(std::sync::Mutex::new(None)),
    }));

    // Failing tool → fires.
    let on_error = dir.path().join("on-error");
    let ctx = ctx_with_marker_hook(
        hooks::HookEvent::PostToolUseFailure,
        &on_error,
        tools.clone(),
    );
    let _ = execute_tool_with_telemetry(&ctx, "Failing", serde_json::json!({})).await;
    assert!(
        on_error.exists(),
        "PostToolUseFailure should fire when the tool returns an error"
    );

    // Succeeding tool → does not fire.
    let on_success = dir.path().join("on-success");
    let ctx = ctx_with_marker_hook(hooks::HookEvent::PostToolUseFailure, &on_success, tools);
    let r = execute_tool_with_telemetry(&ctx, "Probe", serde_json::json!({})).await;
    assert!(r.is_ok());
    assert!(
        !on_success.exists(),
        "PostToolUseFailure must not fire for a successful call"
    );
}

/// `PermissionDenied` fires for a rule-engine `Deny` (this test) and for a
/// host answering a prompt with a denial (the other branch in
/// `execute_tool_inner`).
#[tokio::test]
async fn permission_denied_hook_fires_on_a_rule_engine_deny() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("denied");
    let tools = Arc::new(base::tool::InMemoryToolRegistry::new());
    tools.register(std::sync::Arc::new(ProbeTool {
        called: std::sync::Arc::new(std::sync::Mutex::new(None)),
    }));

    let mut ctx = ctx_with_marker_hook(hooks::HookEvent::PermissionDenied, &marker, tools);
    ctx.permission = Arc::new(DenyingPermission);

    let result = execute_tool_with_telemetry(&ctx, "Probe", serde_json::json!({})).await;
    assert!(result.is_err(), "the call must still be denied");
    assert!(
        marker.exists(),
        "PermissionDenied should have fired for the refused call"
    );
}

/// A lifecycle hook must not be able to veto — every firing site discards
/// the response. A `PostToolUseFailure` hook returning `continue: false`
/// (which *would* stop the turn from `PostToolUse`) changes nothing.
#[tokio::test]
async fn lifecycle_hooks_cannot_discontinue_the_turn() {
    let tools = Arc::new(base::tool::InMemoryToolRegistry::new());
    tools.register(std::sync::Arc::new(FailingTool));

    let mut settings: hooks::config::HooksSettings = std::collections::HashMap::new();
    settings.insert(
        hooks::HookEvent::PostToolUseFailure,
        vec![hooks::config::HookConfig::Command {
            command: r#"echo '{"continue":false}'"#.into(),
            shell: None,
            timeout: None,
            if_pattern: None,
            only_on_error: None,
            once: None,
            async_rewake: None,
            on_error: hooks::OnError::Allow,
        }],
    );
    let ctx = test_exec_ctx(tools, Arc::new(hooks::HookRunner::new(settings)));

    let _ = execute_tool_with_telemetry(&ctx, "Failing", serde_json::json!({})).await;
    assert!(
        !ctx.discontinued.load(std::sync::atomic::Ordering::SeqCst),
        "a lifecycle hook must not be able to end the turn"
    );
}

#[tokio::test]
async fn post_tool_use_hook_discontinue_sets_the_shared_flag() {
    let tools = Arc::new(base::tool::InMemoryToolRegistry::new());
    tools.register(std::sync::Arc::new(ProbeTool {
        called: std::sync::Arc::new(std::sync::Mutex::new(None)),
    }));

    let mut settings: hooks::config::HooksSettings = std::collections::HashMap::new();
    settings.insert(
        hooks::HookEvent::PostToolUse,
        vec![hooks::config::HookConfig::Command {
            command: r#"echo '{"continue":false}'"#.into(),
            shell: None,
            timeout: None,
            if_pattern: None,
            only_on_error: None,
            once: None,
            async_rewake: None,
            on_error: hooks::OnError::Allow,
        }],
    );
    let hooks_runner = Arc::new(hooks::HookRunner::new(settings));
    let ctx = test_exec_ctx(tools, hooks_runner);

    assert!(!ctx.discontinued.load(std::sync::atomic::Ordering::Relaxed));
    let result = execute_tool_with_telemetry(&ctx, "Probe", serde_json::json!({})).await;
    assert!(
        result.is_ok(),
        "the tool call itself still succeeds: {result:?}"
    );
    assert!(
        ctx.discontinued.load(std::sync::atomic::Ordering::Relaxed),
        "PostToolUse discontinue should have set the shared flag"
    );
}

/// A `PostToolUse` hook returning `decision: "block"` denies *this call's*
/// result specifically (`execute_tool_inner`'s return value becomes an
/// `Err`, which is what ends up as the tool_result the model sees),
/// without ending the turn unless the hook *also* sets `continue: false`.
#[tokio::test]
async fn post_tool_use_hook_block_denies_only_this_result() {
    let tools = Arc::new(base::tool::InMemoryToolRegistry::new());
    tools.register(std::sync::Arc::new(ProbeTool {
        called: std::sync::Arc::new(std::sync::Mutex::new(None)),
    }));

    let mut settings: hooks::config::HooksSettings = std::collections::HashMap::new();
    settings.insert(
        hooks::HookEvent::PostToolUse,
        vec![hooks::config::HookConfig::Command {
            command: r#"echo '{"decision":"block","message":"looked risky"}'"#.into(),
            shell: None,
            timeout: None,
            if_pattern: None,
            only_on_error: None,
            once: None,
            async_rewake: None,
            on_error: hooks::OnError::Allow,
        }],
    );
    let hooks_runner = Arc::new(hooks::HookRunner::new(settings));
    let ctx = test_exec_ctx(tools, hooks_runner);

    let result = execute_tool_with_telemetry(&ctx, "Probe", serde_json::json!({})).await;
    let err = result.expect_err("blocked PostToolUse result should surface as an Err");
    assert!(
        err.contains("looked risky"),
        "denial message should carry the hook's reason: {err}"
    );
    assert!(
        !ctx.discontinued.load(std::sync::atomic::Ordering::Relaxed),
        "a bare decision:block (no continue:false) must not end the whole turn, \
             only deny this one call's result"
    );
}

struct DenyAllTestPermission;
#[async_trait::async_trait]
impl base::interface::permission::Permission for DenyAllTestPermission {
    async fn check(
        &self,
        _tool_name: &str,
        _tool_input: &serde_json::Value,
        _cwd: &std::path::Path,
        _session_id: &str,
    ) -> base::interface::permission::PermissionOutcome {
        base::interface::permission::PermissionOutcome::Deny {
            reason: "denied by test".into(),
        }
    }
}

/// Always returns the same fixed `prompt_id` so a test can reach into
/// `ctx.pending_permissions` and answer it from outside `execute_tool_inner`.
struct PromptTestPermission {
    prompt_id: &'static str,
}
#[async_trait::async_trait]
impl base::interface::permission::Permission for PromptTestPermission {
    async fn check(
        &self,
        _tool_name: &str,
        _tool_input: &serde_json::Value,
        _cwd: &std::path::Path,
        _session_id: &str,
    ) -> base::interface::permission::PermissionOutcome {
        base::interface::permission::PermissionOutcome::Prompt {
            prompt_id: self.prompt_id.into(),
            message: "allow?".into(),
            paths: vec![],
            managed: false,
        }
    }
}

#[tokio::test]
async fn permission_deny_blocks_the_call() {
    let called = std::sync::Arc::new(std::sync::Mutex::new(None));
    let tools = Arc::new(base::tool::InMemoryToolRegistry::new());
    tools.register(std::sync::Arc::new(ProbeTool {
        called: called.clone(),
    }));
    let hooks_runner = Arc::new(hooks::HookRunner::new(std::collections::HashMap::new()));
    let ctx = test_exec_ctx_with_permission(tools, hooks_runner, Arc::new(DenyAllTestPermission));

    let result = execute_tool_with_telemetry(&ctx, "Probe", serde_json::json!({})).await;
    assert!(result.is_err(), "expected the tool call to be denied");
    assert!(result.unwrap_err().contains("denied by test"));
    assert!(
        called.lock().unwrap().is_none(),
        "tool must not have actually run"
    );
}

#[tokio::test]
async fn permission_prompt_with_no_response_blocks_until_cancelled() {
    // Conventional confirmation-prompt UX: no arbitrary timeout, the
    // call genuinely waits for an answer. The only thing that should
    // unblock it without one is the session's own cancellation token
    // (`session.close` / client disconnect on the daemon side) — and
    // when that happens, the call must NOT proceed (nobody approved
    // it).
    let called = std::sync::Arc::new(std::sync::Mutex::new(None));
    let tools = Arc::new(base::tool::InMemoryToolRegistry::new());
    tools.register(std::sync::Arc::new(ProbeTool {
        called: called.clone(),
    }));
    let hooks_runner = Arc::new(hooks::HookRunner::new(std::collections::HashMap::new()));
    let mut ctx = test_exec_ctx_with_permission(
        tools,
        hooks_runner,
        Arc::new(PromptTestPermission {
            prompt_id: "unanswered",
        }),
    );
    // The real asker, so the registration this test is about actually
    // exists to be cleaned up.
    let pending: Arc<
        std::sync::Mutex<
            std::collections::HashMap<
                String,
                tokio::sync::oneshot::Sender<crate::agent::PermissionDecision>,
            >,
        >,
    > = Default::default();
    let (events, _events_rx) = tokio::sync::mpsc::unbounded_channel();
    ctx.elicitation = Arc::new(crate::elicitation::ChannelElicitation::new(
        crate::event_bus::EventBus::new(events),
        pending.clone(),
    ));
    let (telemetry_tx, mut telemetry_rx) = tokio::sync::mpsc::channel(16);
    ctx.telemetry_handle = telemetry::TelemetryHandle::new(telemetry_tx);

    let call = execute_tool_with_telemetry(&ctx, "Probe", serde_json::json!({"x": 1}));
    tokio::pin!(call);

    // Give it a real chance to actually be waiting, not just scheduled —
    // if it resolved here, that would mean it did NOT block.
    tokio::select! {
        _ = &mut call => panic!("must not resolve before an answer or cancellation"),
        _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
    }
    assert!(
        called.lock().unwrap().is_none(),
        "tool must not have run while still waiting for an answer"
    );

    ctx.cancel.cancel();
    let result = call.await;
    assert!(result.is_err(), "cancellation must not fall back to Permit");
    assert!(result.unwrap_err().contains("cancelled"));
    assert!(called.lock().unwrap().is_none());
    assert!(
        pending.lock().unwrap().is_empty(),
        "the cancelled registration should have been cleaned up"
    );

    let asked = telemetry_rx
        .recv()
        .await
        .expect("permission_decision should have been recorded");
    assert_eq!(asked.kind(), "permission_decision");
    let event = telemetry_rx
        .recv()
        .await
        .expect("tool_cancelled should have been recorded");
    assert_eq!(event.kind(), "tool_cancelled");
}

#[tokio::test]
async fn permission_prompt_answered_deny_blocks_the_call() {
    let called = std::sync::Arc::new(std::sync::Mutex::new(None));
    let tools = Arc::new(base::tool::InMemoryToolRegistry::new());
    tools.register(std::sync::Arc::new(ProbeTool {
        called: called.clone(),
    }));
    let hooks_runner = Arc::new(hooks::HookRunner::new(std::collections::HashMap::new()));
    let mut ctx = test_exec_ctx_with_permission(
        tools,
        hooks_runner,
        Arc::new(PromptTestPermission {
            prompt_id: "answered-deny",
        }),
    );
    ctx.elicitation = base::interface::elicitation::FixedElicitation::new(
        base::interface::elicitation::ElicitOutcome::answered(
            &crate::agent::PermissionDecision::Deny {
                reason: "answered no".into(),
            },
        ),
    );

    let result = execute_tool_with_telemetry(&ctx, "Probe", serde_json::json!({})).await;
    assert!(result.is_err(), "expected the real Deny response to win");
    assert!(result.unwrap_err().contains("answered no"));
    assert!(called.lock().unwrap().is_none());
}

/// Like `ProbeTool`, but overrides `permission_match_content` so the
/// derived rule pattern is non-empty — needed to exercise the
/// `PermitAlways` disk-write path, since `settings_patch::build_rule_string`
/// returns `None` (nothing to write) for empty/absent content.
struct ContentProbeTool {
    called: std::sync::Arc<std::sync::Mutex<Option<serde_json::Value>>>,
}
#[async_trait::async_trait]
impl base::tool::Tool for ContentProbeTool {
    fn name(&self) -> &str {
        "Probe"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn permission_match_content(&self, _input: &serde_json::Value) -> Option<String> {
        Some("git status".into())
    }
    async fn call(
        &self,
        input: serde_json::Value,
        _ctx: base::tool::ToolContext,
        _progress: base::tool::ProgressSender,
    ) -> Result<base::tool::ToolResult, base::error::ToolError> {
        *self.called.lock().unwrap() = Some(input);
        Ok(base::tool::ToolResult::text("probe-ran"))
    }
}

type RecordedAllows = std::sync::Arc<std::sync::Mutex<Vec<(String, Option<String>)>>>;

/// Records every `add_persistent_allow` call it receives, and always
/// prompts on `check` (fixed `prompt_id`) so a test can drive the
/// `InputMessage::PermissionResponse` -> `PermitAlways` path the same
/// way a real interactive "always allow" answer would.
struct PromptThenRecordPersistentAllow {
    prompt_id: &'static str,
    recorded: RecordedAllows,
}
#[async_trait::async_trait]
impl base::interface::permission::Permission for PromptThenRecordPersistentAllow {
    async fn check(
        &self,
        _tool_name: &str,
        _tool_input: &serde_json::Value,
        _cwd: &std::path::Path,
        _session_id: &str,
    ) -> base::interface::permission::PermissionOutcome {
        base::interface::permission::PermissionOutcome::Prompt {
            prompt_id: self.prompt_id.into(),
            message: "allow?".into(),
            paths: vec![],
            managed: false,
        }
    }
    fn add_persistent_allow(&self, tool_name: &str, rule_content: Option<&str>) {
        self.recorded
            .lock()
            .unwrap()
            .push((tool_name.to_string(), rule_content.map(|s| s.to_string())));
    }
}

#[tokio::test]
async fn permission_prompt_answered_permit_always_local_persists_rule_to_disk() {
    // End-to-end: a `PermitAlways{scope: Local}` response must (1) call
    // `Permission::add_persistent_allow` so the in-memory rule engine
    // allows the rest of this session immediately, using the same
    // `tool.permission_match_content` derivation the rule engine itself
    // uses to match calls, and (2) write the equivalent rule to
    // `<project_root>/.agix/settings.local.json` (NOT `settings.json` —
    // that's the shared/committed project tier), in the shape
    // `Settings::load` reads back — and (3) still let the pending tool
    // call proceed, same as a plain `Permit` would.
    let dir = tempfile::tempdir().unwrap();
    let agix_dir = dir.path().join(".agix");
    let called = std::sync::Arc::new(std::sync::Mutex::new(None));
    let tools = Arc::new(base::tool::InMemoryToolRegistry::new());
    tools.register(std::sync::Arc::new(ContentProbeTool {
        called: called.clone(),
    }));
    let hooks_runner = Arc::new(hooks::HookRunner::new(std::collections::HashMap::new()));
    let recorded = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let permission = Arc::new(PromptThenRecordPersistentAllow {
        prompt_id: "permit-always-local",
        recorded: recorded.clone(),
    });

    let ctx = ToolExecCtx {
        tool_use_id: "toolu_test".into(),
        tools,
        cwd: dir.path().to_path_buf(),
        local_settings_path: Some(agix_dir.join(base::settings::SETTINGS_LOCAL_FILE)),
        session_id: "test-session".into(),
        turn_no: 1,
        agent_depth: 0,
        telemetry_handle: telemetry::TelemetryHandle::noop(),
        turn_id: "test-turn".into(),
        cancel: tokio_util::sync::CancellationToken::new(),
        hooks: hooks_runner,
        discontinued: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        config: Arc::new(base::context::EngineConfig::defaults_for("test")),
        exec: crate::turn::exec_providers_from(
            &base::context::EngineConfig::defaults_for("test"),
            None,
        ),
        permission,
        session_state: Arc::new(base::context::SessionState::new(dir.path().to_path_buf())),
        tool_middleware: Arc::new(Vec::new()),
        result_transformers: Arc::new(Vec::new()),
        elicitation: base::interface::elicitation::FixedElicitation::new(
            base::interface::elicitation::ElicitOutcome::answered(
                &crate::agent::PermissionDecision::PermitAlways {
                    scope: crate::agent::PersistScope::Local,
                },
            ),
        ),
        images: Arc::new(std::sync::Mutex::new(Vec::new())),
        from_code: false,
        events: None,
    };

    let result = execute_tool_with_telemetry(&ctx, "Probe", serde_json::json!({})).await;

    assert!(
        result.is_ok(),
        "PermitAlways must let the pending tool call proceed: {result:?}"
    );
    assert!(
        called.lock().unwrap().is_some(),
        "the tool must actually have run"
    );
    assert_eq!(
        recorded.lock().unwrap().as_slice(),
        &[("Probe".to_string(), Some("git status".to_string()))],
        "add_persistent_allow must be called with the tool's derived match content"
    );

    let settings_local_path = agix_dir.join(base::settings::SETTINGS_LOCAL_FILE);
    assert!(
        settings_local_path.exists(),
        "expected {} to exist — anywhere else is outside the Bash sandbox's \
             deny-file-write protection for this file",
        settings_local_path.display()
    );

    // The rule has to survive a real load, not just be present as JSON:
    // a shape `Settings` doesn't deserialize would leave the user
    // re-answering the same prompt every restart.
    let empty = tempfile::tempdir().unwrap();
    let loaded = base::settings::Settings::load(
        empty.path().to_path_buf(),
        empty.path().to_path_buf(),
        agix_dir.clone(),
        "code",
        "m",
    );
    let rules = permissions::rule::rules_from_all_tiers(&loaded);
    assert_eq!(
        rules.len(),
        1,
        "expected exactly one loaded rule: {rules:?}"
    );
    assert_eq!(rules[0].tool_name, "Probe");
    assert_eq!(rules[0].rule_content.as_deref(), Some("git status"));
    assert_eq!(rules[0].behavior, base::permission::RuleBehavior::Allow);
    assert_eq!(
        rules[0].source,
        base::permission::RuleSource::LocalSettings,
        "must carry the local tier's source so its priority outranks ProjectSettings"
    );

    // Not written to the shared/committed settings.json tier.
    assert!(
        !agix_dir.join(base::settings::SETTINGS_FILE).exists(),
        "must not touch the shared/committed settings.json tier"
    );
}

#[test]
fn a_redacting_transformer_also_takes_the_structured_result_away() {
    let mut ctx = test_exec_ctx(
        Arc::new(base::tool::InMemoryToolRegistry::new()),
        Arc::new(hooks::HookRunner::new(std::collections::HashMap::new())),
    );
    ctx.result_transformers =
        Arc::new(vec![
            Arc::new(base::interface::tool_result::RedactLiterals::new([
                "hunter2",
            ])) as Arc<dyn base::interface::tool_result::ToolResultTransformer>,
        ]);
    let answer = |text: &str| {
        Ok(base::interface::tool_middleware::ToolAnswer {
            text: text.into(),
            new_messages: None,
            reported_failure: false,
            structured: Some(serde_json::json!({"password": text})),
        })
    };

    let redacted = apply_result_transformers(
        &ctx,
        "Read",
        &serde_json::json!({}),
        answer("password: hunter2"),
    )
    .unwrap();
    assert!(!redacted.text.contains("hunter2"), "{}", redacted.text);
    assert_eq!(redacted.structured, None);

    let untouched =
        apply_result_transformers(&ctx, "Read", &serde_json::json!({}), answer("nothing here"))
            .unwrap();
    assert_eq!(
        untouched.structured,
        Some(serde_json::json!({"password": "nothing here"}))
    );
}
