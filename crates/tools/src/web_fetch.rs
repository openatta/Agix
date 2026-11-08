//! WebFetchTool —— "WebFetch"。
//!
//! 通过执行层的 `Network` 抓 URL；如果 content-type 含 html 用 html2text
//! 渲染纯文本；其它（json / text / binary）按 utf-8 解码或标识 binary。
//! 5 MB 上限、~50_000 char 上限、15s 超时。

use crate::cancel::run_with_cancel;
use async_trait::async_trait;
use base::error::ToolError;
use base::interface::exec::{HttpRequest, Network, Origin};
use base::tool::{
    PermissionDecision, ProgressSender, PromptContext, SecondaryLlm, Tool, ToolContext, ToolResult,
    ValidationResult,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

const MAX_BYTES: u64 = 5 * 1024 * 1024;
const MAX_OUTPUT_CHARS: usize = 50_000;
const TIMEOUT: Duration = Duration::from_secs(15);
/// Input cap fed to the secondary LLM. Aligns with
/// `MAX_MARKDOWN_LENGTH`; truncation marker appended on overflow.
const MAX_SECONDARY_INPUT_CHARS: usize = 80_000;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WebFetchInput {
    /// HTTP/HTTPS URL to fetch.
    pub url: String,

    /// Optional question / instruction about the page (recorded for context;
    /// model interprets the returned text itself).
    #[serde(default)]
    pub prompt: Option<String>,
}

/// Optional secondary-LLM extractor. When wired, after fetching
/// the URL the tool calls the secondary LLM with `(prompt, content)` and
/// returns the model's distillation instead of the raw 50KB page. CLI/TUI
/// inject this from the same Anthropic client used by the engine, with the
/// model overridden to `compact_model` (haiku-tier).
#[derive(Default, Clone)]
pub struct WebFetchTool {
    secondary: Option<Arc<dyn SecondaryLlm>>,
}

impl WebFetchTool {
    /// Construct a new instance.
    pub fn new() -> Self {
        Self::default()
    }

    /// Builder: set secondary llm.
    pub fn with_secondary_llm(secondary: Arc<dyn SecondaryLlm>) -> Self {
        Self {
            secondary: Some(secondary),
        }
    }
}

#[async_trait]
impl Tool for WebFetchTool {
    fn callable_from_code(&self) -> bool {
        true
    }
    fn name(&self) -> &str {
        "WebFetch"
    }

    fn description(&self) -> &str {
        "Fetch a URL and extract relevant content via sub-LLM."
    }

    fn input_schema(&self) -> Value {
        crate::input_schema::<WebFetchInput>()
    }

    async fn prompt(&self, _: &PromptContext) -> String {
        include_str!("prompts/coding/web_fetch.prompt.md").to_string()
    }

    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }

    fn is_read_only(&self, _: &Value) -> bool {
        true
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
        // 网络访问默认 Ask；上层 PermissionGate 用 `WebFetch(https://example.com/**)`
        // 之类的规则放行
        PermissionDecision::Ask {
            message: "WebFetch requires confirmation".into(),
            decision_reason: None,
        }
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
                // If a secondary LLM is wired AND the user
                // supplied a prompt, ask the cheap model to extract relevant
                // bits. Otherwise return the raw fetched text (legacy path).
                let body = match (&input.prompt, self.secondary.as_ref()) {
                    (Some(prompt), Some(secondary)) if !prompt.trim().is_empty() => {
                        let truncated = truncate_for_secondary(&text);
                        match secondary.extract_with_prompt(prompt, &truncated).await {
                            Ok(extracted) if !extracted.trim().is_empty() => extracted,
                            Ok(_) => fallback_text(&text),
                            Err(e) => {
                                tracing::warn!(
                                    error = %e,
                                    "secondary LLM extraction failed; returning raw fetched text"
                                );
                                fallback_text(&text)
                            }
                        }
                    }
                    _ => fallback_text(&text),
                };
                let header = if input.prompt.is_some() {
                    let mode = if self.secondary.is_some() {
                        "extracted-by-haiku"
                    } else {
                        "raw"
                    };
                    format!(
                        "[HTTP {status} | url={} | prompt was: {} | mode={mode}]\n\n",
                        input.url,
                        input.prompt.as_deref().unwrap_or("")
                    )
                } else {
                    format!("[HTTP {status} | url={}]\n\n", input.url)
                };
                Ok(ToolResult::text(format!("{header}{body}")))
            }
            Err(FetchError::HttpStatus { status, body }) => Ok(ToolResult::error_text(format!(
                "HTTP {status}\n{}",
                truncate_chars(&body, 4000)
            ))),
            Err(FetchError::Redirect(location)) => {
                // Cross-host redirect — return to model for re-fetch.
                Ok(ToolResult::text(format!(
                    "Redirected to: {location}\n\nRe-fetch this URL to continue."
                )))
            }
            Err(FetchError::Egress(e)) => Err(e.into()),
        }
    }
}

fn fallback_text(text: &str) -> String {
    if text.chars().count() > MAX_OUTPUT_CHARS {
        let truncated: String = text.chars().take(MAX_OUTPUT_CHARS).collect();
        format!("{truncated}\n\n[truncated to first {MAX_OUTPUT_CHARS} chars]")
    } else {
        text.to_string()
    }
}

fn truncate_for_secondary(text: &str) -> String {
    if text.chars().count() <= MAX_SECONDARY_INPUT_CHARS {
        text.to_string()
    } else {
        let head: String = text.chars().take(MAX_SECONDARY_INPUT_CHARS).collect();
        format!("{head}\n\n[Content truncated due to length...]")
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

    fn ctx_in(cwd: &Path) -> ToolContext {
        ToolContext::for_test(cwd.to_path_buf())
    }

    #[tokio::test]
    async fn name_is_webfetch() {
        let tool = WebFetchTool::new();
        assert_eq!(tool.name(), "WebFetch");
        assert!(tool.is_read_only(&Value::Null));
        assert!(tool.is_concurrency_safe(&Value::Null));
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
        let ctx = ToolContext::for_test("/tmp".into());
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
        ctx.exec = base::interface::exec::ExecProviders::local().with_network(Arc::new(
            base::interface::exec::local::LocalNetwork::for_agent_policy(
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
}
