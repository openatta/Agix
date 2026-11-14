//! The executors a host injects so prompt and agent hooks can run: the hooks
//! crate does not depend on a model client or on delegation itself.

use crate::payload::HookInput;
use tokio_util::sync::CancellationToken;

/// Prompt-type hook 执行器。HookRunner 持有 `Option<Arc<dyn PromptHookExecutor>>`；
/// 不注入时 Prompt hook skip 报错 "no executor configured"。`stop` 被 cancel 时
/// 应尽快返回。
///
/// CLI 注入一个 wrap AnthropicClient 的实现 —— hooks crate 自身**不**依赖
/// agix-anthropic，避免循环依赖（anthropic 依赖 core，hooks 依赖 core，
/// 双方独立扩展）。
#[async_trait::async_trait]
pub trait PromptHookExecutor: Send + Sync {
    /// 喂 prompt + payload，返回模型生成的 JSON HookResponse 文本（caller 解析）。
    async fn execute(
        &self,
        prompt: &str,
        model: Option<&str>,
        payload: &HookInput,
        stop: &CancellationToken,
    ) -> Result<String, String>;
}

/// Agent hook executor trait. Consumers (CLI/daemon) implement this to
/// provide sub-agent execution for Agent-type hooks. The executor receives the
/// hook prompt + payload and returns structured JSON: `{"ok": bool, "reason": "..."}`.
#[async_trait::async_trait]
pub trait AgentHookExecutor: Send + Sync {
    /// Execute an agent hook with the given prompt and model; the agent stops
    /// when `stop` does. Returns the agent's JSON response string.
    async fn execute(
        &self,
        prompt: &str,
        model: Option<&str>,
        payload: &HookInput,
        stop: &CancellationToken,
    ) -> Result<String, AgentHookError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentHookError {
    /// The event is a hook agent's own, or one of its descendants': agent
    /// hooks do not judge those, so the hook does not apply rather than fail.
    NotJudged,
    Failed(String),
}
