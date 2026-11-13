//! 工具描述期上下文 + 运行期上下文 + UI 交互类型。

use crate::message::{Message, SystemKind};
use crate::session::AgentContext;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use super::config::EngineConfig;
use super::session::{NoopEffects, SessionState, ToolEffects};
use super::task::BackgroundAgentProgressData;

/// 工具弹给用户看的问询请求。
#[derive(Debug, Clone)]
pub struct PromptRequest {
    /// 来源工具或子系统名（"BashTool" / "FileEditTool" …）—— 仅 UI 用
    pub source: String,
    /// 主问题
    pub message: String,
    /// 备选答案；按 key 顺序展示
    pub options: Vec<PromptOption>,
    /// 工具入参的简短摘要（用于在对话框头部回显）
    pub tool_input_summary: Option<String>,
    /// Canonical content string for rule generation, from
    /// `Tool::permission_match_content`. Used by "always allow" / "always deny"
    /// shortcuts to build a rule like `<tool>(<content>)` without re-parsing
    /// `tool_input_summary`. None when the tool didn't expose a match content
    /// — UI gracefully falls back to single-shot allow/deny.
    pub permission_match_content: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PromptOption {
    /// 键（"y" / "n" / "A"）
    pub key: String,
    /// 给用户看的标签（"yes" / "no" / "always allow"）
    pub label: String,
}

#[derive(Debug, Clone)]
pub struct PromptResponse {
    /// 用户选择的 key
    pub choice: String,
    /// 用户改写过的工具入参；None 表示不改
    pub user_modified_input: Option<serde_json::Value>,
}

/// UI-only system message（对应 Message::System）。
#[derive(Debug, Clone)]
pub struct SystemMessage {
    pub kind: SystemKind,
    pub content: String,
}

impl From<SystemMessage> for Message {
    fn from(m: SystemMessage) -> Self {
        Message::System {
            content: m.content,
            kind: m.kind,
        }
    }
}

/// 工具调用时拿到的运行期上下文。
#[derive(Clone)]
pub struct ToolCtx {
    pub config: Arc<EngineConfig>,
    pub session: Arc<SessionState>,
    pub effects: Arc<dyn ToolEffects>,
    pub cancel: CancellationToken,
    /// API 给的 tool_use id（不是我们的 Id）
    pub tool_use_id: String,
    /// 子 agent 上下文。主线程 None。
    pub agent: Option<AgentContext>,
    /// (fork sub-agent)**: 父会话的当前消息快照。AgentTool 同步路径用
    /// 它构建 forked messages（share parent history → prompt cache 共享）。
    /// 非 AgentTool 工具忽略此字段。
    pub parent_messages: Option<Vec<crate::message::Message>>,
    /// (depth tracking)**: 当前 agent nesting depth。主线程 0，每经
    /// AgentTool spawn 加 1。超限（max_agent_depth）则拒绝 spawn。
    pub agent_depth: u32,
    /// 可选的事件发送器。工具在 turn 期间通过它向父 engine 发送非关键
    /// 性进度事件（如后台 agent 的 tool_use 开始 / 完成）。Engine 会转发为
    /// `EngineEvent::BackgroundAgentProgress`。非 AgentTool 工具忽略此字段。
    pub events_tx: Option<UnboundedSender<BackgroundAgentProgressData>>,
}

impl ToolCtx {
    /// 测试简便构造：cwd + NoopEffects + 默认配置。
    ///
    /// 默认禁用沙盒（`dangerously_disable_sandbox = true`），因为测试 bash 行为
    /// 时不需要过 sandbox-exec（macOS SIP 限制会阻断；sandbox 本身由 sandbox.rs
    /// 单元测试覆盖）。如果测试需要沙盒，clone config 后自己覆写。
    pub fn for_test(cwd: PathBuf) -> Self {
        let mut cfg = EngineConfig::defaults_for("claude-sonnet-4-6");
        cfg.dangerously_disable_sandbox = true;
        Self {
            config: Arc::new(cfg),
            session: Arc::new(SessionState::for_tests(cwd)),
            effects: Arc::new(NoopEffects),
            cancel: CancellationToken::new(),
            tool_use_id: "test_tool_use".to_string(),
            agent: None,
            parent_messages: None,
            agent_depth: 0,
            events_tx: None,
        }
    }
}

impl std::fmt::Debug for ToolCtx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolCtx")
            .field("model", &self.config.model)
            .field("cwd", &self.session.env().cwd)
            .field("tool_use_id", &self.tool_use_id)
            .field("crate", &self.agent)
            .finish()
    }
}
