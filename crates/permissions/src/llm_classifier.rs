//! `LlmClassifier` —— LLM-based permission classifier for Auto mode.
//!
//! Uses a lightweight model (Haiku-tier) to evaluate tool calls and
//! classify them as Allow / Deny / AllowWithEdit / Defer with semantic
//! understanding of the tool name, description, and input.
//!
//! Design decisions:
//! - The classifier never `Deny`s operations that rule engine or tool
//!   itself already allowed (those short-circuit before classifier runs).
//! - `Deny` means "the LLM considers this operation dangerous or outside
//!   project scope despite no rule blocking it" — a judgement call.
//! - Results are cached by `(tool_name, sha256(input))` for 5 minutes
//!   to avoid repeated LLM calls for identical inputs.

use crate::gate::{AutoClassifier, ClassifyDecision};
use async_trait::async_trait;
use base::interface::model::{
    MessageRole, Model, ModelContentBlock, ModelEvent, ModelMessage, StreamParams,
};
use base::interface::prompt::{BlockRole, PromptBlock};
use base::interface::settings::ThinkingMode;
use futures::stream::StreamExt;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Instant;
use tokio_util::sync::CancellationToken;

// ── Constants ──

/// System prompt for the classifier model. Instructs it to output
/// structured JSON with no extra text.
const CLASSIFIER_SYSTEM_PROMPT: &str = "\
You are a permission classifier for a coding agent. \
Classify each tool call as one of:

- ALLOW: safe operation that is reversible, within project scope, and \
  doesn't modify sensitive system files or run destructive commands.
- DENY: dangerous or irreversible operation that modifies system files, \
  runs destructive commands (rm -rf, dd, mkfs), or is clearly outside \
  the project scope.
- ALLOW_WITH_EDIT: operation that is fundamentally safe but needs \
  modifications (e.g., a bash command that needs `--dry-run`, a path \
  that should be restricted to the project directory).
- DEFER: operation that needs user judgment — uncertain, potentially \
  ambiguous with no clear safety profile.

Respond with valid JSON only. Do NOT wrap in markdown fences. \
Do NOT include any text before or after the JSON object.";

/// User prompt template. `{tool_name}`, `{tool_input}`, `{cwd}` are
/// substituted at classification time.
const CLASSIFICATION_PROMPT_TEMPLATE: &str = "\
Classify this tool call for a coding agent. Should it be:

- ALLOW: safe, reversible, within project scope
- DENY: dangerous, irreversible, outside scope
- ALLOW_WITH_EDIT: safe with suggested modifications
- DEFER: needs user judgment

Tool: {tool_name}
Tool description: {tool_description}
Input: {tool_input}
Working directory: {cwd}
Permission mode: Auto

