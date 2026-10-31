//! `ToolSearchTool` (`"ToolSearch"`): on-demand disclosure of what the
//! per-request tool list leaves out.
//!
//! Two things are fetchable:
//!
//! 1. **Deferred tools.** A scene's `deferred_tools()` replaces a tool's JSON
//!    schema in the per-call `tools` array with a stub (see `crate::deferred`).
//!    A match returns the real schema as a `<function>` line inside a
//!    `<functions>` block of the result text and records the tool as activated
//!    in the session. The tool list itself never changes mid-session, so the
//!    prompt cache survives; the schema lives in the transcript instead.
//! 2. **Usage guides.** `Tool::detailed_prompt()`; `build_tool_defs` points at
//!    it with one line rather than paying for every guide on every request.
//!
//! Query forms: `select:A,B` names tools exactly (case-insensitive fallback,
//! words after a name are ignored); anything else is keywords, where a `+term`
//! must appear in the tool name and the rest rank the hits.

use async_trait::async_trait;
use base::error::ToolError;
use base::tool::{
    PermissionDecision, ProgressSender, PromptContext, Tool, ToolContext, ToolResult,
    ValidationResult,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

const DEFAULT_MAX_RESULTS: usize = 5;

/// Ceiling on the combined size of the usage guides one call inlines. Tool
/// results stay in the transcript until compaction, so an unbounded result
/// could pin tens of KB at once; matches past the budget are still listed,
/// with a note on how to fetch their guide alone.
const MAX_GUIDE_BYTES: usize = 24_000;

const MAX_SUGGESTIONS: usize = 3;

struct Match {
    name: String,
    description: String,
    guide: Option<String>,
    /// The real schema, present when the tool list carries only a stub.
    deferred_schema: Option<Value>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ToolSearchInput {
    /// `select:<name>[,<name>...]` to fetch known tools by name; otherwise
    /// keywords to search tool names and descriptions, where `+term` must
    /// appear in the name.
    pub query: String,

    /// Cap on returned matches (default 5).
    #[serde(default)]
    pub max_results: Option<usize>,
}

pub struct ToolSearchTool {
    registry: Arc<base::tool::InMemoryToolRegistry>,
}

impl ToolSearchTool {
    /// Construct a new instance.
    pub fn new(registry: Arc<base::tool::InMemoryToolRegistry>) -> Self {
        Self { registry }
    }
}

enum Query {
    Select(Vec<String>),
    Keywords {
        required: Vec<String>,
        optional: Vec<String>,
    },
}

fn parse_query(q: &str) -> Query {
    let select = q
        .get(..7)
        .filter(|p| p.eq_ignore_ascii_case("select:"))
        .map(|_| &q[7..]);
    if let Some(rest) = select {
        let mut names: Vec<String> = Vec::new();
        for name in rest
            .split(',')
            .filter_map(|seg| seg.split_whitespace().next())
        {
            if !names.iter().any(|n| n == name) {
                names.push(name.to_string());
            }
        }
        return Query::Select(names);
    }
    let mut required = Vec::new();
    let mut optional = Vec::new();
    for term in q.split_whitespace() {
        let lower = term.to_lowercase();
        match lower.strip_prefix('+') {
            Some("") => {}
            Some(r) => required.push(r.to_string()),
            None => optional.push(lower),
        }
    }
    Query::Keywords { required, optional }
}

fn keyword_score(name: &str, description: &str, required: &[String], optional: &[String]) -> usize {
    let name = name.to_lowercase();
    let description = description.to_lowercase();
    if !required.iter().all(|r| name.contains(r.as_str())) {
        return 0;
    }
    let mut score = 0;
    for term in required.iter().chain(optional) {
        if name == *term {
            score += 100;
        } else if name.contains(term.as_str()) {
            score += 50;
        }
        if description.contains(term.as_str()) {
            score += 10;
        }
    }
    if score == 0 && optional.is_empty() && !required.is_empty() {
        score = 1;
    }
    score
}

fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1; b.len() + 1];
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != *cb);
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        prev = cur;
    }
    prev[b.len()]
}

