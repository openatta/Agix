//! WebFetchTool —— "WebFetch"。
//!
//! 通过执行层的 `Network` 抓 URL；content-type 含 html 的用 html2text 转成纯
//! 文本，其它按 utf-8 解码。5 MB 上限、15s 超时，不跟随重定向。
//!
//! 带了 [`PageExtractor`]（`task_models.web_fetch` 配了模型）且调用给了
//! `prompt` 时，返回那个模型对 `prompt` 的回答；否则返回页面文本。抽取失败或
//! 答空时退回页面文本，结果头里写明。调用触及主机（`Reach::Host`），runtime 把整个
//! 结果放进 external-content 定界块；定界之后至多 50_000 字符，与工具结果预算的单条
//! 上限相同。

use crate::cancel::run_with_cancel;
use crate::page_extract::PageExtractor;
use async_trait::async_trait;
use base::error::ToolError;
use base::interface::exec::{HttpRequest, Network, Origin};
use base::interface::permission::{PermissionFacts, Reach, Subject};
use base::tool::{
    PathStyle, PermissionDecision, ProgressSender, PromptContext, Tool, ToolContext, ToolResult,
    ToolSpec, ValidationResult,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

const MAX_BYTES: u64 = 5 * 1024 * 1024;
const MAX_OUTPUT_CHARS: usize = 50_000;
const TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WebFetchInput {
    /// HTTP/HTTPS URL to fetch.
    pub url: String,

    /// What you want from the page. When page extraction is enabled a model
    /// answers this from the page; otherwise it is not acted on and the page
    /// text is returned, the whole result at most 50,000 characters.
    #[serde(default)]
    pub prompt: Option<String>,
}

#[derive(Default, Debug, Clone)]
pub struct WebFetchTool {
    extractor: Option<Arc<PageExtractor>>,
}

impl WebFetchTool {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_extractor(extractor: PageExtractor) -> Self {
        Self {
            extractor: Some(Arc::new(extractor)),
        }
    }

    pub fn extracts(&self) -> bool {
        self.extractor.is_some()
    }

    async fn body_for(
        &self,
        prompt: Option<&str>,
        text: &str,
        ctx: &ToolContext,
    ) -> Result<(String, String), ToolError> {
        let prompt = prompt.filter(|p| !p.trim().is_empty());
        let (Some(extractor), Some(prompt)) = (&self.extractor, prompt) else {
            return Ok(("page text".into(), text.to_string()));
        };
        let extracting = extractor.extract(prompt, text, &ctx.session_id, ctx.cancel.clone());
        match run_with_cancel(&ctx.cancel, extracting).await? {
            Ok(answer) if !answer.trim().is_empty() => {
                Ok((format!("extracted by {}", extractor.model_name()), answer))
            }
            Ok(_) => Ok((
                "page text; the extraction model returned nothing".into(),
                text.to_string(),
            )),
            Err(e) => {
                tracing::warn!(error = %e, "WebFetch extraction failed; returning the page text");
                Ok((
                    format!("page text; extraction failed: {e}"),
                    text.to_string(),
                ))
            }
        }
    }
}

#[async_trait]
impl Tool for WebFetchTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new()
            .callable_from_code(true)
            .concurrency_safe(true)
            .read_only(true)
    }
    fn name(&self) -> &str {
        "WebFetch"
    }

    fn description(&self) -> &str {
        if self.extracts() {
            "Fetch a URL and have a model answer `prompt` from the page."
        } else {
            "Fetch a URL and return the page as plain text."
        }
    }

    fn input_schema(&self, _: PathStyle) -> Value {
        crate::input_schema::<WebFetchInput>()
    }

    async fn guide(&self, _: &PromptContext) -> Option<String> {
        Some(if self.extracts() {
            include_str!("prompts/coding/web_fetch.extract.prompt.md").to_string()
        } else {
            include_str!("prompts/coding/web_fetch.prompt.md").to_string()
        })
    }

    fn accept_edits_allows(&self, input: &Value, ctx: &ToolContext) -> bool {
        serde_json::from_value::<WebFetchInput>(input.clone())
            .is_ok_and(|i| crate::security::allowed_domains_name(&i.url, ctx))
    }

    fn permission_match_content(&self, input: &Value) -> Option<String> {
        serde_json::from_value::<WebFetchInput>(input.clone())
            .ok()
            .map(|i| i.url)
    }

    async fn validate_input(&self, input: &Value, _: &ToolContext) -> ValidationResult {
        let parsed: Result<WebFetchInput, _> = serde_json::from_value(input.clone());
        match parsed {
            Ok(p) if p.url.is_empty() => ValidationResult::err("url must not be empty", 1),
            Ok(p) if !p.url.starts_with("http://") && !p.url.starts_with("https://") => {
                ValidationResult::err("url must start with http:// or https://", 2)
            }
            Ok(_) => ValidationResult::Ok,
            Err(e) => ValidationResult::err(format!("invalid input: {e}"), 3),
        }
    }

    async fn check_permissions(&self, _: &Value, _: &ToolContext) -> PermissionDecision {
        // 网络访问默认 Ask；上层 Policy 用 `WebFetch(https://example.com/**)`
        // 之类的规则放行
        PermissionDecision::Ask {
            message: "WebFetch requires confirmation".into(),
            decision_reason: None,
        }
    }

    async fn permission_facts(&self, input: &Value, ctx: &ToolContext) -> PermissionFacts {
        let subject = self
            .permission_match_content(input)
            .map_or(Subject::None, Subject::Opaque);
        let listed = serde_json::from_value::<WebFetchInput>(input.clone())
            .ok()
            .map(|i| crate::security::allowed_domains_name(&i.url, ctx));
        let reach = listed.map_or(Reach::Unknown, |listed| Reach::Host { listed });
        PermissionFacts::new(subject, reach).with_question("WebFetch requires confirmation")
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolContext,
        _progress: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let input: WebFetchInput = serde_json::from_value(input)?;
        let fetching = tokio::time::timeout(TIMEOUT, fetch(&*ctx.exec.network, &input.url));
        let result = match run_with_cancel(&ctx.cancel, fetching).await? {
            Ok(result) => result,
            Err(_elapsed) => return Err(ToolError::Timeout(TIMEOUT)),
        };
        match result {
            Ok((status, text)) => {
                let (mode, body) = self.body_for(input.prompt.as_deref(), &text, &ctx).await?;
                let prompt = match &input.prompt {
                    Some(p) => format!(" | prompt was: {p}"),
                    None => String::new(),
                };
                let header = format!(
                    "[HTTP {status} | url={}{prompt} | mode={mode}]\n\n",
                    input.url
                );
                Ok(ToolResult::text(within_cap(&header, &input.url, &body)))
            }
            Err(FetchError::HttpStatus { status, body }) => Ok(ToolResult::error_text(format!(
                "HTTP {status}\n{}",
                truncate_chars(&body, 4000)
            ))),
            Err(FetchError::Redirect(location)) => Ok(ToolResult::text(format!(
                "Redirected to: {location}\n\nRe-fetch this URL to continue."
            ))),
            Err(FetchError::Egress(e)) => Err(e.into()),
        }
    }
}

