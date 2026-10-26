//! Anthropic Messages API 请求体类型。
//!
//! 字段名 / 形状对应 API；与 agix-base::Message / ContentBlock 配合使用。

use base::message::{ContentBlock, Role};
use serde::Serialize;
use serde_json::Value;

/// Anthropic's hard cap on `cache_control` breakpoints in a single request,
/// counted across `tools` + `system` + `messages` combined. A request with a
/// fifth breakpoint is rejected with a 400, so every automatic breakpoint this
/// module adds has to be budgeted against the ones the caller placed itself.
pub const MAX_CACHE_BREAKPOINTS: usize = 4;

/// Beta flag historically required to use `ttl: "1h"` on a `cache_control`
/// block. Anthropic has since made the 1-hour TTL generally available and no
/// longer documents the header as a requirement, but it is still accepted, and
/// Anthropic-compatible relays that mirror an older snapshot of the API may
/// still gate on it — sending it costs one header and removes a silent
/// "1h TTL quietly downgraded to 5m" failure mode. See
/// [`MessagesRequest::ensure_cache_ttl_beta`].
pub const EXTENDED_CACHE_TTL_BETA: &str = "extended-cache-ttl-2025-04-11";

/// Built-in Anthropic tool types (e.g., `web_search_20250305`) that go into the
/// `tools` array alongside regular function `ToolDef` entries.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum BuiltinTool {
    /// Server-side web search (requires beta header
    /// `anthropic-beta: web-search-20250305-2025-03-05`).
    #[serde(rename = "web_search_20250305")]
    WebSearch {
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        allowed_domains: Option<Vec<String>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        blocked_domains: Option<Vec<String>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        max_uses: Option<u32>,
    },
}

/// Messages.create 的请求体。`stream: true` 永远开启（loop 是流式的）。
///
/// 注：自定义 `Serialize` —— tools + anthropic_tools 合并成一个 JSON
/// `tools` 数组发送，built-in tools 与 function tools 在同一数组里。
#[derive(Debug, Clone)]
pub struct MessagesRequest {
    pub model: String,
    pub max_tokens: u32,

    /// system prompt 是字符串数组（每段可独立带 cache_control）。
    pub system: Vec<SystemBlock>,

    pub messages: Vec<MessageParam>,

    pub tools: Vec<ToolDef>,

    /// Built-in Anthropic tools (web_search_20250305, etc.) merged into the
    /// same `tools` JSON array as `tools` during serialization.
    pub anthropic_tools: Vec<BuiltinTool>,

    pub tool_choice: Option<ToolChoice>,

    /// 永远 true（流式）
    pub stream: bool,

    /// `None` = 不设字段（让模型按默认走，思考模型仍会思考）。
    /// `Some(Disabled)` = 显式关闭思考（DeepSeek V4 + 部分 Anthropic-compat
    /// 端点接受；-历史 baseline 都用了这条以避免多 turn 400）。
    /// `Some(Enabled{...}) / Some(Adaptive)` = 开思考。
    pub thinking: Option<ThinkingConfig>,

    /// 当 thinking 开启时**必须** None；否则 API 报错
    pub temperature: Option<f32>,

    pub top_p: Option<f32>,

    pub top_k: Option<u32>,

    pub stop_sequences: Vec<String>,

    /// `{ user_id }`，匿名化后的 device + session 标识；用于 abuse 监控
    pub metadata: Option<RequestMetadata>,

    /// beta header 列表：走 HTTP `anthropic-beta` 头而非 body。
    pub betas: Vec<String>,

    /// fast / extended-output / 任务预算等高级参数
    pub speed: Option<Speed>,
}

