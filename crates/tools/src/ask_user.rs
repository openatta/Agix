//! `AskUserQuestionTool` —— 让模型主动向用户提问。
//!
//! 模型显式调用此工具以向用户提问。问题经会话的 `Elicitation` 送到宿主，
//! 等待时长同权限提问（`permission_prompt_timeout_secs`，0 为不限）；
//! 回答作为工具结果交还模型，没人回答也照实告诉模型。

use async_trait::async_trait;
use base::error::ToolError;
use base::interface::elicitation::{ElicitKind, ElicitOption, ElicitOutcome, ElicitRequest};
use base::tool::{
    PermissionDecision, ProgressSender, PromptContext, Tool, ToolContext, ToolResult,
    ValidationResult,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AskUserQuestionInput {
    /// The question to put to the user
    pub question: String,
    /// Short label for the question (UI header, max 12 chars)
    #[serde(default)]
    pub header: Option<String>,
    /// Multiple-choice options. If empty, free-form text answer.
    #[serde(default)]
    pub options: Vec<AskUserOption>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AskUserOption {
    /// Short key (1-5 chars; what the user types)
    pub key: String,
    /// Human-readable label
    pub label: String,
}

pub struct AskUserQuestionTool;

#[async_trait]
impl Tool for AskUserQuestionTool {
    fn name(&self) -> &str {
        "AskUserQuestion"
    }

    fn description(&self) -> &str {
        "Ask the user a question with multiple-choice options."
    }

    /// **P3f **: deferred -- only Bash/Read/Edit/ToolSearch 4 eager.
    /// Other tools activated via ToolSearch, saving ~13KB tools schema.
    fn input_schema(&self) -> Value {
        crate::input_schema::<AskUserQuestionInput>()
    }
    async fn prompt(&self, _: &PromptContext) -> String {
        include_str!("prompts/coding/ask_user.prompt.md").to_string()
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        false
    }
    fn is_read_only(&self, _: &Value) -> bool {
        true
    }
    async fn validate_input(&self, input: &Value, _: &ToolContext) -> ValidationResult {
        match serde_json::from_value::<AskUserQuestionInput>(input.clone()) {
            Ok(p) if p.question.trim().is_empty() => {
                ValidationResult::err("question must not be empty", 1)
            }
            Ok(p) if p.options.iter().any(|o| o.key.trim().is_empty()) => {
                ValidationResult::err("option key must not be empty", 2)
            }
            Ok(_) => ValidationResult::Ok,
            Err(e) => ValidationResult::err(format!("invalid input: {e}"), 3),
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
        let input: AskUserQuestionInput = serde_json::from_value(input)?;
        // The question, in the shape a host that renders its own dialog wants.
        // It rides on the result either way, answered or not.
        let rendered = json!({
            "question": input.question,
            "header": input.header,
            "options": input.options.iter().map(|o| json!({"key": o.key, "label": o.label})).collect::<Vec<_>>(),
        });

        let asked = match &ctx.elicitation {
            Some(asker) => Some(
                asker.ask(ElicitRequest {
                    id: ctx.tool_use_id.clone(),
                    kind: ElicitKind::Clarification {
                        header: input.header.clone(),
                        tool_use_id: Some(ctx.tool_use_id.clone()),
                    },
                    message: input.question.clone(),
                    options: input
                        .options
                        .iter()
                        .map(|o| ElicitOption {
                            key: o.key.clone(),
                            label: o.label.clone(),
                        })
                        .collect(),
                }),
            ),
            None => None,
        };
        let outcome = match asked {
            Some(ask) => match ctx.config.permission_prompt_timeout_secs {
                0 => ask.await,
                secs => tokio::time::timeout(Duration::from_secs(secs), ask)
                    .await
                    .unwrap_or_else(|_| {
                        ElicitOutcome::declined(format!("the user did not answer within {secs}s"))
                    }),
            },
            None => ElicitOutcome::declined(
                "this engine was built with no way to reach the user, so the question \
                 was not asked",
            ),
        };

        // An unanswered question must not read as an answered one. Handing the
        // model back its own question invites it to treat the echo as a reply
        // and carry on as though the user had spoken.
        let text = match &outcome {
            ElicitOutcome::Answered(v) => {
                let answer = v
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| v.to_string());
                match input.options.iter().find(|o| o.key == answer) {
                    Some(picked) => format!("{}: {}", picked.key, picked.label),
                    None => answer,
                }
            }
            ElicitOutcome::Declined { reason } => {
                format!("The user has not answered: {reason}")
            }
        };

        Ok(ToolResult {
            content: base::tool::ToolResultContent::Text(text),
            is_error: false,
            structured_content: Some(rendered),
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn validates_empty_question() {
        let tool = AskUserQuestionTool;
        let r = tool
            .validate_input(
                &json!({"question": "  "}),
                &base::tool::ToolContext::for_test("/tmp".into()),
            )
            .await;
        assert!(!matches!(r, ValidationResult::Ok));
    }

    #[tokio::test]
    async fn validates_empty_option_key() {
        let tool = AskUserQuestionTool;
        let r = tool
            .validate_input(
                &json!({"question": "ok?", "options": [{"key": "", "label": "yes"}]}),
                &base::tool::ToolContext::for_test("/tmp".into()),
            )
            .await;
        assert!(!matches!(r, ValidationResult::Ok));
    }

    #[test]
    fn name_matches_ts() {
        assert_eq!(AskUserQuestionTool.name(), "AskUserQuestion");
    }
}

#[cfg(test)]
mod elicitation_tests {
    use super::*;
    use base::interface::elicitation::FixedElicitation;
    use std::sync::Arc;

    fn ctx_with(
        asker: Option<Arc<dyn base::interface::elicitation::Elicitation>>,
    ) -> base::tool::ToolContext {
        let mut ctx = base::tool::ToolContext::for_test("/tmp".into());
        ctx.elicitation = asker;
        ctx
    }

    async fn ask(ctx: base::tool::ToolContext) -> ToolResult {
        AskUserQuestionTool
            .call(
                json!({"question": "which branch?", "options": [{"key": "a", "label": "main"}]}),
                ctx,
                base::tool::ProgressSender::noop("ask"),
            )
            .await
            .expect("the tool itself does not fail")
    }

    /// The whole point of routing this through the contract: with nobody to
    /// ask, the model is told the question went unanswered instead of being
    /// handed its own question back to read as a reply.
    #[tokio::test]
    async fn with_no_one_to_ask_the_model_is_told_so() {
        let result = ask(ctx_with(None)).await;
        let base::tool::ToolResultContent::Text(text) = &result.content else {
            panic!("expected text");
        };
        assert!(
            text.contains("has not answered"),
            "an unanswered question must say so: {text}"
        );
        assert!(
            !text.contains("which branch?"),
            "echoing the question back is what this replaced: {text}"
        );
        assert!(
            result.structured_content.is_some(),
            "a host that renders its own dialog still needs the question"
        );
    }

    #[tokio::test]
    async fn an_answer_reaches_the_model_as_the_result() {
        let asker = FixedElicitation::new(base::interface::elicitation::ElicitOutcome::answered(
            &"main".to_string(),
        ));
        let result = ask(ctx_with(Some(asker.clone()))).await;
        let base::tool::ToolResultContent::Text(text) = &result.content else {
            panic!("expected text");
        };
        assert_eq!(text, "main");

        let asked = asker.asked();
        assert_eq!(asked.len(), 1);
        assert_eq!(asked[0].message, "which branch?");
        assert_eq!(asked[0].options.len(), 1);
    }
}

#[cfg(test)]
mod waiting_tests {
    use super::*;
    use base::interface::elicitation::{Elicitation, FixedElicitation};
    use std::sync::Arc;

    struct NeverAnswers;

    #[async_trait]
    impl Elicitation for NeverAnswers {
        async fn ask(&self, _: ElicitRequest) -> ElicitOutcome {
            std::future::pending().await
        }
    }

    fn ctx_with(asker: Arc<dyn Elicitation>, timeout_secs: u64) -> ToolContext {
        let mut ctx = ToolContext::for_test("/tmp".into());
        ctx.elicitation = Some(asker);
        let mut config = (*ctx.config).clone();
        config.permission_prompt_timeout_secs = timeout_secs;
        ctx.config = Arc::new(config);
        ctx
    }

    async fn ask(ctx: ToolContext) -> String {
        let result = AskUserQuestionTool
            .call(
                json!({"question": "which branch?", "options": [{"key": "a", "label": "main"}]}),
                ctx,
                ProgressSender::noop("ask"),
            )
            .await
            .expect("the tool itself does not fail");
        match result.content {
            base::tool::ToolResultContent::Text(text) => text,
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_picked_option_reaches_the_model_with_its_label() {
        let asker = FixedElicitation::new(ElicitOutcome::answered(&"a".to_string()));
        assert_eq!(ask(ctx_with(asker, 300)).await, "a: main");
    }

    #[tokio::test]
    async fn free_text_reaches_the_model_as_typed() {
        let asker = FixedElicitation::new(ElicitOutcome::answered(&"the dev branch".to_string()));
        assert_eq!(ask(ctx_with(asker, 300)).await, "the dev branch");
    }

    #[tokio::test]
    async fn a_question_nobody_answers_is_given_up_on_after_the_prompt_timeout() {
        let text = tokio::time::timeout(
            Duration::from_secs(10),
            ask(ctx_with(Arc::new(NeverAnswers), 1)),
        )
        .await
        .expect("the configured timeout must end the wait");
        assert!(
            text.contains("has not answered") && text.contains("within 1s"),
            "{text}"
        );
    }
}
