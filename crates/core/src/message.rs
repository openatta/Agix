//! Message / ContentBlock —— 直接对应 Anthropic Messages API 的形状，
//! 顺手当作我们的内部 transcript 表示（jsonl 持久化也用同一份 schema）。

use serde::{Deserialize, Serialize};

/// transcript 中的一条消息。User / Assistant 进 API；System 仅 UI 持久化用。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum Message {
    User {
        content: Vec<ContentBlock>,
    },
    Assistant {
        content: Vec<ContentBlock>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        stop_reason: Option<StopReason>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        model: Option<String>,
    },
    /// UI-only：本地命令输出、提醒、通知。**不**进 API。
    System {
        content: String,
        kind: SystemKind,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SystemKind {
    LocalCommand,
    Reminder,
    Notice,
}

/// 内容块。`type` 字段做 enum tag。
///
/// 字段名 / 取值与 Anthropic API 一致；外部 JSON 直传可读。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        cache_control: Option<serde_json::Value>,
    },
    Image {
        source: ImageSource,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        /// 可以是 string 或 array of ContentBlock；用 ToolResultContent 区分
        content: ToolResultContent,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        is_error: bool,
    },
    Thinking {
        thinking: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
        /// Which provider produced it. Transcript-only: request builders
        /// always leave it `None`, so it never reaches a provider's wire.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
    },
    RedactedThinking {
        data: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
    },
    /// Anthropic `cache_edits` content block: requests server-side deletion of
    /// cached tool result content by tool_use_id. Only meaningful in the request;
    /// never returned by the API. Requires the `context-management-2025-06-27`
    /// beta header.
    #[serde(rename = "cache_edits")]
    CacheEdits {
        cache_edits: Vec<CacheEdit>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ImageSource {
    Base64 { media_type: String, data: String },
    Url { url: String },
}

/// `tool_result.content` 字段：string 或 ContentBlock 数组。
///
/// `#[serde(untagged)]` 让 JSON `"hello"` 直接解到 Text 变体，`[…]` 解到 Blocks。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum ToolResultContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

impl ToolResultContent {
    /// 简便构造器：纯文本结果。
    pub fn text(s: impl Into<String>) -> Self {
        Self::Text(s.into())
    }
    /// True when no content blocks (empty user/assistant message).
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Text(s) => s.is_empty(),
            Self::Blocks(b) => b.is_empty(),
        }
    }
}

/// A single cache edit operation sent in the `cache_edits` content block.
/// Currently only supports `delete_tool_result`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CacheEdit {
    DeleteToolResult { tool_use_id: String },
}

