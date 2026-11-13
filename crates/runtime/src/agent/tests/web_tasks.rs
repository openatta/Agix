//! WebSearch and WebFetch routed through the session model or `task_models`.

use super::*;

/// Answers a provider search with one page, noting which model name each
/// search asked for.
pub(super) struct SearchSpy {
    pub(super) page: &'static str,
    pub(super) asked: Arc<std::sync::Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl Model for SearchSpy {
    fn api_type(&self) -> base::provider::ApiType {
        base::provider::ApiType::Anthropic
    }
    async fn stream(
        &self,
        _prompt_blocks: Vec<base::prompt::PromptBlock>,
        _tools: Vec<base::interface::model::ToolDef>,
        _messages: Vec<base::interface::model::ModelMessage>,
        params: base::interface::model::StreamParams,
        _cancel: CancellationToken,
    ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError> {
        assert!(params.provider_search.is_some(), "only searches reach here");
        self.asked.lock().unwrap().push(params.model);
        Ok(Box::new(futures::stream::iter(vec![
            Ok(base::interface::model::ModelEvent::SearchResults {
                hits: vec![base::interface::model::SearchHit {
                    title: "page".into(),
                    url: self.page.into(),
                }],
            }),
            Ok(base::interface::model::ModelEvent::EndTurn {
                stop_reason: "end_turn".into(),
                usage: Default::default(),
            }),
        ])))
    }
}

pub(super) fn search_spy(
    page: &'static str,
) -> (Arc<SearchSpy>, Arc<std::sync::Mutex<Vec<String>>>) {
    let asked = Arc::new(std::sync::Mutex::new(Vec::new()));
    (
        Arc::new(SearchSpy {
            page,
            asked: asked.clone(),
        }),
        asked,
    )
}

pub(super) async fn search_once(agent: &Agent) -> String {
    let result = agent
        .tooling
        .tools
        .get("WebSearch")
        .expect("WebSearch is registered")
        .call(
            serde_json::json!({"query": "rust"}),
            platform::tool_context_for_test(std::env::temp_dir()),
            base::tool::ProgressSender::noop("t"),
        )
        .await
        .expect("the search answered");
    format!("{:?}", result.content)
}

#[tokio::test]
async fn web_search_goes_through_the_sessions_own_model() {
    let (session, asked) = search_spy("https://from.session.example");
    let (agent, _rx, _tx) = Builder::new()
        .scene(Arc::new(scene::code::spec(
            scene::code::Interaction::Interactive,
        )))
        .model(session as Arc<dyn Model>)
        .settings(Arc::new(test_settings()))
        .skip_warmup(true)
        .build()
        .expect("build should succeed");
    assert!(search_once(&agent).await.contains("from.session.example"));
    assert_eq!(
        *asked.lock().unwrap(),
        [test_settings().values.model.model_name.clone()]
    );
}

#[tokio::test]
async fn task_models_can_route_web_search_to_another_provider() {
    let (session, session_asked) = search_spy("https://from.session.example");
    let (searcher, searcher_asked) = search_spy("https://from.searcher.example");
    let router = base::provider::TaskRouter::new(
        [("searcher".to_string(), searcher as Arc<dyn Model>)].into(),
        [(
            WEB_SEARCH_TASK.to_string(),
            base::provider::ResolvedModel {
                provider_id: "searcher".into(),
                model: "search-model".into(),
            },
        )]
        .into(),
        session.clone() as Arc<dyn Model>,
    );
    let (agent, _rx, _tx) = Builder::new()
        .scene(Arc::new(scene::code::spec(
            scene::code::Interaction::Interactive,
        )))
        .model(session as Arc<dyn Model>)
        .settings(Arc::new(test_settings()))
        .task_router(Arc::new(router))
        .skip_warmup(true)
        .build()
        .expect("build should succeed");
    assert!(search_once(&agent).await.contains("from.searcher.example"));
    assert_eq!(*searcher_asked.lock().unwrap(), ["search-model"]);
    assert!(session_asked.lock().unwrap().is_empty());
}

/// Answers every call with `answer`, noting the model name and purpose
/// each one was made with.
pub(super) struct ExtractSpy {
    pub(super) answer: &'static str,
    pub(super) asked: Asked,
}

pub(super) type Asked = Arc<std::sync::Mutex<Vec<(String, Option<String>)>>>;

#[async_trait::async_trait]
impl Model for ExtractSpy {
    fn api_type(&self) -> base::provider::ApiType {
        base::provider::ApiType::Anthropic
    }
    async fn stream(
        &self,
        _prompt_blocks: Vec<base::prompt::PromptBlock>,
        _tools: Vec<base::interface::model::ToolDef>,
        _messages: Vec<base::interface::model::ModelMessage>,
        params: base::interface::model::StreamParams,
        _cancel: CancellationToken,
    ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError> {
        let purpose = params
            .origin
            .as_ref()
            .and_then(|o| o.purpose())
            .map(str::to_string);
        self.asked.lock().unwrap().push((params.model, purpose));
        Ok(Box::new(futures::stream::iter(vec![
            Ok(base::interface::model::ModelEvent::TextDelta {
                text: self.answer.into(),
            }),
            Ok(base::interface::model::ModelEvent::EndTurn {
                stop_reason: "end_turn".into(),
                usage: Default::default(),
            }),
        ])))
    }
}

pub(super) fn extract_spy(answer: &'static str) -> (Arc<ExtractSpy>, Asked) {
    let asked = Asked::default();
    (
        Arc::new(ExtractSpy {
            answer,
            asked: asked.clone(),
        }),
        asked,
    )
}

/// A local server answering every request with the same small page.
pub(super) async fn a_page() -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    const RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\n\
content-type: text/plain\r\n\
content-length: 14\r\n\
connection: close\r\n\
\r\n\
the whole page";
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let mut buf = vec![0u8; 4096];
            let _ = sock.read(&mut buf).await;
            let _ = sock.write_all(RESPONSE).await;
            let _ = sock.shutdown().await;
        }
    });
    format!("http://127.0.0.1:{port}/")
}

