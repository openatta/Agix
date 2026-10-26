//! FullCompact — LLM-written structured summary of the conversation that is
//! about to be discarded.
//!
//! # Why this exists
//!
//! Every other strategy in this crate is lossy without a handoff. `Snip` drops
//! the oldest API rounds outright — no summary, no note that anything was
//! removed. `MicroCompact` blanks old tool results. `CollapseContext`, the last
//! resort, truncates each text block to 200 bytes and joins them with `" | "`,
//! which produces noise rather than a summary. Any long session therefore lost
//! its own history silently.
//!
//! # Where it sits in the cascade
//!
//! **First**, ahead of Snip — see the module header of [`crate::compact`].
//! Placement is forced by the requirement that the summary describe what is
//! being thrown away: a summarizer that ran *after* Snip could only ever
//! summarize the rounds Snip chose to keep, which are the rounds still in
//! context and the ones least in need of summarizing. Summarize, then drop.
//!
//! # Why it is not unconditional
//!
//! It costs a real (if cheap-routed) API round-trip and adds latency to the
//! user's turn, so [`should_summarize`] gates it on there being enough history
//! at stake to be worth the call — see [`MIN_SUMMARIZED_TOKENS`]. Below that,
//! the plain Snip cascade is both faster and adequate.
//!
//! # Degradation
//!
//! Nothing here is allowed to fail a turn. A model error, a malformed/empty
//! response, or a timeout all surface as `Err(CompactError)`, and the caller
//! falls through to the LLM-free cascade exactly as if no summarizer had been
//! configured.

use std::sync::Arc;
use std::time::Duration;

use base::interface::model::{
    MessageRole, Model, ModelContentBlock, ModelEvent, ModelMessage, StreamParams,
};
use base::interface::settings::ThinkingMode;
use base::text::truncate_at_char_boundary;
use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::compact::{CompactError, CompactResult, CompactStrategy, SnipProjection};
use crate::grouping::{estimate_tokens, group_by_api_round};

// ── Tuning ──

/// Output cap for the summary itself. The prompt asks for prose under nine
/// headings; ~4k tokens is generous for that and bounds what re-enters context.
pub const DEFAULT_SUMMARY_MAX_TOKENS: u32 = 4_096;

/// Wall-clock cap on the summarization call. Compaction happens inline, between
/// the user's message and the model's first token, so an unbounded wait here is
/// an unbounded stall for the user. On expiry we fall through to Snip.
pub const DEFAULT_SUMMARY_TIMEOUT: Duration = Duration::from_secs(45);

/// Minimum estimated tokens of about-to-be-dropped history required before
/// paying for a summarization call. Below this, whatever Snip discards is small
/// enough that a summary of it would not be worth the latency.
pub const MIN_SUMMARIZED_TOKENS: usize = 2_000;

/// Per-block cap when rendering the transcript into the summarization prompt.
/// The material being summarized is by definition large (that is why we are
/// compacting), and a single 50 KB tool result would otherwise crowd out the
/// conversational content that actually carries intent.
const MAX_RENDERED_BLOCK_CHARS: usize = 2_000;

/// Overall cap on the rendered transcript handed to the summarizer. When the
/// history exceeds it, the *oldest* material is dropped from the render —
/// recency is what the summary most needs to get right.
const MAX_RENDERED_TRANSCRIPT_CHARS: usize = 120_000;

/// Marker wrapping the summary in the transcript. Stable string — hosts and
/// tests match on it to find the compaction boundary.
pub const COMPACTED_CONTEXT_OPEN: &str = "<compacted-context>";
pub const COMPACTED_CONTEXT_CLOSE: &str = "</compacted-context>";

// ── Prompt ──

/// The structured summary prompt.
///
/// A fixed set of headings the model must fill in, rather than a free-form
/// "summarize this" — a fixed shape is what makes the output usable as a
/// handoff instead of a paraphrase. The headings are adapted to this codebase: §8 covers the
/// skill / deferred-tool / background-task state that Agix's post-compact
/// recovery also re-attaches, so the summary and the recovery attachments
/// describe the same world.
const SUMMARY_PROMPT: &str = "\
You are compacting an AI coding agent's conversation. The turns below are about \
to be removed from the agent's context permanently; your summary is the only \
thing that will survive to tell it what happened.

