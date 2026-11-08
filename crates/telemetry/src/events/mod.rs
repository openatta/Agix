//! `TelemetryEvent` 枚举 + 各 payload 结构化定义。

use serde::Serialize;
use time::OffsetDateTime;
use uuid::Uuid;

mod mcp;
mod model;
mod session;
mod team;
mod tool;

pub use mcp::{
    McpConnectionErrorPayload, McpServerConnectedPayload, McpServerDisconnectedPayload,
    McpToolCallPayload,
};
pub use model::{
    ApiErrorPayload, ApiRequestPayload, BudgetEnforcedAction, BudgetEnforcedPayload,
    ContextWindowReportPayload, IntentClassifiedPayload, ModelRoutePayload,
};
pub use session::{
    ConfigLoadedPayload, SessionEndPayload, SessionStartPayload, ShutdownSignalPayload,
    StartupTimingPayload,
};
pub use team::{
    AgentCompletedPayload, AgentMessagePayload, AgentSpawnedPayload, TeamStageCompletePayload,
};
pub use tool::{
    PermissionDecisionOutcome, PermissionDecisionPayload, ToolCancelledPayload,
    ToolDecisionPayload, ToolExecutionPayload, ToolOutcome, ToolStartPayload,
};

/// 一个遥测事件（转成 JSON 时展平 `event_id`/`session_id`/`timestamp` 在顶层）。
#[derive(Debug, Clone, Serialize)]
pub struct TelemetryEvent {
    /// UUID v7，client 生成。
    pub event_id: Uuid,
    /// 关联 session。
    pub session_id: String,
    /// 当前 turn 编号（1-based）。
    pub turn_no: u32,
    /// 关联 turn 的 BASE58(UUID) ID。None = 会话级事件（无 turn 关联）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    /// Client 本地 Unix 毫秒（真毫秒精度，非秒×1000）。
    pub timestamp_ms: i64,
    /// Schema 版本号，初版为 1。用于向前兼容。
    pub schema_version: u32,
    /// 事件体。
    #[serde(flatten)]
    pub payload: EventPayload,
}

/// 事件体枚举 —— 每种变体序列化为 `type` discriminator + 字段。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventPayload {
    SessionStart(SessionStartPayload),
    SessionEnd(SessionEndPayload),
    ConfigLoaded(ConfigLoadedPayload),
    StartupTiming(StartupTimingPayload),
    ApiRequest(ApiRequestPayload),
    ApiError(ApiErrorPayload),
    ToolStart(ToolStartPayload),
    ToolExecution(ToolExecutionPayload),
    ToolCancelled(ToolCancelledPayload),
    ToolDecision(ToolDecisionPayload),
    PermissionDecision(PermissionDecisionPayload),
    IntentClassified(IntentClassifiedPayload),
    UserPromptSubmit(UserPromptSubmitPayload),
    SlashCommandUsed(SlashCommandUsedPayload),
    InterruptSignal(InterruptSignalPayload),
    TurnStart(TurnStartPayload),
    TurnComplete(TurnCompletePayload),
    ContextWindowReport(ContextWindowReportPayload),
    ModelRoute(ModelRoutePayload),
    BudgetEnforced(BudgetEnforcedPayload),
    CompactAction(CompactActionPayload),
    ResumeAction(ResumeActionPayload),
    ErrorRecord(ErrorRecordPayload),
    ShutdownSignal(ShutdownSignalPayload),
    McpServerConnected(McpServerConnectedPayload),
    McpServerDisconnected(McpServerDisconnectedPayload),
    McpToolCall(McpToolCallPayload),
    McpConnectionError(McpConnectionErrorPayload),
    AgentSpawned(AgentSpawnedPayload),
    AgentCompleted(AgentCompletedPayload),
    TeamStageComplete(TeamStageCompletePayload),
    AgentMessage(AgentMessagePayload),
    MemorySnapshot(MemorySnapshotPayload),
    FileOperation(FileOperationPayload),
    TuiAction(TuiActionPayload),
    HookExecution(HookExecutionPayload),
    OutcomeRecord(OutcomeRecordPayload),
}