Respond with JSON:
{\"decision\": \"...\", \"reason\": \"...\", \"suggested_edits\": \"...\"}

Where decision is one of: ALLOW, DENY, ALLOW_WITH_EDIT, DEFER";

/// Cache TTL in seconds (5 minutes).
const CACHE_TTL_SECS: u64 = 300;

/// Maximum number of cache entries before oldest (expired) entries are evicted.
const MAX_CACHE_ENTRIES: usize = 1000;

// ── Cache types ──

struct CacheEntry {
    decision: ClassifyDecision,
    expires_at: Instant,
}

// ── LlmClassifier ──

/// LLM-based permission classifier.
///
/// Uses a lightweight model (typically Claude Haiku or equivalent) to
/// semantically evaluate tool calls and decide Allow / Deny / AllowWithEdit /
/// Defer. Designed for use in `Auto` permission mode.
///
/// The classifier uses a 5-minute cache keyed by `(tool_name, sha256(input))`
/// to avoid redundant LLM calls for repeated or similar tool calls.
pub struct LlmClassifier {
    /// The underlying LLM model (injected; typically a Haiku-tier instance).
    model: Arc<dyn Model>,
    /// Model name string sent in the API request (e.g. "claude-haiku-4-5").
    model_name: String,
    /// Cache of classification results keyed by hash.
    cache: Mutex<HashMap<String, CacheEntry>>,
    /// When false, `classify()` always returns `Defer`.
    enabled: bool,
    /// The session this classifier serves, so its model call is attributed.
    ///
    /// Carried on the struct rather than passed to `classify()`: a classifier
    /// is built per session, while `AutoClassifier::classify` is a narrow
    /// decision interface that has no business growing a session parameter.
    session_id: Option<String>,
}

impl LlmClassifier {
    /// Construct a new LLM classifier.
    ///
    /// `model` — the injected model instance (typically `Arc<dyn Model>`).
    /// `model_name` — the model identifier sent in API requests (e.g.
    ///   `"claude-haiku-4-5"`). Should be a fast/cheap model.
    pub fn new(model: Arc<dyn Model>, model_name: impl Into<String>) -> Self {
        Self {
            model,
            model_name: model_name.into(),
            cache: Mutex::new(HashMap::new()),
            enabled: true,
            session_id: None,
        }
    }

    /// Attribute this classifier's model calls to `session_id`.
    pub fn for_session(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    /// Set the enabled state. When disabled, all calls return `Defer`.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// Builder-style: set enabled state.
    pub fn with_enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// Returns whether this classifier is currently enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Build a deterministic cache key from tool name + JSON input.
    fn cache_key(tool_name: &str, input: &Value) -> String {
        let input_str = serde_json::to_string(input).unwrap_or_default();
        let mut hasher = Sha256::new();
        hasher.update(tool_name.as_bytes());
        hasher.update(b":");
        hasher.update(input_str.as_bytes());
        let hash = hasher.finalize();
        hex::encode(hash)
    }

    /// Look up a cached classification result.
    fn get_cached(&self, key: &str) -> Option<ClassifyDecision> {
        let cache = self.cache.lock().ok()?;
        if let Some(entry) = cache.get(key) {
            if entry.expires_at > Instant::now() {
                return Some(entry.decision.clone());
            }
        }
        None
    }

    /// Store a classification result in the cache.
    fn set_cache(&self, key: String, decision: ClassifyDecision) {
        if let Ok(mut cache) = self.cache.lock() {
            // Evict expired entries if cache is near capacity
            if cache.len() >= MAX_CACHE_ENTRIES {
                cache.retain(|_, e| e.expires_at > Instant::now());
                // If still full after eviction, clear entirely to avoid O(n) churn
                if cache.len() >= MAX_CACHE_ENTRIES {
                    cache.clear();
                }
            }
            cache.insert(
                key,
                CacheEntry {
                    decision,
                    expires_at: Instant::now() + std::time::Duration::from_secs(CACHE_TTL_SECS),
                },
            );
        }
    }

    /// Extract the first JSON object from a text response.
    ///
    /// Handles responses where the LLM wraps JSON in markdown fences or
    /// includes additional commentary — we find the outermost `{…}` brace
    /// pair and parse only that substring.
    fn extract_json(text: &str) -> Option<Value> {
        let start = text.find('{')?;
        let end = text.rfind('}')?;
        if end <= start {
            return None;
        }
        let json_str = &text[start..=end];
        serde_json::from_str(json_str).ok()
    }

    /// Parse the JSON response into a `ClassifyDecision`.
    ///
    /// Expected JSON shape:
    /// ```json
    /// {
    ///   "decision": "ALLOW" | "DENY" | "ALLOW_WITH_EDIT" | "DEFER",
    ///   "reason": "explanation text",
    ///   "suggested_edits": "only for ALLOW_WITH_EDIT"
    /// }
    /// ```
    fn parse_decision(value: &Value) -> ClassifyDecision {
        let decision = value
            .get("decision")
            .and_then(|v| v.as_str())
            .unwrap_or("DEFER");

        let reason = value
            .get("reason")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        match decision {
            "ALLOW" => ClassifyDecision::Allow {
                reason: if reason.is_empty() {
                    "allowed by LLM classifier".into()
                } else {
                    reason
                },
            },
            "DENY" => {
                let deny_reason = if reason.is_empty() {
                    "denied by LLM classifier".into()
                } else {
                    reason
                };
                ClassifyDecision::Deny {
                    reason: deny_reason,
                }
            }
            "ALLOW_WITH_EDIT" => {
                let edits = value
                    .get("suggested_edits")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                ClassifyDecision::AllowWithEdit {
                    reason: if reason.is_empty() {
                        "allowed with suggested edits".into()
                    } else {
                        reason
                    },
                    suggested_edits: edits,
                }
            }
            // DEFER, or unrecognized — gate will ask the user
            _ => ClassifyDecision::Defer,
        }
    }
}

#[async_trait]
impl AutoClassifier for LlmClassifier {
    /// Classify a tool call.
    ///
    /// Returns `Defer` on any error (model failure, bad JSON, disabled state)
    /// so the gate falls through to asking the user — the safe default.
    async fn classify(
        &self,
        tool_name: &str,
        tool_description: &str,
        input: &Value,
    ) -> ClassifyDecision {
        // Fast path: disabled classifier
        if !self.enabled {
            return ClassifyDecision::Defer;
        }

        // 1. Check cache
        let key = Self::cache_key(tool_name, input);
        if let Some(cached) = self.get_cached(&key) {
            return cached;
        }

        // 2. Build classification prompt
        let input_str = serde_json::to_string_pretty(input).unwrap_or_default();
        let cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();

        let user_prompt = CLASSIFICATION_PROMPT_TEMPLATE
            .replace("{tool_name}", tool_name)
            .replace("{tool_description}", tool_description)
            .replace("{tool_input}", &input_str)
            .replace("{cwd}", &cwd);

        let prompt_blocks = vec![PromptBlock {
            role: BlockRole::System,
            content: CLASSIFIER_SYSTEM_PROMPT.to_string(),
            cache_strategy: None,
            name: Some("permissions.classifier".into()),
            origin: base::prompt::BlockOrigin::Kernel,
        }];

        let user_msg = ModelMessage {
            role: MessageRole::User,
            content: vec![ModelContentBlock::Text { text: user_prompt }],
        };

        let params = StreamParams {
            model: self.model_name.clone(),
            max_tokens: 500,
            thinking_mode: ThinkingMode::Off,
            effort: None,
            fallback_model: None,
            cache_edits: vec![],
            origin: self.session_id.as_ref().map(|id| {
                base::interface::model::CallOrigin::auxiliary(
                    id,
                    base::interface::model::call_purpose::CLASSIFY,
                )
            }),
            input_map: None,
            provider_search: None,
        };

        // 3. Call the model
        let response_text = match self
            .model
            .stream(
                prompt_blocks,
                vec![], // no tools — pure text classification
                vec![user_msg],
                params,
                CancellationToken::new(),
            )
            .await
        {
            Ok(mut stream) => {
                let mut text = String::new();
                while let Some(event) = stream.next().await {
                    match event {
                        Ok(ModelEvent::TextDelta { text: delta }) => {
                            text.push_str(&delta);
                        }
                        Ok(ModelEvent::ContentBlockStart {
                            block: ModelContentBlock::Text { text: t },
                            ..
                        }) => {
                            text.push_str(&t);
                        }
                        Ok(ModelEvent::EndTurn { .. }) => break,
                        Err(_) => break, // stream error → use partial text
                        _ => {}
                    }
                }
                text
            }
            Err(_) => {
                // Model call failed — defer to user judgment
                return ClassifyDecision::Defer;
            }
        };

        // 4. Parse JSON from response
        let decision = if response_text.trim().is_empty() {
            ClassifyDecision::Defer
        } else if let Some(json) = Self::extract_json(&response_text) {
            Self::parse_decision(&json)
        } else {
            // Could not extract valid JSON — defer
            ClassifyDecision::Defer
        };

        // 5. Cache and return
        self.set_cache(key, decision.clone());
        decision
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::interface::model::{
        Model, ModelError, ModelEvent, ModelMessage, StreamParams, ToolDef,
    };
    use base::interface::prompt::PromptBlock;
    use futures::stream;
    use serde_json::json;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    /// A mock model that returns a pre-configured text response.
    struct MockClassifierModel {
        response: String,
    }

    #[async_trait]
    impl Model for MockClassifierModel {
        fn api_type(&self) -> base::provider::ApiType {
            base::provider::ApiType::Anthropic
        }

        async fn stream(
            &self,
            _prompt_blocks: Vec<PromptBlock>,
            _tools: Vec<ToolDef>,
            _messages: Vec<ModelMessage>,
            _params: StreamParams,
            _cancel: CancellationToken,
        ) -> Result<
            Box<dyn futures::Stream<Item = Result<ModelEvent, ModelError>> + Send + Unpin>,
            ModelError,
        > {
            let events: Vec<Result<ModelEvent, ModelError>> = vec![
                Ok(ModelEvent::ContentBlockStart {
                    index: 0,
                    block: ModelContentBlock::Text {
                        text: self.response.clone(),
                    },
                }),
                Ok(ModelEvent::ContentBlockStop { index: 0 }),
                Ok(ModelEvent::EndTurn {
                    stop_reason: "end_turn".into(),
                    usage: base::interface::model::Usage::default(),
                }),
            ];
            Ok(Box::new(stream::iter(events)))
        }
    }

    fn make_classifier(response: &str) -> LlmClassifier {
        let model = Arc::new(MockClassifierModel {
            response: response.to_string(),
        });
        LlmClassifier::new(model, "test-model").with_enabled(true)
    }

    #[tokio::test]
    async fn classify_allow() {
        let c = make_classifier(r#"{"decision":"ALLOW","reason":"safe git operation"}"#);
        let decision = c
            .classify(
                "Bash",
                "Run shell commands",
                &json!({"command": "git status"}),
            )
            .await;
        match decision {
            ClassifyDecision::Allow { reason } => assert!(reason.contains("safe git operation")),
            other => panic!("expected Allow, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn classify_deny() {
        let c = make_classifier(r#"{"decision":"DENY","reason":"destructive command"}"#);
        let decision = c
            .classify(
                "Bash",
                "Run shell commands",
                &json!({"command": "rm -rf /"}),
            )
            .await;
        match decision {
            ClassifyDecision::Deny { reason } => assert!(reason.contains("destructive")),
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn classify_allow_with_edit() {
        let c = make_classifier(
            r#"{"decision":"ALLOW_WITH_EDIT","reason":"safe with dry-run","suggested_edits":"add --dry-run flag"}"#,
        );
        let decision = c
            .classify(
                "Bash",
                "Run shell commands",
                &json!({"command": "rm -rf build/"}),
            )
            .await;
        match decision {
            ClassifyDecision::AllowWithEdit {
                reason,
                suggested_edits,
            } => {
                assert!(reason.contains("dry-run"));
                assert!(suggested_edits.contains("--dry-run"));
            }
            other => panic!("expected AllowWithEdit, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn classify_defer() {
        let c = make_classifier(r#"{"decision":"DEFER","reason":"uncertain"}"#);
        let decision = c
            .classify(
                "Bash",
                "Run shell commands",
                &json!({"command": "some weird command"}),
            )
            .await;
        assert!(matches!(decision, ClassifyDecision::Defer));
    }

    #[tokio::test]
    async fn malformed_json_falls_to_defer() {
        let c = make_classifier("this is not valid JSON at all");
        let decision = c
            .classify(
                "Bash",
                "Run shell commands",
                &json!({"command": "echo hello"}),
            )
            .await;
        assert!(matches!(decision, ClassifyDecision::Defer));
    }

    #[tokio::test]
    async fn disabled_always_defers() {
        let model = Arc::new(MockClassifierModel {
            response: r#"{"decision":"ALLOW","reason":"should not be called"}"#.to_string(),
        });
        let c = LlmClassifier::new(model, "test-model").with_enabled(false);
        let decision = c
            .classify(
                "Bash",
                "Run shell commands",
                &json!({"command": "git status"}),
            )
            .await;
        assert!(matches!(decision, ClassifyDecision::Defer));
    }

    #[tokio::test]
    async fn cache_hit_returns_cached() {
        let model = Arc::new(MockClassifierModel {
            response: r#"{"decision":"ALLOW","reason":"cached"}"#.to_string(),
        });
        let c = LlmClassifier::new(model, "test-model");

        // First call
        let input = json!({"command": "git status"});
        let d1 = c.classify("Bash", "Run shell", &input).await;
        assert!(matches!(d1, ClassifyDecision::Allow { .. }));

        // Second call with same input — should use cache, not re-call model.
        // Since the mock only returns one response, if it were re-called it would
        // still work, but we verify cache behavior by checking the key exists.
        let d2 = c.classify("Bash", "Run shell", &input).await;
        assert!(matches!(d2, ClassifyDecision::Allow { .. }));

        // Verify the cache has the entry
        let key = LlmClassifier::cache_key("Bash", &input);
        let cached = c.get_cached(&key);
        assert!(cached.is_some());
    }

    #[tokio::test]
    async fn different_input_produces_different_cache_key() {
        let _c = make_classifier(r#"{"decision":"ALLOW","reason":"ok"}"#);
        let key1 = LlmClassifier::cache_key("Bash", &json!({"command": "git status"}));
        let key2 = LlmClassifier::cache_key("Bash", &json!({"command": "rm -rf /"}));
        assert_ne!(key1, key2);
    }

    #[tokio::test]
    async fn different_tool_different_cache_key() {
        let _c = make_classifier(r#"{"decision":"ALLOW","reason":"ok"}"#);
        let key1 = LlmClassifier::cache_key("Bash", &json!({"command": "ls"}));
        let key2 = LlmClassifier::cache_key("Read", &json!({"command": "ls"}));
        assert_ne!(key1, key2);
    }

    #[test]
    fn extract_json_from_plain_response() {
        let text = r#"{"decision":"ALLOW","reason":"safe"}"#;
        let parsed = LlmClassifier::extract_json(text);
        assert!(parsed.is_some());
        assert_eq!(
            parsed.unwrap().get("decision").and_then(|v| v.as_str()),
            Some("ALLOW")
        );
    }

    #[test]
    fn extract_json_from_markdown_fence() {
        let text = "Here is my analysis:\n\n```json\n{\"decision\":\"DENY\",\"reason\":\"too dangerous\"}\n```\n\nHope this helps!";
        let parsed = LlmClassifier::extract_json(text);
        assert!(parsed.is_some());
        let v = parsed.unwrap();
        assert_eq!(v.get("decision").and_then(|v| v.as_str()), Some("DENY"));
        assert_eq!(
            v.get("reason").and_then(|v| v.as_str()),
            Some("too dangerous")
        );
    }

    #[test]
    fn extract_json_with_extra_text() {
        let text = "Some text before {\"decision\":\"ALLOW_WITH_EDIT\",\"reason\":\"add flag\",\"suggested_edits\":\"use --dry-run\"} and after";
        let parsed = LlmClassifier::extract_json(text);
        assert!(parsed.is_some());
        let v = parsed.unwrap();
        assert_eq!(
            v.get("decision").and_then(|v| v.as_str()),
            Some("ALLOW_WITH_EDIT")
        );
    }

    #[test]
    fn parse_unknown_decision_falls_to_defer() {
        let v = json!({"decision": "MAYBE", "reason": "unknown"});
        assert!(matches!(
            LlmClassifier::parse_decision(&v),
            ClassifyDecision::Defer
        ));
    }

    #[test]
    fn parse_missing_decision_falls_to_defer() {
        let v = json!({"reason": "no decision field"});
        assert!(matches!(
            LlmClassifier::parse_decision(&v),
            ClassifyDecision::Defer
        ));
    }
}
