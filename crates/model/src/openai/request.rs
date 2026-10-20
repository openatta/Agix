//! OpenAI **Chat Completions** request body types, plus the translation from
//! the protocol-agnostic `base::interface::model` shapes into them.
//!
//! Mirrors the role `crate::types` plays for the Anthropic Messages API: pure
//! data + `Serialize`, no I/O, so the wire shape is directly assertable in
//! unit tests without standing up an HTTP server.

use base::interface::model::{MessageRole, ModelContentBlock, ModelMessage, ToolDef};
use base::interface::prompt::PromptBlock;
use serde::Serialize;
use serde_json::Value;

/// `POST <base>/v1/chat/completions` body. `stream` is always true — the
/// engine loop is streaming-only, same as the Anthropic path.
#[derive(Debug, Clone, Serialize)]
pub struct ChatCompletionsRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ChatTool>,
    pub stream: bool,
    /// `{"include_usage": true}` — without it the OpenAI streaming protocol
    /// never reports token usage at all (the non-streaming response carries a
    /// `usage` object; the streamed one does not, unless asked). Losing usage
    /// would silently break cost accounting and the context-budget checks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,
    /// **`max_completion_tokens`, not `max_tokens`.** OpenAI deprecated
    /// `max_tokens` for Chat Completions and rejects it outright on the
    /// reasoning models; `max_completion_tokens` is the supported spelling and
    /// is what every current-generation OpenAI-compatible gateway accepts. A
    /// gateway old enough to only understand `max_tokens` will ignore this
    /// field and fall back to its own default rather than erroring, which is
    /// the safer failure of the two.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StreamOptions {
    pub include_usage: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ChatRole {
    System,
    User,
    Assistant,
    /// Carries a single tool result, keyed by `tool_call_id`.
    Tool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatMessage {
    pub role: ChatRole,
    /// `None` for an assistant turn that is *only* tool calls — OpenAI wants
    /// the field absent (or null) there, not an empty string.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<ChatContent>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    fn new(role: ChatRole) -> Self {
        Self {
            role,
            content: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }
}

/// OpenAI accepts either a bare string or an array of typed parts. We emit the
/// bare string whenever the message is text-only: it is what every
/// OpenAI-compatible gateway understands, whereas the parts array is a newer
/// addition that some relays only implement for vision requests.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum ChatContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text { text: String },
    ImageUrl { image_url: ImageUrl },
}