Write a summary under exactly these nine headings, in this order. Use a heading \
even when you have nothing for it — write \"None.\" underneath. Be specific: \
name real files, real identifiers, real commands, real error text. Do not \
editorialize, do not praise, do not speculate about what the user \"probably\" \
wants.

1. User intent — what the user asked for, in their own framing, including any \
later corrections or changes of direction. Quote short decisive phrases.
2. Key technical context — the concepts, architecture, constraints and \
conventions established so far that the agent must not re-derive or contradict.
3. Files and symbols — every file read, created or modified, with absolute paths \
where known, and for each one what it contains or what changed. Include exact \
code snippets only where the precise text is load-bearing.
4. Errors and fixes — failures encountered (compiler errors, test failures, \
runtime errors, tool errors), what caused each, and how it was resolved. Note \
any that are still unresolved.
5. Problem solving — approaches tried and rejected, and why, so the agent does \
not retry a dead end.
6. User instructions and constraints — standing rules the user gave (scope \
limits, files not to touch, style requirements, commit conventions). These bind \
future work; state them verbatim where wording matters.
7. Pending work — everything explicitly asked for that is not yet done.
8. Environment state — skills invoked, tools activated, background tasks still \
running, working directory, branch, and any long-lived shell/process state.
9. Current work and next step — what was happening at the moment of \
compaction, and the single most immediate next action. If the next step is not \
clearly implied by the user's request, say so rather than inventing one.

Output only the summary. No preamble, no closing remarks.";

// ── Summarizer ──

/// Wraps a [`Model`] with the summarization prompt, output cap and timeout.
///
/// The model is supplied by the caller, which routes it through the task-model
/// router (`TaskRouter::model_for("compact")`) so summarization can run on a
/// cheaper/faster model than the conversation — the same mechanism post-turn
/// memory extraction uses for `"memory"`.
pub struct LlmSummarizer {
    model: Arc<dyn Model>,
    model_name: String,
    max_tokens: u32,
    timeout: Duration,
    /// The session whose conversation is being summarized. Carried so the call
    /// is filed under that session rather than under a shared name that every
    /// other session's auxiliary calls also write to.
    session_id: Option<String>,
}

impl LlmSummarizer {
    pub fn new(model: Arc<dyn Model>, model_name: impl Into<String>) -> Self {
        Self {
            model,
            model_name: model_name.into(),
            max_tokens: DEFAULT_SUMMARY_MAX_TOKENS,
            timeout: DEFAULT_SUMMARY_TIMEOUT,
            session_id: None,
        }
    }