fn closest_names<'a>(wanted: &str, names: impl Iterator<Item = &'a str>) -> Vec<String> {
    let wanted = wanted.to_lowercase();
    let threshold = (wanted.chars().count() / 3).max(2);
    let mut scored: Vec<(usize, &str)> = names
        .filter_map(|n| {
            let lower = n.to_lowercase();
            let d = if lower.contains(&wanted) || wanted.contains(&lower) {
                0
            } else {
                edit_distance(&wanted, &lower)
            };
            (d <= threshold).then_some((d, n))
        })
        .collect();
    scored.sort();
    scored
        .into_iter()
        .take(MAX_SUGGESTIONS)
        .map(|(_, n)| n.to_string())
        .collect()
}

#[async_trait]
impl Tool for ToolSearchTool {
    fn description(&self) -> &str {
        "Fetch the full JSON schema of a deferred tool (returned in a <functions> \
         block) and/or a tool's detailed usage guide. Query \"select:A,B\" for \
         known names, or keywords to search (\"+term\" must appear in the name)."
    }
    fn name(&self) -> &str {
        "ToolSearch"
    }

    fn input_schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(ToolSearchInput))
            .expect("schemars output is valid JSON")
    }

    async fn prompt(&self, _: &PromptContext) -> String {
        include_str!("prompts/coding/tool_search.prompt.md").to_string()
    }

    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }

    fn is_read_only(&self, _: &Value) -> bool {
        true
    }

    async fn validate_input(&self, input: &Value, _: &ToolContext) -> ValidationResult {
        match serde_json::from_value::<ToolSearchInput>(input.clone()) {
            Ok(p) if p.query.trim().is_empty() => {
                ValidationResult::err("query must not be empty", 1)
            }
            Ok(_) => ValidationResult::Ok,
            Err(e) => ValidationResult::err(format!("invalid input: {e}"), 2),
        }
    }

    async fn check_permissions(&self, _: &Value, _: &ToolContext) -> PermissionDecision {
        PermissionDecision::allow()
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolContext,
        _progress: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let input: ToolSearchInput = serde_json::from_value(input)?;
        let max = input.max_results.unwrap_or(DEFAULT_MAX_RESULTS);
        let q = input.query.trim();

        let all = self.registry.all();
        let prompt_ctx = PromptContext {
            cwd: ctx.cwd.clone(),
            model: String::new(),
            session_id: ctx.session_id.clone(),
            is_interactive: false,
            all_tool_names: all.iter().map(|t| t.name().to_string()).collect(),
            allowed_agent_types: vec![],
            path_style: ctx.config.path_style,
        };
        let describe = |t: &Arc<dyn Tool>| {
            t.short_description()
                .unwrap_or_else(|| t.description().to_string())
        };
        let to_match = |t: &Arc<dyn Tool>, guide: Option<String>| Match {
            name: t.name().to_string(),
            description: describe(t),
            guide,
            deferred_schema: t.is_deferred().then(|| t.full_input_schema()),
        };

        let mut matches: Vec<Match> = Vec::new();
        let mut already_loaded: Vec<String> = Vec::new();
        let mut unmatched: Vec<(String, Vec<String>)> = Vec::new();
        let mut total_deferred = 0;
        let mut total_documented = 0;

        match parse_query(q) {
            Query::Select(names) => {
                for wanted in names {
                    let found = all
                        .iter()
                        .find(|t| t.name() == wanted)
                        .or_else(|| all.iter().find(|t| t.name().eq_ignore_ascii_case(&wanted)));
                    let Some(t) = found else {
                        let suggestions = closest_names(&wanted, all.iter().map(|t| t.name()));
                        unmatched.push((wanted, suggestions));
                        continue;
                    };
                    let guide = t.detailed_prompt(&prompt_ctx).await;
                    if !t.is_deferred() && guide.is_none() {
                        already_loaded.push(t.name().to_string());
                    } else if !matches.iter().any(|m| m.name == t.name()) {
                        matches.push(to_match(t, guide));
                    }
                }
            }
            Query::Keywords { required, optional } => {
                let mut scored: Vec<(usize, Match)> = Vec::new();
                for t in &all {
                    let guide = t.detailed_prompt(&prompt_ctx).await;
                    if t.is_deferred() {
                        total_deferred += 1;
                    }
                    if guide.is_some() {
                        total_documented += 1;
                    }
                    if !t.is_deferred() && guide.is_none() {
                        continue;
                    }
                    let score = keyword_score(t.name(), &describe(t), &required, &optional);
                    if score > 0 {
                        scored.push((score, to_match(t, guide)));
                    }
                }
                // Stable, so equal scores keep registry order.
                scored.sort_by_key(|s| std::cmp::Reverse(s.0));
                matches = scored.into_iter().map(|(_, m)| m).collect();
            }
        }
        matches.truncate(max);

        let activated: Vec<String> = matches
            .iter()
            .filter(|m| m.deferred_schema.is_some())
            .map(|m| m.name.clone())
            .collect();
        ctx.session.activate_tools(activated.iter().cloned());

        let is_error = matches.is_empty() && already_loaded.is_empty();
        let body = render(
            q,
            &matches,
            &already_loaded,
            &unmatched,
            total_deferred,
            total_documented,
        );

        Ok(ToolResult {
            content: base::tool::ToolResultContent::Text(body),
            is_error,
            structured_content: Some(json!({
                "matches": matches.iter().map(|m| m.name.clone()).collect::<Vec<String>>(),
                "activated": activated,
                "already_loaded": already_loaded,
                "unmatched": unmatched.iter().map(|(n, _)| n.clone()).collect::<Vec<String>>(),
                "guides_returned": matches.iter().filter(|m| m.guide.is_some())
                    .map(|m| m.name.clone()).collect::<Vec<String>>(),
                "query": q})),
            mcp_meta: None,
            new_messages: Some(vec![]),
        })
    }
}

