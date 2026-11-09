//! `TelemetryEvent` 枚举 + 各 payload 结构化定义。
//!
//! 每一种事件都有生产者：`crates/telemetry/tests/producers.rs` 逐个 kind 查
//! 工作区里有没有构造它的地方，没有就失败。加一种事件要连同发它的地方一起加。

use serde::Serialize;
use time::OffsetDateTime;
use uuid::Uuid;

mod mcp;
mod model;
mod session;
mod team;
mod tool;

pub use mcp::{McpConnectionErrorPayload, McpServerConnectedPayload};
pub use model::{ApiErrorPayload, ApiRequestPayload, BudgetEnforcedAction, BudgetEnforcedPayload};
pub use session::{
    SessionEndPayload, SessionStartPayload, ShutdownSignalPayload, StartupTimingPayload,
};
pub use team::{AgentCompletedPayload, AgentSpawnedPayload, TeamStageCompletePayload};
pub use tool::{
    PermissionDecisionOutcome, PermissionDecisionPayload, ToolCancelledPayload,
    ToolExecutionPayload, ToolOutcome, ToolStartPayload,
};

/// The version of the record shape. 2: payloads no longer repeat the
/// envelope's `turn_no` / `turn_id`, and the kinds nothing emitted are gone.
pub const SCHEMA_VERSION: u32 = 2;

/// 一个遥测事件（转成 JSON 时展平 `event_id`/`session_id`/`timestamp` 在顶层）。
#[derive(Debug, Clone, Serialize)]
pub struct TelemetryEvent {
    /// UUID v4，client 生成。
    pub event_id: Uuid,
    /// 关联 session。
    pub session_id: String,
    /// 当前 turn 编号（从 0 计）。
    pub turn_no: u32,
    /// 关联 turn 的 ID。None = 会话级事件（无 turn 关联）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    /// Client 本地 Unix 毫秒。
    pub timestamp_ms: i64,
    /// [`SCHEMA_VERSION`].
    pub schema_version: u32,
    /// 事件体。
    #[serde(flatten)]
    pub payload: EventPayload,
}

/// One line per event kind: the constructor, the variant, its payload. The
/// constructor's name is the kind the event serializes to.
macro_rules! event_kinds {
    ($($(#[$doc:meta])* $kind:ident => $variant:ident($payload:ty);)*) => {
        /// 事件体枚举 —— 每种变体序列化为 `type` discriminator + 字段。
        #[derive(Debug, Clone, Serialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        pub enum EventPayload {
            $($variant($payload),)*
        }

        impl EventPayload {
            /// Every kind, in declaration order.
            pub const KINDS: &'static [&'static str] = &[$(stringify!($kind),)*];

            /// Stable discriminator, identical to the `type` field this payload
            /// serializes to. Used as the config key for per-event-type toggles
            /// (see `TelemetryConfig::event_toggles`).
            pub fn kind(&self) -> &'static str {
                match self {
                    $(EventPayload::$variant(_) => stringify!($kind),)*
                }
            }
        }

        impl TelemetryEvent {
            $(
                $(#[$doc])*
                pub fn $kind(
                    session_id: &str,
                    turn_no: u32,
                    turn_id: Option<String>,
                    p: $payload,
                ) -> Self {
                    Self::new(session_id, turn_no, turn_id, EventPayload::$variant(p))
                }
            )*
        }
    };
}

