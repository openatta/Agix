//! NativeSearchProvider —— 让模型提供方自己搜索。
//!
//! `WebSearch` 默认的后端：发一次带 `StreamParams::provider_search` 的调用，
//! 收集提供方找到的页面（`ModelEvent::SearchResults`）与它的说明文字。走的是
//! 会话自己的 `Model`，所以跟着会话选的提供方走，也照常进录像与用量统计；
//! 没有自带搜索的提供方会拒绝这次调用，错误原样交给模型。

use crate::web_search::{
    SearchError, SearchOutput, SearchOutputItem, SearchProvider, SearchResult,
};
use async_trait::async_trait;
use base::interface::model::{
    call_purpose, CallOrigin, MessageRole, Model, ModelContentBlock, ModelEvent, ModelMessage,
    ProviderSearch, StreamParams,
};
use base::interface::prompt::PromptBlock;
use base::interface::settings::ThinkingMode;
use futures::stream::StreamExt;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// The most searches one `WebSearch` call lets the provider make.
const MAX_SEARCHES: usize = 8;

pub struct NativeSearchProvider {
    model: Arc<dyn Model>,
    model_name: String,
}

impl std::fmt::Debug for NativeSearchProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeSearchProvider")
            .field("model_name", &self.model_name)
            .finish_non_exhaustive()
    }
}

impl NativeSearchProvider {
    pub fn new(model: Arc<dyn Model>, model_name: impl Into<String>) -> Self {
        Self {
            model,
            model_name: model_name.into(),
        }
    }
}