impl MessagesRequest {
    /// 一个最小可用的请求构造器：模型 + 一条 user 文字。
    /// system / tools 为空；适合冒烟测试和单元测。
    pub fn minimal(model: impl Into<String>, user_text: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            max_tokens: 1024,
            system: Vec::new(),
            messages: vec![MessageParam {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: user_text.into(),
                    cache_control: None,
                }],
            }],
            tools: Vec::new(),
            anthropic_tools: Vec::new(),
            tool_choice: None,
            stream: true,
            thinking: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: Vec::new(),
            metadata: None,
            betas: Vec::new(),
            speed: None,
        }
    }

    // ── Prompt-cache breakpoint placement ──
    //
    // **Why this lives on the request (and is applied during `Serialize`)
    // rather than in `adapter.rs`.**
    //
    // The API renders a request as `tools` → `system` → `messages`, and the
    // 4-breakpoint budget is shared across all three. Deciding where the
    // automatic breakpoints go therefore needs to see the whole request at
    // once; splitting the decision between "who builds the tools array" and
    // "who builds the messages array" is how you end up emitting five
    // breakpoints and getting a 400 on a long session.
    //
    // The messages breakpoint additionally *cannot* be expressed in the Rust
    // value: `base::message::ContentBlock` only carries a `cache_control`
    // field on its `Text` variant, and in an agentic session the tail of the
    // conversation is overwhelmingly `tool_result` blocks. Rather than mark
    // some arbitrary earlier text block (or, worse, append a synthetic text
    // block — which would change the *content* bytes of an older message and
    // invalidate the very prefix we are trying to cache), the marker is
    // injected into the already-serialized JSON, where any block type can
    // carry it. `cache_control` is metadata, not content: moving a breakpoint
    // between turns creates a new read point without invalidating entries
    // written at earlier positions, which is exactly what makes the rolling
    // scheme below work.

    /// Breakpoints the caller placed explicitly on `system` blocks.
    fn explicit_system_breakpoints(&self) -> usize {
        self.system
            .iter()
            .filter(|b| match b {
                SystemBlock::Text { cache_control, .. } => cache_control.is_some(),
            })
            .count()
    }

    /// Breakpoints the caller placed explicitly on `tools` entries.
    fn explicit_tool_breakpoints(&self) -> usize {
        self.tools
            .iter()
            .filter(|t| t.cache_control.is_some())
            .count()
    }

    /// Whether we should append an automatic breakpoint to the **last** entry
    /// of the rendered `tools` array.
    ///
    /// The tool array is the very first thing the API renders and is stable
    /// for the whole session (this repo ships 30+ tools, whose schemas are a
    /// non-trivial share of every request), so a breakpoint on its last entry
    /// caches the entire tool-definition prefix. It is worth having even
    /// though a `system` breakpoint already covers the same bytes: the system
    /// prompt carries runtime state (plan, todos, turn info) and changes
    /// between turns, and when it does, the tools-only entry is the one that
    /// still hits.
    ///
    /// Skipped when the caller placed its own tool breakpoints — an explicit
    /// choice about placement beats a heuristic.
    fn wants_auto_tools_breakpoint(&self) -> bool {
        !(self.tools.is_empty() && self.anthropic_tools.is_empty())
            && self.explicit_tool_breakpoints() == 0
    }

    /// Index into `self.messages` of the message that should carry the rolling
    /// conversation breakpoint, if any.
    ///
    /// **The rule: the second-to-last `user` message.** Not the last one —
    /// that message is new on this turn, so a breakpoint there is a pure cache
    /// *write* that nothing ever reads back. The second-to-last user message
    /// was the last one on the *previous* turn, so the entry written there
    /// last turn is read this turn, and a fresh (longer) entry is written one
    /// message further along. That is the standard rolling scheme: each turn
    /// reads the previous turn's entry and extends it.
    ///
    /// Caveat worth knowing: a breakpoint only searches back ~20 content
    /// blocks for a prior entry, so a single turn that emits more than that
    /// many blocks (a very wide parallel tool fan-out) can miss and pay a full
    /// write. That is still strictly better than today's behaviour, which
    /// re-bills the whole history every turn.
    fn auto_message_breakpoint_index(&self) -> Option<usize> {
        let mut user_indices = self
            .messages
            .iter()
            .enumerate()
            .filter(|(_, m)| m.role == Role::User)
            .map(|(i, _)| i);
        // `nth_back` would need a DoubleEndedIterator; collecting the last two
        // by hand keeps this allocation-free.
        let (mut prev, mut last) = (None, None);
        for i in user_indices.by_ref() {
            prev = last;
            last = Some(i);
        }
        // Needs at least two user turns: with only one, the "second-to-last"
        // doesn't exist and the single turn is the volatile one.
        prev.filter(|&i| self.messages.get(i).is_some_and(|m| !m.content.is_empty()))
    }

    /// True when any `cache_control` this request will emit uses the 1-hour
    /// TTL — including the automatic tools breakpoint, which does.
    pub fn uses_1h_cache_ttl(&self) -> bool {
        let explicit = self.system.iter().any(|b| match b {
            SystemBlock::Text { cache_control, .. } => cache_control
                .as_ref()
                .is_some_and(CacheControl::is_one_hour),
        }) || self.tools.iter().any(|t| {
            t.cache_control
                .as_ref()
                .is_some_and(CacheControl::is_one_hour)
        });
        explicit || self.wants_auto_tools_breakpoint()
    }

    /// Add [`EXTENDED_CACHE_TTL_BETA`] to `betas` when this request uses a
    /// 1-hour cache TTL anywhere. Idempotent.
    ///
    /// Called by the adapter after the request is fully built, so the flag
    /// tracks what will actually be serialized rather than what the caller
    /// intended.
    pub fn ensure_cache_ttl_beta(&mut self) {
        if self.uses_1h_cache_ttl() && !self.betas.iter().any(|b| b == EXTENDED_CACHE_TTL_BETA) {
            self.betas.push(EXTENDED_CACHE_TTL_BETA.to_string());
        }
    }
}