    pub fn for_session(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    /// Produce a structured summary of `messages`.
    ///
    /// Errors (transport failure, timeout, empty response) are the caller's cue
    /// to fall through to the non-LLM cascade — they are never fatal.
    pub async fn summarize(&self, messages: &[ModelMessage]) -> Result<String, CompactError> {
        let transcript = render_transcript(messages);
        if transcript.trim().is_empty() {
            return Err(CompactError::NotApplicable(
                "no summarizable content in the rounds being dropped".into(),
            ));
        }

        let request = vec![ModelMessage {
            role: MessageRole::User,
            content: vec![
                ModelContentBlock::Text {
                    text: SUMMARY_PROMPT.to_string(),
                },
                ModelContentBlock::Text {
                    text: format!(
                        "<conversation-to-summarize>\n{transcript}\n</conversation-to-summarize>"
                    ),
                },
            ],
        }];
        let params = StreamParams {
            model: self.model_name.clone(),
            max_tokens: self.max_tokens,
            thinking_mode: ThinkingMode::Off,
            fallback_model: None,
            cache_edits: vec![],
            origin: self.session_id.as_ref().map(|id| {
                base::interface::model::CallOrigin::auxiliary(
                    id,
                    base::interface::model::call_purpose::COMPACT,
                )
            }),
            input_map: None,
        };

        // The cancel token is dropped with the future on timeout, which is what
        // signals the in-flight request to stop rather than leaking it.
        let cancel = CancellationToken::new();
        let call = async {
            let mut stream = self
                .model
                .stream(vec![], vec![], request, params, cancel.clone())
                .await
                .map_err(|e| CompactError::Internal(format!("summarization call failed: {e}")))?;
            let mut out = String::new();
            while let Some(event) = stream.next().await {
                match event {
                    Ok(ModelEvent::TextDelta { text }) => out.push_str(&text),
                    // Thinking is not part of the summary; other events carry
                    // nothing we need here.
                    Ok(_) => {}
                    Err(e) => {
                        return Err(CompactError::Internal(format!(
                            "summarization stream failed: {e}"
                        )))
                    }
                }
            }
            Ok(out)
        };

        let summary = match tokio::time::timeout(self.timeout, call).await {
            Ok(result) => result?,
            Err(_) => {
                cancel.cancel();
                return Err(CompactError::Internal(format!(
                    "summarization timed out after {:?}",
                    self.timeout
                )));
            }
        };

        let summary = summary.trim().to_string();
        if summary.is_empty() {
            return Err(CompactError::Internal(
                "summarization returned an empty response".into(),
            ));
        }
        Ok(summary)
    }
}

// ── Gating ──

/// Whether an LLM summarization call is worth making for this compaction.
///
/// True only when compaction is genuinely about to *destroy* history:
/// - the conversation is over budget (otherwise nothing is dropped at all), and
/// - there is more than one round to keep back (otherwise there is nothing to
///   summarize), and
/// - the rounds that would be dropped carry at least [`MIN_SUMMARIZED_TOKENS`].
pub fn should_summarize(messages: &[ModelMessage], max_tokens: usize, keep_rounds: usize) -> bool {
    if estimate_tokens(messages) <= max_tokens {
        return false;
    }
    let rounds = group_by_api_round(messages);
    let keep = keep_rounds.clamp(1, rounds.len().max(1));
    if rounds.len() <= keep {
        return false;
    }
    let dropped: usize = rounds[..rounds.len() - keep]
        .iter()
        .map(|r| r.estimated_tokens)
        .sum();
    dropped >= MIN_SUMMARIZED_TOKENS
}

// ── FullCompact ──

/// Summarize the rounds that are about to be dropped, then drop them —
/// replacing them with the summary.
///
/// Returns `Err` when summarization is inapplicable or fails, in which case the
/// caller must fall through to [`crate::compact::DefaultCompactor`].
pub async fn full_compact(
    summarizer: &LlmSummarizer,
    messages: Vec<ModelMessage>,
    max_tokens: usize,
    keep_rounds: usize,
) -> Result<(Vec<ModelMessage>, CompactResult), CompactError> {
    let tokens_before = estimate_tokens(&messages);
    let count_before = messages.len();

    let rounds = group_by_api_round(&messages);
    let keep = keep_rounds.clamp(1, rounds.len().max(1));
    if rounds.len() <= keep {
        return Err(CompactError::NotApplicable(
            "fewer rounds than the keep floor — nothing to summarize".into(),
        ));
    }
    let split = rounds.len() - keep;

    // Summarize BEFORE anything is discarded. If this fails, `messages` is
    // untouched and the caller's fallback sees the original conversation.
    let to_summarize: Vec<ModelMessage> = rounds[..split]
        .iter()
        .flat_map(|r| r.messages.iter().cloned())
        .collect();
    let summary = summarizer.summarize(&to_summarize).await?;

    let mut result = vec![summary_message(&summary)];
    for round in &rounds[split..] {
        result.extend(round.messages.iter().cloned());
    }

    // If the summary plus the kept rounds is still over budget, shed kept
    // rounds from the oldest end — never the summary, and never the newest
    // round. This is Snip's rule, applied to a list whose head is now the
    // handoff rather than raw history.
    let mut kept_rounds = keep;
    while kept_rounds > 1 && estimate_tokens(&result) > max_tokens {
        kept_rounds -= 1;
        result.truncate(1);
        for round in &rounds[rounds.len() - kept_rounds..] {
            result.extend(round.messages.iter().cloned());
        }
    }

    let tokens_after = estimate_tokens(&result);
    let messages_after = result.len();
    let meta = CompactResult {
        strategy: CompactStrategy::FullCompact,
        messages_before: count_before,
        messages_after,
        tokens_before,
        tokens_after,
        projection: Some(SnipProjection {
            dropped_rounds: rounds.len() - kept_rounds,
            dropped_messages: count_before.saturating_sub(messages_after),
            estimated_tokens_saved: tokens_before.saturating_sub(tokens_after),
        }),
    };
    Ok((result, meta))
}

/// Wrap a summary in the transcript message that replaces the dropped rounds.
fn summary_message(summary: &str) -> ModelMessage {
    ModelMessage {
        role: MessageRole::User,
        content: vec![ModelContentBlock::Text {
            text: format!(
                "<system-reminder>\n\
                 The earlier part of this conversation was removed to free context. \
                 It is gone — you cannot scroll back to it. The structured summary \
                 below is all that remains of it; treat it as an accurate record of \
                 what happened and continue from its \"Current work and next step\" \
                 section. Anything not in the summary and not in the messages after \
                 it must be re-established (re-read the file, re-run the command) \
                 rather than assumed.\n\n\
                 {COMPACTED_CONTEXT_OPEN}\n{summary}\n{COMPACTED_CONTEXT_CLOSE}\n\
                 </system-reminder>"
            ),
        }],
    }
}

/// Render messages into the plain-text transcript handed to the summarizer.
///
/// Per-block and overall caps keep a single huge tool result from displacing the
/// conversational content; when the overall cap bites, the *oldest* rendered
/// lines are dropped.
fn render_transcript(messages: &[ModelMessage]) -> String {
    let mut lines: Vec<String> = Vec::new();
    for msg in messages {
        let who = match msg.role {
            MessageRole::User => "User",
            MessageRole::Assistant => "Assistant",
            MessageRole::System => "System",
        };
        for block in &msg.content {
            match block {
                ModelContentBlock::Text { text } => {
                    if text.trim().is_empty() {
                        continue;
                    }
                    lines.push(format!("{who}: {}", cap(text)));
                }
                // Thinking is the model's scratch work, not a record of what
                // happened; it is also the bulkiest thing in a long transcript.
                ModelContentBlock::Thinking { .. } | ModelContentBlock::RedactedThinking { .. } => {
                }
                ModelContentBlock::Image { media_type, .. } => {
                    lines.push(format!("{who}: [image: {media_type}]"));
                }
                ModelContentBlock::ToolUse { name, input, .. } => {
                    lines.push(format!("{who} called {name}({})", cap(&input.to_string())));
                }
                ModelContentBlock::ToolResult {
                    content, is_error, ..
                } => {
                    let tag = if is_error == &Some(true) {
                        "tool error"
                    } else {
                        "tool result"
                    };
                    lines.push(format!("[{tag}] {}", cap(content)));
                }
            }
        }
    }

    // Trim from the front until within the overall cap.
    let mut total: usize = lines.iter().map(|l| l.len() + 1).sum();
    let mut start = 0usize;
    while total > MAX_RENDERED_TRANSCRIPT_CHARS && start < lines.len() {
        total -= lines[start].len() + 1;
        start += 1;
    }
    lines[start..].join("\n")
}

/// Truncate a rendered block at a character boundary. Byte slicing here would
/// panic on any non-ASCII transcript.
fn cap(s: &str) -> String {
    if s.len() <= MAX_RENDERED_BLOCK_CHARS {
        return s.to_string();
    }
    format!(
        "{}… [{} bytes truncated]",
        truncate_at_char_boundary(s, MAX_RENDERED_BLOCK_CHARS),
        s.len() - MAX_RENDERED_BLOCK_CHARS
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compact::Compactor;
    use base::interface::model::{ModelError, ModelStream, ToolDef};
    use base::interface::prompt::PromptBlock;
    use base::provider::ApiType;
    use std::sync::Mutex;

    /// A `Model` that returns a canned summary and records what it was asked to
    /// summarize.
    struct FakeSummarizerModel {
        reply: String,
        seen: Mutex<Vec<String>>,
        seen_params: Mutex<Option<StreamParams>>,
    }

    impl FakeSummarizerModel {
        fn new(reply: &str) -> Arc<Self> {
            Arc::new(Self {
                reply: reply.to_string(),
                seen: Mutex::new(Vec::new()),
                seen_params: Mutex::new(None),
            })
        }
        fn prompt_text(&self) -> String {
            self.seen.lock().unwrap().join("\n")
        }
    }

    #[async_trait::async_trait]
    impl Model for FakeSummarizerModel {
        fn api_type(&self) -> ApiType {
            ApiType::Anthropic
        }
        async fn stream(
            &self,
            _prompt_blocks: Vec<PromptBlock>,
            _tools: Vec<ToolDef>,
            messages: Vec<ModelMessage>,
            params: StreamParams,
            _cancel: CancellationToken,
        ) -> Result<ModelStream, ModelError> {
            let mut seen = self.seen.lock().unwrap();
            for m in &messages {
                for b in &m.content {
                    if let ModelContentBlock::Text { text } = b {
                        seen.push(text.clone());
                    }
                }
            }
            *self.seen_params.lock().unwrap() = Some(params);
            let events = vec![Ok(ModelEvent::TextDelta {
                text: self.reply.clone(),
            })];
            Ok(Box::new(futures::stream::iter(events)))
        }
    }

    /// A `Model` whose `stream()` always fails.
    struct FailingModel;

    #[async_trait::async_trait]
    impl Model for FailingModel {
        fn api_type(&self) -> ApiType {
            ApiType::Anthropic
        }
        async fn stream(
            &self,
            _: Vec<PromptBlock>,
            _: Vec<ToolDef>,
            _: Vec<ModelMessage>,
            _: StreamParams,
            _: CancellationToken,
        ) -> Result<ModelStream, ModelError> {
            Err(ModelError::Overloaded)
        }
    }

    /// A `Model` that never responds — exercises the timeout path.
    struct HangingModel;

    #[async_trait::async_trait]
    impl Model for HangingModel {
        fn api_type(&self) -> ApiType {
            ApiType::Anthropic
        }
        async fn stream(
            &self,
            _: Vec<PromptBlock>,
            _: Vec<ToolDef>,
            _: Vec<ModelMessage>,
            _: StreamParams,
            _: CancellationToken,
        ) -> Result<ModelStream, ModelError> {
            // Longer than any test timeout; the timeout must fire first.
            tokio::time::sleep(Duration::from_secs(3600)).await;
            unreachable!("timeout should have fired")
        }
    }

    fn user(text: &str) -> ModelMessage {
        ModelMessage {
            role: MessageRole::User,
            content: vec![ModelContentBlock::Text {
                text: text.to_string(),
            }],
        }
    }
    fn assistant(text: &str) -> ModelMessage {
        ModelMessage {
            role: MessageRole::Assistant,
            content: vec![ModelContentBlock::Text {
                text: text.to_string(),
            }],
        }
    }

    /// Rounds carrying enough tokens to clear `MIN_SUMMARIZED_TOKENS`.
    fn bulky_conversation(rounds: usize) -> Vec<ModelMessage> {
        let filler = "the quick brown fox jumps over the lazy dog. ".repeat(40);
        let mut msgs = Vec::new();
        for i in 0..rounds {
            msgs.push(user(&format!("request {i}: {filler}")));
            msgs.push(assistant(&format!("response {i}: {filler}")));
        }
        msgs
    }

    fn summary_text(messages: &[ModelMessage]) -> String {
        messages
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|b| match b {
                ModelContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn summary_replaces_dropped_rounds_and_keeps_recent_ones() {
        let msgs = bulky_conversation(12);
        let model = FakeSummarizerModel::new("1. User intent — refactor the compaction chain.");
        let summarizer = LlmSummarizer::new(model.clone(), "cheap-model");

        let (result, meta) = full_compact(&summarizer, msgs.clone(), 500, 3)
            .await
            .expect("summarization should succeed");

        assert_eq!(meta.strategy, CompactStrategy::FullCompact);
        // The summary is spliced in at the head, in place of the dropped rounds.
        let head = summary_text(&result[..1]);
        assert!(head.contains(COMPACTED_CONTEXT_OPEN), "got: {head}");
        assert!(head.contains("refactor the compaction chain"));
        // The most recent round survived verbatim.
        let tail = summary_text(&result);
        assert!(tail.contains("request 11"), "newest round must be kept");
        // The oldest rounds are gone from the transcript itself.
        assert!(
            !tail.contains("request 0:"),
            "oldest rounds must have been dropped"
        );
        assert!(meta.messages_after < meta.messages_before);
    }

    #[tokio::test]
    async fn summarization_sees_the_rounds_that_are_about_to_be_dropped() {
        // The whole point of ordering: the model must be shown the OLD rounds,
        // not the surviving ones.
        let msgs = bulky_conversation(10);
        let model = FakeSummarizerModel::new("summary");
        let summarizer = LlmSummarizer::new(model.clone(), "cheap-model");

        full_compact(&summarizer, msgs, 500, 2).await.unwrap();

        let prompt = model.prompt_text();
        assert!(
            prompt.contains("request 0:"),
            "the oldest round must be in the summarization prompt"
        );
        assert!(
            !prompt.contains("request 9:"),
            "kept rounds are not summarized; they stay in context verbatim"
        );
        // Structured multi-section prompt, not a bare "summarize this".
        assert!(prompt.contains("User intent"));
        assert!(prompt.contains("Errors and fixes"));
        assert!(prompt.contains("Current work and next step"));
    }

    #[tokio::test]
    async fn routed_model_name_reaches_stream_params() {
        let msgs = bulky_conversation(8);
        let model = FakeSummarizerModel::new("summary");
        let summarizer = LlmSummarizer::new(model.clone(), "haiku-for-compaction");
        full_compact(&summarizer, msgs, 500, 2).await.unwrap();
        let params = model.seen_params.lock().unwrap().take().unwrap();
        assert_eq!(params.model, "haiku-for-compaction");
    }

    #[tokio::test]
    async fn failing_model_yields_error_without_touching_messages() {
        let msgs = bulky_conversation(8);
        let summarizer = LlmSummarizer::new(Arc::new(FailingModel), "m");
        let err = full_compact(&summarizer, msgs.clone(), 500, 2)
            .await
            .expect_err("a failing model must not produce a compaction");
        assert!(matches!(err, CompactError::Internal(_)), "got {err:?}");

        // And the caller's fallback still has the full conversation to work
        // with — `full_compact` took `messages` by value but the caller keeps
        // its own copy; verify the non-LLM cascade handles it.
        let (fallback, meta) = crate::compact::DefaultCompactor
            .compact(msgs, 500, 2)
            .await
            .expect("non-LLM cascade must still work");
        assert_ne!(meta.strategy, CompactStrategy::FullCompact);
        assert!(!fallback.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn hanging_model_times_out_rather_than_hanging_the_turn() {
        let msgs = bulky_conversation(8);
        let summarizer =
            LlmSummarizer::new(Arc::new(HangingModel), "m").with_timeout(Duration::from_secs(5));
        let err = full_compact(&summarizer, msgs, 500, 2)
            .await
            .expect_err("must time out");
        let CompactError::Internal(msg) = &err else {
            panic!("expected Internal, got {err:?}");
        };
        assert!(msg.contains("timed out"), "got {msg}");
    }

    #[tokio::test]
    async fn empty_response_is_treated_as_failure() {
        let msgs = bulky_conversation(8);
        let model = FakeSummarizerModel::new("   \n  ");
        let summarizer = LlmSummarizer::new(model, "m");
        assert!(full_compact(&summarizer, msgs, 500, 2).await.is_err());
    }

    #[tokio::test]
    async fn declines_when_there_is_nothing_to_drop() {
        let msgs = bulky_conversation(2);
        let model = FakeSummarizerModel::new("summary");
        let summarizer = LlmSummarizer::new(model, "m");
        let err = full_compact(&summarizer, msgs, 500, 5)
            .await
            .expect_err("keep floor covers the whole conversation");
        assert!(matches!(err, CompactError::NotApplicable(_)), "got {err:?}");
    }

    #[test]
    fn gate_is_off_when_under_budget() {
        let msgs = bulky_conversation(12);
        assert!(!should_summarize(&msgs, usize::MAX, 2));
    }

    #[test]
    fn gate_is_off_for_a_trivially_small_history() {
        // Over budget, but the rounds being dropped are worth almost nothing —
        // not worth an API round-trip.
        let msgs = vec![user("hi"), assistant("hello"), user("ok"), assistant("yes")];
        assert!(!should_summarize(&msgs, 1, 1));
    }

    #[test]
    fn gate_is_on_when_real_history_is_about_to_be_destroyed() {
        let msgs = bulky_conversation(20);
        assert!(should_summarize(&msgs, 500, 2));
    }

    #[tokio::test]
    async fn multibyte_conversation_does_not_panic() {
        // Same regression class as `collapse_context_survives_multibyte_text`:
        // every cap in the renderer is a byte budget over arbitrary UTF-8.
        let zh: String = "这是一段用于测试的中文内容".repeat(400);
        let mut msgs = Vec::new();
        for i in 0..8 {
            msgs.push(user(&format!("{i}{zh}")));
            msgs.push(assistant(&zh));
        }
        let model = FakeSummarizerModel::new("摘要");
        let summarizer = LlmSummarizer::new(model, "m");
        let (result, _) = full_compact(&summarizer, msgs, 500, 2)
            .await
            .expect("must not panic on multi-byte text");
        assert!(!result.is_empty());
    }
}