#[async_trait]
impl SearchProvider for NativeSearchProvider {
    async fn search(
        &self,
        query: &str,
        max_results: usize,
        session_id: &str,
    ) -> Result<SearchOutput, SearchError> {
        let params = StreamParams {
            model: self.model_name.clone(),
            max_tokens: 8192,
            thinking_mode: ThinkingMode::Off,
            fallback_model: None,
            cache_edits: Vec::new(),
            origin: Some(CallOrigin::auxiliary(session_id, call_purpose::WEB_SEARCH)),
            input_map: None,
            provider_search: Some(ProviderSearch {
                max_uses: max_results.clamp(1, MAX_SEARCHES) as u32,
            }),
        };
        let mut stream = self
            .model
            .stream(
                vec![PromptBlock::system(
                    "You are an assistant for performing a web search tool use.",
                )],
                Vec::new(),
                vec![ModelMessage {
                    role: MessageRole::User,
                    content: vec![ModelContentBlock::Text {
                        text: format!("Perform a web search for the query: {query}"),
                    }],
                }],
                params,
                CancellationToken::new(),
            )
            .await
            .map_err(|e| SearchError::Other(anyhow::Error::new(e)))?;

        let mut items: Vec<SearchOutputItem> = Vec::new();
        while let Some(event) = stream.next().await {
            match event.map_err(|e| SearchError::Other(anyhow::Error::new(e)))? {
                ModelEvent::SearchResults { hits } => {
                    items.push(SearchOutputItem::Links(
                        hits.into_iter()
                            .map(|hit| SearchResult {
                                title: hit.title,
                                url: hit.url,
                                snippet: String::new(),
                            })
                            .collect(),
                    ));
                }
                ModelEvent::TextDelta { text }
                | ModelEvent::ContentBlockStart {
                    block: ModelContentBlock::Text { text },
                    ..
                } => match items.last_mut() {
                    Some(SearchOutputItem::Text(last)) => last.push_str(&text),
                    _ if text.is_empty() => {}
                    _ => items.push(SearchOutputItem::Text(text)),
                },
                ModelEvent::EndTurn { .. } => break,
                _ => {}
            }
        }
        Ok(SearchOutput { items })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::interface::model::{ModelError, ModelStream, SearchHit, ToolDef};

    /// Answers with `events`, keeping the params it was called with.
    struct SearchingModel {
        events: Vec<ModelEvent>,
        refuse: bool,
        asked: std::sync::Mutex<Vec<StreamParams>>,
    }

    impl SearchingModel {
        fn answering(events: Vec<ModelEvent>) -> Arc<Self> {
            Arc::new(Self {
                events,
                refuse: false,
                asked: Default::default(),
            })
        }
    }

    #[async_trait]
    impl Model for SearchingModel {
        fn api_type(&self) -> base::provider::ApiType {
            base::provider::ApiType::Anthropic
        }
        async fn stream(
            &self,
            _prompt_blocks: Vec<PromptBlock>,
            tools: Vec<ToolDef>,
            _messages: Vec<ModelMessage>,
            params: StreamParams,
            _cancel: CancellationToken,
        ) -> Result<ModelStream, ModelError> {
            assert!(tools.is_empty(), "a search call offers no tools of its own");
            self.asked.lock().unwrap().push(params);
            if self.refuse {
                return Err(ModelError::Unsupported("no search here".into()));
            }
            Ok(Box::new(futures::stream::iter(
                self.events.clone().into_iter().map(Ok),
            )))
        }
    }

    fn hits(pages: &[(&str, &str)]) -> ModelEvent {
        ModelEvent::SearchResults {
            hits: pages
                .iter()
                .map(|(title, url)| SearchHit {
                    title: title.to_string(),
                    url: url.to_string(),
                })
                .collect(),
        }
    }

    fn end() -> ModelEvent {
        ModelEvent::EndTurn {
            stop_reason: "end_turn".into(),
            usage: Default::default(),
        }
    }

    #[tokio::test]
    async fn the_pages_and_what_the_provider_said_come_back_in_order() {
        let model = SearchingModel::answering(vec![
            ModelEvent::TextDelta {
                text: "Searching. ".into(),
            },
            hits(&[
                ("Rust Lang", "https://rust-lang.org"),
                ("Learn Rust", "https://learn-rust.org"),
            ]),
            ModelEvent::TextDelta {
                text: "Rust is a systems ".into(),
            },
            ModelEvent::TextDelta {
                text: "programming language.".into(),
            },
            end(),
        ]);
        let output = NativeSearchProvider::new(model, "m")
            .search("rust", 10, "s-1")
            .await
            .unwrap();
        assert!(matches!(&output.items[0], SearchOutputItem::Text(t) if t == "Searching. "));
        let SearchOutputItem::Links(links) = &output.items[1] else {
            panic!("{:?}", output.items);
        };
        assert_eq!(links[1].url, "https://learn-rust.org");
        assert!(matches!(
            &output.items[2],
            SearchOutputItem::Text(t) if t == "Rust is a systems programming language."
        ));
    }

    #[tokio::test]
    async fn the_call_asks_the_provider_to_search_on_the_sessions_behalf() {
        let model = SearchingModel::answering(vec![hits(&[]), end()]);
        NativeSearchProvider::new(model.clone(), "the-session-model")
            .search("anything", 25, "s-7")
            .await
            .unwrap();
        let asked = model.asked.lock().unwrap();
        let params = &asked[0];
        assert_eq!(params.model, "the-session-model");
        assert_eq!(
            params.provider_search,
            Some(ProviderSearch {
                max_uses: MAX_SEARCHES as u32
            })
        );
        assert_eq!(params.thinking_mode, ThinkingMode::Off);
        let origin = params.origin.as_ref().expect("attributed");
        assert_eq!(origin.session_id, "s-7");
        assert_eq!(origin.purpose(), Some(call_purpose::WEB_SEARCH));
    }

    #[tokio::test]
    async fn a_provider_without_search_says_so() {
        let model = Arc::new(SearchingModel {
            events: Vec::new(),
            refuse: true,
            asked: Default::default(),
        });
        let err = NativeSearchProvider::new(model, "m")
            .search("anything", 5, "s")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no search here"), "{err}");
    }
}