/// Write `cache_control` into an already-serialized content-block / tool
/// object. No-op if the value isn't a JSON object (which would mean the wire
/// format changed out from under us) or already carries the key.
fn set_cache_control(target: &mut Value, cc: &CacheControl) {
    let Some(obj) = target.as_object_mut() else {
        return;
    };
    if obj.contains_key("cache_control") {
        return;
    }
    match serde_json::to_value(cc) {
        Ok(v) => {
            obj.insert("cache_control".to_string(), v);
        }
        // `CacheControl` is a plain enum of string fields; this cannot fail in
        // practice, and a missing breakpoint is a cost regression, not a
        // correctness one — so it degrades rather than aborting the request.
        Err(e) => tracing::warn!(error = %e, "failed to serialize cache_control"),
    }
}

// Custom Serialize: merges `tools` and `anthropic_tools` into a single
// `tools` JSON array so built-in tools (web_search_20250305) appear in the
// same array as function tools, matching the Anthropic Messages API contract.
//
// It is also where the automatic prompt-cache breakpoints are placed — see the
// block comment on `MessagesRequest`'s breakpoint helpers for why the decision
// has to happen here, with the whole request in view.
impl Serialize for MessagesRequest {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;

        // Breakpoint budget. `system` blocks are already marked by the caller
        // (from `PromptBlock.cache_strategy`), so they are spent first;
        // whatever is left is handed to tools and then to messages, in that
        // order. Tools come first because they sit at the very front of the
        // rendered prefix and are the only entry that survives a system-prompt
        // change — a messages breakpoint is worthless if the prefix ahead of
        // it already moved.
        let mut budget = MAX_CACHE_BREAKPOINTS
            .saturating_sub(self.explicit_system_breakpoints())
            .saturating_sub(self.explicit_tool_breakpoints());

        let mut map = s.serialize_map(None)?;

        // Required scalar fields
        map.serialize_entry("model", &self.model)?;
        map.serialize_entry("max_tokens", &self.max_tokens)?;
        map.serialize_entry("system", &self.system)?;

        // Merge tools + anthropic_tools into one JSON array
        let mut all_tools: Vec<Value> =
            Vec::with_capacity(self.tools.len() + self.anthropic_tools.len());
        for t in &self.tools {
            all_tools.push(serde_json::to_value(t).map_err(serde::ser::Error::custom)?);
        }
        for bt in &self.anthropic_tools {
            all_tools.push(serde_json::to_value(bt).map_err(serde::ser::Error::custom)?);
        }
        // The tool-prefix breakpoint. 1-hour TTL: the tool array is fixed for
        // the lifetime of a session, so it should outlive the gaps between
        // turns (a user thinking for ten minutes shouldn't cost a full
        // re-bill of every tool schema).
        if budget > 0 && self.wants_auto_tools_breakpoint() {
            if let Some(last) = all_tools.last_mut() {
                set_cache_control(last, &CacheControl::ephemeral_1h());
                budget -= 1;
            }
        }