/// `header`, then `body`, cut so the whole stays within [`MAX_OUTPUT_CHARS`]
/// once the runtime has fenced it as content from the web (the result of a
/// call that reaches a host is).
fn within_cap(header: &str, url: &str, body: &str) -> String {
    let render = |body: &str| format!("{header}{body}");
    let fenced_chars = |text: &str| {
        crate::external_content::fence(url, "the web", text)
            .chars()
            .count()
    };
    let whole = render(body);
    let total = fenced_chars(&whole);
    if total <= MAX_OUTPUT_CHARS {
        return whole;
    }
    let body_chars = body.chars().count();
    let note = format!("\n\n[truncated: the page is {body_chars} characters]");
    let mut room = MAX_OUTPUT_CHARS.saturating_sub(total - body_chars + note.chars().count());
    loop {
        let kept: String = body.chars().take(room).collect();
        let out = render(&format!("{kept}{note}"));
        let excess = fenced_chars(&out).saturating_sub(MAX_OUTPUT_CHARS);
        if excess == 0 || room == 0 {
            return out;
        }
        room = room.saturating_sub(excess);
    }
}

#[derive(thiserror::Error, Debug)]
enum FetchError {
    #[error("HTTP {status}")]
    HttpStatus { status: u16, body: String },
    #[error("redirect: {0}")]
    Redirect(String),
    #[error(transparent)]
    Egress(#[from] base::interface::exec::ExecError),
}

/// One model-directed request, through the deployment's egress.
///
/// Redirects are not followed: which URL to fetch next is the model's call,
/// and a provider that chased the chain itself would be reaching hosts the
/// model never named.
async fn fetch(net: &dyn Network, url: &str) -> Result<(u16, String), FetchError> {
    let resp = net
        .send(
            HttpRequest::get(url).max_bytes(MAX_BYTES).max_redirects(0),
            Origin::Agent,
        )
        .await?;

    let status = resp.status;
    if (300..400).contains(&status) {
        if let Some(location) = resp.headers.get("location") {
            return Err(FetchError::Redirect(location.clone()));
        }
    }

    let content_type = resp
        .headers
        .get("content-type")
        .cloned()
        .unwrap_or_else(|| "application/octet-stream".to_string());

    if !(200..300).contains(&status) {
        // 把 body（截断）作为 error message
        return Err(FetchError::HttpStatus {
            status,
            body: resp.body_text(),
        });
    }

    let text = if content_type.contains("html") {
        html2text::from_read(&resp.body[..], 100)
    } else {
        // 其它 content type（json / text / xml / md / 二进制）按 utf-8 lossy
        resp.body_text()
    };

    Ok((status, text))
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{head}…[truncated]")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::Path;

    /// The pages these tests serve are on this machine, which a model-directed
    /// fetch reaches only when the deployment names the host.
    fn ctx_in(cwd: &Path) -> ToolContext {
        let mut ctx = platform::tool_context_for_test(cwd.to_path_buf());
        ctx.exec = platform::local_providers().with_network(Arc::new(
            platform::LocalNetwork::for_agent_policy(
                base::context::config::NetworkModeConfig::Unrestricted,
                vec!["127.0.0.1".into()],
            ),
        ));
        ctx
    }

    #[tokio::test]
    async fn a_page_on_this_machine_is_refused_unless_the_deployment_names_it() {
        use std::sync::atomic::Ordering;

        let (port, seen) = a_server_that_would_answer().await;
        for host in ["127.0.0.1", "localhost", "0.0.0.0", "[::ffff:127.0.0.1]"] {
            let refused = WebFetchTool::new()
                .call(
                    json!({ "url": format!("http://{host}:{port}/") }),
                    platform::tool_context_for_test("/tmp".into()),
                    ProgressSender::noop("t"),
                )
                .await;
            assert!(
                matches!(&refused, Err(ToolError::Denied(m)) if m.contains("allowed_domains")),
                "{host}: {refused:?}"
            );
        }
        assert_eq!(seen.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn name_is_webfetch() {
        let tool = WebFetchTool::new();
        assert_eq!(tool.name(), "WebFetch");
        assert!(tool.call_traits(&Value::Null).read_only);
        assert!(tool.call_traits(&Value::Null).concurrency_safe);
    }

    #[tokio::test]
    async fn empty_url_validates_err() {
        let tool = WebFetchTool::new();
        let r = tool
            .validate_input(&json!({"url": ""}), &ctx_in(Path::new("/tmp")))
            .await;
        assert!(!matches!(r, ValidationResult::Ok));
    }

    #[tokio::test]
    async fn non_http_url_validates_err() {
        let tool = WebFetchTool::new();
        let r = tool
            .validate_input(
                &json!({"url": "ftp://example.com"}),
                &ctx_in(Path::new("/tmp")),
            )
            .await;
        assert!(!matches!(r, ValidationResult::Ok));
    }

    #[tokio::test]
    async fn http_url_validates_ok() {
        let tool = WebFetchTool::new();
        let r = tool
            .validate_input(
                &json!({"url": "https://example.com/"}),
                &ctx_in(Path::new("/tmp")),
            )
            .await;
        assert!(matches!(r, ValidationResult::Ok));
    }

    #[tokio::test]
    async fn permissions_default_ask() {
        let tool = WebFetchTool::new();
        let r = tool
            .check_permissions(&json!({"url": "https://x.com"}), &ctx_in(Path::new("/tmp")))
            .await;
        assert!(matches!(r, PermissionDecision::Ask { .. }));
    }

    #[tokio::test]
    async fn cancel_aborts() {
        let tool = WebFetchTool::new();
        let ctx = platform::tool_context_for_test("/tmp".into());
        ctx.cancel.cancel();
        let r = tool
            .call(
                json!({"url": "https://example.com"}),
                ctx,
                ProgressSender::noop("t"),
            )
            .await;
        assert!(matches!(r, Err(ToolError::Cancelled)));
    }

    /// 用本地 TCP server 喂一个固定 HTML 响应，验证 html → text 转换 + status。
    /// 真网络访问不在 unit test 里。
    #[tokio::test]
    async fn fetches_local_html_and_extracts_text() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        const RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\n\
content-type: text/html; charset=utf-8\r\n\
content-length: 67\r\n\
connection: close\r\n\
\r\n\
<html><body><h1>Hello</h1><p>This is a test page.</p></body></html>";
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = vec![0u8; 4096];
                let _ = tokio::io::AsyncReadExt::read(&mut sock, &mut buf).await;
                let _ = sock.write_all(RESPONSE).await;
                let _ = sock.shutdown().await;
            }
        });

