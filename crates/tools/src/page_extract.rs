//! PageExtractor —— 用一个模型从抓到的页面里取出 `WebFetch` 要的东西。
//!
//! `task_models.web_fetch` 配了模型时 `WebFetch` 才带上它：把页面文本和调用方
//! 的 `prompt` 交给那个模型，返回它的回答而不是整页。走的是 `dyn Model`，所以
//! 照常进录像、用量统计，也跟着路由到任何提供方。

use base::interface::model::{
    call_purpose, CallOrigin, MessageRole, Model, ModelContentBlock, ModelError, ModelEvent,
    ModelMessage, StreamParams,
};
use base::prompt::PromptBlock;
use base::settings::ThinkingMode;
use futures::stream::StreamExt;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

const MAX_OUTPUT_TOKENS: u32 = 2048;
const MAX_PAGE_CHARS: usize = 80_000;

const SYSTEM_PROMPT: &str = "\
You are an information-extraction assistant. You are given the content of a \
web page and a request about it. Answer the request using ONLY information \
found on the page.\n\
\n\
Rules:\n\
- Be terse. Output only the directly relevant content.\n\
- Quote the page verbatim where useful (paragraphs, code blocks, tables).\n\
- If the page doesn't contain the answer, say so in one sentence.\n\
- Do not invent anything and do not add commentary.\n\
- Skip navigation menus, ads and footers.";

pub struct PageExtractor {
    model: Arc<dyn Model>,
    model_name: String,
}

impl std::fmt::Debug for PageExtractor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PageExtractor")
            .field("model_name", &self.model_name)
            .finish_non_exhaustive()
    }
}

impl PageExtractor {
    pub fn new(model: Arc<dyn Model>, model_name: impl Into<String>) -> Self {
        Self {
            model,
            model_name: model_name.into(),
        }
    }

    pub fn model_name(&self) -> &str {
        &self.model_name
    }

    pub async fn extract(
        &self,
        request: &str,
        page: &str,
        session_id: &str,
        cancel: CancellationToken,
    ) -> Result<String, ModelError> {
        let params = StreamParams {
            model: self.model_name.clone(),
            max_tokens: MAX_OUTPUT_TOKENS,
            thinking_mode: ThinkingMode::Off,
            effort: None,
            fallback_model: None,
            cache_edits: Vec::new(),
            origin: Some(CallOrigin::auxiliary(session_id, call_purpose::WEB_FETCH)),
            input_map: None,
            provider_search: None,
        };
        let mut stream = self
            .model
            .stream(
                vec![PromptBlock::system(SYSTEM_PROMPT)],
                Vec::new(),
                vec![ModelMessage {
                    role: MessageRole::User,
                    content: vec![ModelContentBlock::Text {
                        text: format!("Web page content:\n---\n{}\n---\n\n{request}", capped(page)),
                    }],
                }],
                params,
                cancel,
            )
            .await?;

        let mut answer = String::new();
        while let Some(event) = stream.next().await {
            match event? {
                ModelEvent::TextDelta { text }
                | ModelEvent::ContentBlockStart {
                    block: ModelContentBlock::Text { text },
                    ..
                } => answer.push_str(&text),
                ModelEvent::EndTurn { .. } => break,
                _ => {}
            }
        }
        Ok(answer)
    }
}

fn capped(page: &str) -> String {
    if page.chars().count() <= MAX_PAGE_CHARS {
        page.to_string()
    } else {
        let head: String = page.chars().take(MAX_PAGE_CHARS).collect();
        format!("{head}\n\n[Content truncated due to length...]")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use base::interface::model::{ModelStream, ToolDef};

    /// Answers with `events`, keeping the params and messages it was called with.
    struct ExtractingModel {
        events: Vec<ModelEvent>,
        asked: std::sync::Mutex<Vec<(StreamParams, Vec<ModelMessage>)>>,
    }

    impl ExtractingModel {
        fn answering(events: Vec<ModelEvent>) -> Arc<Self> {
            Arc::new(Self {
                events,
                asked: Default::default(),
            })
        }
    }

    #[async_trait]
    impl Model for ExtractingModel {
        fn api_type(&self) -> base::provider::ApiType {
            base::provider::ApiType::Anthropic
        }
        async fn stream(
            &self,
            _prompt_blocks: Vec<PromptBlock>,
            tools: Vec<ToolDef>,
            messages: Vec<ModelMessage>,
            params: StreamParams,
            _cancel: CancellationToken,
        ) -> Result<ModelStream, ModelError> {
            assert!(tools.is_empty(), "an extraction offers no tools");
            self.asked.lock().unwrap().push((params, messages));
            Ok(Box::new(futures::stream::iter(
                self.events.clone().into_iter().map(Ok),
            )))
        }
    }

    fn end() -> ModelEvent {
        ModelEvent::EndTurn {
            stop_reason: "end_turn".into(),
            usage: Default::default(),
        }
    }

    #[tokio::test]
    async fn the_answer_is_the_text_the_model_streamed() {
        let model = ExtractingModel::answering(vec![
            ModelEvent::ThinkingDelta {
                text: "not part of it".into(),
            },
            ModelEvent::TextDelta {
                text: "Version ".into(),
            },
            ModelEvent::TextDelta {
                text: "1.2.3".into(),
            },
            end(),
        ]);
        let answer = PageExtractor::new(model, "m")
            .extract("which version?", "page", "s", CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(answer, "Version 1.2.3");
    }

    #[tokio::test]
    async fn the_call_is_attributed_to_the_session_as_a_web_fetch() {
        let model = ExtractingModel::answering(vec![end()]);
        PageExtractor::new(model.clone(), "the-fetch-model")
            .extract(
                "which version?",
                "the page",
                "s-3",
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let asked = model.asked.lock().unwrap();
        let (params, messages) = &asked[0];
        assert_eq!(params.model, "the-fetch-model");
        assert_eq!(params.max_tokens, MAX_OUTPUT_TOKENS);
        assert_eq!(params.thinking_mode, ThinkingMode::Off);
        assert_eq!(params.provider_search, None);
        let origin = params.origin.as_ref().expect("attributed");
        assert_eq!(origin.session_id, "s-3");
        assert_eq!(origin.purpose(), Some(call_purpose::WEB_FETCH));
        let ModelContentBlock::Text { text } = &messages[0].content[0] else {
            panic!("{messages:?}");
        };
        assert!(text.contains("the page") && text.ends_with("which version?"));
    }

    #[tokio::test]
    async fn a_long_page_is_cut_before_it_is_sent() {
        let model = ExtractingModel::answering(vec![end()]);
        let page = "x".repeat(MAX_PAGE_CHARS + 10);
        PageExtractor::new(model.clone(), "m")
            .extract("q", &page, "s", CancellationToken::new())
            .await
            .unwrap();
        let asked = model.asked.lock().unwrap();
        let ModelContentBlock::Text { text } = &asked[0].1[0].content[0] else {
            panic!();
        };
        assert!(text.contains("[Content truncated due to length...]"));
        assert!(text.chars().filter(|c| *c == 'x').count() == MAX_PAGE_CHARS);
    }
}