event_kinds! {
    /// Session 开始。
    session_start => SessionStart(SessionStartPayload);
    /// Session 结束。
    session_end => SessionEnd(SessionEndPayload);
    /// daemon 启动完成，可以接请求了（记在池级遥测里）。
    startup_timing => StartupTiming(StartupTimingPayload);
    /// 一次模型调用完成。
    api_request => ApiRequest(ApiRequestPayload);
    /// 一次模型调用失败。
    api_error => ApiError(ApiErrorPayload);
    /// 工具开始执行。
    tool_start => ToolStart(ToolStartPayload);
    /// 工具执行完成（含 outcome）。
    tool_execution => ToolExecution(ToolExecutionPayload);
    /// 工具被取消。
    tool_cancelled => ToolCancelled(ToolCancelledPayload);
    /// 会话的权限检查对一次工具调用的判定。
    permission_decision => PermissionDecision(PermissionDecisionPayload);
    /// 一轮开始时用户送来的消息（只记大小）。
    user_prompt_submit => UserPromptSubmit(UserPromptSubmitPayload);
    /// 一轮的消息是斜杠命令。
    slash_command_used => SlashCommandUsed(SlashCommandUsedPayload);
    /// 单轮开始。
    turn_start => TurnStart(TurnStartPayload);
    /// 单轮完成。
    turn_complete => TurnComplete(TurnCompletePayload);
    /// Token 预算强制执行。
    budget_enforced => BudgetEnforced(BudgetEnforcedPayload);
    /// 压缩行为。
    compact_action => CompactAction(CompactActionPayload);
    /// Resume / replay 结果。
    resume_action => ResumeAction(ResumeActionPayload);
    /// 会话退出。
    shutdown_signal => ShutdownSignal(ShutdownSignalPayload);
    /// MCP 服务器连上（记在池级遥测里）。
    mcp_server_connected => McpServerConnected(McpServerConnectedPayload);
    /// MCP 服务器没连上（记在池级遥测里）。
    mcp_connection_error => McpConnectionError(McpConnectionErrorPayload);
    /// 子 Agent 启动。
    agent_spawned => AgentSpawned(AgentSpawnedPayload);
    /// 子 Agent 结束。
    agent_completed => AgentCompleted(AgentCompletedPayload);
    /// Team 流水线阶段完成。
    team_stage_complete => TeamStageComplete(TeamStageCompletePayload);
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResumeOutcome {
    Succeeded,
    Partial,
    Degraded,
    Failed,
}

/// 单轮开始事件的载荷。
#[derive(Debug, Clone, Serialize)]
pub struct TurnStartPayload {
    pub resumed: bool,
    pub is_retry: bool,
}

/// 单轮完成事件的载荷。
#[derive(Debug, Clone, Serialize)]
pub struct TurnCompletePayload {
    pub stop_reason: String,
    pub api_calls: u32,
    pub tool_calls: u32,
    pub permission_denials: u32,
    pub last_tool_name: Option<String>,
    pub last_tool_was_error: bool,
    pub turn_duration_ms: u64,
}

/// 压缩行为事件的载荷。
#[derive(Debug, Clone, Serialize)]
pub struct CompactActionPayload {
    pub strategy: String,
    pub before_tokens: u64,
    pub after_tokens: u64,
    pub before_message_count: usize,
    pub after_message_count: usize,
    pub success: bool,
    pub latency_ms: u64,
}

/// Resume / replay 结果。Payload 只记录结构化类别和计数，不包含 transcript 内容。
#[derive(Debug, Clone, Serialize)]
pub struct ResumeActionPayload {
    pub outcome: ResumeOutcome,
    pub source: String,
    pub entry_count: usize,
    pub projected_message_count: usize,
    pub compact_boundary_count: usize,
    pub sidechain_entry_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning_kind: Option<String>,
    pub latency_ms: u64,
}

/// The message a user turn opens with — its size, never its text.
#[derive(Debug, Clone, Serialize)]
pub struct UserPromptSubmitPayload {
    pub char_count: u32,
    pub attachment_count: u32,
}

/// A turn whose message is a slash command.
#[derive(Debug, Clone, Serialize)]
pub struct SlashCommandUsedPayload {
    /// The command's name, without the leading `/`.
    pub command: String,
    /// `local` (run by the engine), `prompt` (a skill or command file
    /// expanded into the turn), `mcp_prompt`, or `unknown` (sent to the model
    /// as typed).
    pub kind: String,
    pub has_arg: bool,
}

impl TelemetryEvent {
    fn new(session_id: &str, turn_no: u32, turn_id: Option<String>, payload: EventPayload) -> Self {
        Self {
            event_id: Uuid::new_v4(),
            session_id: session_id.to_string(),
            turn_no,
            turn_id,
            timestamp_ms: (OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64,
            schema_version: SCHEMA_VERSION,
            payload,
        }
    }

    /// 事件类型的稳定标识（等同序列化后的 `type` 字段），用于按点过滤配置。
    pub fn kind(&self) -> &'static str {
        self.payload.kind()
    }

    /// Scrub the free-text fields (error messages) as `policy` says. No
    /// event carries a prompt or a tool's input or output.
    pub fn redact(self, policy: &crate::RedactionPolicy) -> Self {
        let payload = match self.payload {
            EventPayload::ApiError(p) => EventPayload::ApiError(p.redact(policy)),
            EventPayload::ToolExecution(p) => EventPayload::ToolExecution(p.redact(policy)),
            EventPayload::McpConnectionError(p) => {
                EventPayload::McpConnectionError(p.redact(policy))
            }
            other => other,
        };
        Self { payload, ..self }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn object_keys_are_unique(json: &str) -> bool {
        // A flattened payload with a field named like an envelope field
        // serializes the key twice; serde_json::Value would keep only one.
        let mut depth = 0usize;
        let mut keys = HashSet::new();
        let mut chars = json.chars().peekable();
        let mut expecting_key = false;
        while let Some(c) = chars.next() {
            match c {
                '{' => {
                    depth += 1;
                    expecting_key = depth == 1;
                }
                '}' | ']' => depth -= 1,
                '[' => depth += 1,
                ',' if depth == 1 => expecting_key = true,
                '"' => {
                    let mut text = String::new();
                    while let Some(c) = chars.next() {
                        match c {
                            '\\' => {
                                chars.next();
                            }
                            '"' => break,
                            c => text.push(c),
                        }
                    }
                    if expecting_key && depth == 1 && !keys.insert(text) {
                        return false;
                    }
                    expecting_key = false;
                }
                _ => {}
            }
        }
        true
    }

    #[test]
    fn no_event_repeats_an_envelope_key() {
        let events = [
            TelemetryEvent::turn_start(
                "sess",
                3,
                Some("t".into()),
                TurnStartPayload {
                    resumed: false,
                    is_retry: false,
                },
            ),
            TelemetryEvent::turn_complete(
                "sess",
                3,
                Some("t".into()),
                TurnCompletePayload {
                    stop_reason: "end_turn".into(),
                    api_calls: 1,
                    tool_calls: 0,
                    permission_denials: 0,
                    last_tool_name: None,
                    last_tool_was_error: false,
                    turn_duration_ms: 1,
                },
            ),
            TelemetryEvent::user_prompt_submit(
                "sess",
                3,
                Some("t".into()),
                UserPromptSubmitPayload {
                    char_count: 5,
                    attachment_count: 0,
                },
            ),
        ];
        for event in events {
            let json = serde_json::to_string(&event).unwrap();
            assert!(object_keys_are_unique(&json), "{json}");
            assert!(json.contains("\"schema_version\":2"), "{json}");
        }
    }

    #[test]
    fn the_duplicate_key_check_sees_a_duplicate() {
        assert!(!object_keys_are_unique(
            r#"{"turn_no":1,"x":{"turn_no":2},"turn_no":1}"#
        ));
        assert!(object_keys_are_unique(r#"{"turn_no":1,"x":{"turn_no":2}}"#));
    }

    #[test]
    fn kinds_are_unique_and_match_kind() {
        let unique: HashSet<_> = EventPayload::KINDS.iter().collect();
        assert_eq!(unique.len(), EventPayload::KINDS.len());
        let event = TelemetryEvent::slash_command_used(
            "sess",
            1,
            None,
            SlashCommandUsedPayload {
                command: "help".into(),
                kind: "local".into(),
                has_arg: false,
            },
        );
        assert!(EventPayload::KINDS.contains(&event.kind()));
        let v = serde_json::to_value(event).unwrap();
        assert_eq!(v["type"], "slash_command_used");
        assert_eq!(v["command"], "help");
    }

    #[test]
    fn resume_action_uses_stable_outcome_taxonomy() {
        for (outcome, expected) in [
            (ResumeOutcome::Succeeded, "succeeded"),
            (ResumeOutcome::Partial, "partial"),
            (ResumeOutcome::Degraded, "degraded"),
            (ResumeOutcome::Failed, "failed"),
        ] {
            let event = TelemetryEvent::resume_action(
                "sess",
                3,
                None,
                ResumeActionPayload {
                    outcome,
                    source: "jsonl".into(),
                    entry_count: 12,
                    projected_message_count: 7,
                    compact_boundary_count: 1,
                    sidechain_entry_count: 2,
                    warning_kind: Some("missing_sidecar".into()),
                    latency_ms: 9,
                },
            );
            assert_eq!(event.kind(), "resume_action");
            let value = serde_json::to_value(event).unwrap();
            assert_eq!(value["outcome"], expected);
            assert_eq!(value["warning_kind"], "missing_sidecar");
        }
    }

    #[test]
    fn compact_action_serializes() {
        let event = TelemetryEvent::compact_action(
            "sess",
            5,
            None,
            CompactActionPayload {
                strategy: "auto".into(),
                before_tokens: 1000,
                after_tokens: 200,
                before_message_count: 20,
                after_message_count: 5,
                success: true,
                latency_ms: 50,
            },
        );
        let v = serde_json::to_value(event).unwrap();
        assert_eq!(v["type"], "compact_action");
        assert_eq!(v["before_tokens"], 1000);
    }
}