        let tool = WebFetchTool::new();
        let r = tool
            .call(
                json!({"url": format!("http://127.0.0.1:{port}/")}),
                ctx_in(Path::new("/tmp")),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        assert!(!r.is_error);
        match r.content {
            base::tool::ToolResultContent::Text(t) => {
                assert!(t.contains("HTTP 200"));
                assert!(t.contains("Hello"));
                assert!(t.contains("test page"));
            }
            _ => panic!(),
        }
    }

    /// A server that answers 200 to everything and counts what it was asked.
    async fn a_server_that_would_answer() -> (u16, Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        const RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\n\
content-type: text/plain\r\n\
content-length: 5\r\n\
connection: close\r\n\
\r\n\
hello";
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(AtomicUsize::new(0));
        let counter = seen.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                counter.fetch_add(1, Ordering::SeqCst);
                let mut buf = vec![0u8; 4096];
                let _ = tokio::io::AsyncReadExt::read(&mut sock, &mut buf).await;
                let _ = sock.write_all(RESPONSE).await;
                let _ = sock.shutdown().await;
            }
        });
        (port, seen)
    }

    fn ctx_under(cwd: &Path, domains: &[&str]) -> ToolContext {
        use base::context::config::NetworkModeConfig;
        let mut ctx = ctx_in(cwd);
        ctx.exec = platform::local_providers().with_network(Arc::new(
            platform::LocalNetwork::for_agent_policy(
                NetworkModeConfig::Allowlist,
                domains.iter().map(|d| d.to_string()).collect(),
            ),
        ));
        ctx
    }

    /// `settings.sandbox.allowed_domains` binds what the model may reach, and
    /// a url the model chose to fetch is exactly that.
    #[tokio::test]
    async fn the_allowed_domains_setting_reaches_this_tool() {
        use std::sync::atomic::Ordering;

        let (port, seen) = a_server_that_would_answer().await;
        let url = format!("http://127.0.0.1:{port}/");
        let tool = WebFetchTool::new();

        let refused = tool
            .call(
                json!({ "url": url }),
                ctx_under(Path::new("/tmp"), &["allowed.test"]),
                ProgressSender::noop("t"),
            )
            .await;
        assert!(
            matches!(refused, Err(ToolError::Denied(_))),
            "a host off the allowlist must be refused, got: {refused:?}"
        );
        assert_eq!(
            seen.load(Ordering::SeqCst),
            0,
            "the refusal has to happen before the connection, or the policy is              only a filter on what comes back"
        );

        let allowed = tool
            .call(
                json!({ "url": url }),
                ctx_under(Path::new("/tmp"), &["127.0.0.1"]),
                ProgressSender::noop("t"),
            )
            .await
            .expect("the same url, with its host on the list");
        assert!(!allowed.is_error);
        assert_eq!(
            seen.load(Ordering::SeqCst),
            1,
            "the allowed fetch really went out"
        );
    }

    #[tokio::test]
    async fn returns_error_result_for_4xx() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        const RESPONSE: &[u8] = b"HTTP/1.1 404 Not Found\r\n\
content-type: text/plain\r\n\
content-length: 9\r\n\
connection: close\r\n\
\r\n\
not found";
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = vec![0u8; 4096];
                let _ = tokio::io::AsyncReadExt::read(&mut sock, &mut buf).await;
                let _ = sock.write_all(RESPONSE).await;
                let _ = sock.shutdown().await;
            }
        });

        let tool = WebFetchTool::new();
        let r = tool
            .call(
                json!({"url": format!("http://127.0.0.1:{port}/")}),
                ctx_in(Path::new("/tmp")),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        // 404 → ToolResult is_error=true（不是 ToolError；让模型看到细节）
        assert!(r.is_error);
        match r.content {
            base::tool::ToolResultContent::Text(t) => {
                assert!(t.contains("HTTP 404"));
                assert!(t.contains("not found"));
            }
            _ => panic!(),
        }
    }

    /// Answers every extraction the same way, keeping the params it was asked with.
    struct ExtractingModel {
        answer: Result<&'static str, &'static str>,
        hang: bool,
        asked: std::sync::Mutex<Vec<base::interface::model::StreamParams>>,
    }

    impl ExtractingModel {
        fn new(answer: Result<&'static str, &'static str>) -> Arc<Self> {
            Arc::new(Self {
                answer,
                hang: false,
                asked: Default::default(),
            })
        }
    }

    #[async_trait]
    impl base::interface::model::Model for ExtractingModel {
        fn api_type(&self) -> base::provider::ApiType {
            base::provider::ApiType::Anthropic
        }
        async fn stream(
            &self,
            _prompt_blocks: Vec<base::prompt::PromptBlock>,
            _tools: Vec<base::interface::model::ToolDef>,
            _messages: Vec<base::interface::model::ModelMessage>,
            params: base::interface::model::StreamParams,
            _cancel: tokio_util::sync::CancellationToken,
        ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError>
        {
            use base::interface::model::{ModelError, ModelEvent};
            self.asked.lock().unwrap().push(params);
            if self.hang {
                return Ok(Box::new(futures::stream::pending()));
            }
            let text = self
                .answer
                .map_err(|e| ModelError::unsupported(e.to_string()))?;
            Ok(Box::new(futures::stream::iter([
                Ok(ModelEvent::TextDelta { text: text.into() }),
                Ok(ModelEvent::EndTurn {
                    stop_reason: "end_turn".into(),
                    usage: Default::default(),
                }),
            ])))
        }
    }

    fn extracting(model: &Arc<ExtractingModel>) -> WebFetchTool {
        WebFetchTool::with_extractor(PageExtractor::new(model.clone(), "fetch-model"))
    }

    async fn fetch_text(tool: &WebFetchTool, input: Value) -> String {
        let r = tool
            .call(input, ctx_in(Path::new("/tmp")), ProgressSender::noop("t"))
            .await
            .unwrap();
        assert!(!r.is_error);
        match r.content {
            base::tool::ToolResultContent::Text(t) => t,
            other => panic!("{other:?}"),
        }
    }

    /// What a result carries after its header.
    fn body(text: &str) -> &str {
        let open = text.find("]\n\n").expect("a header") + 3;
        &text[open..]
    }

    #[tokio::test]
    async fn with_an_extractor_the_models_answer_replaces_the_page() {
        let (port, _) = a_server_that_would_answer().await;
        let model = ExtractingModel::new(Ok("the answer"));
        let text = fetch_text(
            &extracting(&model),
            json!({"url": format!("http://127.0.0.1:{port}/"), "prompt": "what is it?"}),
        )
        .await;
        assert!(text.contains("mode=extracted by fetch-model"), "{text}");
        assert_eq!(body(&text), "the answer", "{text}");
        assert!(!text.contains("hello"), "the page itself stays out: {text}");

        let asked = model.asked.lock().unwrap();
        assert_eq!(asked.len(), 1);
        assert_eq!(asked[0].model, "fetch-model");
        let origin = asked[0].origin.as_ref().expect("attributed");
        assert_eq!(origin.session_id, "test");
        assert_eq!(
            origin.purpose(),
            Some(base::interface::model::call_purpose::WEB_FETCH)
        );
    }

    #[tokio::test]
    async fn a_failed_extraction_returns_the_page_and_says_why() {
        let (port, _) = a_server_that_would_answer().await;
        let model = ExtractingModel::new(Err("provider down"));
        let text = fetch_text(
            &extracting(&model),
            json!({"url": format!("http://127.0.0.1:{port}/"), "prompt": "what is it?"}),
        )
        .await;
        assert!(
            text.contains("mode=page text; extraction failed:") && text.contains("provider down"),
            "{text}"
        );
        assert_eq!(body(&text), "hello", "{text}");
    }

    #[tokio::test]
    async fn an_empty_answer_returns_the_page_and_says_so() {
        let (port, _) = a_server_that_would_answer().await;
        let model = ExtractingModel::new(Ok("  "));
        let text = fetch_text(
            &extracting(&model),
            json!({"url": format!("http://127.0.0.1:{port}/"), "prompt": "what is it?"}),
        )
        .await;
        assert!(text.contains("returned nothing"), "{text}");
        assert_eq!(body(&text), "hello", "{text}");
    }

    #[tokio::test]
    async fn without_a_prompt_nothing_is_extracted() {
        let (port, _) = a_server_that_would_answer().await;
        let model = ExtractingModel::new(Ok("the answer"));
        let text = fetch_text(
            &extracting(&model),
            json!({"url": format!("http://127.0.0.1:{port}/"), "prompt": " "}),
        )
        .await;
        assert!(text.contains("mode=page text]"), "{text}");
        assert_eq!(body(&text), "hello", "{text}");
        assert!(model.asked.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn without_an_extractor_the_prompt_is_only_echoed() {
        let (port, _) = a_server_that_would_answer().await;
        let text = fetch_text(
            &WebFetchTool::new(),
            json!({"url": format!("http://127.0.0.1:{port}/"), "prompt": "what is it?"}),
        )
        .await;
        assert!(
            text.contains("prompt was: what is it? | mode=page text]"),
            "{text}"
        );
        assert_eq!(body(&text), "hello", "{text}");
    }

    #[tokio::test]
    async fn cancelling_stops_a_running_extraction() {
        let (port, _) = a_server_that_would_answer().await;
        let model = Arc::new(ExtractingModel {
            answer: Ok(""),
            hang: true,
            asked: Default::default(),
        });
        let ctx = ctx_in(Path::new("/tmp"));
        let cancel = ctx.cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            cancel.cancel();
        });
        let r = extracting(&model)
            .call(
                json!({"url": format!("http://127.0.0.1:{port}/"), "prompt": "q"}),
                ctx,
                ProgressSender::noop("t"),
            )
            .await;
        assert!(matches!(r, Err(ToolError::Cancelled)), "{r:?}");
        assert_eq!(model.asked.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn what_the_model_is_told_matches_whether_pages_are_extracted() {
        let ctx = PromptContext::default();
        let plain = WebFetchTool::new();
        assert!(plain.description().contains("plain text"));
        let plain_prompt = plain.guide(&ctx).await.unwrap();
        assert!(plain_prompt.contains("not acted on"));

        let model = ExtractingModel::new(Ok("x"));
        let extracting = extracting(&model);
        assert!(extracting.description().contains("answer `prompt`"));
        assert!(extracting
            .guide(&ctx)
            .await
            .unwrap()
            .contains("separate model"));

        for prompt in [plain_prompt, extracting.guide(&ctx).await.unwrap()] {
            assert!(!prompt.contains("HTTPS"), "no upgrade happens");
            assert!(!prompt.contains("cache"), "nothing is cached");
            assert!(prompt.contains("external-content"), "{prompt}");
        }
    }

    const HEADER: &str = "[HTTP 200 | url=https://x.test/ | mode=page text]\n\n";

    /// The whole result — header, the runtime's fence and all — stays within
    /// the 50 000 characters the tool-result budget lets one result have.
    #[test]
    fn a_capped_page_fits_the_tool_result_budget() {
        let page = format!("{}EDGE{}PAST", "a".repeat(49_000), "b".repeat(2_000));
        let out = within_cap(HEADER, "https://x.test/", &page);
        let fenced = crate::external_content::fence("https://x.test/", "the web", &out);
        assert_eq!(fenced.chars().count(), MAX_OUTPUT_CHARS);
        assert!(out.starts_with(HEADER));
        assert!(out.contains("EDGE") && !out.contains("PAST"));
        assert!(out.contains("[truncated: the page is 51008 characters]"));
    }

    #[test]
    fn a_short_page_in_wide_characters_is_kept_whole() {
        let page = format!("{}END", "中".repeat(20_000));
        let out = within_cap(HEADER, "https://x.test/", &page);
        assert!(out.contains(&page));
        assert!(!out.contains("[truncated"));
    }

    #[tokio::test]
    async fn the_host_is_listed_when_allowed_domains_names_it() {
        let mut ctx = ctx_in(Path::new("/tmp"));
        ctx.sandbox.allowed_domains = vec!["listed.test".into()];
        for (url, reach) in [
            ("https://listed.test/a", Reach::Host { listed: true }),
            ("https://other.test/a", Reach::Host { listed: false }),
        ] {
            let facts = WebFetchTool::new()
                .permission_facts(&json!({ "url": url, "prompt": "x" }), &ctx)
                .await;
            assert_eq!(facts.subject, Subject::Opaque(url.into()));
            assert_eq!(facts.reach, reach, "{url}");
            assert_eq!(
                facts.question.as_deref(),
                Some("WebFetch requires confirmation")
            );
        }
        let unparsed = WebFetchTool::new().permission_facts(&json!({}), &ctx).await;
        assert_eq!(unparsed.reach, Reach::Unknown);
    }
}
