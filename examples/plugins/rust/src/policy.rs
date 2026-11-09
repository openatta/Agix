//! The interceptions: what sits on the path of a tool call, a tool result, a
//! model call, a memory recall and a user message.
//!
//! These follow the session into its sub-agents, which is what makes them a
//! policy rather than a suggestion: a rule against reading `.secret` files that
//! a delegated agent did not have would be one `Agent` call from bypassed.

use std::sync::Arc;

use async_trait::async_trait;
use base::interface::memory_contracts::{RetrievalHook, RetrievalRequest};
use base::interface::model::{MessageRole, ModelContentBlock, ModelMessage};
use base::interface::model_interceptor::{ModelInterceptor, ModelRequestView};
use base::interface::prompt_middleware::{PromptCall, PromptDecision, PromptMiddleware};
use base::interface::tool_middleware::{
    NextDispatch, ToolAnswer, ToolCall, ToolExec, ToolMiddleware, ToolOutcome,
};
use base::interface::tool_result::{ToolResultDraft, ToolResultTransformer};

use crate::activity::Activity;

pub const SECRET_SUFFIX: &str = ".secret";
const SECRET_TOKEN: &str = "SECRET-";
const REDACTED: &str = "[redacted]";

/// `tool.around`: a call that would touch a `.secret` file never runs.
pub struct SecretFiles {
    activity: Arc<Activity>,
}

impl SecretFiles {
    pub fn new(activity: Arc<Activity>) -> Self {
        Self { activity }
    }
}

/// The arguments that name what a call acts on. Free text — an `Agent` prompt,
/// a commit message — may mention a file without touching it.
const TARGETS: [&str; 4] = ["file_path", "notebook_path", "path", "command"];

fn names_a_secret_file(input: &serde_json::Value) -> bool {
    TARGETS
        .iter()
        .filter_map(|field| input.get(field).and_then(|v| v.as_str()))
        .any(|s| {
            s.split(|c: char| {
                c.is_whitespace()
                    || matches!(
                        c,
                        '"' | '\'' | '`' | '>' | '<' | ';' | '|' | '&' | '(' | ')' | '$' | '='
                    )
            })
            .any(|word| word.ends_with(SECRET_SUFFIX))
        })
}

#[async_trait]
impl ToolMiddleware for SecretFiles {
    async fn around(
        &self,
        call: &ToolCall,
        exec: &mut ToolExec,
        next: NextDispatch<'_>,
    ) -> ToolOutcome {
        if names_a_secret_file(&call.input) {
            Activity::count(&self.activity.refused);
            return Ok(ToolAnswer::failure(format!(
                "{}: files ending in `{SECRET_SUFFIX}` are off limits in this project",
                crate::NAME
            )));
        }
        next.run(call, exec).await
    }
}

/// Every `SECRET-<letters and digits>` in `text` replaced, and how many.
pub fn redact(text: &str) -> (String, usize) {
    let mut out = String::with_capacity(text.len());
    let mut count = 0;
    let mut rest = text;
    while let Some(at) = rest.find(SECRET_TOKEN) {
        out.push_str(&rest[..at]);
        let after = &rest[at + SECRET_TOKEN.len()..];
        let body = after
            .find(|c: char| !c.is_ascii_alphanumeric())
            .unwrap_or(after.len());
        if body == 0 {
            out.push_str(SECRET_TOKEN);
        } else {
            out.push_str(REDACTED);
            count += 1;
        }
        rest = &after[body..];
    }
    out.push_str(rest);
    (out, count)
}

/// `tool.result`: the model never reads a `SECRET-…` token, whichever tool
/// found it.
pub struct RedactSecrets {
    activity: Arc<Activity>,
}

impl RedactSecrets {
    pub fn new(activity: Arc<Activity>) -> Self {
        Self { activity }
    }
}

impl ToolResultTransformer for RedactSecrets {
    fn transform(&self, _call: &ToolCall, draft: &mut ToolResultDraft) {
        let (text, count) = redact(&draft.text);
        if count > 0 {
            draft.text = text;
            for _ in 0..count {
                Activity::count(&self.activity.redacted);
            }
        }
    }
}

