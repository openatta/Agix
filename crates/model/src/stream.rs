//! SSE 流事件类型 —— 严格对应 Anthropic API 的事件格式。

use base::message::StopReason;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    MessageStart {
        message: MessageStartPayload,
    },
    ContentBlockStart {
        index: u32,
        content_block: ContentBlockStart,
    },
    ContentBlockDelta {
        index: u32,
        delta: BlockDelta,
    },
    ContentBlockStop {
        index: u32,
    },
    MessageDelta {
        delta: MessageDeltaPayload,
        #[serde(default)]
        usage: Option<Usage>,
    },
    MessageStop,
    Ping,
    Error {
        error: ApiErrorPayload,
    },
    /// 兜底未来新增事件类型 —— Anthropic 加新事件不打断流
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct MessageStartPayload {
    pub id: String,
    pub model: String,
    pub role: String,
    pub usage: Usage,
    /// 通常 None；终值在 MessageDelta
    #[serde(default)]
    pub stop_reason: Option<StopReason>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlockStart {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// Server-initiated tool use (e.g. built-in `web_search`).
    /// Same shape as `ToolUse` but type tag is `server_tool_use`.
    ServerToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// Server-side search results from the built-in web search tool.
    /// `content` is either `Vec<{title, url}>` or an error object.
    WebSearchToolResult {
        tool_use_id: String,
        content: serde_json::Value,
    },
    Thinking {
        thinking: String,
    },
    RedactedThinking {
        data: String,
    },
    /// 新增的块类型 —— 跳过这一块，不打断整条流
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BlockDelta {
    TextDelta {
        text: String,
    },
    /// tool_use 块的输入 JSON 字符流（碎片，需 concat 累积）
    InputJsonDelta {
        partial_json: String,
    },
    ThinkingDelta {
        thinking: String,
    },
    /// thinking 块的签名（结束时一次性给）
    SignatureDelta {
        signature: String,
    },
    /// 引用块
    CitationsDelta {
        citation: serde_json::Value,
    },
    /// 新增的增量类型
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct MessageDeltaPayload {
    pub stop_reason: Option<StopReason>,
    #[serde(default)]
    pub stop_sequence: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    pub cache_read_input_tokens: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ApiErrorPayload {
    /// "overloaded_error" / "rate_limit_error" / "api_error" 等
    #[serde(rename = "type")]
    pub kind: String,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::AnthropicError;
    use serde_json::json;

    #[test]
    fn message_start_decodes() -> Result<(), AnthropicError> {
        let v = json!({
            "type": "message_start",
            "message": {
                "id": "msg_01",
                "model": "claude-sonnet-4-6",
                "role": "assistant",
                "usage": { "input_tokens": 10, "output_tokens": 0 }
            }
        });
        let ev: StreamEvent = serde_json::from_value(v).unwrap();
        match ev {
            StreamEvent::MessageStart { message } => {
                assert_eq!(message.id, "msg_01");
                assert_eq!(message.usage.input_tokens, 10);
                Ok(())
            }
            other => Err(AnthropicError::ParseError(format!(
                "wrong variant: {:?}",
                other
            ))),
        }
    }

    #[test]
    fn text_delta_decodes() -> Result<(), AnthropicError> {
        let v = json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": { "type": "text_delta", "text": "Hello" }
        });
        let ev: StreamEvent = serde_json::from_value(v).unwrap();
        match ev {
            StreamEvent::ContentBlockDelta { index, delta } => {
                assert_eq!(index, 0);
                assert_eq!(
                    delta,
                    BlockDelta::TextDelta {
                        text: "Hello".into()
                    }
                );
                Ok(())
            }
            other => Err(AnthropicError::ParseError(format!(
                "expected ContentBlockDelta, got {:?}",
                other
            ))),
        }
    }

    #[test]
    fn input_json_delta_decodes() -> Result<(), AnthropicError> {
        let v = json!({
            "type": "content_block_delta",
            "index": 1,
            "delta": { "type": "input_json_delta", "partial_json": "{\"command\":\"ls" }
        });
        let ev: StreamEvent = serde_json::from_value(v).unwrap();
        match ev {
            StreamEvent::ContentBlockDelta { delta, .. } => {
                assert_eq!(
                    delta,
                    BlockDelta::InputJsonDelta {
                        partial_json: "{\"command\":\"ls".into()
                    }
                );
                Ok(())
            }
            other => Err(AnthropicError::ParseError(format!(
                "expected ContentBlockDelta, got {:?}",
                other
            ))),
        }
    }

    #[test]
    fn message_delta_with_stop_reason() -> Result<(), AnthropicError> {
        let v = json!({
            "type": "message_delta",
            "delta": { "stop_reason": "end_turn", "stop_sequence": null },
            "usage": { "input_tokens": 0, "output_tokens": 15 }
        });
        let ev: StreamEvent = serde_json::from_value(v).unwrap();
        match ev {
            StreamEvent::MessageDelta { delta, usage } => {
                assert_eq!(delta.stop_reason, Some(StopReason::EndTurn));
                assert_eq!(usage.unwrap().output_tokens, 15);
                Ok(())
            }
            other => Err(AnthropicError::ParseError(format!(
                "expected MessageDelta, got {:?}",
                other
            ))),
        }
    }

    #[test]
    fn unknown_event_does_not_break() {
        let v = json!({ "type": "some_future_event_2030" });
        let ev: StreamEvent = serde_json::from_value(v).unwrap();
        assert_eq!(ev, StreamEvent::Unknown);
    }

    #[test]
    fn unknown_block_and_delta_types_do_not_break() {
        let start: StreamEvent = serde_json::from_value(json!({
            "type": "content_block_start", "index": 2,
            "content_block": { "type": "mcp_tool_use", "id": "x", "name": "y", "input": {} }
        }))
        .unwrap();
        assert_eq!(
            start,
            StreamEvent::ContentBlockStart {
                index: 2,
                content_block: ContentBlockStart::Unknown
            }
        );
        let delta: StreamEvent = serde_json::from_value(json!({
            "type": "content_block_delta", "index": 2,
            "delta": { "type": "some_future_delta", "payload": 1 }
        }))
        .unwrap();
        assert_eq!(
            delta,
            StreamEvent::ContentBlockDelta {
                index: 2,
                delta: BlockDelta::Unknown
            }
        );
    }

    #[test]
    fn ping_event() {
        let v = json!({ "type": "ping" });
        let ev: StreamEvent = serde_json::from_value(v).unwrap();
        assert_eq!(ev, StreamEvent::Ping);
    }

    #[test]
    fn api_error_event() -> Result<(), AnthropicError> {
        let v = json!({
            "type": "error",
            "error": { "type": "overloaded_error", "message": "服务繁忙" }
        });
        let ev: StreamEvent = serde_json::from_value(v).unwrap();
        match ev {
            StreamEvent::Error { error } => {
                assert_eq!(error.kind, "overloaded_error");
                Ok(())
            }
            other => Err(AnthropicError::ParseError(format!(
                "expected Error event, got {:?}",
                other
            ))),
        }
    }
}