/// OpenAI's vision input. There is no base64+media-type pair as in the
/// Anthropic shape — the two are folded into a `data:` URI.
#[derive(Debug, Clone, Serialize)]
pub struct ImageUrl {
    pub url: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: ToolCallKind,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolCallKind {
    Function,
}

#[derive(Debug, Clone, Serialize)]
pub struct FunctionCall {
    pub name: String,
    /// **A JSON string, not a JSON object.** OpenAI transports tool arguments
    /// as an opaque string that the caller parses; sending an object here is a
    /// 400. This is the mirror image of the Anthropic `input` field.
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatTool {
    #[serde(rename = "type")]
    pub kind: ToolCallKind,
    pub function: FunctionDef,
}

#[derive(Debug, Clone, Serialize)]
pub struct FunctionDef {
    pub name: String,
    pub description: String,
    /// The JSON Schema. Same content as Anthropic's `input_schema`, different
    /// field name.
    pub parameters: Value,
}

/// Translate one protocol-agnostic turn's worth of inputs into a Chat
/// Completions body.
///
/// Three shape mismatches are worth calling out, because they are where a
/// naive 1:1 mapping produces a request the API rejects:
///
/// 1. **System prompt.** Anthropic takes an *array* of system blocks, each
///    independently cacheable. OpenAI takes a single `system` message. The
///    blocks are concatenated with a blank line between them and
///    `cache_strategy` is dropped — OpenAI-compatible endpoints have no
///    explicit cache-control surface (server-side prefix caching, where it
///    exists, is automatic and invisible).
///
/// 2. **Tool results are their own message.** Anthropic carries them as
///    `tool_result` blocks inside a user message; OpenAI needs one
///    `role: "tool"` message per result, and they must directly follow the
///    assistant message that requested them. So a single incoming user
///    `ModelMessage` can fan out into several `tool` messages plus at most one
///    `user` message — and the tool messages are emitted *first*, even if the
///    incoming block order put text ahead of them.
///
/// 3. **Thinking blocks are dropped.** There is no representation for
///    `thinking` / `redacted_thinking` (nor for their signatures) in the Chat
///    Completions request schema. Unlike the Anthropic path — where dropping
///    them makes the *next* request malformed — dropping them here is safe:
///    OpenAI-compatible endpoints do not require reasoning to be echoed back.
pub fn build_request(
    model: String,
    prompt_blocks: Vec<PromptBlock>,
    tools: Vec<ToolDef>,
    messages: Vec<ModelMessage>,
    max_completion_tokens: Option<u32>,
) -> ChatCompletionsRequest {
    let mut chat_messages = Vec::with_capacity(messages.len() + 1);

    let system: String = prompt_blocks
        .into_iter()
        .map(|b| b.content)
        .filter(|c| !c.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    if !system.is_empty() {
        chat_messages.push(ChatMessage {
            content: Some(ChatContent::Text(system)),
            ..ChatMessage::new(ChatRole::System)
        });
    }

    for m in messages {
        push_translated(&mut chat_messages, m);
    }

    ChatCompletionsRequest {
        model,
        messages: chat_messages,
        tools: tools.into_iter().map(to_chat_tool).collect(),
        stream: true,
        stream_options: Some(StreamOptions {
            include_usage: true,
        }),
        max_completion_tokens,
    }
}

fn to_chat_tool(t: ToolDef) -> ChatTool {
    ChatTool {
        kind: ToolCallKind::Function,
        function: FunctionDef {
            name: t.name,
            description: t.description,
            parameters: t.input_schema,
        },
    }
}

/// Append the OpenAI messages one `ModelMessage` translates into.
///
/// Split out from `build_request` so the fan-out rules (tool results first,
/// content-only-if-non-empty, thinking dropped) are directly unit-testable.
fn push_translated(out: &mut Vec<ChatMessage>, m: ModelMessage) {
    let assistant = matches!(m.role, MessageRole::Assistant);

    let mut parts: Vec<ContentPart> = Vec::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut tool_messages: Vec<ChatMessage> = Vec::new();

    for block in m.content {
        match block {
            ModelContentBlock::Text { text } => parts.push(ContentPart::Text { text }),
            ModelContentBlock::Image { media_type, data } => parts.push(ContentPart::ImageUrl {
                image_url: ImageUrl {
                    url: format!("data:{media_type};base64,{data}"),
                },
            }),
            ModelContentBlock::ToolUse { id, name, input } => tool_calls.push(ToolCall {
                id,
                kind: ToolCallKind::Function,
                function: FunctionCall {
                    name,
                    // `input` is already a JSON value; OpenAI wants it as a
                    // string. `to_string` cannot fail for a `Value`.
                    arguments: input.to_string(),
                },
            }),
            ModelContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                // There is no `is_error` flag on an OpenAI tool message. The
                // model only ever sees the content, so the error signal has to
                // be carried in the text or it is lost entirely — losing it
                // makes a failed tool look like a successful one that returned
                // an error-shaped string.
                let text = if is_error.unwrap_or(false) {
                    format!("Error: {content}")
                } else {
                    content
                };
                tool_messages.push(ChatMessage {
                    content: Some(ChatContent::Text(text)),
                    tool_call_id: Some(tool_use_id),
                    ..ChatMessage::new(ChatRole::Tool)
                });
            }
            // Not representable; see `build_request`'s doc comment.
            ModelContentBlock::Thinking { .. } | ModelContentBlock::RedactedThinking { .. } => {}
        }
    }

    // Tool results first: OpenAI requires every `tool` message to follow the
    // assistant turn carrying the matching `tool_calls`, with nothing in
    // between.
    out.append(&mut tool_messages);

    let content = collapse(parts);
    if content.is_none() && tool_calls.is_empty() {
        // A message that was nothing but thinking blocks (or empty to begin
        // with) has no OpenAI representation — emitting an empty message would
        // be rejected as malformed.
        return;
    }
    out.push(ChatMessage {
        role: if assistant {
            ChatRole::Assistant
        } else {
            ChatRole::User
        },
        content,
        tool_calls,
        tool_call_id: None,
    });
}

/// Collapse content parts to the most compatible representation: a bare string
/// when everything is text, the typed parts array once an image is involved.
fn collapse(parts: Vec<ContentPart>) -> Option<ChatContent> {
    if parts.is_empty() {
        return None;
    }
    if parts.iter().all(|p| matches!(p, ContentPart::Text { .. })) {
        let joined = parts
            .into_iter()
            .map(|p| match p {
                ContentPart::Text { text } => text,
                ContentPart::ImageUrl { .. } => unreachable!("checked above"),
            })
            .collect::<Vec<_>>()
            .join("\n");
        return Some(ChatContent::Text(joined));
    }
    Some(ChatContent::Parts(parts))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::interface::prompt::{BlockRole, CacheStrategy};
    use serde_json::json;

    fn msg(role: MessageRole, content: Vec<ModelContentBlock>) -> ModelMessage {
        ModelMessage { role, content }
    }

    fn text(s: &str) -> ModelContentBlock {
        ModelContentBlock::Text { text: s.into() }
    }

    /// Whole-request wire snapshot: `stream` + `stream_options`, the collapsed
    /// system message, `max_completion_tokens`, and the tool shape.
    #[test]
    fn request_wire_shape() {
        let req = build_request(
            "gpt-4o".into(),
            vec![
                PromptBlock {
                    role: BlockRole::System,
                    content: "You are a coding agent.".into(),
                    // Dropped — OpenAI has no cache_control surface.
                    cache_strategy: Some(CacheStrategy::Ephemeral),
                    // Dropped too: annotation never reaches the wire.
                    name: Some("scene.skeleton".into()),
                    origin: Default::default(),
                },
                PromptBlock::system("Be terse."),
                // Empty blocks must not produce stray blank lines.
                PromptBlock::system("   "),
            ],
            vec![ToolDef {
                name: "Bash".into(),
                description: "Run a shell command".into(),
                input_schema: json!({"type": "object", "properties": {"cmd": {"type": "string"}}}),
                // Dropped by `to_chat_tool` — asserted below.
                source: Some("builtin".into()),
            }],
            vec![msg(MessageRole::User, vec![text("hi")])],
            Some(4096),
        );
        let v = serde_json::to_value(&req).unwrap();

        assert_eq!(v["model"], "gpt-4o");
        assert_eq!(v["stream"], true);
        assert_eq!(v["stream_options"]["include_usage"], true);
        assert_eq!(v["max_completion_tokens"], 4096);
        // No `max_tokens` — see the field's doc comment.
        assert!(v.get("max_tokens").is_none());

        assert_eq!(v["messages"][0]["role"], "system");
        assert_eq!(
            v["messages"][0]["content"],
            "You are a coding agent.\n\nBe terse."
        );
        assert!(v["messages"][0].get("cache_control").is_none());
        assert_eq!(v["messages"][1]["role"], "user");
        assert_eq!(v["messages"][1]["content"], "hi");

        assert_eq!(v["tools"][0]["type"], "function");
        assert_eq!(v["tools"][0]["function"]["name"], "Bash");
        assert_eq!(
            v["tools"][0]["function"]["description"],
            "Run a shell command"
        );
        assert_eq!(v["tools"][0]["function"]["parameters"]["type"], "object");

        // Recorder annotations are not part of the protocol. A request carrying
        // them must serialize to the same bytes as one that does not, or every
        // labelled request becomes a cache miss.
        assert!(!serde_json::to_string(&req).unwrap().contains("source"));
    }

    /// No system blocks ⇒ no system message at all (not an empty one), and no
    /// `tools` key when there are no tools.
    #[test]
    fn empty_system_and_tools_are_omitted() {
        let req = build_request(
            "m".into(),
            vec![],
            vec![],
            vec![msg(MessageRole::User, vec![text("hi")])],
            None,
        );
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["messages"].as_array().unwrap().len(), 1);
        assert_eq!(v["messages"][0]["role"], "user");
        assert!(v.get("tools").is_none());
        assert!(v.get("max_completion_tokens").is_none());
    }

