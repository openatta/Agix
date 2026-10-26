//! Tool 执行结果与进度。
//!
//! `ToolResult` —— 工具的成功输出（mapToolResultToToolResultBlockParam 在
//!     agix-engine 里把它翻成 ContentBlock::ToolResult 喂回 API）
//! `ProgressSender` —— 工具执行期间向上层发增量更新（spinner、partial output）
//! `ToolProgressData` —— 进度的语义负载

// Re-export std Result for proc-macro compatibility (our crate is named 'core')
pub use std::result::Result;

use crate::message::{ContentBlock, Message, ToolResultContent};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// 工具成功执行后的返回。
#[derive(Debug, Clone)]
pub struct ToolResult {
    /// 给模型看的内容（API 会把它装进 ContentBlock::ToolResult.content）
    pub content: ToolResultContent,
    /// 是否报错（`is_error` 字段透传给 API）
    pub is_error: bool,
    /// MCP structuredContent
    pub structured_content: Option<serde_json::Value>,
    /// MCP _meta
    pub mcp_meta: Option<serde_json::Value>,
    /// 偶尔工具会插入额外的 message（如 AgentTool 的中间报告；）
    pub new_messages: Vec<Message>,
}

impl ToolResult {
    /// 简便构造：纯文本结果，非错误。
    pub fn text(s: impl Into<String>) -> Self {
        Self {
            content: ToolResultContent::Text(s.into()),
            is_error: false,
            structured_content: None,
            mcp_meta: None,
            new_messages: Vec::new(),
        }
    }

    /// 简便构造：纯文本错误结果（送给模型看错误说明）。
    pub fn error_text(s: impl Into<String>) -> Self {
        Self {
            content: ToolResultContent::Text(s.into()),
            is_error: true,
            structured_content: None,
            mcp_meta: None,
            new_messages: Vec::new(),
        }
    }

    /// 多块（文本 + 图像等）。
    pub fn blocks(blocks: Vec<ContentBlock>) -> Self {
        Self {
            content: ToolResultContent::Blocks(blocks),
            is_error: false,
            structured_content: None,
            mcp_meta: None,
            new_messages: Vec::new(),
        }
    }
}

/// 工具运行期向上层（Engine / UI）发的进度数据。
///
/// 不同工具不同形状；Engine 把它包进 EngineEvent::ToolProgress 派发。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolProgressData {
    /// BashTool / 子进程类工具的 stdout/stderr 增量
    Bash { stdout: String, stderr: String },
    /// WebSearch 已收到的结果数
    WebSearch { results_so_far: u32 },
    /// 工具自定义；UI 不识别就忽略
    Generic(serde_json::Value),
}

/// 进度回调 trait —— Engine 实现它，把数据派到 EngineEvent::ToolProgress。
/// 测试 / headless 路径用 `NoopProgress`。
pub trait ProgressCallback: Send + Sync {
    fn on_progress(&self, tool_use_id: &str, data: ToolProgressData);
}

/// 工具调用时拿到的进度发送器。
///
/// 不知道下游 sink 的具体形状（mpsc / println / null），全靠 trait object。
#[derive(Clone)]
pub struct ProgressSender {
    sink: Option<Arc<dyn ProgressCallback>>,
    tool_use_id: String,
}

impl ProgressSender {
    /// Wrap a progress callback for one-tool dispatch.
    pub fn new(sink: Arc<dyn ProgressCallback>, tool_use_id: impl Into<String>) -> Self {
        Self {
            sink: Some(sink),
            tool_use_id: tool_use_id.into(),
        }
    }

    /// 测试 / headless 友好：丢弃所有进度。
    pub fn noop(tool_use_id: impl Into<String>) -> Self {
        Self {
            sink: None,
            tool_use_id: tool_use_id.into(),
        }
    }

    /// Forward a progress event to the underlying sink (no-op if dropped).
    pub fn send(&self, data: ToolProgressData) {
        if let Some(s) = &self.sink {
            s.on_progress(&self.tool_use_id, data);
        }
    }

    /// The tool_use_id this sender is associated with.
    pub fn tool_use_id(&self) -> &str {
        &self.tool_use_id
    }
}

impl std::fmt::Debug for ProgressSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProgressSender")
            .field("tool_use_id", &self.tool_use_id)
            .field("active", &self.sink.is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct CountingSink {
        count: Mutex<u32>,
    }
    impl ProgressCallback for CountingSink {
        fn on_progress(&self, _: &str, _: ToolProgressData) {
            *self.count.lock().unwrap() += 1;
        }
    }

    #[test]
    fn noop_sender_silently_drops() {
        let p = ProgressSender::noop("toolu_01");
        p.send(ToolProgressData::Bash {
            stdout: "ls\n".into(),
            stderr: String::new(),
        });
        assert_eq!(p.tool_use_id(), "toolu_01");
    }

    #[test]
    fn sink_sender_calls_callback() {
        let sink = Arc::new(CountingSink {
            count: Mutex::new(0),
        });
        let p = ProgressSender::new(sink.clone() as Arc<dyn ProgressCallback>, "t1");
        for _ in 0..3 {
            p.send(ToolProgressData::WebSearch { results_so_far: 1 });
        }
        assert_eq!(*sink.count.lock().unwrap(), 3);
    }

    #[test]
    fn tool_result_text_helpers() {
        let ok = ToolResult::text("done");
        assert!(!ok.is_error);
        let err = ToolResult::error_text("oops");
        assert!(err.is_error);
    }
}