impl EventPayload {
    /// Stable discriminator, identical to the `type` field this payload
    /// serializes to under `#[serde(tag = "type")]`. Used as the config key
    /// for per-event-type toggles (see `TelemetryConfig::event_toggles`).
    pub fn kind(&self) -> &'static str {
        match self {
            EventPayload::SessionStart(_) => "session_start",
            EventPayload::SessionEnd(_) => "session_end",
            EventPayload::ConfigLoaded(_) => "config_loaded",
            EventPayload::StartupTiming(_) => "startup_timing",
            EventPayload::ApiRequest(_) => "api_request",
            EventPayload::ApiError(_) => "api_error",
            EventPayload::ToolStart(_) => "tool_start",
            EventPayload::ToolExecution(_) => "tool_execution",
            EventPayload::ToolCancelled(_) => "tool_cancelled",
            EventPayload::ToolDecision(_) => "tool_decision",
            EventPayload::PermissionDecision(_) => "permission_decision",
            EventPayload::IntentClassified(_) => "intent_classified",
            EventPayload::UserPromptSubmit(_) => "user_prompt_submit",
            EventPayload::SlashCommandUsed(_) => "slash_command_used",
            EventPayload::InterruptSignal(_) => "interrupt_signal",
            EventPayload::TurnStart(_) => "turn_start",
            EventPayload::TurnComplete(_) => "turn_complete",
            EventPayload::ContextWindowReport(_) => "context_window_report",
            EventPayload::ModelRoute(_) => "model_route",
            EventPayload::BudgetEnforced(_) => "budget_enforced",
            EventPayload::CompactAction(_) => "compact_action",
            EventPayload::ResumeAction(_) => "resume_action",
            EventPayload::ErrorRecord(_) => "error_record",
            EventPayload::ShutdownSignal(_) => "shutdown_signal",
            EventPayload::McpServerConnected(_) => "mcp_server_connected",
            EventPayload::McpServerDisconnected(_) => "mcp_server_disconnected",
            EventPayload::McpToolCall(_) => "mcp_tool_call",
            EventPayload::McpConnectionError(_) => "mcp_connection_error",
            EventPayload::AgentSpawned(_) => "agent_spawned",
            EventPayload::AgentCompleted(_) => "agent_completed",
            EventPayload::TeamStageComplete(_) => "team_stage_complete",
            EventPayload::AgentMessage(_) => "agent_message",
            EventPayload::MemorySnapshot(_) => "memory_snapshot",
            EventPayload::FileOperation(_) => "file_operation",
            EventPayload::TuiAction(_) => "tui_action",
            EventPayload::HookExecution(_) => "hook_execution",
            EventPayload::OutcomeRecord(_) => "outcome_record",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CompactOutcome {
    Triggered,
    Skipped,
    Succeeded,
    Failed,
    Fallback,
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
    pub turn_no: u32,
    /// 当前 turn 的 BASE58(UUID) ID。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    pub resumed: bool,
    pub is_retry: bool,
}

/// 单轮完成事件的载荷。
#[derive(Debug, Clone, Serialize)]
pub struct TurnCompletePayload {
    pub turn_no: u32,
    /// 当前 turn 的 BASE58(UUID) ID。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
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

/// 错误记录事件的载荷。
#[derive(Debug, Clone, Serialize)]
pub struct ErrorRecordPayload {
    pub error_kind: String,
    pub error_message: String,
    pub context: String,
    pub related_tools: Vec<String>,
}

/// 内存快照事件的载荷。
#[derive(Debug, Clone, Serialize)]
pub struct MemorySnapshotPayload {
    pub rss_kb: u64,
    pub heap_kb: u64,
    pub turn_no: u32,
    pub elapsed_ms: u64,
}

/// 文件操作统计事件的载荷（聚合，非每次操作）。
#[derive(Debug, Clone, Serialize)]
pub struct FileOperationPayload {
    pub operation: String,
    pub file_count: u32,
    pub total_bytes: u64,
    pub duration_ms: u64,
    pub errors: u32,
}

/// TUI 交互动作事件的载荷。
#[derive(Debug, Clone, Serialize)]
pub struct TuiActionPayload {
    pub action: String,
    pub view: String,
    pub detail: Option<String>,
}

/// Hook 执行事件的载荷。
#[derive(Debug, Clone, Serialize)]
pub struct HookExecutionPayload {
    pub hook_name: String,
    pub tool_name: Option<String>,
    pub decision: String,
    pub latency_ms: u64,
    pub error_message: Option<String>,
}

/// 轻量 outcome 事件。用于 compact/tool/permission/resume 以外的临时诊断点。
#[derive(Debug, Clone, Serialize)]
pub struct OutcomeRecordPayload {
    pub category: String,
    pub outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub latency_ms: u64,
}

/// 用户提交 prompt 事件的载荷。
#[derive(Debug, Clone, Serialize)]
pub struct UserPromptSubmitPayload {
    pub char_count: u32,
    pub has_attachments: bool,
    pub attachment_count: u32,
    pub heuristic_class: Option<String>,
    pub turn_no: u32,
}

/// Slash 命令使用事件的载荷。
#[derive(Debug, Clone, Serialize)]
pub struct SlashCommandUsedPayload {
    pub command: String,
    pub is_alias: bool,
    pub resolved_command: String,
    pub has_arg: bool,
}

/// 用户中断事件的载荷（Ctrl-C 等）。
#[derive(Debug, Clone, Serialize)]
pub struct InterruptSignalPayload {
    pub signal: String,
    pub elapsed_ms: u64,
    pub turn_no: u32,
    pub in_tool_execution: bool,
    pub current_tool: Option<String>,
}

pub(crate) fn redact_string(s: &str) -> String {
    if s.is_empty() {
        return s.to_string();
    }
    "[REDACTED]".to_string()
}

impl TelemetryEvent {
    fn new(session_id: &str, turn_no: u32, turn_id: Option<String>, payload: EventPayload) -> Self {
        Self {
            event_id: Uuid::new_v4(),
            session_id: session_id.to_string(),
            turn_no,
            turn_id,
            timestamp_ms: (OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64,
            schema_version: 1,
            payload,
        }
    }

    /// Session 开始。
    pub fn session_start(
        session_id: &str,
        turn_no: u32,
        turn_id: Option<String>,
        p: SessionStartPayload,
    ) -> Self {
        Self::new(session_id, turn_no, turn_id, EventPayload::SessionStart(p))
    }

    /// Session 结束。
    pub fn session_end(
        session_id: &str,
        turn_no: u32,
        turn_id: Option<String>,
        p: SessionEndPayload,
    ) -> Self {
        Self::new(session_id, turn_no, turn_id, EventPayload::SessionEnd(p))
    }

    /// 配置加载（session 开始后立即触发）。
    pub fn config_loaded(
        session_id: &str,
        turn_no: u32,
        turn_id: Option<String>,
        p: ConfigLoadedPayload,
    ) -> Self {
        Self::new(session_id, turn_no, turn_id, EventPayload::ConfigLoaded(p))
    }

    /// 启动耗时分解。
    pub fn startup_timing(
        session_id: &str,
        turn_no: u32,
        turn_id: Option<String>,
        p: StartupTimingPayload,
    ) -> Self {
        Self::new(session_id, turn_no, turn_id, EventPayload::StartupTiming(p))
    }

    /// API 请求成功。
    pub fn api_request(
        session_id: &str,
        turn_no: u32,
        turn_id: Option<String>,
        p: ApiRequestPayload,
    ) -> Self {
        Self::new(session_id, turn_no, turn_id, EventPayload::ApiRequest(p))
    }

    /// API 请求失败。
    pub fn api_error(
        session_id: &str,
        turn_no: u32,
        turn_id: Option<String>,
        p: ApiErrorPayload,
    ) -> Self {
        Self::new(session_id, turn_no, turn_id, EventPayload::ApiError(p))
    }

    /// 工具开始执行。
    pub fn tool_start(
        session_id: &str,
        turn_no: u32,
        turn_id: Option<String>,
        p: ToolStartPayload,
    ) -> Self {
        Self::new(session_id, turn_no, turn_id, EventPayload::ToolStart(p))
    }

    /// 工具执行完成（含 outcome）。
    pub fn tool_execution(
        session_id: &str,
        turn_no: u32,
        turn_id: Option<String>,
        p: ToolExecutionPayload,
    ) -> Self {
        Self::new(session_id, turn_no, turn_id, EventPayload::ToolExecution(p))
    }

    /// 工具被取消。
    pub fn tool_cancelled(
        session_id: &str,
        turn_no: u32,
        turn_id: Option<String>,
        p: ToolCancelledPayload,
    ) -> Self {
        Self::new(session_id, turn_no, turn_id, EventPayload::ToolCancelled(p))
    }

    /// 权限决策（permission engine 层）。
    pub fn tool_decision(
        session_id: &str,
        turn_no: u32,
        turn_id: Option<String>,
        p: ToolDecisionPayload,
    ) -> Self {
        Self::new(session_id, turn_no, turn_id, EventPayload::ToolDecision(p))
    }

    /// 统一权限决策（用户面）。
    pub fn permission_decision(
        session_id: &str,
        turn_no: u32,
        turn_id: Option<String>,
        p: PermissionDecisionPayload,
    ) -> Self {
        Self::new(
            session_id,
            turn_no,
            turn_id,
            EventPayload::PermissionDecision(p),
        )
    }

    /// 意图分类。
    pub fn intent_classified(
        session_id: &str,
        turn_no: u32,
        turn_id: Option<String>,
        p: IntentClassifiedPayload,
    ) -> Self {
        Self::new(
            session_id,
            turn_no,
            turn_id,
            EventPayload::IntentClassified(p),
        )
    }

    /// 用户提交 prompt。
    pub fn user_prompt_submit(
        session_id: &str,
        turn_no: u32,
        turn_id: Option<String>,
        p: UserPromptSubmitPayload,
    ) -> Self {
        Self::new(
            session_id,
            turn_no,
            turn_id,
            EventPayload::UserPromptSubmit(p),
        )
    }

    /// Slash 命令使用。
    pub fn slash_command_used(
        session_id: &str,
        turn_no: u32,
        turn_id: Option<String>,
        p: SlashCommandUsedPayload,
    ) -> Self {
        Self::new(
            session_id,
            turn_no,
            turn_id,
            EventPayload::SlashCommandUsed(p),
        )
    }

    /// 用户中断（Ctrl-C 等）。
    pub fn interrupt_signal(
        session_id: &str,
        turn_no: u32,
        turn_id: Option<String>,
        p: InterruptSignalPayload,
    ) -> Self {
        Self::new(
            session_id,
            turn_no,
            turn_id,
            EventPayload::InterruptSignal(p),
        )
    }

    /// 单轮开始。
    pub fn turn_start(
        session_id: &str,
        turn_no: u32,
        turn_id: Option<String>,
        p: TurnStartPayload,
    ) -> Self {
        Self::new(session_id, turn_no, turn_id, EventPayload::TurnStart(p))
    }

    /// 单轮完成。
    pub fn turn_complete(
        session_id: &str,
        turn_no: u32,
        turn_id: Option<String>,
        p: TurnCompletePayload,
    ) -> Self {
        Self::new(session_id, turn_no, turn_id, EventPayload::TurnComplete(p))
    }

    /// 上下文窗口状态报告。
    pub fn context_window_report(
        session_id: &str,
        turn_no: u32,
        turn_id: Option<String>,
        p: ContextWindowReportPayload,
    ) -> Self {
        Self::new(
            session_id,
            turn_no,
            turn_id,
            EventPayload::ContextWindowReport(p),
        )
    }

    /// 模型路由决策（含 fallback）。
    pub fn model_route(
        session_id: &str,
        turn_no: u32,
        turn_id: Option<String>,
        p: ModelRoutePayload,
    ) -> Self {
        Self::new(session_id, turn_no, turn_id, EventPayload::ModelRoute(p))
    }

    /// 会话 token 预算强制执行（90% 提醒 / 100% 硬停）。
    pub fn budget_enforced(
        session_id: &str,
        turn_no: u32,
        turn_id: Option<String>,
        p: BudgetEnforcedPayload,
    ) -> Self {
        Self::new(
            session_id,
            turn_no,
            turn_id,
            EventPayload::BudgetEnforced(p),
        )
    }

    /// 压缩行为。
    pub fn compact_action(
        session_id: &str,
        turn_no: u32,
        turn_id: Option<String>,
        p: CompactActionPayload,
    ) -> Self {
        Self::new(session_id, turn_no, turn_id, EventPayload::CompactAction(p))
    }

    /// Resume / replay 行为。
    pub fn resume_action(
        session_id: &str,
        turn_no: u32,
        turn_id: Option<String>,
        p: ResumeActionPayload,
    ) -> Self {
        Self::new(session_id, turn_no, turn_id, EventPayload::ResumeAction(p))
    }

    /// 错误记录。
    pub fn error_record(
        session_id: &str,
        turn_no: u32,
        turn_id: Option<String>,
        p: ErrorRecordPayload,
    ) -> Self {
        Self::new(session_id, turn_no, turn_id, EventPayload::ErrorRecord(p))
    }

    /// 关闭信号（session 结束原因）。
    pub fn shutdown_signal(
        session_id: &str,
        turn_no: u32,
        turn_id: Option<String>,
        p: ShutdownSignalPayload,
    ) -> Self {
        Self::new(
            session_id,
            turn_no,
            turn_id,
            EventPayload::ShutdownSignal(p),
        )
    }

    /// MCP 服务器已连接。
    pub fn mcp_server_connected(
        session_id: &str,
        turn_no: u32,
        turn_id: Option<String>,
        p: McpServerConnectedPayload,
    ) -> Self {
        Self::new(
            session_id,
            turn_no,
            turn_id,
            EventPayload::McpServerConnected(p),
        )
    }

    /// MCP 服务器已断开。
    pub fn mcp_server_disconnected(
        session_id: &str,
        turn_no: u32,
        turn_id: Option<String>,
        p: McpServerDisconnectedPayload,
    ) -> Self {
        Self::new(
            session_id,
            turn_no,
            turn_id,
            EventPayload::McpServerDisconnected(p),
        )
    }

    /// MCP 工具调用。
    pub fn mcp_tool_call(
        session_id: &str,
        turn_no: u32,
        turn_id: Option<String>,
        p: McpToolCallPayload,
    ) -> Self {
        Self::new(session_id, turn_no, turn_id, EventPayload::McpToolCall(p))
    }

    /// MCP 连接错误。
    pub fn mcp_connection_error(
        session_id: &str,
        turn_no: u32,
        turn_id: Option<String>,
        p: McpConnectionErrorPayload,
    ) -> Self {
        Self::new(
            session_id,
            turn_no,
            turn_id,
            EventPayload::McpConnectionError(p),
        )
    }

    /// Agent 已创建。
    pub fn agent_spawned(
        session_id: &str,
        turn_no: u32,
        turn_id: Option<String>,
        p: AgentSpawnedPayload,
    ) -> Self {
        Self::new(session_id, turn_no, turn_id, EventPayload::AgentSpawned(p))
    }

    /// Agent 已完成。
    pub fn agent_completed(
        session_id: &str,
        turn_no: u32,
        turn_id: Option<String>,
        p: AgentCompletedPayload,
    ) -> Self {
        Self::new(
            session_id,
            turn_no,
            turn_id,
            EventPayload::AgentCompleted(p),
        )
    }

    /// Team 阶段完成。
    pub fn team_stage_complete(
        session_id: &str,
        turn_no: u32,
        turn_id: Option<String>,
        p: TeamStageCompletePayload,
    ) -> Self {
        Self::new(
            session_id,
            turn_no,
            turn_id,
            EventPayload::TeamStageComplete(p),
        )
    }

    /// Agent 间消息。
    pub fn agent_message(
        session_id: &str,
        turn_no: u32,
        turn_id: Option<String>,
        p: AgentMessagePayload,
    ) -> Self {
        Self::new(session_id, turn_no, turn_id, EventPayload::AgentMessage(p))
    }

    /// 内存快照。
    pub fn memory_snapshot(
        session_id: &str,
        turn_no: u32,
        turn_id: Option<String>,
        p: MemorySnapshotPayload,
    ) -> Self {
        Self::new(
            session_id,
            turn_no,
            turn_id,
            EventPayload::MemorySnapshot(p),
        )
    }

    /// 文件操作统计。
    pub fn file_operation(
        session_id: &str,
        turn_no: u32,
        turn_id: Option<String>,
        p: FileOperationPayload,
    ) -> Self {
        Self::new(session_id, turn_no, turn_id, EventPayload::FileOperation(p))
    }

    /// TUI 交互动作。
    pub fn tui_action(
        session_id: &str,
        turn_no: u32,
        turn_id: Option<String>,
        p: TuiActionPayload,
    ) -> Self {
        Self::new(session_id, turn_no, turn_id, EventPayload::TuiAction(p))
    }

    /// Hook 执行。
    pub fn hook_execution(
        session_id: &str,
        turn_no: u32,
        turn_id: Option<String>,
        p: HookExecutionPayload,
    ) -> Self {
        Self::new(session_id, turn_no, turn_id, EventPayload::HookExecution(p))
    }

    /// 轻量 outcome 记录，用于尚未有专属 payload 的诊断点。
    pub fn outcome_record(
        session_id: &str,
        turn_no: u32,
        turn_id: Option<String>,
        p: OutcomeRecordPayload,
    ) -> Self {
        Self::new(session_id, turn_no, turn_id, EventPayload::OutcomeRecord(p))
    }

    /// 事件类型的稳定标识（等同序列化后的 `type` 字段），用于按点过滤配置。
    pub fn kind(&self) -> &'static str {
        self.payload.kind()
    }

    /// 应用 RedactionPolicy 到所有可能含 PII 的字段。
    pub fn redact(self, policy: &crate::RedactionPolicy) -> Self {
        if !policy.redact_prompts && !policy.redact_tool_content {
            return self;
        }
        Self {
            payload: self.payload.redact(policy),
            ..self
        }
    }
}

impl EventPayload {
    /// 递归地对 payload 内的 PII 字段做 redaction。
    fn redact(self, policy: &crate::RedactionPolicy) -> Self {
        match self {
            EventPayload::ApiError(p) => EventPayload::ApiError(p.redact(policy)),
            EventPayload::ToolDecision(p) => EventPayload::ToolDecision(p.redact(policy)),
            EventPayload::ToolExecution(p) => EventPayload::ToolExecution(p.redact(policy)),
            EventPayload::ErrorRecord(p) => EventPayload::ErrorRecord(p.redact(policy)),
            EventPayload::PermissionDecision(p) => {
                EventPayload::PermissionDecision(p.redact(policy))
            }
            EventPayload::HookExecution(p) => EventPayload::HookExecution(p.redact(policy)),
            EventPayload::CompactAction(p) => EventPayload::CompactAction(p.redact(policy)),
            EventPayload::ResumeAction(p) => EventPayload::ResumeAction(p.redact(policy)),
            EventPayload::OutcomeRecord(p) => EventPayload::OutcomeRecord(p.redact(policy)),
            EventPayload::McpConnectionError(p) => {
                EventPayload::McpConnectionError(p.redact(policy))
            }
            EventPayload::McpToolCall(p) => EventPayload::McpToolCall(p.redact(policy)),
            EventPayload::AgentMessage(p) => EventPayload::AgentMessage(p.redact(policy)),
            // 以下 payload 不包含 PII 字段，跳过
            other => other,
        }
    }
}

impl ErrorRecordPayload {
    pub(crate) fn redact(mut self, _policy: &crate::RedactionPolicy) -> Self {
        self.error_message = redact_string(&self.error_message);
        self.context = redact_string(&self.context);
        self
    }
}

impl HookExecutionPayload {
    pub(crate) fn redact(mut self, _policy: &crate::RedactionPolicy) -> Self {
        self.error_message = self.error_message.map(|_| "[REDACTED]".to_string());
        self
    }
}

impl CompactActionPayload {
    pub(crate) fn redact(self, _policy: &crate::RedactionPolicy) -> Self {
        self
    }
}

impl ResumeActionPayload {
    pub(crate) fn redact(self, _policy: &crate::RedactionPolicy) -> Self {
        self
    }
}

impl OutcomeRecordPayload {
    pub(crate) fn redact(self, _policy: &crate::RedactionPolicy) -> Self {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::redact::RedactionPolicy;

    #[test]
    fn resume_action_uses_stable_outcome_taxonomy() {
        let event = TelemetryEvent::resume_action(
            "sess",
            3,
            None,
            ResumeActionPayload {
                outcome: ResumeOutcome::Degraded,
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
        let value =
            serde_json::to_value(event).expect("serialization of telemetry event should not fail");
        assert_eq!(value["type"], "resume_action");
        assert_eq!(value["outcome"], "degraded");
        assert_eq!(value["warning_kind"], "missing_sidecar");
        assert_eq!(value["sidechain_entry_count"], 2);
    }

    #[test]
    fn compact_outcome_all_variants() {
        for (variant, _expected) in [
            (CompactOutcome::Triggered, "triggered"),
            (CompactOutcome::Skipped, "skipped"),
            (CompactOutcome::Succeeded, "succeeded"),
            (CompactOutcome::Failed, "failed"),
            (CompactOutcome::Fallback, "fallback"),
        ] {
            let payload = CompactActionPayload {
                strategy: "auto".into(),
                before_tokens: 1000,
                after_tokens: 200,
                before_message_count: 20,
                after_message_count: 5,
                success: matches!(variant, CompactOutcome::Succeeded),
                latency_ms: 50,
            };
            let event = TelemetryEvent::compact_action("sess", 5, None, payload);
            assert_eq!(event.kind(), "compact_action");
            let v = serde_json::to_value(event)
                .expect("serialization of telemetry event should not fail");
            assert_eq!(v["type"], "compact_action");
            assert_eq!(v["strategy"], "auto");
            assert_eq!(v["before_tokens"], 1000);
            assert_eq!(v["after_tokens"], 200);
        }
    }

    #[test]
    fn resume_outcome_all_variants() {
        for (outcome, expected) in [
            (ResumeOutcome::Succeeded, "succeeded"),
            (ResumeOutcome::Partial, "partial"),
            (ResumeOutcome::Degraded, "degraded"),
            (ResumeOutcome::Failed, "failed"),
        ] {
            let payload = ResumeActionPayload {
                outcome,
                source: "jsonl".into(),
                entry_count: 5,
                projected_message_count: 3,
                compact_boundary_count: 0,
                sidechain_entry_count: 0,
                warning_kind: None,
                latency_ms: 10,
            };
            let event = TelemetryEvent::resume_action("sess", 1, None, payload);
            assert_eq!(event.kind(), "resume_action");
            let v = serde_json::to_value(event)
                .expect("serialization of telemetry event should not fail");
            assert_eq!(v["type"], "resume_action");
            assert_eq!(v["outcome"], expected);
        }
    }

    #[test]
    fn error_record_redacts_message_and_context() {
        let policy = RedactionPolicy::all();
        let event = TelemetryEvent::error_record(
            "sess",
            2,
            None,
            ErrorRecordPayload {
                error_kind: "tool_failure".into(),
                error_message: "sensitive error details".into(),
                context: "/home/user/project/secret.env".into(),
                related_tools: vec!["Read".into()],
            },
        )
        .redact(&policy);
        assert_eq!(event.kind(), "error_record");
        let v =
            serde_json::to_value(event).expect("serialization of telemetry event should not fail");
        assert_eq!(v["error_message"], "[REDACTED]");
        assert_eq!(v["context"], "[REDACTED]");
        assert_eq!(v["error_kind"], "tool_failure");
        assert_eq!(v["related_tools"][0], "Read");
    }

    #[test]
    fn hook_execution_redacts_error_message() {
        let policy = RedactionPolicy::all();
        let event = TelemetryEvent::hook_execution(
            "sess",
            1,
            None,
            HookExecutionPayload {
                hook_name: "PreToolUse".into(),
                tool_name: Some("Write".into()),
                decision: "blocked".into(),
                latency_ms: 15,
                error_message: Some("hook crashed: SIGSEGV".into()),
            },
        )
        .redact(&policy);
        assert_eq!(event.kind(), "hook_execution");
        let v =
            serde_json::to_value(event).expect("serialization of telemetry event should not fail");
        assert_eq!(v["error_message"], "[REDACTED]");
        assert_eq!(v["hook_name"], "PreToolUse");
        assert_eq!(v["decision"], "blocked");
    }

    #[test]
    fn noop_redaction_leaves_all_fields() {
        let policy = RedactionPolicy::none();
        let event = TelemetryEvent::error_record(
            "sess",
            1,
            None,
            ErrorRecordPayload {
                error_kind: "io".into(),
                error_message: "/tmp/secret.key: permission denied".into(),
                context: "read config".into(),
                related_tools: vec![],
            },
        )
        .redact(&policy);
        let v =
            serde_json::to_value(event).expect("serialization of telemetry event should not fail");
        assert_eq!(v["error_message"], "/tmp/secret.key: permission denied");
        assert_eq!(v["context"], "read config");
    }

    #[test]
    fn outcome_record_serializes_category_and_outcome() {
        let event = TelemetryEvent::outcome_record(
            "sess",
            4,
            None,
            OutcomeRecordPayload {
                category: "context_window".into(),
                outcome: "compact_triggered".into(),
                reason: Some("80% threshold".into()),
                latency_ms: 5,
            },
        );
        assert_eq!(event.kind(), "outcome_record");
        let v =
            serde_json::to_value(event).expect("serialization of telemetry event should not fail");
        assert_eq!(v["type"], "outcome_record");
        assert_eq!(v["category"], "context_window");
        assert_eq!(v["outcome"], "compact_triggered");
        assert_eq!(v["reason"], "80% threshold");
    }

    #[test]
    fn turn_complete_serializes_all_fields() {
        let event = TelemetryEvent::turn_complete(
            "sess",
            7,
            None,
            TurnCompletePayload {
                turn_no: 7,
                turn_id: None,
                stop_reason: "end_turn".into(),
                api_calls: 3,
                tool_calls: 2,
                permission_denials: 0,
                last_tool_name: Some("Read".into()),
                last_tool_was_error: false,
                turn_duration_ms: 12000,
            },
        );
        assert_eq!(event.kind(), "turn_complete");
        let v =
            serde_json::to_value(event).expect("serialization of telemetry event should not fail");
        assert_eq!(v["type"], "turn_complete");
        assert_eq!(v["turn_no"], 7);
        assert_eq!(v["stop_reason"], "end_turn");
        assert_eq!(v["api_calls"], 3);
        assert_eq!(v["tool_calls"], 2);
        assert_eq!(v["last_tool_name"], "Read");
    }

    #[test]
    fn user_prompt_submit_serializes() {
        let event = TelemetryEvent::user_prompt_submit(
            "sess",
            2,
            None,
            UserPromptSubmitPayload {
                char_count: 80,
                has_attachments: false,
                attachment_count: 0,
                heuristic_class: Some("coding".into()),
                turn_no: 2,
            },
        );
        assert_eq!(event.kind(), "user_prompt_submit");
        let v =
            serde_json::to_value(event).expect("serialization of telemetry event should not fail");
        assert_eq!(v["type"], "user_prompt_submit");
        assert_eq!(v["char_count"], 80);
        assert_eq!(v["heuristic_class"], "coding");
    }

    #[test]
    fn slash_command_used_serializes() {
        let event = TelemetryEvent::slash_command_used(
            "sess",
            1,
            None,
            SlashCommandUsedPayload {
                command: "/help".into(),
                is_alias: false,
                resolved_command: "help".into(),
                has_arg: false,
            },
        );
        assert_eq!(event.kind(), "slash_command_used");
        let v =
            serde_json::to_value(event).expect("serialization of telemetry event should not fail");
        assert_eq!(v["type"], "slash_command_used");
        assert_eq!(v["command"], "/help");
    }

    #[test]
    fn interrupt_signal_serializes() {
        let event = TelemetryEvent::interrupt_signal(
            "sess",
            3,
            None,
            InterruptSignalPayload {
                signal: "SIGINT".into(),
                elapsed_ms: 5000,
                turn_no: 3,
                in_tool_execution: true,
                current_tool: Some("Bash".into()),
            },
        );
        assert_eq!(event.kind(), "interrupt_signal");
        let v =
            serde_json::to_value(event).expect("serialization of telemetry event should not fail");
        assert_eq!(v["type"], "interrupt_signal");
        assert_eq!(v["signal"], "SIGINT");
        assert_eq!(v["in_tool_execution"], true);
    }

    #[test]
    fn turn_start_serializes() {
        let event = TelemetryEvent::turn_start(
            "sess",
            3,
            None,
            TurnStartPayload {
                turn_no: 3,
                turn_id: None,
                resumed: false,
                is_retry: false,
            },
        );
        assert_eq!(event.kind(), "turn_start");
        let v =
            serde_json::to_value(event).expect("serialization of telemetry event should not fail");
        assert_eq!(v["type"], "turn_start");
        assert_eq!(v["turn_no"], 3);
    }

    #[test]
    fn memory_snapshot_serializes() {
        let event = TelemetryEvent::memory_snapshot(
            "sess",
            5,
            None,
            MemorySnapshotPayload {
                rss_kb: 256000,
                heap_kb: 128000,
                turn_no: 5,
                elapsed_ms: 60000,
            },
        );
        assert_eq!(event.kind(), "memory_snapshot");
        let v =
            serde_json::to_value(event).expect("serialization of telemetry event should not fail");
        assert_eq!(v["type"], "memory_snapshot");
        assert_eq!(v["rss_kb"], 256000);
    }

    #[test]
    fn file_operation_serializes() {
        let event = TelemetryEvent::file_operation(
            "sess",
            5,
            None,
            FileOperationPayload {
                operation: "write".into(),
                file_count: 3,
                total_bytes: 4096,
                duration_ms: 50,
                errors: 0,
            },
        );
        assert_eq!(event.kind(), "file_operation");
        let v =
            serde_json::to_value(event).expect("serialization of telemetry event should not fail");
        assert_eq!(v["type"], "file_operation");
        assert_eq!(v["operation"], "write");
    }

    #[test]
    fn tui_action_serializes() {
        let event = TelemetryEvent::tui_action(
            "sess",
            1,
            None,
            TuiActionPayload {
                action: "search".into(),
                view: "transcript".into(),
                detail: Some("regex".into()),
            },
        );
        assert_eq!(event.kind(), "tui_action");
        let v =
            serde_json::to_value(event).expect("serialization of telemetry event should not fail");
        assert_eq!(v["type"], "tui_action");
        assert_eq!(v["view"], "transcript");
    }
}