/// `model.request` / `model.message`: a ceiling on what one call may spend,
/// and a count of what came back.
pub struct TokenCeiling {
    max_tokens: u32,
    activity: Arc<Activity>,
}

impl TokenCeiling {
    pub fn new(max_tokens: u32, activity: Arc<Activity>) -> Self {
        Self {
            max_tokens,
            activity,
        }
    }
}

impl ModelInterceptor for TokenCeiling {
    fn on_request(&self, request: &mut ModelRequestView) {
        request.params.max_tokens = request.params.max_tokens.min(self.max_tokens);
    }

    fn on_message(&self, message: &mut ModelMessage) {
        let answers = message.role == MessageRole::Assistant
            && message
                .content
                .iter()
                .any(|b| matches!(b, ModelContentBlock::Text { .. }));
        if answers {
            Activity::count(&self.activity.replies);
        }
    }
}

/// `memory.retrieval_hook`: notes marked `draft-` are not recalled, and recall
/// brings back at most five.
pub struct SkipDrafts;

impl RetrievalHook for SkipDrafts {
    fn before_retrieve(&self, request: &mut RetrievalRequest) {
        request.limit = request.limit.min(5);
    }

    fn after_retrieve(&self, _request: &RetrievalRequest, names: &mut Vec<String>) {
        names.retain(|name| !name.starts_with("draft-"));
    }
}

/// `prompt.submit`: `!stamp <text>` is short for asking for the stamp.
pub struct StampShortcut;

pub const SHORTCUT: &str = "!stamp ";

#[async_trait]
impl PromptMiddleware for StampShortcut {
    async fn inspect(&self, call: &PromptCall<'_>) -> PromptDecision {
        let Some(text) = call.content.strip_prefix(SHORTCUT) else {
            return PromptDecision::Continue;
        };
        if text.trim().is_empty() {
            return PromptDecision::Block {
                message: Some(format!("`{}` needs some text to stamp", SHORTCUT.trim())),
            };
        }
        PromptDecision::Rewrite(format!(
            "Call the {} tool with exactly this text: {text:?}. Reply with only what it returns.",
            crate::tool::NAME
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_is_redacted_and_its_prefix_alone_is_not() {
        assert_eq!(
            redact("key SECRET-AB12 and SECRET-CD34."),
            ("key [redacted] and [redacted].".to_string(), 2)
        );
        assert_eq!(redact("SECRET- alone"), ("SECRET- alone".to_string(), 0));
    }

    #[test]
    fn a_secret_file_is_found_in_what_a_call_acts_on() {
        assert!(names_a_secret_file(
            &serde_json::json!({"file_path": "/p/keys.secret"})
        ));
        assert!(names_a_secret_file(
            &serde_json::json!({"command": "cat 'keys.secret' | head"})
        ));
        for command in [
            "cat keys.secret;",
            "cat keys.secret|head",
            "cat keys.secret&&ls",
            "echo $(cat keys.secret)",
            "X=keys.secret",
        ] {
            assert!(
                names_a_secret_file(&serde_json::json!({ "command": command })),
                "{command}"
            );
        }
        assert!(!names_a_secret_file(
            &serde_json::json!({"file_path": "/p/secret.txt"})
        ));
        assert!(!names_a_secret_file(
            &serde_json::json!({"prompt": "audit keys.secret"})
        ));
    }

    #[test]
    fn recall_leaves_drafts_out_and_asks_for_at_most_five() {
        let mut request = RetrievalRequest {
            query: "q".into(),
            limit: 20,
            already_surfaced: Default::default(),
            recent_tools: Vec::new(),
            model_name: "m".into(),
            session_id: None,
        };
        SkipDrafts.before_retrieve(&mut request);
        assert_eq!(request.limit, 5);
        let mut names = vec!["deploy".to_string(), "draft-deploy".to_string()];
        SkipDrafts.after_retrieve(&request, &mut names);
        assert_eq!(names, ["deploy"]);
    }
}