    /// A tool-use assistant turn: text plus `tool_calls`, with the arguments
    /// serialized to a JSON *string*.
    #[test]
    fn assistant_tool_use_becomes_tool_calls_with_string_arguments() {
        let req = build_request(
            "m".into(),
            vec![],
            vec![],
            vec![msg(
                MessageRole::Assistant,
                vec![
                    text("Let me check."),
                    ModelContentBlock::ToolUse {
                        id: "call_1".into(),
                        name: "Read".into(),
                        input: json!({"file_path": "a.rs"}),
                    },
                ],
            )],
            None,
        );
        let v = serde_json::to_value(&req).unwrap();
        let m = &v["messages"][0];

        assert_eq!(m["role"], "assistant");
        assert_eq!(m["content"], "Let me check.");
        assert_eq!(m["tool_calls"][0]["id"], "call_1");
        assert_eq!(m["tool_calls"][0]["type"], "function");
        assert_eq!(m["tool_calls"][0]["function"]["name"], "Read");
        // A string, not an object — sending an object here is a 400.
        let args = m["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(args).unwrap(),
            json!({"file_path": "a.rs"})
        );
    }

    /// An assistant turn that is *only* a tool call must omit `content`
    /// entirely rather than sending an empty string.
    #[test]
    fn tool_call_only_assistant_turn_omits_content() {
        let req = build_request(
            "m".into(),
            vec![],
            vec![],
            vec![msg(
                MessageRole::Assistant,
                vec![ModelContentBlock::ToolUse {
                    id: "c".into(),
                    name: "Glob".into(),
                    input: json!({}),
                }],
            )],
            None,
        );
        let v = serde_json::to_value(&req).unwrap();
        assert!(v["messages"][0].get("content").is_none(), "got {v:#}");
    }

