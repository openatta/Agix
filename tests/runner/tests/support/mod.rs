//! Normalization and golden-file plumbing for `turn_behavior_net`.
//!
//! Normalization is the load-bearing part. A raw event stream is full of
//! things that change every run — ids, wall-clock timestamps, durations,
//! temp-dir paths — and a snapshot that includes them fails constantly for
//! reasons no one cares about, which is how a net gets disabled. Everything
//! here is about keeping the *decisions* and dropping the rest.
//!
//! The line to walk: drop too little and the net is flaky; drop too much and
//! it stops noticing real changes. So values are replaced with placeholders
//! rather than deleted — `<id>` still records that there *was* an id, and a
//! tool result that becomes empty is still a visible difference.

use base::event::AgentEvent;
use base::interface::tool::{
    PermissionDecision, ProgressSender, PromptContext, Tool, ToolContext, ToolResult,
};
use std::sync::Arc;

// ── tools ───────────────────────────────────────────────────────────────

/// Echoes its `say` argument. Concurrency-safe, so a batch of them exercises
/// the parallel dispatch path.
#[derive(Debug)]
struct GoldenEcho;

#[async_trait::async_trait]
impl Tool for GoldenEcho {
    fn name(&self) -> &str {
        "GoldenEcho"
    }
    fn description(&self) -> &str {
        "Echo the `say` argument back."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"say": {"type": "string"}},
            "required": ["say"]
        })
    }
    fn is_concurrency_safe(&self, _: &serde_json::Value) -> bool {
        true
    }
    fn is_read_only(&self, _: &serde_json::Value) -> bool {
        true
    }
    async fn check_permissions(
        &self,
        _: &serde_json::Value,
        _: &ToolContext,
    ) -> PermissionDecision {
        PermissionDecision::allow()
    }
    async fn prompt(&self, _: &PromptContext) -> String {
        self.description().to_string()
    }
    async fn call(
        &self,
        input: serde_json::Value,
        _ctx: ToolContext,
        _p: ProgressSender,
    ) -> Result<ToolResult, base::error::ToolError> {
        let say = input.get("say").and_then(|v| v.as_str()).unwrap_or("");
        Ok(ToolResult::text(format!("echo: {say}")))
    }
}

/// Returns a result far past the per-result tool-result budget.
///
/// Exists so the net can pin the truncation decision, which is otherwise
/// invisible: every other tool here returns a few bytes, so the budget never
/// fires and a change to it would go unnoticed.
#[derive(Debug)]
struct GoldenFlood;