        let mut messages: Vec<Value> = Vec::with_capacity(self.messages.len());
        for m in &self.messages {
            messages.push(serde_json::to_value(m).map_err(serde::ser::Error::custom)?);
        }
        // The rolling conversation breakpoint. 5-minute TTL rather than 1h:
        // this entry is superseded by the next turn's, so it only has to
        // survive one round trip, and a 5m write costs 1.25x base against 2x
        // for 1h.
        if budget > 0 {
            if let Some(idx) = self.auto_message_breakpoint_index() {
                if let Some(last) = messages
                    .get_mut(idx)
                    .and_then(|m| m.get_mut("content"))
                    .and_then(|c| c.as_array_mut())
                    .and_then(|blocks| blocks.last_mut())
                {
                    set_cache_control(last, &CacheControl::ephemeral_5m());
                    budget -= 1;
                }
            }
        }
        let _ = budget; // no further automatic breakpoints today

        map.serialize_entry("messages", &messages)?;
        map.serialize_entry("stream", &self.stream)?;
        map.serialize_entry("tools", &all_tools)?;

        // Optional fields (only if non-None / non-empty)
        if let Some(ref tc) = self.tool_choice {
            map.serialize_entry("tool_choice", tc)?;
        }
        if let Some(ref th) = self.thinking {
            map.serialize_entry("thinking", th)?;
        }
        if let Some(ref t) = self.temperature {
            map.serialize_entry("temperature", t)?;
        }
        if let Some(ref p) = self.top_p {
            map.serialize_entry("top_p", p)?;
        }
        if let Some(ref k) = self.top_k {
            map.serialize_entry("top_k", k)?;
        }
        if !self.stop_sequences.is_empty() {
            map.serialize_entry("stop_sequences", &self.stop_sequences)?;
        }
        if let Some(ref m) = self.metadata {
            map.serialize_entry("metadata", m)?;
        }
        if let Some(ref sp) = self.speed {
            map.serialize_entry("speed", sp)?;
        }

