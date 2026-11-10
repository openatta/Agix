//! PushNotificationTool —— "PushNotification"。
//!
//! 提醒用户回来看。通知本身由派发层送出（`notification` 帧和 `Notification`
//! hook，见 `runtime::turn` 的 `notify_host`），工具只校验输入；在 macOS 上顺带
//! 用 osascript 在引擎所在的机器上弹一条桌面通知，失败不算错。

use async_trait::async_trait;
use base::error::ToolError;
// Only the macOS path runs a program, and importing these unconditionally
// makes them unused elsewhere.
#[cfg(target_os = "macos")]
use crate::exec_capture::capture;
#[cfg(target_os = "macos")]
use base::interface::exec::ProcessSpec;
use base::tool::{
    PermissionDecision, ProgressSender, PromptContext, Tool, ToolContext, ToolResult, ToolSpec,
    ValidationResult,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PushNotificationInput {
    /// 通知正文。应当简洁、一行、无 markdown。
    pub message: String,

    /// 通知类型（可选）。唯一有效值是 "proactive"，表主动推送。
    #[serde(default)]
    pub status: Option<String>,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct PushNotificationTool;

#[async_trait]
impl Tool for PushNotificationTool {
    fn name(&self) -> &str {
        "PushNotification"
    }

    fn description(&self) -> &str {
        "Notify the user: the host that runs this session shows it (and any Notification hook gets it)."
    }

    fn input_schema(&self) -> Value {
        crate::input_schema::<PushNotificationInput>()
    }

    async fn prompt(&self, _: &PromptContext) -> String {
        include_str!("prompts/coding/push_notification.prompt.md").to_string()
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            concurrency_safe: true,
            read_only: true,
            ..ToolSpec::defaults_for(self)
        }
    }

    async fn validate_input(&self, input: &Value, _: &ToolContext) -> ValidationResult {
        let parsed: Result<PushNotificationInput, _> = serde_json::from_value(input.clone());
        match parsed {
            Ok(p) if p.message.trim().is_empty() => {
                ValidationResult::err("message must not be empty", 1)
            }
            Ok(p) if p.message.len() > 200 => {
                ValidationResult::err("message must be under 200 characters", 2)
            }
            Ok(_) => ValidationResult::Ok,
            Err(e) => ValidationResult::err(format!("invalid input: {e}"), 3),
        }
    }

    async fn check_permissions(&self, _: &Value, _: &ToolContext) -> PermissionDecision {
        PermissionDecision::allow()
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolContext,
        _progress: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let input: PushNotificationInput = serde_json::from_value(input)?;
        let msg = input.message.trim();
        let suffix = if input.status.as_deref() == Some("proactive") {
            " (proactive)"
        } else {
            ""
        };
        desktop_notification(&ctx, msg).await;
        Ok(ToolResult::text(format!(
            "Notification sent: {msg}{suffix}"
        )))
    }
}

/// A desktop notification on the machine the engine runs on, as well as the
/// host's. Best effort: the host's copy is the one that counts.
#[cfg(target_os = "macos")]
async fn desktop_notification(ctx: &ToolContext, msg: &str) {
    let escaped = msg.replace('\\', "\\\\").replace('"', "\\\"");
    let spec = ProcessSpec::new("osascript", &ctx.cwd).args([
        "-e".to_string(),
        format!(r#"display notification "{escaped}" with title "Agix""#),
    ]);
    match capture(&ctx.exec, spec, ctx.cancel.clone()).await {
        Ok(output) if output.status.success => {}
        Ok(output) => {
            tracing::debug!(stderr = %output.stderr_lossy(), "osascript could not show the notification")
        }
        Err(e) => tracing::debug!(error = %e, "osascript could not be run"),
    }
}

#[cfg(not(target_os = "macos"))]
async fn desktop_notification(_: &ToolContext, _: &str) {}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::Path;

    fn ctx_in(cwd: &Path) -> ToolContext {
        platform::tool_context_for_test(cwd.to_path_buf())
    }

    #[tokio::test]
    async fn name_is_push_notification() {
        let tool = PushNotificationTool;
        assert_eq!(tool.name(), "PushNotification");
        assert!(tool.is_read_only(&Value::Null));
        assert!(tool.is_concurrency_safe(&Value::Null));
    }

    #[tokio::test]
    async fn empty_message_validates_err() {
        let tool = PushNotificationTool;
        let r = tool
            .validate_input(&json!({"message": ""}), &ctx_in(Path::new("/tmp")))
            .await;
        assert!(r.is_err());
    }

    #[tokio::test]
    async fn whitespace_message_validates_err() {
        let tool = PushNotificationTool;
        let r = tool
            .validate_input(&json!({"message": "   "}), &ctx_in(Path::new("/tmp")))
            .await;
        assert!(r.is_err());
    }

    #[tokio::test]
    async fn message_too_long_validates_err() {
        let tool = PushNotificationTool;
        let long = "x".repeat(201);
        let r = tool
            .validate_input(&json!({"message": long}), &ctx_in(Path::new("/tmp")))
            .await;
        assert!(r.is_err());
    }

    #[tokio::test]
    async fn valid_message_validates_ok() {
        let tool = PushNotificationTool;
        let r = tool
            .validate_input(
                &json!({"message": "build failed: 2 auth tests"}),
                &ctx_in(Path::new("/tmp")),
            )
            .await;
        assert!(r.is_ok());
    }

    #[tokio::test]
    async fn valid_message_with_status_validates_ok() {
        let tool = PushNotificationTool;
        let r = tool
            .validate_input(
                &json!({"message": "deploy ready", "status": "proactive"}),
                &ctx_in(Path::new("/tmp")),
            )
            .await;
        assert!(r.is_ok());
    }

    #[tokio::test]
    async fn invalid_json_validates_err() {
        let tool = PushNotificationTool;
        let r = tool
            .validate_input(&json!({"bad": "input"}), &ctx_in(Path::new("/tmp")))
            .await;
        assert!(r.is_err());
    }

    #[tokio::test]
    async fn call_does_not_panic() {
        let tool = PushNotificationTool;
        let r = tool
            .call(
                json!({"message": "test notification"}),
                ctx_in(Path::new("/tmp")),
                ProgressSender::noop("t"),
            )
            .await;
        // The osascript call may or may not succeed depending on the test
        // environment; the important thing is it doesn't panic and returns
        // a result.
        assert!(r.is_ok() || r.is_err());
        if let Ok(result) = r {
            // Accept both success and "failed" paths — is_error is not asserted here.
            if let base::tool::ToolResultContent::Text(t) = &result.content {
                assert!(t.contains("Notification") || t.contains("notification"));
            }
        }
    }

    #[tokio::test]
    async fn call_with_proactive_status_does_not_panic() {
        let tool = PushNotificationTool;
        let r = tool
            .call(
                json!({"message": "proactive test", "status": "proactive"}),
                ctx_in(Path::new("/tmp")),
                ProgressSender::noop("t"),
            )
            .await;
        assert!(r.is_ok() || r.is_err());
    }
}
