//! Model / LLM API events.

use serde::Serialize;

use crate::RedactionPolicy;

/// One completed model call: what it cost and what it was.
///
/// Emitted per call, which is what makes it the event a cost can be computed
/// from — a turn is several calls, and every other number the engine reports is
/// per turn.
///
/// No time-to-first-token and no retry count: the first is measured where the
/// stream is consumed, not where a call is accounted for, and retries happen
/// inside the client, under the model contract.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ApiRequestPayload {
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Tokens this call wrote into the prompt cache, and tokens it read back
    /// from it. Separate fields because they are separate prices — a read is
    /// a fraction of ordinary input and a write is a premium on it.
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    /// Wall clock for the whole call, request to last event.
    pub latency_ms: u64,
    pub stop_reason: String,
    pub input_message_count: usize,
    pub tool_count: usize,
    /// Whether this call went to the model the session was configured with. A
    /// fallback or a recovery switch makes it false.
    pub default_model: bool,
    /// What this call was for, when it was not a turn: extracting memories,
    /// summarizing for compaction, answering a prompt hook. `None` is a call
    /// the turn loop made, which is the common case and the one a reader
    /// summing a turn's cost wants on its own.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub purpose: Option<String>,
}

/// A model call that failed: the provider refused it, or its stream broke
/// before the reply was complete. Emitted once per failed attempt, whether
/// or not the turn then recovers.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ApiErrorPayload {
    pub model: String,
    /// `api`, `auth`, `rate_limited`, `overloaded`, `network`, `internal`,
    /// `unsupported`.
    pub error_kind: String,
    /// The HTTP status, for an `api` error.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    pub error_message: String,
    pub latency_ms: u64,
}

/// 会话 token 预算强制执行事件的载荷——`max_budget_tokens` 90% 提醒注入
/// 或 100% 硬停两种动作都用这个 payload，用 `action` 区分。
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct BudgetEnforcedPayload {
    pub action: BudgetEnforcedAction,
    pub total_tokens_used: u64,
    pub budget: u64,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BudgetEnforcedAction {
    /// 达到 90% 阈值，向对话注入"请收尾"提醒，轮次继续。
    WarningInjected,
    /// 达到 100% 阈值，轮次被硬性中止。
    TurnStopped,
}

impl ApiErrorPayload {
    pub(crate) fn redact(mut self, policy: &RedactionPolicy) -> Self {
        self.error_message = policy.scrub_message(&self.error_message);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::TelemetryEvent;
    use crate::redact::RedactionPolicy;

    #[test]
    fn api_error_redacts_error_message() {
        let policy = RedactionPolicy::all();
        let event = TelemetryEvent::api_error(
            "sess",
            1,
            None,
            ApiErrorPayload {
                model: "claude".into(),
                error_kind: "api".into(),
                http_status: Some(429),
                error_message: "You have exceeded your rate limit".into(),
                latency_ms: 500,
            },
        )
        .redact(&policy);
        assert_eq!(event.kind(), "api_error");
        let v =
            serde_json::to_value(event).expect("serialization of telemetry event should not fail");
        assert_eq!(v["error_message"], "[REDACTED]");
        assert_eq!(v["error_kind"], "api");
        assert_eq!(v["http_status"], 429);
    }

    #[test]
    fn api_request_serializes() {
        let event = TelemetryEvent::api_request(
            "sess",
            1,
            None,
            ApiRequestPayload {
                model: "claude-sonnet-5".into(),
                input_tokens: 1200,
                output_tokens: 300,
                cache_creation_tokens: 64,
                cache_read_tokens: 900,
                latency_ms: 2100,
                stop_reason: "end_turn".into(),
                input_message_count: 5,
                tool_count: 12,
                default_model: true,
                purpose: None,
            },
        );
        assert_eq!(event.kind(), "api_request");
        let v =
            serde_json::to_value(event).expect("serialization of telemetry event should not fail");
        assert_eq!(v["type"], "api_request");
        assert_eq!(v["input_tokens"], 1200);
        assert_eq!(v["cache_read_tokens"], 900);
    }

    #[test]
    fn budget_enforced_serializes_both_actions() {
        for (action, expected) in [
            (BudgetEnforcedAction::WarningInjected, "warning_injected"),
            (BudgetEnforcedAction::TurnStopped, "turn_stopped"),
        ] {
            let event = TelemetryEvent::budget_enforced(
                "sess",
                3,
                None,
                BudgetEnforcedPayload {
                    action,
                    total_tokens_used: 950,
                    budget: 1000,
                },
            );
            assert_eq!(event.kind(), "budget_enforced");
            let v = serde_json::to_value(event)
                .expect("serialization of telemetry event should not fail");
            assert_eq!(v["type"], "budget_enforced");
            assert_eq!(v["action"], expected);
            assert_eq!(v["budget"], 1000);
        }
    }
}
