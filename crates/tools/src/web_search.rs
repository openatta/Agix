//! WebSearchTool —— "WebSearch"。
//!
//! 引擎自己的搜索工具：模型调它，它交给 `SearchProvider` 去搜，结果作为工具结果
//! 回给模型。在所有提供方下都是这一个名字、这一条路，所以 hook、权限规则、宿主
//! 看到的帧与 `inspectContext` 报的都是它。默认后端是会话的模型提供方自带的搜索
//! （[`crate::native_search`]）；`SearchProvider` 可插拔，别的后端实现它即可。

use crate::cancel::run_with_cancel;
use async_trait::async_trait;
use base::error::ToolError;
use base::tool::{
    PermissionDecision, ProgressSender, PromptContext, Tool, ToolContext, ToolResult, ToolSpec,
    ValidationResult,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

const DEFAULT_MAX_RESULTS: usize = 10;
const HARD_CAP_RESULTS: usize = 25;
const MAX_QUERY_CHARS: usize = 200;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WebSearchInput {
    /// Search query.
    pub query: String,
    /// Max results to return (default 10, max 25).
    #[serde(default)]
    pub max_results: Option<usize>,
    /// Only include search results from these domains.
    #[serde(default)]
    pub allowed_domains: Option<Vec<String>>,
    /// Never include search results from these domains.
    #[serde(default)]
    pub blocked_domains: Option<Vec<String>>,
}

/// An item in the ordered search output, preserving the natural interleaving
/// of model commentary and search result links from the API response.
#[derive(Debug)]
pub enum SearchOutputItem {
    /// Model-generated text commentary.
    Text(String),
    /// Structured search result links.
    Links(Vec<SearchResult>),
}

/// Return type from a search provider: ordered items preserving the sequence
/// of text commentary and search result links from the sub-call response.
#[derive(Debug)]
pub struct SearchOutput {
    /// Ordered items from the sub-call. Preserves the natural sequence so the
    /// main model can match data commentary to its source links.
    pub items: Vec<SearchOutputItem>,
}

/// Pluggable search backend.
#[async_trait]
pub trait SearchProvider: Send + Sync + std::fmt::Debug {
    /// Search for `query` on behalf of `session_id`.
    async fn search(
        &self,
        query: &str,
        max_results: usize,
        session_id: &str,
    ) -> Result<SearchOutput, SearchError>;
}

#[derive(Debug)]
pub struct WebSearchTool {
    provider: Box<dyn SearchProvider>,
}

impl WebSearchTool {
    /// Create a WebSearchTool with a search provider.
    pub fn new(provider: Box<dyn SearchProvider>) -> Self {
        Self { provider }
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new()
            .callable_from_code(true)
            .concurrency_safe(true)
            .read_only(true)
    }
    fn description(&self) -> &str {
        "Search the web and return results with linked sources"
    }
    fn name(&self) -> &str {
        "WebSearch"
    }

    fn input_schema(&self) -> Value {
        crate::input_schema::<WebSearchInput>()
    }

    async fn prompt(&self, _: &PromptContext) -> String {
        let prompt = include_str!("prompts/coding/web_search.prompt.md");
        let now = time::OffsetDateTime::now_utc();
        let current_month_year = now
            .format(&time::macros::format_description!(
                "[month repr:long] [year]"
            ))
            .unwrap_or_else(|_| "May 2026".to_string());
        prompt.replace("{currentMonthYear}", &current_month_year)
    }

    fn permission_match_content(&self, input: &Value) -> Option<String> {
        serde_json::from_value::<WebSearchInput>(input.clone())
            .ok()
            .map(|i| i.query)
    }

    async fn validate_input(&self, input: &Value, _: &ToolContext) -> ValidationResult {
        match serde_json::from_value::<WebSearchInput>(input.clone()) {
            Ok(i) if i.query.trim().is_empty() => {
                ValidationResult::err("query must not be empty", 1)
            }
            Ok(i) if i.query.chars().count() > MAX_QUERY_CHARS => {
                ValidationResult::err(format!("query exceeds {MAX_QUERY_CHARS} chars"), 2)
            }
            Ok(i) if i.allowed_domains.is_some() && i.blocked_domains.is_some() => {
                ValidationResult::err("cannot specify both allowed_domains and blocked_domains", 4)
            }
            Ok(_) => ValidationResult::Ok,
            Err(e) => ValidationResult::err(format!("invalid input: {e}"), 3),
        }
    }

    async fn check_permissions(&self, _: &Value, _: &ToolContext) -> PermissionDecision {
        PermissionDecision::Allow {
            decision_reason: None,
        }
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolContext,
        _progress: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let input: WebSearchInput = serde_json::from_value(input)?;
        let max = input
            .max_results
            .unwrap_or(DEFAULT_MAX_RESULTS)
            .min(HARD_CAP_RESULTS);

        let SearchOutput { items } = run_with_cancel(
            &ctx.cancel,
            self.provider.search(&input.query, max, &ctx.session_id),
        )
        .await?
        .map_err(|e| match e {
            SearchError::Timeout => ToolError::Timeout(std::time::Duration::from_secs(30)),
            SearchError::Other(e) => ToolError::exec(e.to_string()),
        })?;
        let items = filter_items_by_domain(items, &input.allowed_domains, &input.blocked_domains);

        let has_content = items.iter().any(|item| match item {
            SearchOutputItem::Text(t) => !t.is_empty(),
            SearchOutputItem::Links(l) => !l.is_empty(),
        });
        if !has_content {
            Ok(ToolResult::text(format!(
                "(no results for '{}')",
                input.query
            )))
        } else {
            Ok(ToolResult::text(format_results(&input.query, &items)))
        }
    }
}

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

#[derive(thiserror::Error, Debug)]
pub enum SearchError {
    #[error("timeout")]
    Timeout,
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Filter search results by domain constraints.
fn filter_by_domain(
    items: Vec<SearchResult>,
    allowed: &Option<Vec<String>>,
    blocked: &Option<Vec<String>>,
) -> Vec<SearchResult> {
    match (allowed, blocked) {
        (Some(allowed), None) => items
            .into_iter()
            .filter(|r| {
                let host = r.url_host().unwrap_or("");
                allowed
                    .iter()
                    .any(|d| host.contains(d.as_str()) || host.ends_with(d.as_str()))
            })
            .collect(),
        (None, Some(blocked)) => items
            .into_iter()
            .filter(|r| {
                let host = r.url_host().unwrap_or("");
                !blocked.iter().any(|d| host.contains(d.as_str()))
            })
            .collect(),
        _ => items,
    }
}

impl SearchResult {
    fn url_host(&self) -> Option<&str> {
        self.url
            .strip_prefix("https://")
            .or_else(|| self.url.strip_prefix("http://"))
            .and_then(|s| s.split('/').next())
    }
}

/// Filter search result items by domain constraints, preserving item order.
fn filter_items_by_domain(
    items: Vec<SearchOutputItem>,
    allowed: &Option<Vec<String>>,
    blocked: &Option<Vec<String>>,
) -> Vec<SearchOutputItem> {
    if allowed.is_none() && blocked.is_none() {
        return items;
    }
    items
        .into_iter()
        .map(|item| match item {
            SearchOutputItem::Links(results) => {
                SearchOutputItem::Links(filter_by_domain(results, allowed, blocked))
            }
            other => other,
        })
        .collect()
}

fn format_results(query: &str, items: &[SearchOutputItem]) -> String {
    let mut s = String::new();
    s.push_str(&format!("Web search results for query: \"{query}\"\n\n"));
    for item in items {
        match item {
            SearchOutputItem::Text(text) => {
                if !text.is_empty() {
                    s.push_str(text);
                    s.push_str("\n\n");
                }
            }
            SearchOutputItem::Links(links) => {
                for link in links {
                    s.push_str(&format!("- {}\n  来源：{}\n\n", link.title, link.url));
                }
            }
        }
    }
    s.push_str("REMINDER: You MUST include the sources above in your response to the user using markdown hyperlinks.");
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::Path;

    fn ctx_in(cwd: &Path) -> ToolContext {
        platform::tool_context_for_test(cwd.to_path_buf())
    }

    struct MockProvider(Vec<SearchResult>);

    impl std::fmt::Debug for MockProvider {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("MockProvider")
                .field("count", &self.0.len())
                .finish()
        }
    }

    #[async_trait]
    impl SearchProvider for MockProvider {
        async fn search(
            &self,
            _query: &str,
            _max_results: usize,
            _session_id: &str,
        ) -> Result<SearchOutput, SearchError> {
            Ok(SearchOutput {
                items: if self.0.is_empty() {
                    vec![]
                } else {
                    vec![SearchOutputItem::Links(self.0.clone())]
                },
            })
        }
    }

    #[tokio::test]
    async fn name_is_websearch() {
        let tool = WebSearchTool::new(Box::new(MockProvider(vec![])));
        assert_eq!(tool.name(), "WebSearch");
        assert!(tool.call_traits(&Value::Null).read_only);
        assert!(tool.call_traits(&Value::Null).concurrency_safe);
    }

    #[tokio::test]
    async fn empty_query_validates_err() {
        let tool = WebSearchTool::new(Box::new(MockProvider(vec![])));
        let r = tool
            .validate_input(&json!({"query": "  "}), &ctx_in(Path::new("/tmp")))
            .await;
        assert!(!matches!(r, ValidationResult::Ok));
    }

    #[tokio::test]
    async fn overlong_query_validates_err() {
        let tool = WebSearchTool::new(Box::new(MockProvider(vec![])));
        let q: String = "x".repeat(MAX_QUERY_CHARS + 1);
        let r = tool
            .validate_input(&json!({"query": q}), &ctx_in(Path::new("/tmp")))
            .await;
        assert!(!matches!(r, ValidationResult::Ok));
    }

    #[tokio::test]
    async fn both_domain_filters_validates_err() {
        let tool = WebSearchTool::new(Box::new(MockProvider(vec![])));
        let r = tool
            .validate_input(
                &json!({"query": "rust", "allowed_domains": ["rust-lang.org"], "blocked_domains": ["example.com"]}),
                &ctx_in(Path::new("/tmp")),
            )
            .await;
        assert!(!matches!(r, ValidationResult::Ok));
    }

    #[tokio::test]
    async fn empty_results_when_search_returns_empty() {
        let tool = WebSearchTool::new(Box::new(MockProvider(vec![])));
        let r = tool
            .call(
                json!({"query": "nothing"}),
                ctx_in(Path::new("/tmp")),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        let text = match &r.content {
            base::tool::ToolResultContent::Text(t) => t.clone(),
            _ => String::new(),
        };
        assert!(text.contains("no results"));
    }

    #[tokio::test]
    async fn returns_formatted_results() {
        let items = vec![
            SearchResult {
                title: "Rust Lang".into(),
                url: "https://rust-lang.org".into(),
                snippet: "Systems language".into(),
            },
            SearchResult {
                title: "Crates.io".into(),
                url: "https://crates.io".into(),
                snippet: "".into(),
            },
        ];
        let tool = WebSearchTool::new(Box::new(MockProvider(items)));
        let r = tool
            .call(
                json!({"query": "rust", "max_results": 5}),
                ctx_in(Path::new("/tmp")),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        let text = match &r.content {
            base::tool::ToolResultContent::Text(t) => t.clone(),
            _ => String::new(),
        };
        assert!(
            text.contains("Web search results for query: \"rust\""),
            "{text}"
        );
        assert!(text.contains("Rust Lang"));
        assert!(text.contains("https://rust-lang.org"));
        assert!(text.contains("Crates.io"));
        assert!(text.contains("REMINDER: You MUST include the sources"));
    }

    #[tokio::test]
    async fn cancel_aborts() {
        let tool = WebSearchTool::new(Box::new(MockProvider(vec![])));
        let ctx = platform::tool_context_for_test("/tmp".into());
        ctx.cancel.cancel();
        let r = tool
            .call(json!({"query": "rust"}), ctx, ProgressSender::noop("t"))
            .await;
        assert!(matches!(r, Err(ToolError::Cancelled)));
    }

    #[tokio::test]
    async fn permissions_default_allow() {
        let tool = WebSearchTool::new(Box::new(MockProvider(vec![])));
        let r = tool
            .check_permissions(&json!({"query": "x"}), &ctx_in(Path::new("/tmp")))
            .await;
        assert!(
            matches!(r, PermissionDecision::Allow { .. }),
            "WebSearch should auto-allow, matching TS passthrough behavior"
        );
    }

    #[test]
    fn format_results_renders_numbered_list() {
        let items = vec![SearchOutputItem::Links(vec![
            SearchResult {
                title: "Hello".into(),
                url: "https://x.com".into(),
                snippet: "World".into(),
            },
            SearchResult {
                title: "Foo".into(),
                url: "https://y.com".into(),
                snippet: "".into(),
            },
        ])];
        let s = format_results("test", &items);
        assert!(s.contains("Web search results for query: \"test\""));
        assert!(s.contains("Hello"));
        assert!(s.contains("https://x.com"));
        assert!(s.contains("Foo"));
        assert!(s.contains("REMINDER: You MUST include the sources"));
    }

    #[test]
    fn filter_by_domain_allowed_only_keeps_matching() {
        let items = vec![
            SearchResult {
                title: "A".into(),
                url: "https://rust-lang.org".into(),
                snippet: "".into(),
            },
            SearchResult {
                title: "B".into(),
                url: "https://example.com".into(),
                snippet: "".into(),
            },
        ];
        let allowed = Some(vec!["rust-lang.org".into()]);
        let r = filter_by_domain(items, &allowed, &None);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].title, "A");
    }

    #[test]
    fn filter_by_domain_blocked_removes_matching() {
        let items = vec![
            SearchResult {
                title: "A".into(),
                url: "https://rust-lang.org".into(),
                snippet: "".into(),
            },
            SearchResult {
                title: "B".into(),
                url: "https://example.com".into(),
                snippet: "".into(),
            },
        ];
        let blocked = Some(vec!["example.com".into()]);
        let r = filter_by_domain(items, &None, &blocked);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].title, "A");
    }

    #[test]
    fn url_host_extracts_hostname() {
        let r = SearchResult {
            title: "t".into(),
            url: "https://example.com/path".into(),
            snippet: "".into(),
        };
        assert_eq!(r.url_host(), Some("example.com"));
    }

    #[test]
    fn url_host_handles_http() {
        let r = SearchResult {
            title: "t".into(),
            url: "http://x.org".into(),
            snippet: "".into(),
        };
        assert_eq!(r.url_host(), Some("x.org"));
    }

    #[test]
    fn url_host_handles_no_scheme() {
        let r = SearchResult {
            title: "t".into(),
            url: "example.com".into(),
            snippet: "".into(),
        };
        assert!(r.url_host().is_none());
    }
}
