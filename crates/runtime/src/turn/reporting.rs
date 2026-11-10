//! 回合内的遥测记录：turn_complete、API 错误、斜杠命令、权限决定。

use super::process::hook_permission_mode;
use super::tool_exec::ToolExecCtx;
use crate::agent::Agent;
use std::sync::Arc;

/// The head of a failed call's text, as much of it as a telemetry line keeps.
pub(super) fn telemetry_error_text(text: &str) -> String {
    const KEPT_CHARS: usize = 512;
    match text.char_indices().nth(KEPT_CHARS) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_string(),
    }
}

pub(super) fn record_permission_decision(
    ctx: &ToolExecCtx,
    tool: &Arc<dyn base::tool::Tool>,
    name: &str,
    input: &serde_json::Value,
    checked: &base::interface::permission::PermissionOutcome,
    started: std::time::Instant,
) {
    use base::interface::permission::PermissionOutcome;
    let outcome = match checked {
        PermissionOutcome::Permit => telemetry::PermissionDecisionOutcome::AutoAllow,
        PermissionOutcome::Deny { .. } => telemetry::PermissionDecisionOutcome::Deny,
        PermissionOutcome::Prompt { .. } => telemetry::PermissionDecisionOutcome::Ask,
    };
    let _ = ctx
        .telemetry_handle
        .record(telemetry::TelemetryEvent::permission_decision(
            &ctx.session_id,
            ctx.turn_no,
            Some(ctx.turn_id.clone()),
            telemetry::PermissionDecisionPayload {
                tool_name: name.to_string(),
                outcome,
                permission_mode: hook_permission_mode(ctx.session_state.permission_mode()),
                destructive: tool.is_destructive(input),
                latency_ms: started.elapsed().as_millis() as u64,
            },
        ));
}

impl Agent {
    /// Telemetry `turn_complete` for the paths that end a turn early and
    /// know nothing about its last tool. The `AgentEvent` is `finish_turn`'s.
    pub(super) fn record_turn_complete(
        &self,
        stop_reason: &str,
        api_calls: u32,
        tool_calls: u32,
        start: std::time::Instant,
    ) {
        self.record_turn_complete_in(
            self.session.turn_count,
            stop_reason,
            api_calls,
            tool_calls,
            start,
        );
    }

    /// For the one path that has already advanced `turn_count` and still
    /// reports the turn under the number it ran as.
    pub(super) fn record_turn_complete_in(
        &self,
        turn_no: u32,
        stop_reason: &str,
        api_calls: u32,
        tool_calls: u32,
        start: std::time::Instant,
    ) {
        let tid = self.current_turn_id.clone();
        let _ = self
            .telemetry
            .telemetry_handle
            .record(telemetry::TelemetryEvent::turn_complete(
                &self.session.session_id,
                turn_no,
                Some(tid.clone()),
                telemetry::TurnCompletePayload {
                    stop_reason: stop_reason.to_string(),
                    api_calls,
                    tool_calls,
                    permission_denials: self
                        .gate
                        .permission_denial_count
                        .load(std::sync::atomic::Ordering::Relaxed),
                    last_tool_name: None,
                    last_tool_was_error: false,
                    turn_duration_ms: start.elapsed().as_millis() as u64,
                },
            ));
    }

    pub(super) fn record_api_error(
        &self,
        model: &str,
        error: &base::interface::model::ModelError,
        started: std::time::Instant,
    ) {
        use base::interface::model::ModelError;
        let (error_kind, http_status) = match error {
            ModelError::Cancelled | ModelError::NotRecorded(_) => return,
            ModelError::Api { status, .. } => ("api", Some(*status)),
            ModelError::Auth(_) => ("auth", None),
            ModelError::RateLimited => ("rate_limited", None),
            ModelError::Overloaded => ("overloaded", None),
            ModelError::Network(_) => ("network", None),
            ModelError::Internal(_) => ("internal", None),
            ModelError::Unsupported(_) => ("unsupported", None),
        };
        let _ = self
            .telemetry
            .telemetry_handle
            .record(telemetry::TelemetryEvent::api_error(
                &self.session.session_id,
                self.session.turn_count,
                Some(self.current_turn_id.clone()),
                telemetry::ApiErrorPayload {
                    model: model.to_string(),
                    error_kind: error_kind.to_string(),
                    http_status,
                    error_message: error.to_string(),
                    latency_ms: started.elapsed().as_millis() as u64,
                },
            ));
    }

    pub(super) fn record_slash_command(&self, command: &crate::commands::SlashCommand) {
        use crate::commands::Command;
        let kind = match self.tooling.commands.resolve(&command.name) {
            Some(Command::Local { .. }) => "local",
            Some(Command::Prompt { .. }) => "prompt",
            Some(Command::McpPrompt { .. }) => "mcp_prompt",
            None if self.tooling.mcp.has_prompt_command(&command.name) => "mcp_prompt",
            None => "unknown",
        };
        let _ =
            self.telemetry
                .telemetry_handle
                .record(telemetry::TelemetryEvent::slash_command_used(
                    &self.session.session_id,
                    self.session.turn_count,
                    Some(self.current_turn_id.clone()),
                    telemetry::SlashCommandUsedPayload {
                        command: command.name.clone(),
                        kind: kind.to_string(),
                        has_arg: !command.args.trim().is_empty(),
                    },
                ));
    }
}