        map.end()
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SystemBlock {
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
}

impl SystemBlock {
    pub fn text(s: impl Into<String>) -> Self {
        Self::Text {
            text: s.into(),
            cache_control: None,
        }
    }
    pub fn text_cached(s: impl Into<String>, cc: CacheControl) -> Self {
        Self::Text {
            text: s.into(),
            cache_control: Some(cc),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct MessageParam {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
    /// ToolSearch 延迟加载标志
    #[serde(skip_serializing_if = "Option::is_none")]
    pub defer_loading: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolChoice {
    Auto {
        #[serde(skip_serializing_if = "Option::is_none")]
        disable_parallel_tool_use: Option<bool>,
    },
    Any {
        #[serde(skip_serializing_if = "Option::is_none")]
        disable_parallel_tool_use: Option<bool>,
    },
    Tool {
        name: String,
    },
    None,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CacheControl {
    Ephemeral {
        #[serde(skip_serializing_if = "Option::is_none")]
        ttl: Option<CacheTtl>,
        #[serde(skip_serializing_if = "Option::is_none")]
        scope: Option<CacheScope>,
    },
}

impl CacheControl {
    pub fn ephemeral_5m() -> Self {
        Self::Ephemeral {
            ttl: Some(CacheTtl::FiveMin),
            scope: None,
        }
    }
    pub fn ephemeral_1h() -> Self {
        Self::Ephemeral {
            ttl: Some(CacheTtl::OneHour),
            scope: None,
        }
    }
    pub fn ephemeral_1h_global() -> Self {
        Self::Ephemeral {
            ttl: Some(CacheTtl::OneHour),
            scope: Some(CacheScope::Global),
        }
    }

    /// Whether this breakpoint asks for the 1-hour TTL, which is what gates
    /// [`EXTENDED_CACHE_TTL_BETA`].
    pub fn is_one_hour(&self) -> bool {
        matches!(
            self,
            Self::Ephemeral {
                ttl: Some(CacheTtl::OneHour),
                ..
            }
        )
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub enum CacheTtl {
    #[serde(rename = "5m")]
    FiveMin,
    #[serde(rename = "1h")]
    OneHour,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub enum CacheScope {
    #[serde(rename = "global")]
    Global,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ThinkingConfig {
    /// 模型自适应预算（部分新模型支持；不支持的模型会报错）
    Adaptive,
    /// 显式 token 预算
    Enabled { budget_tokens: u32 },
    /// **L1 **: explicit "no thinking". DeepSeek V4 (thinking model
    /// by default) and some Anthropic-compat backends accept this to
    /// suppress reasoning_content emission. Important for multi-turn
    /// flows where the client doesn't echo thinking blocks back — DS V4
    /// rejects with 400 in that case (see compare-vs-claude-ts/scripts/
    /// llm_quality_diff_batch.py for historical use).
    Disabled,
}

#[derive(Debug, Clone, Serialize)]
pub struct RequestMetadata {
    pub user_id: String,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Speed {
    Fast,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn minimal_request_serializes() {
        let req = MessagesRequest::minimal("claude-sonnet-4-6", "hello");
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["model"], "claude-sonnet-4-6");
        assert_eq!(v["max_tokens"], 1024);
        assert_eq!(v["stream"], true);
        assert_eq!(v["system"], json!([]));
        assert_eq!(v["messages"][0]["role"], "user");
        assert_eq!(v["messages"][0]["content"][0]["type"], "text");
        // betas 不进 body
        assert!(v.get("betas").is_none());
        // 可选字段缺省全省略
        assert!(v.get("temperature").is_none());
        assert!(v.get("thinking").is_none());
        assert!(v.get("tool_choice").is_none());
    }

    #[test]
    fn cache_control_serializes_5m_and_1h() {
        let cc5 = CacheControl::ephemeral_5m();
        let cc1 = CacheControl::ephemeral_1h();
        assert_eq!(
            serde_json::to_value(&cc5).unwrap(),
            json!({"type": "ephemeral", "ttl": "5m"})
        );
        assert_eq!(
            serde_json::to_value(&cc1).unwrap(),
            json!({"type": "ephemeral", "ttl": "1h"})
        );
        assert_eq!(
            serde_json::to_value(CacheControl::ephemeral_1h_global()).unwrap(),
            json!({"type": "ephemeral", "ttl": "1h", "scope": "global"})
        );
    }

    #[test]
    fn tool_def_with_cache_control() {
        let t = ToolDef {
            name: "Bash".into(),
            description: "run shell".into(),
            input_schema: json!({"type": "object"}),
            cache_control: Some(CacheControl::ephemeral_1h()),
            defer_loading: None,
            strict: None,
        };
        let v = serde_json::to_value(&t).unwrap();
        assert_eq!(v["name"], "Bash");
        assert_eq!(v["cache_control"]["type"], "ephemeral");
        assert!(v.get("defer_loading").is_none());
    }

    #[test]
    fn thinking_enabled_with_budget() {
        let t = ThinkingConfig::Enabled {
            budget_tokens: 8000,
        };
        assert_eq!(
            serde_json::to_value(&t).unwrap(),
            json!({"type": "enabled", "budget_tokens": 8000})
        );
    }

    #[test]
    fn tool_choice_variants() {
        assert_eq!(
            serde_json::to_value(ToolChoice::Auto {
                disable_parallel_tool_use: None
            })
            .unwrap(),
            json!({"type": "auto"})
        );
        assert_eq!(
            serde_json::to_value(ToolChoice::Tool {
                name: "Read".into()
            })
            .unwrap(),
            json!({"type": "tool", "name": "Read"})
        );
        assert_eq!(
            serde_json::to_value(ToolChoice::None).unwrap(),
            json!({"type": "none"})
        );
    }

    #[test]
    fn empty_stop_sequences_omitted() {
        let req = MessagesRequest::minimal("m", "u");
        let v = serde_json::to_value(&req).unwrap();
        assert!(v.get("stop_sequences").is_none());
    }

    #[test]
    fn thinking_disabled_serializes() {
        let t = ThinkingConfig::Disabled;
        assert_eq!(
            serde_json::to_value(&t).unwrap(),
            json!({"type": "disabled"})
        );
    }

    /// Full request snapshot — system blocks with cache breakpoints, tools,
    /// messages, and thinking enabled. Verifies the wire shape matches the
    /// Anthropic Messages API contract.
    #[test]
    fn full_request_snapshot() {
        let req = MessagesRequest {
            model: "claude-sonnet-4-6".into(),
            max_tokens: 4096,
            system: vec![
                SystemBlock::text_cached("Core instructions", CacheControl::ephemeral_5m()),
                SystemBlock::text("Dynamic context"),
            ],
            messages: vec![MessageParam {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "hello".into(),
                    cache_control: None,
                }],
            }],
            tools: vec![ToolDef {
                name: "Bash".into(),
                description: "Execute shell commands".into(),
                input_schema: json!({"type": "object", "properties": {"cmd": {"type": "string"}}}),
                cache_control: Some(CacheControl::ephemeral_5m()),
                defer_loading: None,
                strict: None,
            }],
            anthropic_tools: vec![BuiltinTool::WebSearch {
                name: "web_search".into(),
                allowed_domains: None,
                blocked_domains: None,
                max_uses: Some(8),
            }],
            tool_choice: Some(ToolChoice::Auto {
                disable_parallel_tool_use: None,
            }),
            stream: true,
            thinking: Some(ThinkingConfig::Enabled {
                budget_tokens: 8000,
            }),
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: vec![],
            metadata: None,
            betas: vec![],
            speed: None,
        };

        let v = serde_json::to_value(&req).unwrap();
        let obj = v.as_object().unwrap();

        // --- Top-level fields ---
        assert_eq!(obj["model"], "claude-sonnet-4-6");
        assert_eq!(obj["max_tokens"], 4096);
        assert_eq!(obj["stream"], true);

        // --- System blocks (two, first with cache_control) ---
        let sys = obj["system"].as_array().unwrap();
        assert_eq!(sys.len(), 2);
        assert_eq!(sys[0]["type"], "text");
        assert_eq!(sys[0]["text"], "Core instructions");
        assert_eq!(sys[0]["cache_control"]["type"], "ephemeral");
        assert_eq!(sys[0]["cache_control"]["ttl"], "5m");
        assert_eq!(sys[1]["type"], "text");
        assert_eq!(sys[1]["text"], "Dynamic context");
        // Second block has no cache_control
        assert!(sys[1].get("cache_control").is_none());

        // --- Messages ---
        let msgs = obj["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"][0]["type"], "text");
        assert_eq!(msgs[0]["content"][0]["text"], "hello");

        // --- Tools (function tool + built-in web search) ---
        let tools = obj["tools"].as_array().unwrap();
        assert_eq!(
            tools.len(),
            2,
            "tools array should contain both function and built-in tools"
        );
        // Index 0: function tool
        assert_eq!(tools[0]["name"], "Bash");
        assert!(tools[0].get("input_schema").is_some());
        assert_eq!(tools[0]["cache_control"]["type"], "ephemeral");
        // Index 1: built-in web search
        assert_eq!(tools[1]["type"], "web_search_20250305");
        assert_eq!(tools[1]["name"], "web_search");
        assert_eq!(tools[1]["max_uses"], 8);
        assert!(tools[1].get("input_schema").is_none());

        // --- Thinking ---
        assert_eq!(obj["thinking"]["type"], "enabled");
        assert_eq!(obj["thinking"]["budget_tokens"], 8000);

        // --- Optional fields omitted ---
        assert!(obj.get("temperature").is_none());
        assert!(obj.get("top_p").is_none());
        assert!(obj.get("metadata").is_none());
        // betas never appears in body
        assert!(obj.get("betas").is_none());
        // empty stop_sequences is omitted
        assert!(obj.get("stop_sequences").is_none());

        // --- tool_choice ---
        assert_eq!(obj["tool_choice"]["type"], "auto");
    }

    // ── N-10: automatic prompt-cache breakpoints ──
    //
    // These assert on the *emitted JSON*, not on the Rust value, because the
    // messages breakpoint only exists in the serialized form (see the block
    // comment on `MessagesRequest`'s breakpoint helpers).

    fn tool(name: &str) -> ToolDef {
        ToolDef {
            name: name.into(),
            description: "t".into(),
            input_schema: json!({"type": "object"}),
            cache_control: None,
            defer_loading: None,
            strict: None,
        }
    }

    fn text_msg(role: Role, text: &str) -> MessageParam {
        MessageParam {
            role,
            content: vec![ContentBlock::Text {
                text: text.into(),
                cache_control: None,
            }],
        }
    }

    /// Every `cache_control` in the request, as `(location, ttl)` pairs.
    /// Counting these is the whole point: more than four is a 400.
    fn breakpoints(v: &Value) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for (i, b) in v["system"].as_array().into_iter().flatten().enumerate() {
            if let Some(cc) = b.get("cache_control") {
                out.push((
                    format!("system[{i}]"),
                    cc["ttl"].as_str().unwrap_or("").into(),
                ));
            }
        }
        for (i, t) in v["tools"].as_array().into_iter().flatten().enumerate() {
            if let Some(cc) = t.get("cache_control") {
                out.push((
                    format!("tools[{i}]"),
                    cc["ttl"].as_str().unwrap_or("").into(),
                ));
            }
        }
        for (i, m) in v["messages"].as_array().into_iter().flatten().enumerate() {
            for (j, b) in m["content"].as_array().into_iter().flatten().enumerate() {
                if let Some(cc) = b.get("cache_control") {
                    out.push((
                        format!("messages[{i}].content[{j}]"),
                        cc["ttl"].as_str().unwrap_or("").into(),
                    ));
                }
            }
        }
        out
    }

    fn request_with(
        system: Vec<SystemBlock>,
        tools: Vec<ToolDef>,
        messages: Vec<MessageParam>,
    ) -> MessagesRequest {
        MessagesRequest {
            system,
            tools,
            messages,
            ..MessagesRequest::minimal("m", "ignored")
        }
    }

    /// The core of N-10: the last tool gets a breakpoint so the whole
    /// tool-definition prefix is cached, and the *second-to-last* user message
    /// gets the rolling conversation breakpoint. The last user message must
    /// NOT be marked — it is new this turn, so a breakpoint there could only
    /// ever be written, never read.
    #[test]
    fn tools_tail_and_second_to_last_user_message_get_breakpoints() {
        let req = request_with(
            vec![],
            vec![tool("Bash"), tool("Read"), tool("Write")],
            vec![
                text_msg(Role::User, "first"),
                text_msg(Role::Assistant, "ok"),
                text_msg(Role::User, "second"),
                text_msg(Role::Assistant, "ok"),
                text_msg(Role::User, "third — this turn's message"),
            ],
        );
        let v = serde_json::to_value(&req).unwrap();

        assert_eq!(
            breakpoints(&v),
            vec![
                // last tool, 1h — the tool array is fixed for the session
                ("tools[2]".to_string(), "1h".to_string()),
                // messages[2] is the second-to-last *user* message
                ("messages[2].content[0]".to_string(), "5m".to_string()),
            ],
            "got {v:#}"
        );
    }

    /// A tool-use turn's user message is all `tool_result` blocks, which
    /// `base::message::ContentBlock` cannot carry a `cache_control` field on.
    /// The marker still has to land there — that is where the tokens are —
    /// which is why it is injected into the serialized JSON.
    #[test]
    fn breakpoint_lands_on_a_tool_result_block() {
        use base::message::ToolResultContent;
        let tool_results = MessageParam {
            role: Role::User,
            content: vec![
                ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: ToolResultContent::text("a"),
                    is_error: false,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "t2".into(),
                    content: ToolResultContent::text("b"),
                    is_error: false,
                },
            ],
        };
        let req = request_with(
            vec![],
            vec![],
            vec![
                text_msg(Role::User, "go"),
                text_msg(Role::Assistant, "calling tools"),
                tool_results,
                text_msg(Role::Assistant, "done"),
                text_msg(Role::User, "thanks"),
            ],
        );
        let v = serde_json::to_value(&req).unwrap();

        // messages[2] is the second-to-last user message; the marker goes on
        // its *last* block so the whole message is inside the cached prefix.
        assert_eq!(
            breakpoints(&v),
            vec![("messages[2].content[1]".to_string(), "5m".to_string())],
            "got {v:#}"
        );
        assert_eq!(v["messages"][2]["content"][1]["type"], "tool_result");
    }

    /// The budget is shared across system + tools + messages. Four system
    /// blocks already exhaust it, so nothing else may be added — a fifth
    /// breakpoint is a 400.
    #[test]
    fn system_breakpoints_consume_the_budget_first() {
        let system = (0..MAX_CACHE_BREAKPOINTS)
            .map(|i| SystemBlock::text_cached(format!("block {i}"), CacheControl::ephemeral_1h()))
            .collect();
        let req = request_with(
            system,
            vec![tool("Bash")],
            vec![
                text_msg(Role::User, "a"),
                text_msg(Role::Assistant, "b"),
                text_msg(Role::User, "c"),
            ],
        );
        let v = serde_json::to_value(&req).unwrap();

        let bps = breakpoints(&v);
        assert_eq!(bps.len(), MAX_CACHE_BREAKPOINTS, "got {bps:?}");
        assert!(
            bps.iter().all(|(loc, _)| loc.starts_with("system")),
            "no budget left for tools/messages, got {bps:?}"
        );
    }

    /// With three system breakpoints there is exactly one slot left. Tools win
    /// it: the tool prefix is rendered ahead of `system`, so it is the only
    /// breakpoint that still hits when the (runtime-state-carrying) system
    /// prompt changes between turns.
    #[test]
    fn last_budget_slot_goes_to_tools_not_messages() {
        let system = (0..3)
            .map(|i| SystemBlock::text_cached(format!("block {i}"), CacheControl::ephemeral_1h()))
            .collect();
        let req = request_with(
            system,
            vec![tool("Bash")],
            vec![
                text_msg(Role::User, "a"),
                text_msg(Role::Assistant, "b"),
                text_msg(Role::User, "c"),
            ],
        );
        let v = serde_json::to_value(&req).unwrap();

        let bps = breakpoints(&v);
        assert_eq!(bps.len(), MAX_CACHE_BREAKPOINTS);
        assert_eq!(
            bps[3],
            ("tools[0]".to_string(), "1h".to_string()),
            "got {bps:?}"
        );
    }

    /// An explicit tool breakpoint means the caller has made a placement
    /// decision; don't second-guess it by adding another.
    #[test]
    fn explicit_tool_breakpoint_suppresses_the_automatic_one() {
        let mut first = tool("Bash");
        first.cache_control = Some(CacheControl::ephemeral_5m());
        let req = request_with(vec![], vec![first, tool("Read")], vec![]);
        let v = serde_json::to_value(&req).unwrap();

        assert_eq!(
            breakpoints(&v),
            vec![("tools[0]".to_string(), "5m".to_string())],
            "got {v:#}"
        );
    }

    /// A single-turn request (the very first message of a session, or a
    /// one-shot classifier call) has no stable conversation prefix to cache —
    /// marking the only user message would be a write nothing reads.
    #[test]
    fn single_user_turn_gets_no_message_breakpoint() {
        let req = request_with(vec![], vec![], vec![text_msg(Role::User, "hello")]);
        let v = serde_json::to_value(&req).unwrap();
        assert!(breakpoints(&v).is_empty(), "got {v:#}");
    }

    /// Empty tool array ⇒ no tool breakpoint (there is no entry to put it on,
    /// and an empty `tools: []` prefix is worth nothing).
    #[test]
    fn empty_tools_get_no_breakpoint() {
        let req = request_with(vec![], vec![], vec![]);
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["tools"], json!([]));
        assert!(breakpoints(&v).is_empty());
    }

    /// The built-in tool array participates too: `anthropic_tools` are merged
    /// into the same JSON array, so the breakpoint belongs on whatever ends up
    /// last, built-in or not.
    #[test]
    fn breakpoint_goes_on_the_merged_tail_including_builtins() {
        let req = MessagesRequest {
            tools: vec![tool("Bash")],
            anthropic_tools: vec![BuiltinTool::WebSearch {
                name: "web_search".into(),
                allowed_domains: None,
                blocked_domains: None,
                max_uses: None,
            }],
            ..MessagesRequest::minimal("m", "hi")
        };
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(
            breakpoints(&v),
            vec![("tools[1]".to_string(), "1h".to_string())],
            "got {v:#}"
        );
        assert_eq!(v["tools"][1]["type"], "web_search_20250305");
    }

    /// The 1h TTL used by the tool breakpoint drags in the beta flag that
    /// gates it, and doing so twice must not duplicate the header value.
    #[test]
    fn ensure_cache_ttl_beta_is_added_once_when_1h_is_used() {
        let mut req = MessagesRequest {
            tools: vec![tool("Bash")],
            ..MessagesRequest::minimal("m", "hi")
        };
        assert!(req.uses_1h_cache_ttl());
        req.ensure_cache_ttl_beta();
        req.ensure_cache_ttl_beta();
        assert_eq!(req.betas, vec![EXTENDED_CACHE_TTL_BETA.to_string()]);
    }

    /// No 1h TTL anywhere (no tools, no cached system blocks) ⇒ no beta flag.
    /// The 5m messages breakpoint does not need it.
    #[test]
    fn no_beta_flag_without_a_1h_ttl() {
        let mut req = request_with(
            vec![SystemBlock::text("plain")],
            vec![],
            vec![
                text_msg(Role::User, "a"),
                text_msg(Role::Assistant, "b"),
                text_msg(Role::User, "c"),
            ],
        );
        assert!(!req.uses_1h_cache_ttl());
        req.ensure_cache_ttl_beta();
        assert!(req.betas.is_empty());
    }
}