    /// The fan-out rule: tool results become their own `role: "tool"`
    /// messages, emitted ahead of any text in the same incoming message, and
    /// the error flag is folded into the text because OpenAI has nowhere else
    /// to put it.
    #[test]
    fn tool_results_fan_out_into_tool_messages_before_user_text() {
        let req = build_request(
            "m".into(),
            vec![],
            vec![],
            vec![msg(
                MessageRole::User,
                vec![
                    text("and also, please hurry"),
                    ModelContentBlock::ToolResult {
                        tool_use_id: "call_1".into(),
                        content: "file contents".into(),
                        is_error: None,
                    },
                    ModelContentBlock::ToolResult {
                        tool_use_id: "call_2".into(),
                        content: "no such file".into(),
                        is_error: Some(true),
                    },
                ],
            )],
            None,
        );
        let v = serde_json::to_value(&req).unwrap();
        let ms = v["messages"].as_array().unwrap();

        assert_eq!(ms.len(), 3);
        assert_eq!(ms[0]["role"], "tool");
        assert_eq!(ms[0]["tool_call_id"], "call_1");
        assert_eq!(ms[0]["content"], "file contents");
        assert_eq!(ms[1]["role"], "tool");
        assert_eq!(ms[1]["tool_call_id"], "call_2");
        assert_eq!(ms[1]["content"], "Error: no such file");
        // The text tags along after, as a plain user message.
        assert_eq!(ms[2]["role"], "user");
        assert_eq!(ms[2]["content"], "and also, please hurry");
    }

    /// Images become a `data:` URI inside an `image_url` part, and the
    /// presence of an image switches the whole message to the parts array.
    #[test]
    fn image_becomes_a_data_uri_content_part() {
        let req = build_request(
            "m".into(),
            vec![],
            vec![],
            vec![msg(
                MessageRole::User,
                vec![
                    text("what is this?"),
                    ModelContentBlock::Image {
                        media_type: "image/png".into(),
                        data: "AAAA".into(),
                    },
                ],
            )],
            None,
        );
        let v = serde_json::to_value(&req).unwrap();
        let content = &v["messages"][0]["content"];

        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "what is this?");
        assert_eq!(content[1]["type"], "image_url");
        assert_eq!(content[1]["image_url"]["url"], "data:image/png;base64,AAAA");
    }

    /// Thinking blocks have no Chat Completions representation. Dropping them
    /// is safe here (unlike the Anthropic path, where the signature is
    /// load-bearing for the *next* request) — but an assistant turn left with
    /// nothing else must be dropped whole rather than sent empty.
    #[test]
    fn thinking_blocks_are_dropped_and_empty_turns_disappear() {
        let req = build_request(
            "m".into(),
            vec![],
            vec![],
            vec![
                msg(
                    MessageRole::Assistant,
                    vec![
                        ModelContentBlock::Thinking {
                            text: "hmm".into(),
                            signature: Some("sig".into()),
                        },
                        ModelContentBlock::RedactedThinking {
                            data: "opaque".into(),
                        },
                    ],
                ),
                msg(MessageRole::User, vec![text("still here")]),
            ],
            None,
        );
        let v = serde_json::to_value(&req).unwrap();
        let ms = v["messages"].as_array().unwrap();

        assert_eq!(ms.len(), 1, "the thinking-only turn is gone: {v:#}");
        assert_eq!(ms[0]["role"], "user");
    }

    /// `MessageRole::System` inside the message history maps to `user`, for
    /// the same reason the Anthropic adapter does it: these are synthetic
    /// `<system-reminder>` injections, not operator-authority turns, and the
    /// leading `system` message is already reserved for the real prompt.
    #[test]
    fn system_role_in_history_maps_to_user() {
        let req = build_request(
            "m".into(),
            vec![],
            vec![],
            vec![msg(MessageRole::System, vec![text("<system-reminder>")])],
            None,
        );
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["messages"][0]["role"], "user");
    }
}