fn render(
    query: &str,
    matches: &[Match],
    already_loaded: &[String],
    unmatched: &[(String, Vec<String>)],
    total_deferred: usize,
    total_documented: usize,
) -> String {
    let mut s = String::new();

    let functions: Vec<String> = matches
        .iter()
        .filter_map(|m| {
            let schema = m.deferred_schema.as_ref()?;
            let def = json!({"description": m.description, "name": m.name, "parameters": schema});
            Some(format!("<function>{def}</function>"))
        })
        .collect();
    if !functions.is_empty() {
        s.push_str("<functions>\n");
        for f in &functions {
            s.push_str(f);
            s.push('\n');
        }
        s.push_str("</functions>\n\n");
        s.push_str(
            "The tools above are now callable with exactly these parameters; \
             call them directly, there is no need to search for them again.\n\n",
        );
    }

    if !already_loaded.is_empty() {
        s.push_str(&format!(
            "Already in your tool list with their full schema (call directly): {}\n",
            already_loaded.join(", ")
        ));
    }
    for (name, suggestions) in unmatched {
        if suggestions.is_empty() {
            s.push_str(&format!("No tool named '{name}'.\n"));
        } else {
            s.push_str(&format!(
                "No tool named '{name}'. Did you mean: {}?\n",
                suggestions.join(", ")
            ));
        }
    }

    if matches.is_empty() {
        if already_loaded.is_empty() && unmatched.is_empty() {
            s.push_str(&format!(
                "No matches for query '{query}'. {total_deferred} tool(s) have a deferred \
                 schema, {total_documented} have a detailed usage guide."
            ));
        }
        return s;
    }

    s.push_str(&format!("Found {} tool(s):\n", matches.len()));
    for m in matches {
        s.push_str(&format!("  · {}", m.name));
        if !m.description.is_empty() {
            s.push_str(&format!(" — {}", m.description));
        }
        s.push('\n');
    }

    // Guides last, so a budget cut costs the least important part.
    let mut budget = MAX_GUIDE_BYTES;
    let mut over_budget: Vec<&str> = Vec::new();
    for m in matches {
        let Some(guide) = m.guide.as_deref() else {
            continue;
        };
        let guide = guide.trim();
        if guide.len() > budget {
            over_budget.push(&m.name);
            continue;
        }
        budget -= guide.len();
        s.push_str(&format!("\n## {} — usage guide\n\n{guide}\n", m.name));
    }
    if !over_budget.is_empty() {
        s.push_str(&format!(
            "\n(Usage guides for {} were omitted to stay within this result's \
             size budget — request them one at a time with \
             ToolSearch{{\"query\": \"select:<name>\"}}.)\n",
            over_budget.join(", ")
        ));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::tool::InMemoryToolRegistry;

    use std::path::PathBuf;
    use std::sync::Arc;

    /// 一个声称 deferred 的虚构工具。
    struct DeferredTool {
        name: &'static str,
        desc: &'static str,
    }
    #[async_trait]
    impl Tool for DeferredTool {
        fn name(&self) -> &str {
            self.name
        }
        fn input_schema(&self) -> Value {
            json!({"type": "object"})
        }
        async fn prompt(&self, _: &PromptContext) -> String {
            self.desc.to_string()
        }
        fn is_deferred(&self) -> bool {
            true
        }
        fn short_description(&self) -> Option<String> {
            Some(self.desc.to_string())
        }
        async fn call(
            &self,
            _: Value,
            _: ToolContext,
            _: ProgressSender,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::text(""))
        }
    }

    /// 一个非 deferred 的对照工具。
    struct ActiveTool;
    #[async_trait]
    impl Tool for ActiveTool {
        fn name(&self) -> &str {
            "Active"
        }
        fn input_schema(&self) -> Value {
            json!({"type": "object"})
        }
        async fn prompt(&self, _: &PromptContext) -> String {
            "always active".into()
        }
        async fn call(
            &self,
            _: Value,
            _: ToolContext,
            _: ProgressSender,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::text(""))
        }
    }

    fn make_registry() -> Arc<InMemoryToolRegistry> {
        let reg = InMemoryToolRegistry::new();
        reg.register(Arc::new(ActiveTool));
        reg.register(Arc::new(DeferredTool {
            name: "GitHubIssues",
            desc: "Fetch GitHub issues for a repo",
        }));
        reg.register(Arc::new(DeferredTool {
            name: "GitHubPRs",
            desc: "List GitHub pull requests",
        }));
        reg.register(Arc::new(DeferredTool {
            name: "Datadog",
            desc: "Query Datadog metrics",
        }));
        Arc::new(reg)
    }

    fn make_ctx() -> ToolContext {
        ToolContext::for_test(PathBuf::from("/tmp"))
    }

    #[tokio::test]
    async fn keyword_match_finds_relevant_deferred_tools() {
        let reg = make_registry();
        let tool = ToolSearchTool::new(reg);
        let ctx = make_ctx();
        let result = tool
            .call(
                json!({"query": "github"}),
                ctx.clone(),
                ProgressSender::noop("tu_1"),
            )
            .await
            .unwrap();
        match result.content {
            base::tool::ToolResultContent::Text(t) => {
                assert!(t.contains("GitHubIssues"));
                assert!(t.contains("GitHubPRs"));
                assert!(!t.contains("Datadog"), "Datadog should not match 'github'");
            }
            _ => panic!(),
        }
    }

    #[tokio::test]
    async fn select_prefix_does_exact_match() {
        let reg = make_registry();
        let tool = ToolSearchTool::new(reg);
        let ctx = make_ctx();
        let result = tool
            .call(
                json!({"query": "select:Datadog"}),
                ctx.clone(),
                ProgressSender::noop("tu_1"),
            )
            .await
            .unwrap();
        match result.content {
            base::tool::ToolResultContent::Text(t) => {
                assert!(t.contains("Datadog"));
            }
            _ => panic!(),
        }
    }

    #[tokio::test]
    async fn select_prefix_with_unknown_name_is_an_error_with_suggestions() {
        let tool = ToolSearchTool::new(make_registry());
        let result = tool
            .call(
                json!({"query": "select:GitHubIssue"}),
                make_ctx(),
                ProgressSender::noop("tu_1"),
            )
            .await
            .unwrap();
        assert!(result.is_error);
        let text = text_of(&result);
        assert!(
            text.contains("Did you mean: GitHubIssues"),
            "a near miss must point at the real name: {text}"
        );
        assert_eq!(
            result.structured_content.unwrap()["unmatched"],
            json!(["GitHubIssue"])
        );
    }

    #[tokio::test]
    async fn keywords_never_match_non_deferred_undocumented_tools() {
        let reg = InMemoryToolRegistry::new();
        reg.register(schema_tool("TicketLookup"));
        let tool = ToolSearchTool::new(Arc::new(reg));
        let result = tool
            .call(
                json!({"query": "ticket"}),
                make_ctx(),
                ProgressSender::noop("tu_1"),
            )
            .await
            .unwrap();
        assert_eq!(result.structured_content.unwrap()["matches"], json!([]));
    }

    fn text_of(result: &ToolResult) -> String {
        match &result.content {
            base::tool::ToolResultContent::Text(t) => t.clone(),
            _ => panic!("expected text"),
        }
    }

    fn schema_tool(name: &'static str) -> Arc<dyn Tool> {
        struct WithSchema(&'static str);
        #[async_trait]
        impl Tool for WithSchema {
            fn name(&self) -> &str {
                self.0
            }
            fn description(&self) -> &str {
                "Look up a ticket"
            }
            fn input_schema(&self) -> Value {
                json!({"type": "object", "properties": {"ticket": {"type": "string"}},
                       "required": ["ticket"]})
            }
            async fn call(
                &self,
                input: Value,
                _: ToolContext,
                _: ProgressSender,
            ) -> Result<ToolResult, ToolError> {
                Ok(ToolResult::text(format!("looked up {}", input["ticket"])))
            }
        }
        Arc::new(WithSchema(name))
    }

    fn deferred_registry() -> Arc<InMemoryToolRegistry> {
        let reg = InMemoryToolRegistry::new();
        for t in crate::deferred::apply_deferred_policy(
            vec![
                schema_tool("mcp__desk__ticket_get"),
                schema_tool("mcp__desk__ticket_list"),
                Arc::new(ActiveTool),
            ],
            &[
                "mcp__desk__ticket_get".into(),
                "mcp__desk__ticket_list".into(),
            ],
        ) {
            reg.register(t);
        }
        Arc::new(reg)
    }

    /// The contract the prompt promises: the real schema comes back in a
    /// `<functions>` block, the tool is recorded as activated, and calling it
    /// afterwards through the registry runs the wrapped tool.
    #[tokio::test]
    async fn select_returns_the_real_schema_and_activates_the_tool() {
        let reg = deferred_registry();
        let tool = ToolSearchTool::new(reg.clone());
        let ctx = make_ctx();
        let result = tool
            .call(
                json!({"query": "select:mcp__desk__ticket_get"}),
                ctx.clone(),
                ProgressSender::noop("tu_1"),
            )
            .await
            .unwrap();
        assert!(!result.is_error);

        let text = text_of(&result);
        let start = text.find("<function>").expect("a <function> line") + "<function>".len();
        let end = text.find("</function>").unwrap();
        let def: Value = serde_json::from_str(&text[start..end]).unwrap();
        assert_eq!(def["name"], "mcp__desk__ticket_get");
        assert_eq!(def["parameters"]["required"], json!(["ticket"]));
        assert!(text.starts_with("<functions>\n"));

        assert!(ctx
            .session
            .activated_tools()
            .contains("mcp__desk__ticket_get"));

        let ran = reg
            .get("mcp__desk__ticket_get")
            .unwrap()
            .call(json!({"ticket": "T-1"}), ctx, ProgressSender::noop("tu_2"))
            .await
            .unwrap();
        assert_eq!(text_of(&ran), "looked up \"T-1\"");
    }

    #[tokio::test]
    async fn select_takes_a_comma_list_ignores_trailing_words_and_case() {
        let tool = ToolSearchTool::new(deferred_registry());
        let result = tool
            .call(
                json!({"query": "select: MCP__desk__ticket_get please, mcp__desk__ticket_list"}),
                make_ctx(),
                ProgressSender::noop("tu_1"),
            )
            .await
            .unwrap();
        assert_eq!(
            result.structured_content.as_ref().unwrap()["matches"],
            json!(["mcp__desk__ticket_get", "mcp__desk__ticket_list"])
        );
        assert_eq!(text_of(&result).matches("<function>").count(), 2);
    }

    #[tokio::test]
    async fn plus_term_must_appear_in_the_name() {
        let tool = ToolSearchTool::new(make_registry());
        let result = tool
            .call(
                json!({"query": "+prs github"}),
                make_ctx(),
                ProgressSender::noop("tu_1"),
            )
            .await
            .unwrap();
        assert_eq!(
            result.structured_content.unwrap()["matches"],
            json!(["GitHubPRs"])
        );
    }

    #[tokio::test]
    async fn keywords_are_matched_separately() {
        let tool = ToolSearchTool::new(make_registry());
        let result = tool
            .call(
                json!({"query": "datadog metrics"}),
                make_ctx(),
                ProgressSender::noop("tu_1"),
            )
            .await
            .unwrap();
        assert_eq!(
            result.structured_content.unwrap()["matches"][0],
            json!("Datadog")
        );
    }

    /// A-1's payoff: `select:` on a documented tool returns the *body* of
    /// `Tool::prompt()`, which before this reached the model on exactly zero
    /// requests. `Documented` is deliberately **not** deferred — the tools
    /// this defect was really about (`Agent`, `TeamCreate`) ship full schemas
    /// and were still undiscoverable here.
    #[tokio::test]
    async fn select_returns_the_tools_usage_guide() {
        const GUIDE: &str = "# Agent tool\n\nSpawn a sub-agent when the search is wide.";
        struct Documented;
        #[async_trait]
        impl Tool for Documented {
            fn name(&self) -> &str {
                "Agent"
            }
            fn description(&self) -> &str {
                "Launch a sub-agent."
            }
            fn input_schema(&self) -> Value {
                json!({"type": "object"})
            }
            async fn prompt(&self, _: &PromptContext) -> String {
                GUIDE.to_string()
            }
            async fn call(
                &self,
                _: Value,
                _: ToolContext,
                _: ProgressSender,
            ) -> Result<ToolResult, ToolError> {
                Ok(ToolResult::text(""))
            }
        }

        let reg = InMemoryToolRegistry::new();
        reg.register(Arc::new(Documented));
        let tool = ToolSearchTool::new(Arc::new(reg));
        let result = tool
            .call(
                json!({"query": "select:Agent"}),
                make_ctx(),
                ProgressSender::noop("tu_1"),
            )
            .await
            .unwrap();

        match result.content {
            base::tool::ToolResultContent::Text(t) => {
                assert!(
                    t.contains(GUIDE),
                    "usage guide must be returned in full: {t}"
                );
                assert!(t.contains("Agent"));
            }
            _ => panic!("expected text"),
        }
        let sc = result.structured_content.unwrap();
        assert_eq!(sc["guides_returned"], json!(["Agent"]));
    }

    /// A deferred tool with no `prompt()` override must still resolve — the
    /// original schema-activation behaviour cannot regress just because
    /// guides were added alongside it.
    #[tokio::test]
    async fn select_still_works_for_a_tool_without_a_guide() {
        struct Bare;
        #[async_trait]
        impl Tool for Bare {
            fn name(&self) -> &str {
                "Bare"
            }
            fn description(&self) -> &str {
                "Does one thing."
            }
            fn input_schema(&self) -> Value {
                json!({"type": "object"})
            }
            fn is_deferred(&self) -> bool {
                true
            }
            async fn call(
                &self,
                _: Value,
                _: ToolContext,
                _: ProgressSender,
            ) -> Result<ToolResult, ToolError> {
                Ok(ToolResult::text(""))
            }
        }

        let reg = InMemoryToolRegistry::new();
        reg.register(Arc::new(Bare));
        let tool = ToolSearchTool::new(Arc::new(reg));
        let result = tool
            .call(
                json!({"query": "select:Bare"}),
                make_ctx(),
                ProgressSender::noop("tu_1"),
            )
            .await
            .unwrap();

        let sc = result.structured_content.unwrap();
        assert_eq!(sc["matches"], json!(["Bare"]));
        assert_eq!(
            sc["guides_returned"],
            json!([]),
            "a tool with no prompt() override must report no guide, not an \
             echo of its own description"
        );
        match result.content {
            base::tool::ToolResultContent::Text(t) => {
                assert!(t.contains("Bare"));
                assert!(!t.contains("usage guide"));
            }
            _ => panic!("expected text"),
        }
    }

    /// A tool that is neither deferred nor documented has nothing to
    /// disclose: asking for it by name says to call it directly, and it never
    /// shows up as a search hit.
    #[tokio::test]
    async fn plain_tools_are_reported_as_already_loaded() {
        struct Plain;
        #[async_trait]
        impl Tool for Plain {
            fn name(&self) -> &str {
                "Plain"
            }
            fn description(&self) -> &str {
                "Plain tool."
            }
            fn input_schema(&self) -> Value {
                json!({"type": "object"})
            }
            async fn call(
                &self,
                _: Value,
                _: ToolContext,
                _: ProgressSender,
            ) -> Result<ToolResult, ToolError> {
                Ok(ToolResult::text(""))
            }
        }

        let reg = InMemoryToolRegistry::new();
        reg.register(Arc::new(Plain));
        let tool = ToolSearchTool::new(Arc::new(reg));
        let result = tool
            .call(
                json!({"query": "select:Plain"}),
                make_ctx(),
                ProgressSender::noop("tu_1"),
            )
            .await
            .unwrap();
        assert!(!result.is_error);
        assert!(!text_of(&result).contains("<functions>"));
        let sc = result.structured_content.unwrap();
        assert_eq!(sc["matches"], json!([]));
        assert_eq!(sc["already_loaded"], json!(["Plain"]));
    }

    #[tokio::test]
    async fn empty_query_validates_err() {
        let reg = make_registry();
        let tool = ToolSearchTool::new(reg);
        let r = tool
            .validate_input(&json!({"query": "   "}), &make_ctx())
            .await;
        assert!(!matches!(r, ValidationResult::Ok));
    }

    #[tokio::test]
    async fn max_results_caps_returns() {
        let reg = make_registry();
        let tool = ToolSearchTool::new(reg);
        let ctx = make_ctx();
        let result = tool
            .call(
                json!({"query": "g", "max_results": 1}),
                ctx.clone(),
                ProgressSender::noop("tu_1"),
            )
            .await
            .unwrap();
        // 'g' 命中 GitHubIssues / GitHubPRs / 但 max 1 时只返回 1 个
        match result.content {
            base::tool::ToolResultContent::Text(t) => {
                assert!(t.contains("1 tool"), "expected 1 tool, got: {t}");
            }
            _ => panic!(),
        }
    }
}