pub(super) async fn fetch_once(agent: &Agent, url: &str) -> String {
    let mut ctx = platform::tool_context_for_test(std::env::temp_dir());
    ctx.exec = platform::local_providers().with_network(Arc::new(
        platform::LocalNetwork::for_agent_policy(
            base::context::config::NetworkModeConfig::Unrestricted,
            vec!["127.0.0.1".into()],
        ),
    ));
    let result = agent
        .tooling
        .tools
        .get("WebFetch")
        .expect("WebFetch is registered")
        .call(
            serde_json::json!({"url": url, "prompt": "what does it say?"}),
            ctx,
            base::tool::ProgressSender::noop("t"),
        )
        .await
        .expect("the fetch answered");
    format!("{:?}", result.content)
}

pub(super) fn templated() -> Arc<InMemoryToolRegistry> {
    let template = Arc::new(InMemoryToolRegistry::new());
    tools::register_builtin_tools(&template);
    template
}

#[tokio::test]
async fn task_models_web_fetch_extracts_pages_on_the_routed_model() {
    let (session, session_asked) = extract_spy("from the session");
    let (fetcher, fetcher_asked) = extract_spy("from the fetcher");
    let router = base::provider::TaskRouter::new(
        [("fetcher".to_string(), fetcher as Arc<dyn Model>)].into(),
        [(
            WEB_FETCH_TASK.to_string(),
            base::provider::ResolvedModel {
                provider_id: "fetcher".into(),
                model: "fetch-model".into(),
            },
        )]
        .into(),
        session.clone() as Arc<dyn Model>,
    );
    let (agent, _rx, _tx) = Builder::new()
        .scene(Arc::new(scene::code::spec(
            scene::code::Interaction::Interactive,
        )))
        .model(session as Arc<dyn Model>)
        .settings(Arc::new(test_settings()))
        .tools(templated())
        .task_router(Arc::new(router))
        .skip_warmup(true)
        .build()
        .expect("build should succeed");
    assert_eq!(
        agent
            .tooling
            .tools
            .names()
            .iter()
            .filter(|n| *n == "WebFetch")
            .count(),
        1,
        "the template's WebFetch is replaced, not shadowed"
    );

    let result = fetch_once(&agent, &a_page().await).await;
    assert!(result.contains("from the fetcher"), "{result}");
    assert!(result.contains("mode=extracted by fetch-model"), "{result}");
    assert_eq!(
        *fetcher_asked.lock().unwrap(),
        [(
            "fetch-model".to_string(),
            Some(base::interface::model::call_purpose::WEB_FETCH.to_string())
        )]
    );
    assert!(session_asked.lock().unwrap().is_empty());
}

#[tokio::test]
async fn without_task_models_web_fetch_no_model_reads_the_page() {
    let (session, session_asked) = extract_spy("from the session");
    let (other, other_asked) = extract_spy("from another task's model");
    let router = base::provider::TaskRouter::new(
        [("other".to_string(), other as Arc<dyn Model>)].into(),
        [(
            WEB_SEARCH_TASK.to_string(),
            base::provider::ResolvedModel {
                provider_id: "other".into(),
                model: "search-model".into(),
            },
        )]
        .into(),
        session.clone() as Arc<dyn Model>,
    );
    let (agent, _rx, _tx) = Builder::new()
        .scene(Arc::new(scene::code::spec(
            scene::code::Interaction::Interactive,
        )))
        .model(session as Arc<dyn Model>)
        .settings(Arc::new(test_settings()))
        .tools(templated())
        .task_router(Arc::new(router))
        .skip_warmup(true)
        .build()
        .expect("build should succeed");

    let result = fetch_once(&agent, &a_page().await).await;
    assert!(result.contains("the whole page"), "{result}");
    assert!(result.contains("mode=page text"), "{result}");
    assert!(session_asked.lock().unwrap().is_empty());
    assert!(other_asked.lock().unwrap().is_empty());
}
