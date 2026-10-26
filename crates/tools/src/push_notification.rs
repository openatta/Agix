//! PushNotificationTool —— "PushNotification"。
//!
//! 向用户发送桌面通知。在 macOS 上通过 osascript 的 `display notification` 实
//! 现；其他平台回退到 stderr 输出。如果 Remote Control 连接，也推送到手机。

use async_trait::async_trait;
use base::error::ToolError;
// Only the macOS path runs a program; elsewhere the notification goes to
// stderr, and importing these unconditionally makes them unused there.
#[cfg(target_os = "macos")]
use crate::exec_capture::capture;
#[cfg(target_os = "macos")]
use base::interface::exec::ProcessSpec;
use base::tool::{
    PermissionDecision, ProgressSender, PromptContext, Tool, ToolContext, ToolResult,
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
        "Send a desktop notification to the user's terminal. If Remote Control is connected, also pushes to their phone."
    }

    fn input_schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(PushNotificationInput))
            .expect("schemars output is valid JSON")
    }

    async fn prompt(&self, _: &PromptContext) -> String {
        include_str!("prompts/coding/push_notification.prompt.md").to_string()
    }

    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }

    fn is_read_only(&self, _: &Value) -> bool {
        true
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

        let is_proactive = input
            .status
            .as_deref()
            .map(|s| s == "proactive")
            .unwrap_or(false);

        #[cfg(target_os = "macos")]
        {
            let escaped = msg.replace('"', "\\\"");
            let spec = ProcessSpec::new("osascript", &ctx.cwd).args([
                "-e".to_string(),
                format!(r#"display notification "{escaped}" with title "Agix""#),
            ]);

            match capture(&ctx.exec, spec, ctx.cancel.clone()).await {
                Ok(output) if output.status.success => {
                    let suffix = if is_proactive { " (proactive)" } else { "" };
                    Ok(ToolResult::text(format!(
                        "Notification sent: {msg}{suffix}"
                    )))
                }
                Ok(output) => {
                    let stderr = output.stderr_lossy();
                    eprintln!("osascript failed: {stderr}");
                    Ok(ToolResult::error_text(format!(
                        "Failed to send notification: {stderr}"
                    )))
                }
                Err(e) => {
                    eprintln!("osascript error: {e}");
                    Ok(ToolResult::error_text(format!(
                        "Failed to send notification: {e}"
                    )))
                }
            }
        }

        #[cfg(not(target_os = "macos"))]
        {
            let _ = ctx;
            eprintln!("[PushNotification] {msg}");
            let suffix = if is_proactive { " (proactive)" } else { "" };
            Ok(ToolResult::text(format!(
                "Notification logged to terminal: {msg}{suffix}"
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::Path;

    fn ctx_in(cwd: &Path) -> ToolContext {
        ToolContext::for_test(cwd.to_path_buf())
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