/// 模型停止原因。Anthropic API 取值。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    ToolUse,
    StopSequence,
    PauseTurn,
    /// +**: agent hit `EngineConfig.max_api_calls_per_turn`. Not from
    /// the Anthropic API — emitted by our engine when the configured per-
    /// turn budget is exhausted. Caller (e.g. `AgentTool` for sub-agents)
    /// should handle gracefully: return whatever partial work exists,
    /// flagged but **not** is_error=true. Carries the same
    /// `max_turns_reached` attachment + early-return semantics.
    MaxTurnsReached,
    /// 兜底未来新增：避免反序列化失败
    #[serde(other)]
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn user_text_roundtrip() {
        let m = Message::User {
            content: vec![ContentBlock::Text {
                text: "hi".into(),
                cache_control: None,
            }],
        };
        let s = serde_json::to_value(&m).unwrap();
        assert_eq!(s["role"], "user");
        assert_eq!(s["content"][0]["type"], "text");
        let back: Message = serde_json::from_value(s).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn assistant_with_tool_use() {
        let m = Message::Assistant {
            content: vec![ContentBlock::ToolUse {
                id: "toolu_01".into(),
                name: "Bash".into(),
                input: json!({"command": "ls"}),
            }],
            stop_reason: Some(StopReason::ToolUse),
            model: Some("claude-sonnet-4-6".into()),
        };
        let s = serde_json::to_string(&m).unwrap();
        let back: Message = serde_json::from_str(&s).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn tool_result_content_string_form() {
        let v = json!({
            "type": "tool_result",
            "tool_use_id": "toolu_01",
            "content": "hello stdout",
        });
        let block: ContentBlock = serde_json::from_value(v).unwrap();
        match block {
            ContentBlock::ToolResult {
                content, is_error, ..
            } => {
                assert_eq!(content, ToolResultContent::Text("hello stdout".into()));
                assert!(!is_error);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn tool_result_content_blocks_form() {
        let v = json!({
            "type": "tool_result",
            "tool_use_id": "toolu_01",
            "content": [{"type":"text","text":"a"}, {"type":"text","text":"b"}],
            "is_error": true,
        });
        let block: ContentBlock = serde_json::from_value(v).unwrap();
        match block {
            ContentBlock::ToolResult {
                content, is_error, ..
            } => {
                assert!(matches!(content, ToolResultContent::Blocks(ref v) if v.len() == 2));
                assert!(is_error);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn unknown_stop_reason_does_not_break() {
        let v = json!("a_new_reason_anthropic_invented");
        let r: StopReason = serde_json::from_value(v).unwrap();
        assert_eq!(r, StopReason::Unknown);
    }

    #[test]
    fn system_message_does_not_have_role_lowercase_collision() {
        let m = Message::System {
            content: "compacted".into(),
            kind: SystemKind::Notice,
        };
        let s = serde_json::to_value(&m).unwrap();
        assert_eq!(s["role"], "system");
        assert_eq!(s["kind"], "notice");
    }

    #[test]
    fn stop_reason_known_variants_roundtrip() {
        for reason in [
            StopReason::EndTurn,
            StopReason::MaxTokens,
            StopReason::ToolUse,
            StopReason::StopSequence,
            StopReason::PauseTurn,
            StopReason::MaxTurnsReached,
        ] {
            let json = serde_json::to_value(reason).unwrap();
            let back: StopReason = serde_json::from_value(json).unwrap();
            assert_eq!(reason, back);
        }
    }

    #[test]
    fn cache_control_text_block_roundtrip() {
        let block = ContentBlock::Text {
            text: "cached prefix".into(),
            cache_control: Some(serde_json::json!({"type": "ephemeral"})),
        };
        let json = serde_json::to_value(&block).unwrap();
        assert_eq!(json["type"], "text");
        assert_eq!(json["cache_control"]["type"], "ephemeral");
        let back: ContentBlock = serde_json::from_value(json).unwrap();
        assert_eq!(block, back);
    }

    #[test]
    fn cache_control_none_omitted_from_json() {
        let block = ContentBlock::Text {
            text: "no cache".into(),
            cache_control: None,
        };
        let json = serde_json::to_value(&block).unwrap();
        assert!(json.get("cache_control").is_none());
    }

    #[test]
    fn image_base64_roundtrip() {
        let block = ContentBlock::Image {
            source: ImageSource::Base64 {
                media_type: "image/png".into(),
                data: "iVBORw0KGgo=".into(),
            },
        };
        let json = serde_json::to_value(&block).unwrap();
        assert_eq!(json["type"], "image");
        assert_eq!(json["source"]["type"], "base64");
        let back: ContentBlock = serde_json::from_value(json).unwrap();
        assert_eq!(block, back);
    }

    #[test]
    fn image_url_roundtrip() {
        let block = ContentBlock::Image {
            source: ImageSource::Url {
                url: "https://example.com/img.png".into(),
            },
        };
        let json = serde_json::to_string(&block).unwrap();
        let back: ContentBlock = serde_json::from_str(&json).unwrap();
        assert_eq!(block, back);
    }

    #[test]
    fn thinking_block_roundtrip() {
        let block = ContentBlock::Thinking {
            thinking: "Let me think...".into(),
            signature: Some("sig_abc123".into()),
            provider: None,
        };
        let json = serde_json::to_string(&block).unwrap();
        let back: ContentBlock = serde_json::from_str(&json).unwrap();
        assert_eq!(block, back);
    }

    #[test]
    fn unsigned_thinking_has_no_signature_key_and_reads_back() {
        let block = ContentBlock::Thinking {
            thinking: "reasoning".into(),
            signature: None,
            provider: None,
        };
        let json = serde_json::to_value(&block).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"type": "thinking", "thinking": "reasoning"})
        );
        let back: ContentBlock = serde_json::from_value(json).unwrap();
        assert_eq!(block, back);
    }

    #[test]
    fn redacted_thinking_block_roundtrip() {
        let block = ContentBlock::RedactedThinking {
            data: "redacted_data".into(),
            provider: None,
        };
        let json = serde_json::to_string(&block).unwrap();
        let back: ContentBlock = serde_json::from_str(&json).unwrap();
        assert_eq!(block, back);
    }

    #[test]
    fn role_serialization_user() {
        assert_eq!(
            serde_json::to_value(Role::User).unwrap(),
            serde_json::json!("user")
        );
    }

    #[test]
    fn role_serialization_assistant() {
        assert_eq!(
            serde_json::to_value(Role::Assistant).unwrap(),
            serde_json::json!("assistant")
        );
    }

    #[test]
    fn tool_result_content_is_empty() {
        assert!(ToolResultContent::Text("".into()).is_empty());
        assert!(ToolResultContent::Blocks(vec![]).is_empty());
        assert!(!ToolResultContent::Text("hello".into()).is_empty());
        assert!(!ToolResultContent::Blocks(vec![ContentBlock::Text {
            text: "x".into(),
            cache_control: None,
        }])
        .is_empty());
    }

    #[test]
    fn system_kind_serialization() {
        assert_eq!(
            serde_json::to_value(SystemKind::LocalCommand).unwrap(),
            serde_json::json!("local_command")
        );
        assert_eq!(
            serde_json::to_value(SystemKind::Reminder).unwrap(),
            serde_json::json!("reminder")
        );
        assert_eq!(
            serde_json::to_value(SystemKind::Notice).unwrap(),
            serde_json::json!("notice")
        );
    }
}
