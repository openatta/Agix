//! **Q1 **: concrete `SecondaryLlm` implementation backed by an
//! AnthropicClient. Used by `WebFetchTool` to route per-fetch extraction
//! through a cheap (haiku-tier) model so the main turn doesn't have to
//! ingest a full HTML page.
//!
//! Routing model: caller supplies the model id. Conventionally
//! `EngineConfig.compact_model` — same cheap-model knob compactor uses.

use async_trait::async_trait;
use base::message::{ContentBlock, Role};
use base::tool::SecondaryLlm;
use futures::stream::StreamExt;
use model::client::AnthropicClient;
use model::stream::{BlockDelta, ContentBlockStart, StreamEvent};
use model::types::{MessageParam, MessagesRequest, SystemBlock};
use std::sync::Arc;

const SECONDARY_LLM_SYSTEM_PROMPT: &str = "\
You are an information-extraction assistant. The user pasted a web page's \
content and asked a question about it. Your job: read the page, then answer \
the question using ONLY information found on the page.\n\
\n\
Rules:\n\
- Be terse. Output only the directly relevant content.\n\
- Quote the page verbatim where useful (paragraphs, code blocks, tables).\n\
- If the page doesn't contain the answer, say so in one sentence.\n\
- Do NOT hallucinate. Do NOT add commentary.\n\
- Skip nav menus, ads, footers — they're noise.";

pub struct AnthropicSecondaryLlm {
    client: Arc<dyn AnthropicClient>,
    model: String,
    /// Output token cap for the secondary call. Default 2048.
    max_tokens: u32,
}

impl AnthropicSecondaryLlm {
    /// Construct a new instance.
    pub fn new(client: Arc<dyn AnthropicClient>, model: impl Into<String>) -> Self {
        Self {
            client,
            model: model.into(),
            max_tokens: 2048,
        }
    }

    /// Builder: set max tokens.
    pub fn with_max_tokens(mut self, n: u32) -> Self {
        self.max_tokens = n;
        self
    }
}

#[async_trait]
impl SecondaryLlm for AnthropicSecondaryLlm {
    async fn extract_with_prompt(&self, prompt: &str, content: &str) -> Result<String, String> {
        let req = MessagesRequest {
            model: self.model.clone(),
            max_tokens: self.max_tokens,
            system: vec![SystemBlock::text(SECONDARY_LLM_SYSTEM_PROMPT)],
            messages: vec![MessageParam {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: format!(
                        "Page content:\n\n---\n{content}\n---\n\nUser's question / instruction:\n{prompt}"
                    ),
                    cache_control: None,
                }],
            }],
            tools: vec![],
            anthropic_tools: vec![],
            tool_choice: None,
            stream: true,
            thinking: None,
            effort: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: vec![],
            metadata: None,
            betas: vec![],
            speed: None,
        };

        let mut stream = self.client.stream_messages(req);
        let mut answer = String::new();
        while let Some(ev) = stream.next().await {
            let event = ev.map_err(|e| format!("network error: {e}"))?;
            match event {
                StreamEvent::ContentBlockStart {
                    content_block: ContentBlockStart::Text { text },
                    ..
                } => answer.push_str(&text),
                StreamEvent::ContentBlockDelta {
                    delta: BlockDelta::TextDelta { text },
                    ..
                } => answer.push_str(&text),
                StreamEvent::MessageStop => break,
                _ => {}
            }
        }
        let answer = answer.trim();
        if answer.is_empty() {
            return Err("empty response from secondary model".to_string());
        }
        Ok(answer.to_string())
    }
}

// AgentSecondaryLlm bridge impl removed — SecondaryLlm already implemented above.

#[cfg(test)]
mod tests {
    use super::*;
    use model::mock::MockAnthropicClient;
    use model::stream::{
        ContentBlockStart, MessageDeltaPayload, MessageStartPayload, StreamEvent, Usage,
    };

    fn text_response(text: &'static str) -> Vec<StreamEvent> {
        vec![
            StreamEvent::MessageStart {
                message: MessageStartPayload {
                    id: "msg".into(),
                    role: "assistant".into(),
                    model: "test".into(),
                    usage: Usage::default(),
                    stop_reason: None,
                },
            },
            StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlockStart::Text { text: text.into() },
            },
            StreamEvent::ContentBlockStop { index: 0 },
            StreamEvent::MessageDelta {
                delta: MessageDeltaPayload {
                    stop_reason: Some(base::message::StopReason::EndTurn),
                    stop_sequence: None,
                },
                usage: None,
            },
            StreamEvent::MessageStop,
        ]
    }

    #[tokio::test]
    async fn extract_returns_assistant_text() {
        let mock = Arc::new(MockAnthropicClient::new());
        mock.push_turn(text_response("the answer is 42"));
        let secondary = AnthropicSecondaryLlm::new(mock, "claude-haiku-4-5");
        let r = <AnthropicSecondaryLlm as SecondaryLlm>::extract_with_prompt(
            &secondary,
            "what's the answer?",
            "page text",
        )
        .await
        .unwrap();
        assert!(r.contains("42"));
    }

    #[tokio::test]
    async fn empty_response_returns_error() {
        let mock = Arc::new(MockAnthropicClient::new());
        mock.push_turn(text_response("   "));
        let secondary = AnthropicSecondaryLlm::new(mock, "claude-haiku-4-5");
        let r = <AnthropicSecondaryLlm as SecondaryLlm>::extract_with_prompt(
            &secondary, "question", "page",
        )
        .await;
        assert!(r.is_err());
    }
}
