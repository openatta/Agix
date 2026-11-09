//! `TelemetryEvent` 枚举 + 各 payload 结构化定义。
//!
//! 每一种事件都有生产者：`crates/telemetry/tests/producers.rs` 逐个 kind 查
//! 工作区里有没有构造它的地方，没有就失败。加一种事件要连同发它的地方一起加。

use serde::Serialize;
use time::OffsetDateTime;
use uuid::Uuid;

pub mod inventory;
mod mcp;
mod model;
mod session;
mod team;
mod tool;

pub use mcp::{McpConnectionErrorPayload, McpServerConnectedPayload};
pub use model::{ApiErrorPayload, ApiRequestPayload, BudgetEnforcedAction, BudgetEnforcedPayload};
pub use session::{SessionEndPayload, SessionStartPayload, StartupTimingPayload};
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

/// One line per event kind: when it is emitted, the constructor, the
/// variant, its payload, and which file records it. The constructor's name is
/// the kind the event serializes to; the doc line is the "when" column of the
/// inventory in `docs/telemetry.md`.
macro_rules! event_kinds {
    ($($(#[doc = $doc:literal])+ $kind:ident => $variant:ident($payload:ty) in $recorded:ident;)*) => {
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

            /// Every kind with what the inventory says about it, in
            /// declaration order.
            pub fn inventory() -> Vec<inventory::KindInfo> {
                vec![$(inventory::KindInfo {
                    kind: stringify!($kind),
                    when: concat!($($doc),+),
                    recorded: inventory::Recorded::$recorded,
                    schema: || schemars::schema_for!($payload),
                },)*]
            }
        }

        impl TelemetryEvent {
            $(
                $(#[doc = $doc])+
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
    /// daemon 建好一个会话之后、它的第一轮之前;新建与按 id 重建都算。
    session_start => SessionStart(SessionStartPayload) in Session;
    /// 会话的主循环退出(关闭、输入通道关闭、被取消);合计是这次载入以来的。
    session_end => SessionEnd(SessionEndPayload) in Session;
    /// daemon 启动完成、开始接请求时,一次。
    startup_timing => StartupTiming(StartupTimingPayload) in Pool;
    /// 每次模型调用完成。一轮里的调用不带 `purpose`;辅助调用带:`compact`、`memory`、`title`、`hook`、`team_judge`、`web_search`、`web_fetch`。
    api_request => ApiRequest(ApiRequestPayload) in Session;
    /// 一轮里的一次模型调用失败:请求被拒,或流在回复完整之前断了。每次失败的尝试一条,不论这一轮后来恢复没有;取消不算。
    api_error => ApiError(ApiErrorPayload) in Session;
    /// 工具开始执行:PreToolUse hook、输入校验、权限都放行之后。
    tool_start => ToolStart(ToolStartPayload) in Session;
    /// 工具执行结束,成功或失败;工具自己报告的失败(如 Bash 非零退出)算失败。
    tool_execution => ToolExecution(ToolExecutionPayload) in Session;
    /// 一次工具调用在等权限回答时会话被关闭。
    tool_cancelled => ToolCancelled(ToolCancelledPayload) in Session;
    /// 会话的权限检查对一次工具调用作出判定,在询问任何人之前。
    permission_decision => PermissionDecision(PermissionDecisionPayload) in Session;
    /// 一轮开始,紧接 `turn_start`:开这一轮的用户消息,只记大小。
    user_prompt_submit => UserPromptSubmit(UserPromptSubmitPayload) in Session;
    /// 开一轮的消息是斜杠命令。
    slash_command_used => SlashCommandUsed(SlashCommandUsedPayload) in Session;
    /// 一轮开始,UserPromptSubmit hook 放行之后。
    turn_start => TurnStart(TurnStartPayload) in Session;
    /// 一轮结束,`stop_reason` 说怎么结束的。
    turn_complete => TurnComplete(TurnCompletePayload) in Session;
    /// 会话 token 预算到 90% 注入收尾提醒,或到 100% 中止这一轮。
    budget_enforced => BudgetEnforced(BudgetEnforcedPayload) in Session;
    /// 一次压缩完成或失败。
    compact_action => CompactAction(CompactActionPayload) in Session;
    /// 会话从日志重建(会话文件);恢复请求落到已在内存里、找不到、已结束的侧链会话上(池文件);团队从 scratchpad 接上(会话文件)。
    resume_action => ResumeAction(ResumeActionPayload) in Both;
    /// 一个 MCP server 连上:启动时、重载时、新增时。
    mcp_server_connected => McpServerConnected(McpServerConnectedPayload) in Pool;
    /// 一个要连的 MCP server 没连上:起不来、握手失败或超时、配置被拒。
    mcp_connection_error => McpConnectionError(McpConnectionErrorPayload) in Pool;
    /// 子 Agent 开始:`Agent` 工具起的(前台、后台)、常驻团队成员、`TeamCreate` 流水线的成员。
    agent_spawned => AgentSpawned(AgentSpawnedPayload) in Session;
    /// 子 Agent 结束;常驻团队成员在被停下时。
    agent_completed => AgentCompleted(AgentCompletedPayload) in Session;
    /// `TeamCreate` 流水线的一个阶段跑完。
    team_stage_complete => TeamStageComplete(TeamStageCompletePayload) in Session;
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResumeOutcome {
    Succeeded,
    Partial,
    Degraded,
    Failed,
}

/// 单轮开始事件的载荷。
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct TurnStartPayload {
    pub resumed: bool,
    pub is_retry: bool,
}

/// 单轮完成事件的载荷。
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
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
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
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
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
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
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct UserPromptSubmitPayload {
    pub char_count: u32,
    pub attachment_count: u32,
}

/// A turn whose message is a slash command.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
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
