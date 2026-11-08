//! Model / LLM API events.

use serde::Serialize;

use crate::RedactionPolicy;

/// One completed model call: what it cost and what it was.
///
/// Emitted per call, which is what makes it the event a cost can be computed
/// from — a turn is several calls, and every other number the engine reports is
/// per turn.
///
/// # What is not on it
///
/// It had four more fields and no producer at all. Each of them was a number
/// nothing could have filled truthfully, and a telemetry field that is always
/// zero is worse than an absent one: it reads as a measurement.
///
/// - `cache_creation` / `cache_read` — the cross-provider [`Usage`] carries
///   input and output and nothing else. Cache accounting is Anthropic's shape;
///   OpenAI's chunk usage has no equivalent, so putting it on the shared type
///   would make every provider answer a question one of them asks.
/// - `ttfb_ms` — time to first token is measured where the stream is consumed,
///   which is not where a call is accounted for.
/// - `retry_count` — retries happen inside the client, under the model
///   contract, and do not surface through it.
///
/// [`Usage`]: base::interface::model::Usage
#[derive(Debug, Clone, Serialize)]
pub struct ApiRequestPayload {
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Tokens this call wrote into the prompt cache, and tokens it read back
    /// from it. Separate fields because they are separate prices — a read is
    /// a fraction of ordinary input and a write is a premium on it — and
    /// `crate::cost` has priced all four since it was written, with nothing
    /// feeding it the last two.
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

/// API 请求失败事件的载荷。
#[derive(Debug, Clone, Serialize)]
pub struct ApiErrorPayload {
    pub model: String,
    pub error_kind: String,
    pub http_status: u16,
    pub error_message: String,
    pub latency_ms: u64,
    pub retry_count: u32,
}

/// 模型路由决策事件的载荷（含 fallback）。
#[derive(Debug, Clone, Serialize)]
pub struct ModelRoutePayload {
    pub requested_model: String,
    pub resolved_model: String,
    pub reason: String,
    pub is_fallback: bool,
    pub is_escalated: bool,
}

/// 意图分类事件的载荷。
#[derive(Debug, Clone, Serialize)]
pub struct IntentClassifiedPayload {
    pub heuristic_class: Option<String>,
    pub llm_class: Option<String>,
    pub llm_latency_ms: u64,
    pub cache_hit: bool,
}

/// 上下文窗口状态报告事件的载荷（压缩决策前）。
#[derive(Debug, Clone, Serialize)]
pub struct ContextWindowReportPayload {
    pub total_tokens: u64,
    pub message_count: usize,
    pub tool_result_tokens: u64,
    pub compact_eligible: bool,
    pub exceeded_threshold: bool,
    pub threshold_pct: f64,
}

/// 会话 token 预算强制执行事件的载荷——`max_budget_tokens` 90% 提醒注入
/// 或 100% 硬停两种动作都用这个 payload，用 `action` 区分。
#[derive(Debug, Clone, Serialize)]
pub struct BudgetEnforcedPayload {
    pub action: BudgetEnforcedAction,
    pub total_tokens_used: u64,
    pub budget: u64,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BudgetEnforcedAction {
    /// 达到 90% 阈值，向对话注入"请收尾"提醒，轮次继续。
    WarningInjected,
    /// 达到 100% 阈值，轮次被硬性中止。
    TurnStopped,
}

impl ApiErrorPayload {
    pub(crate) fn redact(mut self, _policy: &RedactionPolicy) -> Self {
        self.error_message = crate::events::redact_string(&self.error_message);
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
                error_kind: "rate_limited".into(),
                http_status: 429,
                error_message: "You have exceeded your rate limit".into(),
                latency_ms: 500,
                retry_count: 0,
            },
        )
        .redact(&policy);
        assert_eq!(event.kind(), "api_error");
        let v =
            serde_json::to_value(event).expect("serialization of telemetry event should not fail");
        assert_eq!(v["error_message"], "[REDACTED]");
        assert_eq!(v["error_kind"], "rate_limited");
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
    fn intent_classified_serializes() {
        let event = TelemetryEvent::intent_classified(
            "sess",
            1,
            None,
            IntentClassifiedPayload {
                heuristic_class: Some("coding".into()),
                llm_class: None,
                llm_latency_ms: 0,
                cache_hit: true,
            },
        );
        assert_eq!(event.kind(), "intent_classified");
        let v =
            serde_json::to_value(event).expect("serialization of telemetry event should not fail");
        assert_eq!(v["type"], "intent_classified");
        assert_eq!(v["heuristic_class"], "coding");
    }

    #[test]
    fn context_window_report_serializes() {
        let event = TelemetryEvent::context_window_report(
            "sess",
            5,
            None,
            ContextWindowReportPayload {
                total_tokens: 45000,
                message_count: 12,
                tool_result_tokens: 20000,
                compact_eligible: true,
                exceeded_threshold: true,
                threshold_pct: 80.0,
            },
        );
        assert_eq!(event.kind(), "context_window_report");
        let v =
            serde_json::to_value(event).expect("serialization of telemetry event should not fail");
        assert_eq!(v["type"], "context_window_report");
        assert_eq!(v["total_tokens"], 45000);
        assert_eq!(v["exceeded_threshold"], true);
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

    #[test]
    fn model_route_serializes() {
        let event = TelemetryEvent::model_route(
            "sess",
            1,
            None,
            ModelRoutePayload {
                requested_model: "claude-sonnet-4-6".into(),
                resolved_model: "claude-opus-4-6".into(),
                reason: "rate_limited".into(),
                is_fallback: true,
                is_escalated: false,
            },
        );
        assert_eq!(event.kind(), "model_route");
        let v =
            serde_json::to_value(event).expect("serialization of telemetry event should not fail");
        assert_eq!(v["type"], "model_route");
        assert_eq!(v["resolved_model"], "claude-opus-4-6");
        assert_eq!(v["is_fallback"], true);
    }
}