#[async_trait::async_trait]
impl Tool for GoldenFlood {
    fn name(&self) -> &str {
        "GoldenFlood"
    }
    fn description(&self) -> &str {
        "Return a very large result."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    fn is_concurrency_safe(&self, _: &serde_json::Value) -> bool {
        true
    }
    fn is_read_only(&self, _: &serde_json::Value) -> bool {
        true
    }
    async fn check_permissions(
        &self,
        _: &serde_json::Value,
        _: &ToolContext,
    ) -> PermissionDecision {
        PermissionDecision::allow()
    }
    async fn prompt(&self, _: &PromptContext) -> String {
        self.description().to_string()
    }
    async fn call(
        &self,
        _input: serde_json::Value,
        _ctx: ToolContext,
        _p: ProgressSender,
    ) -> Result<ToolResult, base::error::ToolError> {
        // Past `enforce_tool_result_budget`'s 50_000-byte per-result cap.
        Ok(ToolResult::text("x".repeat(60_000)))
    }
}

/// Runs fine and calls its own result a failure.
///
/// The other half of failing, and the one that had no fixture. `GoldenBoom`
/// returns `Err` — the engine could not run the tool — which has always
/// reached the model as `is_error: true`. Nine production tools instead
/// return `Ok` with `ToolResult::is_error` set: a shell command that exited
/// non-zero, an HTTP status a fetch will not follow, an MCP server answering
/// `isError`, a plugin whose guest trapped. That flag was dropped on the way
/// to the model for as long as it existed, and no fixture here failed that
/// way, so nothing noticed.
#[derive(Debug)]
struct GoldenReports;

#[async_trait::async_trait]
impl Tool for GoldenReports {
    fn name(&self) -> &str {
        "GoldenReports"
    }
    fn description(&self) -> &str {
        "Runs, and reports that the work failed."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    async fn check_permissions(
        &self,
        _: &serde_json::Value,
        _: &ToolContext,
    ) -> PermissionDecision {
        PermissionDecision::allow()
    }
    async fn prompt(&self, _: &PromptContext) -> String {
        self.description().to_string()
    }
    async fn call(
        &self,
        _input: serde_json::Value,
        _ctx: ToolContext,
        _p: ProgressSender,
    ) -> Result<ToolResult, base::error::ToolError> {
        Ok(ToolResult::error_text("exit status 1"))
    }
}

/// Always fails. Exists so the net covers "a tool errored" as a distinct
/// decision from "the model errored".
#[derive(Debug)]
struct GoldenBoom;

#[async_trait::async_trait]
impl Tool for GoldenBoom {
    fn name(&self) -> &str {
        "GoldenBoom"
    }
    fn description(&self) -> &str {
        "Always fails."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    async fn check_permissions(
        &self,
        _: &serde_json::Value,
        _: &ToolContext,
    ) -> PermissionDecision {
        PermissionDecision::allow()
    }
    async fn prompt(&self, _: &PromptContext) -> String {
        self.description().to_string()
    }
    async fn call(
        &self,
        _input: serde_json::Value,
        _ctx: ToolContext,
        _p: ProgressSender,
    ) -> Result<ToolResult, base::error::ToolError> {
        Err(base::error::ToolError::Execution(anyhow::anyhow!(
            "golden boom"
        )))
    }
}

/// Defers to the rule engine instead of self-approving, so a session running
/// in a mode that asks will actually produce a `PermissionPrompt`.
#[derive(Debug)]
struct GoldenAsk;

#[async_trait::async_trait]
impl Tool for GoldenAsk {
    fn name(&self) -> &str {
        "GoldenAsk"
    }
    fn description(&self) -> &str {
        "Requires approval."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    async fn check_permissions(
        &self,
        _: &serde_json::Value,
        _: &ToolContext,
    ) -> PermissionDecision {
        PermissionDecision::Ask {
            message: "GoldenAsk wants to run".into(),
            decision_reason: None,
        }
    }
    async fn prompt(&self, _: &PromptContext) -> String {
        self.description().to_string()
    }
    async fn call(
        &self,
        _input: serde_json::Value,
        _ctx: ToolContext,
        _p: ProgressSender,
    ) -> Result<ToolResult, base::error::ToolError> {
        Ok(ToolResult::text("approved and ran"))
    }
}

/// Returns a kilobyte under the name `Read`.
///
/// The name is the point: `micro_compact` only blanks results from a fixed
/// whitelist of tools, and nothing in the default set here is on it. A case
/// about a pass that clears old tool results needs a tool whose results that
/// pass is willing to clear, so this one is registered per-case rather than
/// globally — adding it to `fake_tools` would change the tool schemas every
/// other case sends, and with them every other golden.
#[derive(Debug)]
struct GoldenReadFile;

#[async_trait::async_trait]
impl Tool for GoldenReadFile {
    fn name(&self) -> &str {
        "Read"
    }
    fn description(&self) -> &str {
        "Return a kilobyte of file contents."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    fn is_concurrency_safe(&self, _: &serde_json::Value) -> bool {
        true
    }
    fn is_read_only(&self, _: &serde_json::Value) -> bool {
        true
    }
    async fn check_permissions(
        &self,
        _: &serde_json::Value,
        _: &ToolContext,
    ) -> PermissionDecision {
        PermissionDecision::allow()
    }
    async fn prompt(&self, _: &PromptContext) -> String {
        self.description().to_string()
    }
    async fn call(
        &self,
        _input: serde_json::Value,
        _ctx: ToolContext,
        _p: ProgressSender,
    ) -> Result<ToolResult, base::error::ToolError> {
        Ok(ToolResult::text("r".repeat(1_000)))
    }
}

/// A tool whose results the micro-compact whitelist will actually clear.
pub fn compactable_tool() -> Arc<dyn Tool> {
    Arc::new(GoldenReadFile)
}

pub fn fake_tools() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(GoldenEcho),
        Arc::new(GoldenBoom),
        Arc::new(GoldenReports),
        Arc::new(GoldenAsk),
        Arc::new(GoldenFlood),
    ]
}

// ── normalization ───────────────────────────────────────────────────────

/// Replace run-varying substrings with stable placeholders.
fn scrub(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for line in s.lines() {
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// One line per event: the variant name plus only the fields that encode a
/// decision. Text is kept (it is the model's scripted answer, so it is
/// stable); ids and turn ids are not.
pub fn normalize_events(events: &[AgentEvent]) -> String {
    let mut out = String::new();
    for ev in events {
        let line = match ev {
            AgentEvent::TextDelta { text, .. } => format!("TextDelta {text:?}"),
            AgentEvent::ThinkingDelta { text, .. } => format!("ThinkingDelta {text:?}"),
            AgentEvent::ToolUseStart { name, .. } => format!("ToolUseStart {name}"),
            AgentEvent::ToolArgsDelta { partial_json, .. } => {
                format!("ToolArgsDelta {partial_json:?}")
            }
            AgentEvent::ToolUse { name, input, .. } => {
                format!("ToolUse {name} {}", canonical_json(input))
            }
            // The content matters: without it "the hook blocked this" and
            // "the tool ran and failed" both render as is_error=true, and the
            // net stops distinguishing two very different outcomes.
            AgentEvent::ToolResult {
                name,
                is_error,
                content,
                ..
            } => format!(
                "ToolResult {name} is_error={is_error:?} content={:?}",
                elide(content)
            ),
            AgentEvent::PermissionPrompt { message, .. } => {
                format!("PermissionPrompt {:?}", first_line(message))
            }
            AgentEvent::TurnStart { .. } => "TurnStart".to_string(),
            AgentEvent::StepStart { step, .. } => format!("StepStart step={step}"),
            AgentEvent::StepEnd {
                step,
                stop_reason,
                tool_calls,
                ..
            } => format!("StepEnd step={step} stop_reason={stop_reason} tool_calls={tool_calls}"),
            AgentEvent::TurnComplete { stop_reason, .. } => {
                format!("TurnComplete stop_reason={stop_reason}")
            }
            AgentEvent::UserMessageDelivered { content, .. } => {
                format!("UserMessageDelivered {content:?}")
            }
            AgentEvent::UserMessagesReturned { messages, .. } => {
                format!("UserMessagesReturned count={}", messages.len())
            }
            AgentEvent::SystemInit { .. } => "SystemInit".to_string(),
            AgentEvent::System { message } => format!("System {:?}", first_line(message)),
            AgentEvent::CompactAction { strategy, .. } => format!("CompactAction {strategy}"),
            AgentEvent::SessionChanged { .. } => "SessionChanged".to_string(),
            AgentEvent::SessionPersisted { .. } => "SessionPersisted".to_string(),
            AgentEvent::SkillsChanged { .. } => "SkillsChanged".to_string(),
            AgentEvent::AgentSpawned { parent_turn, .. } => {
                format!("AgentSpawned parent_turn={parent_turn}")
            }
            AgentEvent::AgentCompleted { .. } => "AgentCompleted".to_string(),
            AgentEvent::SubagentProgress { .. } => "SubagentProgress".to_string(),
            AgentEvent::TeamProgress { .. } => "TeamProgress".to_string(),
            AgentEvent::Error { code, .. } => format!("Error code={code}"),
        };
        out.push_str(&line);
        out.push('\n');
    }
    scrub(&out)
}

/// One line per log entry: its kind, and for the entries that carry a decision
/// (compaction, session end) the discriminating field.
pub fn normalize_log(entries: &[history::entry::EnvelopedEntry]) -> String {
    let mut out = String::new();
    for e in entries {
        let line = match &e.entry {
            history::entry::LogEntry::Meta { .. } => "Meta".to_string(),
            history::entry::LogEntry::User { content } => {
                format!("User blocks={}", content.len())
            }
            history::entry::LogEntry::Assistant { .. } => "Assistant".to_string(),
            history::entry::LogEntry::ToolResult { .. } => "ToolResult".to_string(),
            history::entry::LogEntry::System { .. } => "System".to_string(),
            history::entry::LogEntry::Compact { .. } => "Compact".to_string(),
            history::entry::LogEntry::UsageSnapshot { .. } => "UsageSnapshot".to_string(),
            history::entry::LogEntry::PasteRef { .. } => "PasteRef".to_string(),
            history::entry::LogEntry::Blob { blob } => format!("Blob {}", blob.describes),
            history::entry::LogEntry::SessionEnd { state } => format!("SessionEnd {state:?}"),
            history::entry::LogEntry::Extension { ns, event, .. } => {
                format!("Extension {ns}/{event}")
            }
        };
        out.push_str(&line);
        out.push('\n');
    }
    out
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("")
}

/// A stable, readable stand-in for content that is long on purpose.
///
/// Without this, a case about a 60 KB tool result writes a 60 KB golden, and
/// a golden nobody can read is a golden nobody reviews — which is the failure
/// mode a regression net is supposed to prevent, arriving by a different
/// road. The length is kept because the length is the point.
fn elide(s: &str) -> String {
    const KEEP: usize = 80;
    let head = first_line(s);
    if head.len() <= KEEP && head.len() == s.len() {
        return head.to_string();
    }
    format!("{}… <{} bytes total>", truncate_chars(head, KEEP), s.len())
}

/// Character-boundary-safe prefix. A tool result is arbitrary bytes, so a
/// byte-index cut lands mid-character routinely.
pub fn truncate_chars(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Sorted-key JSON so argument ordering (a serialization detail) never shows
/// up as a behavior change.
fn canonical_json(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Object(m) => {
            let mut keys: Vec<_> = m.keys().collect();
            keys.sort();
            let inner: Vec<String> = keys
                .iter()
                .map(|k| format!("{k:?}:{}", canonical_json(&m[k.as_str()])))
                .collect();
            format!("{{{}}}", inner.join(","))
        }
        serde_json::Value::Array(a) => {
            let inner: Vec<String> = a.iter().map(canonical_json).collect();
            format!("[{}]", inner.join(","))
        }
        other => other.to_string(),
    }
}

// ── golden files ────────────────────────────────────────────────────────

pub struct GoldenFile {
    path: std::path::PathBuf,
    name: String,
}

impl GoldenFile {
    pub fn new(name: &str) -> Self {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("tests/")
            .join("fixtures")
            .join("turn_traces");
        Self {
            path: dir.join(format!("{name}.txt")),
            name: name.to_string(),
        }
    }

    pub fn assert_eq(&self, actual: &str) {
        if std::env::var("AGIX_UPDATE_GOLDEN").is_ok() {
            if let Some(p) = self.path.parent() {
                std::fs::create_dir_all(p).expect("golden dir");
            }
            std::fs::write(&self.path, actual).expect("write golden");
            return;
        }

        let expected = std::fs::read_to_string(&self.path).unwrap_or_else(|_| {
            panic!(
                "no golden trace for `{}` at {}. If this case is new, create it with \
                 AGIX_UPDATE_GOLDEN=1 and read what it produced before committing.\n\
                 --- would be written ---\n{actual}",
                self.name,
                self.path.display()
            )
        });

        if expected != actual {
            panic!(
                "turn behavior for `{}` changed.\n\n\
                 --- expected ({}) ---\n{expected}\n\
                 --- actual ---\n{actual}\n\
                 If this change is intended, re-run with AGIX_UPDATE_GOLDEN=1 — after \
                 reading the diff above.",
                self.name,
                self.path.display()
            );
        }
    }
}

// ── scene ───────────────────────────────────────────────────────────────

/// `ChatScene` with a caller-chosen compaction threshold, offering whatever
/// tools the case registers.
///
/// The threshold lives on the scene, not in settings, so a case that wants to
/// exercise compaction cannot get there by tweaking `Settings` — it needs a
/// scene of its own. The tool surface is opened because the scene's
/// whitelist decides what a session has at all, and the cases' fake tools
/// are on no real scene's list. Everything else delegates.
pub struct BudgetScene {
    inner: scene::scene::chat::ChatScene,
    pub compact_threshold: usize,
    /// `None` leaves the chat scene's own ceiling. `Some` is how a case pins
    /// the stop-on-max-turns decision without running dozens of model calls.
    pub max_api_calls: Option<u32>,
}

impl BudgetScene {
    pub fn new(compact_threshold: usize) -> Self {
        Self {
            inner: scene::scene::chat::ChatScene,
            compact_threshold,
            max_api_calls: None,
        }
    }

    pub fn with_max_api_calls(mut self, max: u32) -> Self {
        self.max_api_calls = Some(max);
        self
    }
}

impl base::interface::scene::AgentScene for BudgetScene {
    fn id(&self) -> &str {
        self.inner.id()
    }
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn description(&self) -> &str {
        self.inner.description()
    }
    fn build_system_prompt(
        &self,
        ctx: &base::interface::scene::ScenePromptContext,
    ) -> Vec<base::interface::prompt::PromptBlock> {
        self.inner.build_system_prompt(ctx)
    }
    fn tools(&self) -> Vec<String> {
        Vec::new()
    }
    fn token_budget(&self) -> base::interface::scene::TokenBudget {
        base::interface::scene::TokenBudget {
            compact_threshold: self.compact_threshold,
            compact_keep_recent: 2,
        }
    }
    fn execution_params(&self) -> base::interface::scene::ExecutionParams {
        let mut params = self.inner.execution_params();
        if let Some(max) = self.max_api_calls {
            params.max_api_calls_per_turn = max;
        }
        params
    }
}
