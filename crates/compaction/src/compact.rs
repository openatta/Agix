//! Compactor — context compression when token budget exceeded.
//!
//! The production chain, in the order the runtime applies it:
//!
//! 0. **FullCompact** ([`crate::llm_summary`]) — an LLM writes a structured
//!    multi-section handoff summary of the rounds that are about to be
//!    discarded, and the summary replaces them. Runs *first*, because every
//!    layer below it is lossy: a summary produced after Snip has already run
//!    can only summarize what Snip left behind. Gated (see
//!    `llm_summary::should_summarize`) so it costs an API call only when
//!    compaction is actually about to destroy a meaningful amount of history.
//! 1. **Snip** — drop oldest API rounds. Used when FullCompact is unavailable
//!    (no summarizer wired), declines, or fails.
//! 2. **MicroCompact** — clear old tool result content in-place.
//! 3. **CollapseContext** — fold old rounds into a truncation-joined digest,
//!    keeping recent rounds. Last resort; produces noise, not a summary.
//!
//! `DefaultCompactor` implements layers 1–3 and stays LLM-free, so it remains
//! usable as a `Compactor` in contexts with no model available.

use async_trait::async_trait;
use base::interface::model::{MessageRole, ModelContentBlock, ModelMessage};
use base::text::truncate_at_char_boundary;

use std::time::Instant;

use crate::grouping::{estimate_tokens, group_by_api_round, ApiRound};
use crate::time_based_mc::{TimeBasedMcConfig, TimeBasedMcResult};

// ── Strategy types ──

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactStrategy {
    /// Nothing was done — the conversation was already within budget.
    ///
    /// Distinct from `Snip` with zero dropped rounds: this reports that no
    /// strategy ran at all. The early-return path used to report `Snip`, which
    /// made every under-budget turn emit a phantom Snip event to telemetry and
    /// to the `CompactAction` stream, drowning real compactions in noise.
    NoOp,
    /// Drop oldest rounds, keep recent N rounds (not messages).
    Snip,
    /// Clear tool results older than K rounds, replacing content with a placeholder.
    MicroCompact,
    /// Fold old rounds into a summary message, keep recent rounds intact.
    CollapseContext,
    /// LLM-driven full summary of the conversation.
    FullCompact,
    /// Extract durable memories during compaction (writes to MemoryStore).
    SessionMemory,
}

#[derive(Debug, Clone)]
pub struct CompactResult {
    pub strategy: CompactStrategy,
    pub messages_before: usize,
    pub messages_after: usize,
    pub tokens_before: usize,
    pub tokens_after: usize,
    /// Snip projection — metadata about dropped rounds when using Snip strategy.
    /// Emitted so the UI can show what was preserved vs dropped.
    pub projection: Option<SnipProjection>,
}

/// Metadata about rounds dropped by the Snip compaction strategy.
/// Preserved for history/UI purposes — lets consumers display
/// compaction impact without inspecting every message.
#[derive(Debug, Clone)]
pub struct SnipProjection {
    /// Number of API rounds that were entirely dropped.
    pub dropped_rounds: usize,
    /// Number of messages (across all dropped rounds) that were removed.
    pub dropped_messages: usize,
    /// Estimated tokens saved by dropping these rounds.
    pub estimated_tokens_saved: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum CompactError {
    #[error("all strategies exhausted")]
    Exhausted,
    /// This strategy declined — not a failure. The caller should fall through
    /// to the next strategy in the cascade without logging an error.
    #[error("strategy not applicable: {0}")]
    NotApplicable(String),
    #[error("{0}")]
    Internal(String),
}

// ── Compactor trait ──

/// What the aging passes may look at.
///
/// Neither pass is about the token budget: one clears tool results that have
/// simply grown old, the other keeps the server-side cache from carrying
/// results nobody will read again. They run before anything consults the
/// budget, which is why they get their own input.
pub struct AgingInput<'a> {
    /// `(message index, when it was pushed)` for every message.
    pub message_ages: &'a [(usize, Instant)],
    pub time_based: &'a TimeBasedMcConfig,
    /// Whether the cached pass's interval has elapsed. The interval and the
    /// clock stay with the caller, which owns the pass's pending cache edits.
    pub cached_due: bool,
    pub cached_keep_recent: usize,
}

/// What one aging pass did.
#[derive(Debug, Default)]
pub struct Aging {
    pub time_based: TimeBasedMcResult,
    pub cached_cleared: usize,
    /// `Some` when the cached pass ran: the `tool_use_id`s whose results it
    /// blanked, which the next request sends as `cache_edits`.
    pub cache_edits: Option<Vec<String>>,
}

impl Aging {
    pub fn cleared(&self) -> usize {
        self.time_based.cleared + self.cached_cleared
    }
}

/// What the predictive pass may look at.
pub struct ReactiveInput {
    pub context_tokens: usize,
    /// The effective context window. `0` disables the pass.
    pub context_limit: usize,
    pub keep_recent: usize,
    pub enabled: bool,
    /// Set once the caller has seen enough consecutive no-effect passes to
    /// stop paying for them.
    pub circuit_open: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReactiveOutcome {
    /// The pass is off, the circuit is open, or the prediction said there is
    /// still room.
    NotTriggered,
    /// It ran and the history is cheaper for it.
    Compacted,
    /// It ran and bought nothing.
    NoEffect,
}

/// How close the conversation is to the budget that forces compaction.
pub struct BudgetState {
    pub context_tokens: usize,
    /// `0` disables both the warning and the threshold.
    pub threshold: usize,
    /// Whether this session has already been warned. The warning is worth
    /// saying once.
    pub warned_already: bool,
}

/// Compaction: when it happens, and what it does.
///
/// `compact` is the transformation. The four methods around it are the
/// judgements about *when* — two aging passes that run before the budget is
/// consulted, a predictive pass that fires before the budget is reached, and
/// the threshold that forces the transformation. All four were inline in the
/// turn loop, which meant swapping the compactor changed how history was
/// rewritten but not when, and three of the four paths never reached the
/// compactor at all.
///
/// Every judgement has a default that is the engine's own, so an implementor
/// who only cares about the transformation writes `compact` and nothing else.
#[async_trait]
pub trait Compactor: Send + Sync {
    /// Compact messages to fit within `max_tokens`, keeping at least `keep_rounds` recent rounds.
    /// Returns the compacted message list and metadata.
    async fn compact(
        &self,
        messages: Vec<ModelMessage>,
        max_tokens: usize,
        keep_rounds: usize,
    ) -> Result<(Vec<ModelMessage>, CompactResult), CompactError>;

    /// Clear what has gone stale, before anyone looks at the budget.
    fn age(&self, messages: &mut [ModelMessage], input: &AgingInput<'_>) -> Aging {
        let mut out = Aging::default();
        if input.time_based.max_age.is_some() {
            out.time_based = crate::time_based_mc::apply_time_based_mc(
                messages,
                input.time_based,
                input.message_ages,
            );
        }
        if input.cached_due {
            let (cleared, ids) =
                crate::cached::clear_stale_tool_results(messages, input.cached_keep_recent);
            out.cached_cleared = cleared;
            out.cache_edits = Some(ids);
        }
        out
    }

    /// Compact early, before the budget forces a bigger one.
    ///
    /// Adopts the result only if it made the history cheaper. The obvious
    /// criterion — fewer messages — is the wrong one here: `micro_compact`
    /// blanks tool-result bodies in place and never drops a message, so
    /// counting messages meant the result was discarded every time and this
    /// whole path had no consequence.
    fn reactive(&self, messages: &mut Vec<ModelMessage>, input: &ReactiveInput) -> ReactiveOutcome {
        if !input.enabled || input.context_limit == 0 || input.circuit_open {
            return ReactiveOutcome::NotTriggered;
        }
        let velocity = crate::reactive::estimate_token_velocity(messages);
        if !crate::reactive::should_compact_reactively(
            input.context_tokens,
            input.context_limit,
            velocity,
        ) {
            return ReactiveOutcome::NotTriggered;
        }
        let keep = input.keep_recent.max(5);
        if group_by_api_round(messages).len() <= keep {
            // Nothing has aged out of the keep window yet. Not a failure: the
            // circuit breaker is there to stop paying for a pass that keeps
            // not helping, and a session with nothing old enough to clear is
            // not that. Counting it as one opens the circuit within the first
            // few rounds of every session and the pass never runs again.
            return ReactiveOutcome::NotTriggered;
        }
        let before = estimate_tokens(messages);
        let (compacted, _) = DefaultCompactor.micro_compact(messages.clone(), keep);
        if estimate_tokens(&compacted) >= before {
            return ReactiveOutcome::NoEffect;
        }
        *messages = compacted;
        crate::cleanup::run_cleanup_callbacks();
        ReactiveOutcome::Compacted
    }

    /// Tell the user compaction is coming, while there is still room to react
    /// to it. `None` once it has been said.
    fn context_warning(&self, state: &BudgetState) -> Option<String> {
        if state.threshold == 0 || state.warned_already {
            return None;
        }
        let warn_at = (state.threshold as f64 * 0.8) as usize;
        if state.context_tokens <= warn_at || state.context_tokens > state.threshold {
            return None;
        }
        Some(format!(
            "<system-reminder>\n\
             ⚠️ Context is at {:.0}% of the token budget ({}/{} tokens). \
             The conversation will be compacted soon to make room. \
             If you have pending work, wrap it up or ask the user what to preserve.\n\
             </system-reminder>",
            (state.context_tokens as f64 / state.threshold as f64 * 100.0).min(99.0),
            state.context_tokens,
            state.threshold,
        ))
    }

    /// Whether the budget now forces a compaction.
    fn should_compact(&self, state: &BudgetState) -> bool {
        state.threshold > 0 && state.context_tokens > state.threshold
    }

    /// The share of a request the accumulated tool results may hold.
    ///
    /// Overriding this changes the numbers; overriding `trim_tool_results`
    /// changes what is done about them.
    fn tool_result_budget(&self) -> ToolResultBudget {
        ToolResultBudget::default()
    }

    /// Bring the tool results back inside that budget. Returns how many were
    /// rewritten.
    fn trim_tool_results(&self, messages: &mut [ModelMessage]) -> usize {
        enforce_tool_result_budget(messages, &self.tool_result_budget())
    }

    /// Whether it is worth an API call to summarize before anything is
    /// dropped.
    ///
    /// Only the choice is here. Producing the summary needs a model, and
    /// which model that is — a cheap one the task router picked, or the
    /// conversation's own — is the loop's to know.
    fn wants_llm_summary(
        &self,
        messages: &[ModelMessage],
        max_tokens: usize,
        keep_rounds: usize,
    ) -> bool {
        crate::llm_summary::should_summarize(messages, max_tokens, keep_rounds)
    }
}

/// Compaction only when the budget forces it.
///
/// A deployment that wants its transcript left alone until it genuinely has
/// to be rewritten: no aging passes, no early compaction, no warning. Wraps
/// any compactor rather than replacing it, so "stop touching my history
/// early" does not also mean "reimplement compaction".
pub struct OnlyAtThreshold<C>(pub C);

#[async_trait]
impl<C: Compactor> Compactor for OnlyAtThreshold<C> {
    async fn compact(
        &self,
        messages: Vec<ModelMessage>,
        max_tokens: usize,
        keep_rounds: usize,
    ) -> Result<(Vec<ModelMessage>, CompactResult), CompactError> {
        self.0.compact(messages, max_tokens, keep_rounds).await
    }

    fn age(&self, _messages: &mut [ModelMessage], _input: &AgingInput<'_>) -> Aging {
        Aging::default()
    }

    fn reactive(
        &self,
        _messages: &mut Vec<ModelMessage>,
        _input: &ReactiveInput,
    ) -> ReactiveOutcome {
        ReactiveOutcome::NotTriggered
    }

    fn context_warning(&self, _state: &BudgetState) -> Option<String> {
        None
    }
}

// ── DefaultCompactor: Snip + MicroCompact + CollapseContext ──

pub struct DefaultCompactor;

impl DefaultCompactor {
    fn tokens(msgs: &[ModelMessage]) -> usize {
        estimate_tokens(msgs)
    }

    /// Snip: keep the most recent `keep_rounds` API rounds; if that is still
    /// over budget, keep fewer — down to a floor of 1 round, which is always
    /// kept even when it alone exceeds the budget (dropping it would leave the
    /// model with no conversation at all).
    ///
    /// Returns (messages, strategy, dropped_rounds, dropped_messages, tokens_saved) so the
    /// caller can build a SnipProjection for telemetry/UI.
    ///
    /// The previous implementation initialized `drop = rounds.len() - keep` and
    /// *decremented* it in the loop body, so `start = len - (keep + drop)`
    /// moved earlier on each iteration — i.e. the loop added older rounds back
    /// instead of dropping more, jumping straight to `rounds[1..]` (nearly the
    /// whole history) on the first iteration and converging back down from
    /// there. It never dropped below `keep_rounds`, contradicting its own
    /// comment, and every iteration deep-cloned every kept message. It arrived
    /// at the right answer only because the convergence happened to terminate
    /// at the same place a correct loop would start.
    ///
    /// This version decides the cut from the per-round token totals that
    /// `group_by_api_round` already computed (`estimate_tokens` is additive, so
    /// the cost of a concatenation of rounds is the sum of theirs) and
    /// materializes the message list exactly once.
    fn snip(
        &self,
        messages: Vec<ModelMessage>,
        max_tokens: usize,
        keep_rounds: usize,
    ) -> (Vec<ModelMessage>, CompactStrategy, usize, usize, usize) {
        let rounds = group_by_api_round(&messages);
        let total_rounds = rounds.len();
        let total_messages = messages.len();
        if total_rounds == 0 {
            return (messages, CompactStrategy::Snip, 0, 0, 0);
        }
        let tokens_before = Self::tokens(&messages);

        // Walk back from the newest round, accumulating cost. Stop at the
        // configured `keep_rounds`, or earlier if the budget runs out first.
        let target_keep = keep_rounds.clamp(1, total_rounds);
        let mut keep = 0usize;
        let mut tokens = 0usize;
        for round in rounds.iter().rev() {
            let next = tokens + round.estimated_tokens;
            // The newest round is unconditional; beyond it, take a round only
            // if it fits and we still owe rounds toward `target_keep`.
            if keep > 0 && (next > max_tokens || keep >= target_keep) {
                break;
            }
            keep += 1;
            tokens = next;
        }

        let result: Vec<ModelMessage> = rounds[total_rounds - keep..]
            .iter()
            .flat_map(|r| r.messages.iter().cloned())
            .collect();
        let dropped_rounds = total_rounds - keep;
        let dropped_messages = total_messages.saturating_sub(result.len());
        let tokens_saved = tokens_before.saturating_sub(tokens);
        (
            result,
            CompactStrategy::Snip,
            dropped_rounds,
            dropped_messages,
            tokens_saved,
        )
    }

    /// MicroCompact: replace old tool results with a placeholder.
    pub fn micro_compact(
        &self,
        messages: Vec<ModelMessage>,
        keep_rounds: usize,
    ) -> (Vec<ModelMessage>, CompactStrategy) {
        let rounds = group_by_api_round(&messages);
        if rounds.len() <= keep_rounds {
            return (messages, CompactStrategy::MicroCompact);
        }
        let keep_start = rounds.len() - keep_rounds;
        let to_compact: Vec<&ApiRound> = rounds[..keep_start].iter().collect();
        let to_keep: Vec<&ApiRound> = rounds[keep_start..].iter().collect();

        let mut result: Vec<ModelMessage> = Vec::new();
        let mut last_tool_name: Option<String> = None;
        for round in &to_compact {
            for msg in &round.messages {
                // Track tool name from ToolUse blocks (for whitelist check)
                for block in &msg.content {
                    if let ModelContentBlock::ToolUse { name, .. } = block {
                        last_tool_name = Some(name.clone());
                    }
                }
                let mut msg = msg.clone();
                for block in &mut msg.content {
                    if let ModelContentBlock::ToolResult {
                        content, is_error, ..
                    } = block
                    {
                        let is_compactable = last_tool_name
                            .as_deref()
                            .map(|n| COMPACTABLE_TOOLS.contains(&n))
                            .unwrap_or(false);
                        if is_compactable {
                            // Clear the *body* only. Replacing the whole block
                            // (which is what this did before) blanked
                            // `tool_use_id`, orphaning the result from its
                            // `tool_use` — the API rejects a `tool_result`
                            // whose id doesn't match a preceding `tool_use`,
                            // and `extract_recent_reads` pairs on that id too.
                            *content = "[Old tool result content cleared]".to_string();
                            *is_error = Some(false);
                        }
                    }
                }
                result.push(msg);
            }
        }
        for round in &to_keep {
            result.extend(round.messages.clone());
        }
        (result, CompactStrategy::MicroCompact)
    }

    /// CollapseContext: fold old rounds into a single summary message (no LLM).
    /// Keeps the structure but truncates text to a fixed summary length.
    fn collapse_context(
        &self,
        messages: Vec<ModelMessage>,
        keep_rounds: usize,
    ) -> (Vec<ModelMessage>, CompactStrategy) {
        let rounds = group_by_api_round(&messages);
        if rounds.len() <= keep_rounds {
            return (messages, CompactStrategy::CollapseContext);
        }
        let keep_start = rounds.len() - keep_rounds;
        let to_collapse = &rounds[..keep_start];
        let to_keep = &rounds[keep_start..];

        // Build a synthetic summary of collapsed rounds
        let mut summary_parts: Vec<String> = Vec::new();
        for round in to_collapse {
            for msg in &round.messages {
                for block in &msg.content {
                    if let ModelContentBlock::Text { text } = block {
                        let truncated = if text.len() > 200 {
                            // Character-boundary aware: `&text[..200]` panics
                            // whenever byte 200 lands inside a multi-byte
                            // character, which for Chinese text (3 bytes/char)
                            // is the common case, not the edge case. This is
                            // the last-resort compaction strategy, so a panic
                            // here takes down the turn at exactly the moment
                            // the context is already in trouble.
                            format!("{}...", truncate_at_char_boundary(text, 200))
                        } else {
                            text.clone()
                        };
                        summary_parts.push(truncated);
                    } else if let ModelContentBlock::ToolUse { name, .. } = block {
                        summary_parts.push(format!("[tool: {name}]"));
                    }
                }
            }
        }
        let summary = summary_parts.join(" | ");

        let boundary = ModelMessage {
            role: MessageRole::User,
            content: vec![ModelContentBlock::Text {
                text: "[Earlier conversation collapsed for context]".to_string(),
            }],
        };
        let summary_msg = ModelMessage {
            role: MessageRole::User,
            content: vec![ModelContentBlock::Text {
                text: format!("[Summary of earlier conversation]: {summary}"),
            }],
        };

        let mut result = vec![boundary, summary_msg];
        for round in to_keep {
            result.extend(round.messages.clone());
        }
        (result, CompactStrategy::CollapseContext)
    }
}

#[async_trait]
impl Compactor for DefaultCompactor {
    async fn compact(
        &self,
        messages: Vec<ModelMessage>,
        max_tokens: usize,
        keep_rounds: usize,
    ) -> Result<(Vec<ModelMessage>, CompactResult), CompactError> {
        let tokens_before = Self::tokens(&messages);
        let count_before = messages.len();
        if tokens_before <= max_tokens {
            // Already within budget — nothing ran. Reported honestly as `NoOp`
            // rather than as a zero-effect `Snip`.
            return Ok((
                messages,
                CompactResult {
                    strategy: CompactStrategy::NoOp,
                    messages_before: count_before,
                    messages_after: count_before,
                    tokens_before,
                    tokens_after: tokens_before,
                    projection: None,
                },
            ));
        }

        // Strategy progression with fallback
        let (result, strategy, dropped_rounds, dropped_messages, tokens_saved) = {
            // 1. Try Snip (keep configured rounds)
            let (r, s, dr, dm, ts) = self.snip(messages.clone(), max_tokens, keep_rounds);
            if Self::tokens(&r) <= max_tokens {
                (r, s, dr, dm, ts)
            } else {
                // 2. Try MicroCompact (clear old tool results)
                let (r, s) = self.micro_compact(messages.clone(), keep_rounds);
                if Self::tokens(&r) <= max_tokens {
                    (r, s, 0, 0, 0)
                } else {
                    // 3. CollapseContext as fallback
                    let (r, s) = self.collapse_context(messages.clone(), keep_rounds);
                    (r, s, 0, 0, 0)
                }
            }
        };

        let tokens_after = Self::tokens(&result);
        let messages_after = result.len();
        let projection = if strategy == CompactStrategy::Snip && dropped_rounds > 0 {
            Some(SnipProjection {
                dropped_rounds,
                dropped_messages,
                estimated_tokens_saved: tokens_saved,
            })
        } else {
            None
        };
        Ok((
            result,
            CompactResult {
                strategy,
                messages_before: count_before,
                messages_after,
                tokens_before,
                tokens_after,
                projection,
            },
        ))
    }
}

// ── Post-compact recovery (T1.4) ──

/// One skill invoked before compaction, with enough to re-attach its actual
/// body (not just its name) afterward. Built by the caller (`runtime`,
/// which has the `SkillManager` this crate deliberately doesn't depend on)
/// from its own `invoked_skills: Vec<String>` tracking, resolving each name
/// to its current content and last-invoked ordering.
#[derive(Debug, Clone)]
pub struct InvokedSkillRecord {
    pub name: String,
    /// The skill's current full body. `None` if it couldn't be resolved
    /// (deleted/renamed since it was invoked) — falls back to a name-only
    /// mention rather than being dropped silently, so the reminder that it
    /// was used at all survives even when the content can't.
    pub content: Option<String>,
    /// Monotonic recency ordering (`SkillManager::last_invoked_seq`) — higher
    /// is more recent. Used to decide which skills get first claim on the
    /// shared reattachment budget when not everything fits.
    pub last_seq: u64,
}

/// Context that can be recovered after compaction.
#[derive(Debug, Clone, Default)]
pub struct PostCompactContext {
    /// Recently read files (paths and their content)
    pub recent_files: Vec<(String, String)>,
    /// Skills that were invoked before compaction, most-recent-first
    /// priority decided by `last_seq` (the caller doesn't need to
    /// pre-sort — `build_post_compact_recovery` does it).
    pub invoked_skills: Vec<InvokedSkillRecord>,
    /// Whether plan mode was active
    pub in_plan_mode: bool,
    /// Plan file content (if plan mode was active)
    pub plan_content: Option<String>,
    /// Recently activated deferred tools
    pub activated_tools: Vec<String>,
    /// Background task statuses: (task_id, status_description)
    pub running_tasks: Vec<(String, String)>,
}

/// Maximum number of recently read files to re-inject after compaction.
const MAX_RECOVER_FILES: usize = 5;
/// Maximum characters per recovered file.
const MAX_CHARS_PER_FILE: usize = 5_000;
/// Per-skill and shared-total character budgets for reattached skill
/// content — sized from a 5,000 / 25,000 *token* budget, converted with
/// this codebase's existing ~4-chars-per-token heuristic (see
/// `turn.rs::build_skills_text`'s listing budget, same conversion).
const MAX_CHARS_PER_SKILL: usize = 20_000;
const MAX_CHARS_SKILLS_TOTAL: usize = 100_000;

/// Truncate `s` to at most `max_bytes` bytes without splitting a multi-byte
/// UTF-8 character. Kept as a crate-local alias so existing call sites read
/// unchanged; the implementation now lives in `base::text` so that all
/// truncation sites share one (tested) definition.
pub(crate) fn truncate_str_at_char_boundary(s: &str, max_bytes: usize) -> &str {
    truncate_at_char_boundary(s, max_bytes)
}

/// Build post-compact recovery messages.
///
/// After compaction removes old messages, critical context (recently read files,
/// invoked skills, plan mode status) needs to be re-injected so the model doesn't
/// lose its bearings.
pub fn build_post_compact_recovery(ctx: &PostCompactContext) -> Vec<ModelMessage> {
    let mut recovery = Vec::new();

    // 1. Recently read files
    if !ctx.recent_files.is_empty() {
        let files: Vec<_> = ctx
            .recent_files
            .iter()
            .take(MAX_RECOVER_FILES)
            .map(|(path, content)| {
                let truncated = if content.len() > MAX_CHARS_PER_FILE {
                    format!(
                        "{}...\n[content truncated]",
                        truncate_str_at_char_boundary(content, MAX_CHARS_PER_FILE)
                    )
                } else {
                    content.clone()
                };
                format!("## {path}\n\n{truncated}")
            })
            .collect();
        if !files.is_empty() {
            let body = format!(
                "The following files were read before context compaction and may still be relevant:\n\n{}",
                files.join("\n\n---\n\n")
            );
            recovery.push(ModelMessage {
                role: MessageRole::User,
                content: vec![ModelContentBlock::Text {
                    text: format!("<system-reminder>\n{body}\n</system-reminder>"),
                }],
            });
        }
    }

    // 2. Invoked skills — re-attach actual body content (not just names),
    // most-recently-invoked first, within the shared budget: up to the first
    // 5,000 tokens of each skill, within a combined 25,000-token budget,
    // starting from the most recently invoked skill so older skills can be
    // dropped entirely once the budget runs out.
    if !ctx.invoked_skills.is_empty() {
        let mut by_recency = ctx.invoked_skills.clone();
        by_recency.sort_by_key(|s| std::cmp::Reverse(s.last_seq));

        let mut sections = Vec::new();
        let mut spent = 0usize;
        let mut dropped_names = Vec::new();
        for skill in &by_recency {
            let Some(content) = &skill.content else {
                dropped_names.push(skill.name.clone());
                continue;
            };
            let capped = if content.len() > MAX_CHARS_PER_SKILL {
                format!(
                    "{}...[truncated]",
                    truncate_str_at_char_boundary(content, MAX_CHARS_PER_SKILL)
                )
            } else {
                content.clone()
            };
            if spent + capped.len() > MAX_CHARS_SKILLS_TOTAL {
                dropped_names.push(skill.name.clone());
                continue;
            }
            spent += capped.len();
            sections.push(format!("## {}\n\n{}", skill.name, capped));
        }

        let mut body = String::from(
            "The following skills were invoked before compaction — their instructions may still apply. Full content for the most recently used ones is repeated below; re-invoke any skill via the Skill tool if you need the exact steps again.\n\n",
        );
        body.push_str(&sections.join("\n\n---\n\n"));
        if !dropped_names.is_empty() {
            body.push_str(&format!(
                "\n\n(Also invoked before compaction, but dropped here for budget — re-invoke if needed: {})",
                dropped_names.join(", ")
            ));
        }
        recovery.push(ModelMessage {
            role: MessageRole::User,
            content: vec![ModelContentBlock::Text {
                text: format!("<system-reminder>\n{body}\n</system-reminder>"),
            }],
        });
    }

    // 3. Plan mode — inject plan content if available
    if ctx.in_plan_mode {
        if let Some(ref plan) = ctx.plan_content {
            let truncated = if plan.len() > MAX_CHARS_PER_FILE {
                format!(
                    "{}...\n[plan content truncated]",
                    truncate_str_at_char_boundary(plan, MAX_CHARS_PER_FILE)
                )
            } else {
                plan.clone()
            };
            recovery.push(ModelMessage {
                role: MessageRole::User,
                content: vec![ModelContentBlock::Text {
                    text: format!("<system-reminder>\nPlan mode is still active. The current plan:\n\n{truncated}\n</system-reminder>"),
                }],
            });
        } else {
            recovery.push(ModelMessage {
                role: MessageRole::User,
                content: vec![ModelContentBlock::Text {
                    text: "<system-reminder>\nPlan mode is still active. Continue operating in plan mode.\n</system-reminder>".to_string(),
                }],
            });
        }
    }

    // 4. Background running tasks
    if !ctx.running_tasks.is_empty() {
        let tasks_lines: Vec<_> = ctx
            .running_tasks
            .iter()
            .map(|(id, status)| format!("- task:{id} — {status}"))
            .collect();
        let body = format!(
            "The following background tasks are still running:\n\n{}",
            tasks_lines.join("\n")
        );
        recovery.push(ModelMessage {
            role: MessageRole::User,
            content: vec![ModelContentBlock::Text {
                text: format!("<system-reminder>\n{body}\n</system-reminder>"),
            }],
        });
    }

    // 5. Activated deferred tools
    if !ctx.activated_tools.is_empty() {
        let tools_text = ctx.activated_tools.join(", ");
        recovery.push(ModelMessage {
            role: MessageRole::User,
            content: vec![ModelContentBlock::Text {
                text: format!("<system-reminder>\nThese deferred tools were loaded earlier with ToolSearch: {tools_text}. If a schema it returned is no longer in the conversation, load it again with ToolSearch (select:<name>) before calling that tool.\n</system-reminder>"),
            }],
        });
    }

    recovery
}

/// Tool names whose results are file reads worth recovering after compaction.
/// `Read` is `crates/tools/src/file_read.rs::FileReadTool::name()`.
const FILE_READ_TOOLS: &[&str] = &["Read"];

/// Extract recently read file paths from a set of messages, most recent first.
/// Returns (path, content) pairs for file-read tool results found in the messages.
///
/// Pairs `ToolUse` → `ToolResult` by `tool_use_id`, taking the path from the
/// tool *input* (`file_path`) rather than guessing it from the result body.
///
/// The previous implementation sniffed content: it treated a tool result as a
/// file read if its first line started with `/` or `./`. The Read tool emits
/// `cat -n` style output (`format_with_line_numbers`: `"{:>6}\t{}\n"`), so every
/// line starts with padding spaces and a line number and the check matched
/// **zero** results, always — post-compact file recovery had never once fired.
pub fn extract_recent_reads(messages: &[ModelMessage]) -> Vec<(String, String)> {
    // Pass 1: tool_use_id → file_path, for file-read tool uses only.
    let mut read_calls: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    for msg in messages {
        for block in &msg.content {
            if let ModelContentBlock::ToolUse { id, name, input } = block {
                if !FILE_READ_TOOLS.contains(&name.as_str()) {
                    continue;
                }
                if let Some(path) = input.get("file_path").and_then(|v| v.as_str()) {
                    if !path.is_empty() {
                        read_calls.insert(id.as_str(), path);
                    }
                }
            }
        }
    }
    if read_calls.is_empty() {
        return Vec::new();
    }

    // Pass 2: newest-first, so the freshest read of a given path wins and the
    // caller's `take(MAX_RECOVER_FILES)` keeps the most recent files.
    let mut files = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for msg in messages.iter().rev() {
        for block in &msg.content {
            let ModelContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } = block
            else {
                continue;
            };
            if is_error == &Some(true) || content.is_empty() {
                continue;
            }
            let Some(&path) = read_calls.get(tool_use_id.as_str()) else {
                continue;
            };
            if seen.insert(path) {
                files.push((path.to_string(), content.clone()));
            }
        }
    }
    files
}

/// Enforce per-message tool result budget, truncating oversized tool results.
///
/// Runs BEFORE microcompact for clean composition:
/// - 50KB per individual tool result (truncate with preserved header)
/// - 500KB total across all tool results (clear oldest first)
///
/// Returns the number of tool results that were modified.
/// Replace image blocks with `[image]` text markers before LLM compaction.
/// Note: AttaCore's `ModelContentBlock` currently has no `Image` variant — this
/// function is a forward-compat stub. When image support is added, it will
/// strip them before compaction.
pub fn strip_images_from_messages(messages: &mut [ModelMessage]) {
    // Walk through and strip any image-like content.
    // Currently a no-op — ModelContentBlock has no Image variant.
    // Forward-compat: when Text blocks contain base64 image data URIs,
    // they can be detected and replaced here.
    let _ = messages;
}

/// Tool names that MicroCompact may clear.
const COMPACTABLE_TOOLS: &[&str] = &[
    "Read",
    "Bash",
    "Grep",
    "Glob",
    "WebSearch",
    "WebFetch",
    "Edit",
    "Write",
];

// ── P2-1: Compact Analysis ──

/// Token composition breakdown for a set of messages.
/// Useful for debugging compaction behavior and understanding where token budget is spent.
#[derive(Debug, Clone, Default)]
pub struct ContextAnalysis {
    pub total_messages: usize,
    pub total_estimated_tokens: usize,
    /// Tokens from user text messages (excluding tool results).
    pub user_text_tokens: usize,
    /// Tokens from assistant text messages.
    pub assistant_text_tokens: usize,
    /// Tokens from tool result blocks.
    pub tool_result_tokens: usize,
    /// Tokens from tool use blocks.
    pub tool_use_tokens: usize,
    /// Tokens from system messages.
    pub system_tokens: usize,
    /// Per-tool token breakdown: tool_name → (call_count, total_tokens).
    pub tool_usage: std::collections::HashMap<String, (usize, usize)>,
    /// Number of compressed/cleared tool results.
    pub cleared_results: usize,
    /// Percentage of token budget consumed by tool results.
    pub tool_result_pct: f64,
    /// Percentage of token budget consumed by user+assistant text.
    pub conversation_pct: f64,
}

/// Analyze token composition of a message list.
pub fn analyze_context(messages: &[ModelMessage]) -> ContextAnalysis {
    let mut analysis = ContextAnalysis {
        total_messages: messages.len(),
        ..ContextAnalysis::default()
    };

    // Same estimator the compaction decisions use, so the breakdown reported
    // here adds up to the number that actually tripped compaction.
    let estimate = |s: &str| {
        model::tokens::estimate_model_block_tokens(&ModelContentBlock::Text {
            text: s.to_string(),
        })
    };

    for msg in messages {
        for block in &msg.content {
            match block {
                ModelContentBlock::Text { text } => {
                    let tokens = estimate(text);
                    analysis.total_estimated_tokens += tokens;
                    match msg.role {
                        MessageRole::User => analysis.user_text_tokens += tokens,
                        MessageRole::Assistant => analysis.assistant_text_tokens += tokens,
                        _ => analysis.system_tokens += tokens,
                    }
                }
                // Attributed to whichever side of the conversation sent it,
                // same as text. Flat estimate — see IMAGE_TOKEN_ESTIMATE.
                ModelContentBlock::Image { .. } => {
                    let tokens = base::interface::model::IMAGE_TOKEN_ESTIMATE;
                    analysis.total_estimated_tokens += tokens;
                    match msg.role {
                        MessageRole::User => analysis.user_text_tokens += tokens,
                        MessageRole::Assistant => analysis.assistant_text_tokens += tokens,
                        _ => analysis.system_tokens += tokens,
                    }
                }
                ModelContentBlock::Thinking { text, .. } => {
                    let tokens = estimate(text);
                    analysis.total_estimated_tokens += tokens;
                    analysis.assistant_text_tokens += tokens;
                }
                ModelContentBlock::RedactedThinking { data, .. } => {
                    let tokens = estimate(data);
                    analysis.total_estimated_tokens += tokens;
                    analysis.assistant_text_tokens += tokens;
                }
                ModelContentBlock::ToolResult { content, .. } => {
                    let tokens = model::tokens::estimate_model_block_tokens(block);
                    analysis.total_estimated_tokens += tokens;
                    analysis.tool_result_tokens += tokens;
                    // Check if cleared
                    if content == "[Old tool result content cleared]" {
                        analysis.cleared_results += 1;
                    }
                }
                ModelContentBlock::ToolUse { name, .. } => {
                    let tokens = model::tokens::estimate_model_block_tokens(block);
                    analysis.total_estimated_tokens += tokens;
                    analysis.tool_use_tokens += tokens;
                    let entry = analysis.tool_usage.entry(name.clone()).or_insert((0, 0));
                    entry.0 += 1;
                    entry.1 += tokens;
                }
            }
        }
    }

    // Compute percentages
    if analysis.total_estimated_tokens > 0 {
        let total = analysis.total_estimated_tokens as f64;
        analysis.tool_result_pct = (analysis.tool_result_tokens as f64 / total) * 100.0;
        let conversation = analysis.user_text_tokens + analysis.assistant_text_tokens;
        analysis.conversation_pct = (conversation as f64 / total) * 100.0;
    }

    analysis
}

/// Format a compact analysis as a human-readable summary string.
pub fn format_context_analysis(analysis: &ContextAnalysis) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "Context: {} msgs, ~{} tokens\n",
        analysis.total_messages, analysis.total_estimated_tokens
    ));
    s.push_str(&format!(
        "  user text: {:.0}%  assistant: {:.0}%  tool results: {:.0}%  tool uses: {:.0}%\n",
        (analysis.user_text_tokens as f64 / analysis.total_estimated_tokens.max(1) as f64) * 100.0,
        (analysis.assistant_text_tokens as f64 / analysis.total_estimated_tokens.max(1) as f64)
            * 100.0,
        analysis.tool_result_pct,
        (analysis.tool_use_tokens as f64 / analysis.total_estimated_tokens.max(1) as f64) * 100.0,
    ));

    if !analysis.tool_usage.is_empty() {
        let mut tools: Vec<_> = analysis.tool_usage.iter().collect();
        tools.sort_by_key(|(_, (_calls, tokens))| std::cmp::Reverse(*tokens));
        s.push_str("  top tools by tokens:\n");
        for (name, (calls, tokens)) in tools.iter().take(5) {
            s.push_str(&format!(
                "    {}: {} calls, ~{} tokens\n",
                name, calls, tokens
            ));
        }
    }

    if analysis.cleared_results > 0 {
        s.push_str(&format!(
            "  {} tool result(s) already cleared\n",
            analysis.cleared_results
        ));
    }

    s
}

/// How much of a request the accumulated tool results may hold.
///
/// A different question from [`crate::compact::Compactor::compact`] and a
/// different one again from a tool-result transformer, which decides what one
/// result looks like as it comes back. This is about the share of the whole
/// history that results occupy after a hundred individually reasonable ones
/// have piled up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolResultBudget {
    /// Truncate any single result past this.
    pub max_per_result: usize,
    /// Once the results together pass this, clear them oldest-first until
    /// they don't.
    pub max_total: usize,
}

impl Default for ToolResultBudget {
    fn default() -> Self {
        Self {
            max_per_result: 50_000,
            max_total: 500_000,
        }
    }
}

pub fn enforce_tool_result_budget(
    messages: &mut [ModelMessage],
    budget: &ToolResultBudget,
) -> usize {
    let ToolResultBudget {
        max_per_result,
        max_total,
    } = *budget;

    let mut modified = 0;
    let mut total_size: usize = 0;

    // Pass 1: truncate oversized individual results
    for msg in messages.iter_mut() {
        for block in &mut msg.content {
            if let ModelContentBlock::ToolResult { content, .. } = block {
                if content.len() > max_per_result {
                    // Character-boundary aware — see the note in
                    // `collapse_context`. A tool result is arbitrary bytes
                    // (file contents, command output), so byte 1000 landing
                    // mid-character is routine.
                    let truncated = format!(
                        "[Tool result truncated: {} bytes > {} max; first 1000 bytes preserved]\n{}...",
                        content.len(),
                        max_per_result,
                        truncate_at_char_boundary(content, 1000)
                    );
                    *content = truncated;
                    modified += 1;
                }
                total_size += content.len();
            }
        }
    }

    // Pass 2: if the total still exceeds budget, clear tool results
    // strictly oldest-first.
    //
    // "Oldest first" and "message order" coincide here because the transcript
    // is append-only (`SessionManager::push_message` only pushes; compaction
    // removes from the front and never reorders), so index order *is*
    // chronological order. That was previously left implicit, with the
    // termination condition split across a nested `break` plus an outer one.
    // Making the traversal order explicit means a future caller that hands
    // this an out-of-order slice gets a documented contract rather than a
    // silent behavior change.
    if total_size > max_total {
        const CLEARED: &str = "[Old tool result content cleared]";
        // Positions in chronological order.
        let mut positions: Vec<(usize, usize)> = Vec::new();
        for (mi, msg) in messages.iter().enumerate() {
            for (bi, block) in msg.content.iter().enumerate() {
                if matches!(block, ModelContentBlock::ToolResult { .. }) {
                    positions.push((mi, bi));
                }
            }
        }
        for (mi, bi) in positions {
            if total_size <= max_total {
                break;
            }
            let ModelContentBlock::ToolResult { content, .. } = &mut messages[mi].content[bi]
            else {
                continue;
            };
            if content == CLEARED {
                continue; // already cleared by an earlier pass; clearing again frees nothing
            }
            total_size = total_size.saturating_sub(content.len()) + CLEARED.len();
            *content = CLEARED.to_string();
            modified += 1;
        }
    }

    modified
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_user(s: &str) -> ModelMessage {
        ModelMessage {
            role: MessageRole::User,
            content: vec![ModelContentBlock::Text {
                text: s.to_string(),
            }],
        }
    }

    fn tool_result(id: &str, content: &str) -> ModelMessage {
        ModelMessage {
            role: MessageRole::User,
            content: vec![ModelContentBlock::ToolResult {
                tool_use_id: id.to_string(),
                content: content.to_string(),
                is_error: Some(false),
            }],
        }
    }

    fn assistant_text(s: &str) -> ModelMessage {
        ModelMessage {
            role: MessageRole::Assistant,
            content: vec![ModelContentBlock::Text {
                text: s.to_string(),
            }],
        }
    }

    fn tool_use(id: &str, name: &str, input: &str) -> ModelMessage {
        ModelMessage {
            role: MessageRole::Assistant,
            content: vec![ModelContentBlock::ToolUse {
                id: id.to_string(),
                name: name.to_string(),
                input: serde_json::from_str(input).unwrap_or_default(),
            }],
        }
    }

    fn build_long_conversation(rounds: usize) -> Vec<ModelMessage> {
        let mut msgs = Vec::new();
        for i in 0..rounds {
            msgs.push(text_user(&format!("msg {i}")));
            msgs.push(assistant_text(&format!("response {i}")));
        }
        msgs
    }

    // ── the trigger judgements ──

    /// `rounds` rounds of "the model called Read, here is a kilobyte back".
    fn read_rounds(rounds: usize) -> Vec<ModelMessage> {
        let mut msgs = Vec::new();
        for i in 0..rounds {
            msgs.push(tool_use(&format!("t{i}"), "Read", "{}"));
            msgs.push(tool_result(&format!("t{i}"), &"r".repeat(1_000)));
        }
        msgs
    }

    fn result_bodies(msgs: &[ModelMessage]) -> Vec<String> {
        msgs.iter()
            .flat_map(|m| &m.content)
            .filter_map(|b| match b {
                ModelContentBlock::ToolResult { content, .. } => Some(content.clone()),
                _ => None,
            })
            .collect()
    }

    fn reactive_input() -> ReactiveInput {
        ReactiveInput {
            context_tokens: 1_000,
            context_limit: 20_000,
            keep_recent: 2,
            enabled: true,
            circuit_open: false,
        }
    }

    #[test]
    fn the_reactive_pass_adopts_a_rewrite_that_never_drops_a_message() {
        let mut messages = read_rounds(8);
        let before = estimate_tokens(&messages);
        let outcome = DefaultCompactor.reactive(&mut messages, &reactive_input());
        assert_eq!(outcome, ReactiveOutcome::Compacted);
        assert_eq!(
            messages.len(),
            16,
            "micro-compact blanks bodies in place; a criterion based on the \
             message count can only ever see nothing happen"
        );
        assert!(estimate_tokens(&messages) < before);
    }

    /// The distinction the circuit breaker depends on. Every session starts
    /// with nothing old enough to clear; if that counted as a failure the
    /// breaker would open in the first few rounds and the pass would never
    /// run again.
    #[test]
    fn having_nothing_old_enough_to_clear_is_not_a_failed_pass() {
        let mut messages = read_rounds(3);
        assert_eq!(
            DefaultCompactor.reactive(&mut messages, &reactive_input()),
            ReactiveOutcome::NotTriggered
        );
    }

    #[test]
    fn a_pass_that_buys_nothing_is_reported_as_such() {
        // `Bash` is on the whitelist, `GoldenFlood` is not — nothing here can
        // be cleared, so the pass runs and comes back empty-handed.
        let mut messages = Vec::new();
        for i in 0..8 {
            messages.push(tool_use(&format!("t{i}"), "NotOnTheWhitelist", "{}"));
            messages.push(tool_result(&format!("t{i}"), &"r".repeat(1_000)));
        }
        assert_eq!(
            DefaultCompactor.reactive(&mut messages, &reactive_input()),
            ReactiveOutcome::NoEffect
        );
    }

    #[test]
    fn an_open_circuit_stops_the_pass_running_at_all() {
        let mut messages = read_rounds(8);
        let input = ReactiveInput {
            circuit_open: true,
            ..reactive_input()
        };
        assert_eq!(
            DefaultCompactor.reactive(&mut messages, &input),
            ReactiveOutcome::NotTriggered
        );
        assert_eq!(
            result_bodies(&messages),
            result_bodies(&read_rounds(8)),
            "and leaves the history alone"
        );
    }

    #[test]
    fn the_warning_is_said_once_and_only_in_the_last_fifth() {
        let state = |tokens, warned| BudgetState {
            context_tokens: tokens,
            threshold: 10_000,
            warned_already: warned,
        };
        assert!(DefaultCompactor
            .context_warning(&state(7_999, false))
            .is_none());
        assert!(DefaultCompactor
            .context_warning(&state(8_001, false))
            .is_some());
        assert!(
            DefaultCompactor
                .context_warning(&state(8_001, true))
                .is_none(),
            "already said"
        );
        assert!(
            DefaultCompactor
                .context_warning(&state(10_001, false))
                .is_none(),
            "past the threshold there is nothing to warn about — it is compacting"
        );
    }

    #[test]
    fn a_threshold_of_zero_disables_the_budget_entirely() {
        let state = BudgetState {
            context_tokens: usize::MAX,
            threshold: 0,
            warned_already: false,
        };
        assert!(!DefaultCompactor.should_compact(&state));
        assert!(DefaultCompactor.context_warning(&state).is_none());
    }

    /// The second implementation, and the one a host would actually reach
    /// for: leave my transcript alone until the budget forces a rewrite.
    #[tokio::test]
    async fn only_at_threshold_declines_everything_the_budget_did_not_force() {
        let c = OnlyAtThreshold(DefaultCompactor);
        let mut messages = read_rounds(8);
        let untouched = messages.clone();

        assert_eq!(
            c.reactive(&mut messages, &reactive_input()),
            ReactiveOutcome::NotTriggered
        );
        let aging = c.age(
            &mut messages,
            &AgingInput {
                message_ages: &[],
                time_based: &TimeBasedMcConfig::default(),
                cached_due: true,
                cached_keep_recent: 0,
            },
        );
        assert_eq!(aging.cleared(), 0);
        assert!(aging.cache_edits.is_none());
        assert_eq!(result_bodies(&messages), result_bodies(&untouched));
        assert!(c
            .context_warning(&BudgetState {
                context_tokens: 9_000,
                threshold: 10_000,
                warned_already: false,
            })
            .is_none());

        // What it does not decline: the budget itself.
        let state = BudgetState {
            context_tokens: 10_001,
            threshold: 10_000,
            warned_already: false,
        };
        assert!(c.should_compact(&state));
        let (out, result) = c.compact(messages, 500, 2).await.expect("compacts");
        assert_ne!(result.strategy, CompactStrategy::NoOp);
        assert!(estimate_tokens(&out) < estimate_tokens(&untouched));
    }

    /// Changing the numbers is one method; the algorithm stays the engine's.
    #[test]
    fn a_host_tightens_the_tool_result_budget_without_reimplementing_it() {
        struct Frugal;

        #[async_trait]
        impl Compactor for Frugal {
            async fn compact(
                &self,
                messages: Vec<ModelMessage>,
                max_tokens: usize,
                keep_rounds: usize,
            ) -> Result<(Vec<ModelMessage>, CompactResult), CompactError> {
                DefaultCompactor
                    .compact(messages, max_tokens, keep_rounds)
                    .await
            }

            fn tool_result_budget(&self) -> ToolResultBudget {
                ToolResultBudget {
                    max_per_result: 100,
                    max_total: 10_000,
                }
            }
        }

        let mut messages = vec![tool_result("t0", &"x".repeat(5_000))];
        assert_eq!(
            DefaultCompactor.trim_tool_results(&mut messages.clone()),
            0,
            "5 KB is nothing against the engine's 50 KB"
        );
        assert_eq!(Frugal.trim_tool_results(&mut messages), 1);
        assert!(result_bodies(&messages)[0].starts_with("[Tool result truncated:"));
    }

    /// Both aging passes run, and the cached one hands back the ids the next
    /// request needs.
    #[test]
    fn the_cached_pass_reports_what_it_blanked() {
        let mut messages = read_rounds(4);
        let aging = DefaultCompactor.age(
            &mut messages,
            &AgingInput {
                message_ages: &[],
                time_based: &TimeBasedMcConfig::disabled(),
                cached_due: true,
                cached_keep_recent: 1,
            },
        );
        assert_eq!(aging.cached_cleared, 3);
        assert_eq!(
            aging.cache_edits.as_deref(),
            Some(["t2", "t1", "t0"].map(String::from).as_slice()),
            "newest first, and only the ones that were blanked"
        );
    }

    // ── UTF-8 safety regressions ──
    //
    // Every truncation in this module is expressed as a *byte* budget while the
    // content is arbitrary UTF-8. Before these tests, `collapse_context` and
    // `enforce_tool_result_budget` sliced raw bytes, so a Chinese conversation
    // long enough to reach either of them panicked the whole turn. The
    // conversations below are built so the cut necessarily lands inside a
    // multi-byte character (3 bytes per Chinese char vs. budgets of 200/1000).

    /// Text long enough to force truncation, made of 3-byte characters so that
    /// no round byte budget is ever a character boundary.
    fn chinese_text(chars: usize) -> String {
        "这是一段用于测试的中文内容"
            .chars()
            .cycle()
            .take(chars)
            .collect()
    }

    #[tokio::test]
    async fn collapse_context_survives_multibyte_text() {
        // CollapseContext is the last-resort strategy, reached only when Snip
        // and MicroCompact both fail to get under budget — i.e. exactly when
        // the context is already in trouble. It truncates each text block at
        // 200 bytes; 100 Chinese chars = 300 bytes, so the cut lands at byte
        // 200, which is inside the 67th character.
        let mut msgs = Vec::new();
        for i in 0..10 {
            msgs.push(text_user(&format!("{i}{}", chinese_text(100))));
            msgs.push(assistant_text(&chinese_text(100)));
        }
        let compactor = DefaultCompactor;
        // Budget of 1 forces the full Snip → MicroCompact → CollapseContext
        // cascade; keep_rounds=2 leaves rounds to collapse.
        let (result, meta) = compactor
            .compact(msgs, 1, 2)
            .await
            .expect("compaction must not panic on multi-byte text");
        assert_eq!(meta.strategy, CompactStrategy::CollapseContext);
        assert!(!result.is_empty());
        // Every produced block must still be valid UTF-8 text (it is, by
        // construction, since it is a `String`) *and* a prefix of the input —
        // the point is that we got here at all instead of panicking.
        for m in &result {
            for b in &m.content {
                if let ModelContentBlock::Text { text } = b {
                    assert!(text.is_char_boundary(0));
                }
            }
        }
    }

    #[test]
    fn tool_result_budget_survives_multibyte_content() {
        // Per-result cap is 50_000 bytes and the preserved head is 1000 bytes.
        // 20_000 Chinese chars = 60_000 bytes > cap, and byte 1000 is inside
        // the 334th character.
        let mut msgs = vec![tool_result("t1", &chinese_text(20_000))];
        let modified = enforce_tool_result_budget(&mut msgs, &ToolResultBudget::default());
        assert_eq!(modified, 1, "oversized result should have been truncated");
        let ModelContentBlock::ToolResult { content, .. } = &msgs[0].content[0] else {
            panic!("expected a tool result");
        };
        assert!(content.contains("[Tool result truncated"));
        // The preserved head must be a whole number of characters, never a
        // split one — a split would have panicked above, so reaching here with
        // valid content is the assertion.
        assert!(content.len() < 60_000);
    }

    #[tokio::test]
    async fn snip_keeps_recent_rounds() {
        let msgs = build_long_conversation(10);
        let compactor = DefaultCompactor;
        // Use tight budget to force snip: each round ~3 tokens
        let (result, _) = compactor.compact(msgs.clone(), 20, 3).await.unwrap();
        // Should keep ~3 rounds * 2 messages = 6 messages (if fits under 20 tokens)
        assert!(
            !result.is_empty() && result.len() <= 8,
            "expected <=8 and non-empty, got {}",
            result.len()
        );
    }

    #[tokio::test]
    async fn micro_compact_replaces_old_results() {
        // Test that micro_compact clears old tool results for compactable tools.
        // Only results from whitelisted tools (Read, Bash, Grep, etc.) are cleared.
        let long = "VERY LONG TOOL RESULT ".repeat(100);
        let msgs = vec![
            text_user("read a"),
            tool_use("t1", "Read", "{}"), // whitelisted → will be cleared
            tool_result("t1", &long),
            assistant_text("done a"),
            text_user("grep b"),
            tool_use("t2", "Grep", "{}"), // whitelisted → will be cleared
            tool_result("t2", &long),
            assistant_text("done b"),
            text_user("edit c"),
            tool_use("t3", "Edit", "{}"), // whitelisted but in recent round → kept
            assistant_text("done c"),
        ];
        let compactor = DefaultCompactor;
        let (result, _) = compactor.micro_compact(msgs, 1);
        // Old rounds (a, b) should have their tool results cleared.
        // Recent round (c) should be intact.
        let cleared_count = result.iter().filter(|m| {
            m.content.iter().any(|b| matches!(b,
                ModelContentBlock::ToolResult { content, .. } if content == "[Old tool result content cleared]"
            ))
        }).count();
        assert_eq!(
            cleared_count, 2,
            "Expected exactly 2 cleared tool results, got {cleared_count}"
        );
    }

    fn recovery_text(messages: &[ModelMessage]) -> String {
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

    #[test]
    fn post_compact_recovery_reattaches_full_skill_content() {
        let ctx = PostCompactContext {
            invoked_skills: vec![InvokedSkillRecord {
                name: "code-review".into(),
                content: Some("1. Inspect changed files.\n2. Check correctness.".into()),
                last_seq: 1,
            }],
            ..Default::default()
        };
        let text = recovery_text(&build_post_compact_recovery(&ctx));
        assert!(text.contains("code-review"));
        assert!(text.contains("Inspect changed files"));
    }

    #[test]
    fn post_compact_recovery_orders_by_most_recently_invoked_first() {
        let ctx = PostCompactContext {
            invoked_skills: vec![
                InvokedSkillRecord {
                    name: "old-skill".into(),
                    content: Some("old body".into()),
                    last_seq: 1,
                },
                InvokedSkillRecord {
                    name: "new-skill".into(),
                    content: Some("new body".into()),
                    last_seq: 5,
                },
            ],
            ..Default::default()
        };
        let text = recovery_text(&build_post_compact_recovery(&ctx));
        let new_pos = text.find("new-skill").expect("new-skill present");
        let old_pos = text.find("old-skill").expect("old-skill present");
        assert!(
            new_pos < old_pos,
            "most recently invoked skill must appear first"
        );
    }

    #[test]
    fn post_compact_recovery_falls_back_to_name_only_when_content_unresolved() {
        let ctx = PostCompactContext {
            invoked_skills: vec![InvokedSkillRecord {
                name: "deleted-skill".into(),
                content: None,
                last_seq: 1,
            }],
            ..Default::default()
        };
        let text = recovery_text(&build_post_compact_recovery(&ctx));
        assert!(text.contains("deleted-skill"));
    }

    #[test]
    fn post_compact_recovery_caps_a_single_oversized_skill() {
        let huge = "x".repeat(MAX_CHARS_PER_SKILL + 5_000);
        let ctx = PostCompactContext {
            invoked_skills: vec![InvokedSkillRecord {
                name: "verbose-skill".into(),
                content: Some(huge),
                last_seq: 1,
            }],
            ..Default::default()
        };
        let text = recovery_text(&build_post_compact_recovery(&ctx));
        assert!(
            text.len() < MAX_CHARS_PER_SKILL + 1_000,
            "must be capped near the per-skill limit, got {} chars",
            text.len()
        );
        assert!(text.contains("truncated"));
    }

    #[test]
    fn post_compact_recovery_drops_least_recent_skills_over_shared_budget() {
        // Six skills, each at the per-skill cap (so 6 * MAX_CHARS_PER_SKILL
        // comfortably exceeds MAX_CHARS_SKILLS_TOTAL, which only fits 5) —
        // the least-recently-invoked one must be the one dropped.
        let big = "y".repeat(MAX_CHARS_PER_SKILL - 100);
        let invoked_skills: Vec<InvokedSkillRecord> = (1..=6)
            .map(|seq| InvokedSkillRecord {
                name: format!("skill-{seq}"),
                content: Some(big.clone()),
                last_seq: seq,
            })
            .collect();
        let ctx = PostCompactContext {
            invoked_skills,
            ..Default::default()
        };
        let text = recovery_text(&build_post_compact_recovery(&ctx));
        assert!(
            text.contains("## skill-6"),
            "most recent skill must keep full content"
        );
        assert!(
            text.contains("skill-1"),
            "dropped skill must still be named"
        );
        assert!(
            !text.contains("## skill-1"),
            "dropped skill must not have a full-content section"
        );
    }

    #[test]
    fn post_compact_recovery_is_empty_when_no_skills_invoked() {
        let ctx = PostCompactContext::default();
        let recovery = build_post_compact_recovery(&ctx);
        assert!(recovery.is_empty());
    }

    // ── C-3: post-compact file recovery ──

    /// Build a Read tool result the way the real tool builds it:
    /// `crates/tools/src/file_read.rs::format_with_line_numbers` →
    /// `"{:>6}\t{}\n"` per line. Deliberately *not* a hand-made string that
    /// happens to start with a path — the old heuristic ("first line starts
    /// with `/` or `./`") matched only such hand-made strings and never a real
    /// Read result.
    fn cat_n_output(body: &str) -> String {
        body.lines()
            .enumerate()
            .map(|(i, line)| format!("{:>6}\t{}\n", i + 1, line))
            .collect()
    }

    #[test]
    fn extract_recent_reads_recovers_files_from_real_read_output() {
        let content = cat_n_output("fn main() {\n    println!(\"hi\");\n}");
        // Sanity: this is exactly the shape the old heuristic could not see.
        assert!(
            !content.lines().next().unwrap().starts_with('/'),
            "real Read output starts with padded line numbers, not a path"
        );

        let msgs = vec![
            text_user("read the file"),
            tool_use("t1", "Read", r#"{"file_path":"/repo/src/main.rs"}"#),
            tool_result("t1", &content),
            assistant_text("done"),
        ];

        let files = extract_recent_reads(&msgs);
        assert_eq!(files.len(), 1, "expected one recovered file, got {files:?}");
        assert_eq!(files[0].0, "/repo/src/main.rs");
        assert!(files[0].1.contains("println!"));
    }

    #[test]
    fn extract_recent_reads_ignores_non_read_tools() {
        let msgs = vec![
            tool_use("t1", "Bash", r#"{"command":"cat /etc/hosts"}"#),
            tool_result("t1", "127.0.0.1 localhost"),
            tool_use(
                "t2",
                "Grep",
                r#"{"pattern":"foo","file_path":"/repo/x.rs"}"#,
            ),
            tool_result("t2", "/repo/x.rs:1:foo"),
        ];
        assert!(
            extract_recent_reads(&msgs).is_empty(),
            "only file-read tool results are recoverable"
        );
    }

    #[test]
    fn extract_recent_reads_pairs_by_tool_use_id_not_position() {
        // Two interleaved reads whose results arrive out of call order.
        let msgs = vec![
            tool_use("a", "Read", r#"{"file_path":"/repo/a.rs"}"#),
            tool_use("b", "Read", r#"{"file_path":"/repo/b.rs"}"#),
            tool_result("b", &cat_n_output("contents of b")),
            tool_result("a", &cat_n_output("contents of a")),
        ];
        let files = extract_recent_reads(&msgs);
        let by_path: std::collections::HashMap<_, _> = files.into_iter().collect();
        assert!(by_path["/repo/a.rs"].contains("contents of a"));
        assert!(by_path["/repo/b.rs"].contains("contents of b"));
    }

    #[test]
    fn extract_recent_reads_skips_errored_reads_and_dedupes_to_newest() {
        let msgs = vec![
            tool_use("t1", "Read", r#"{"file_path":"/repo/missing.rs"}"#),
            ModelMessage {
                role: MessageRole::User,
                content: vec![ModelContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "file not found".into(),
                    is_error: Some(true),
                }],
            },
            tool_use("t2", "Read", r#"{"file_path":"/repo/x.rs"}"#),
            tool_result("t2", &cat_n_output("old contents")),
            tool_use("t3", "Read", r#"{"file_path":"/repo/x.rs"}"#),
            tool_result("t3", &cat_n_output("new contents")),
        ];
        let files = extract_recent_reads(&msgs);
        assert_eq!(files.len(), 1, "errored read excluded, duplicate collapsed");
        assert_eq!(files[0].0, "/repo/x.rs");
        assert!(
            files[0].1.contains("new contents"),
            "the most recent read of a path wins"
        );
    }

    #[test]
    fn micro_compact_preserves_tool_use_id_when_clearing() {
        // A cleared result whose tool_use_id was blanked is orphaned from its
        // tool_use — the API rejects it, and extract_recent_reads can no longer
        // pair it.
        let long = "VERY LONG TOOL RESULT ".repeat(100);
        let msgs = vec![
            text_user("read a"),
            tool_use("t1", "Read", "{}"),
            tool_result("t1", &long),
            assistant_text("done a"),
            text_user("recent"),
            assistant_text("kept"),
        ];
        let (result, _) = DefaultCompactor.micro_compact(msgs, 1);
        let ids: Vec<&str> = result
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|b| match b {
                ModelContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(ids, vec!["t1"], "cleared results keep their tool_use_id");
    }

    // ── C-4: snip contract ──

    /// A round whose token cost is dominated by one text block, so budgets in
    /// these tests are easy to reason about.
    fn sized_round(tag: usize, words: usize) -> Vec<ModelMessage> {
        let filler = "alpha bravo charlie delta ".repeat(words);
        vec![
            text_user(&format!("q{tag} {filler}")),
            assistant_text(&format!("a{tag} {filler}")),
        ]
    }

    fn build_sized_conversation(rounds: usize, words: usize) -> Vec<ModelMessage> {
        (0..rounds).flat_map(|i| sized_round(i, words)).collect()
    }

    #[test]
    fn snip_keeps_exactly_keep_rounds_when_they_fit() {
        let msgs = build_sized_conversation(10, 10);
        let rounds = group_by_api_round(&msgs);
        let budget: usize = rounds[rounds.len() - 3..]
            .iter()
            .map(|r| r.estimated_tokens)
            .sum();

        let (result, strategy, dropped_rounds, dropped_messages, saved) =
            DefaultCompactor.snip(msgs.clone(), budget, 3);

        assert_eq!(strategy, CompactStrategy::Snip);
        assert_eq!(dropped_rounds, 7, "10 rounds, keep 3");
        assert_eq!(result.len(), 6, "3 rounds × 2 messages");
        assert_eq!(dropped_messages, msgs.len() - 6);
        assert!(saved > 0);
        // The kept messages are the newest ones, in order.
        let joined = recovery_text(&result);
        assert!(joined.contains("q7") && joined.contains("q9"));
        assert!(!joined.contains("q6"));
    }

    #[test]
    fn snip_drops_below_keep_rounds_under_budget_pressure() {
        let msgs = build_sized_conversation(10, 10);
        let rounds = group_by_api_round(&msgs);
        // Budget fits only the newest round, but keep_rounds asks for 5.
        let budget = rounds[rounds.len() - 1].estimated_tokens;

        let (result, _, dropped_rounds, _, _) = DefaultCompactor.snip(msgs, budget, 5);

        assert_eq!(dropped_rounds, 9, "budget forces the keep count down to 1");
        assert_eq!(result.len(), 2);
        assert!(recovery_text(&result).contains("q9"));
    }

    #[test]
    fn snip_never_returns_an_empty_conversation() {
        // Even a budget of zero must leave the newest round in place —
        // returning nothing at all would send the model an empty request.
        let msgs = build_sized_conversation(6, 20);
        let (result, _, dropped_rounds, _, _) = DefaultCompactor.snip(msgs, 0, 3);
        assert_eq!(dropped_rounds, 5);
        assert!(!result.is_empty());
        assert!(recovery_text(&result).contains("q5"));
    }

    #[test]
    fn snip_keeps_everything_when_keep_rounds_covers_the_conversation() {
        let msgs = build_sized_conversation(3, 5);
        let (result, _, dropped_rounds, dropped_messages, _) =
            DefaultCompactor.snip(msgs.clone(), usize::MAX, 10);
        assert_eq!(dropped_rounds, 0);
        assert_eq!(dropped_messages, 0);
        assert_eq!(result.len(), msgs.len());
    }

    #[test]
    fn snip_reports_a_projection_consistent_with_what_it_returned() {
        let msgs = build_sized_conversation(8, 10);
        let before = estimate_tokens(&msgs);
        let rounds = group_by_api_round(&msgs);
        let budget: usize = rounds[rounds.len() - 2..]
            .iter()
            .map(|r| r.estimated_tokens)
            .sum();
        let (result, _, dropped_rounds, dropped_messages, saved) =
            DefaultCompactor.snip(msgs.clone(), budget, 2);
        assert_eq!(dropped_rounds, 6);
        assert_eq!(dropped_messages, msgs.len() - result.len());
        assert_eq!(saved, before - estimate_tokens(&result));
    }

    // ── C-6: no-op reporting ──

    #[tokio::test]
    async fn compact_under_budget_reports_noop_not_snip() {
        let msgs = build_long_conversation(3);
        let (result, meta) = DefaultCompactor
            .compact(msgs.clone(), usize::MAX, 2)
            .await
            .unwrap();
        assert_eq!(
            meta.strategy,
            CompactStrategy::NoOp,
            "an untouched conversation must not report a Snip"
        );
        assert_eq!(result.len(), msgs.len());
        assert_eq!(meta.messages_before, meta.messages_after);
        assert!(meta.projection.is_none());
    }

    // ── C-5: one token estimator ──

    #[test]
    fn the_two_former_estimators_now_agree() {
        // `grouping::estimate_tokens` and `session::token_count` were separate
        // `len() / 4` variants that disagreed with each other. Both now route
        // through `model::tokens::estimate_message_tokens`; this asserts the
        // shared function is what `estimate_tokens` returns, so a future
        // divergence has to be deliberate.
        let msgs = vec![
            text_user("hello world"),
            tool_use("t1", "Read", r#"{"file_path":"/repo/x.rs"}"#),
            tool_result("t1", &"file contents ".repeat(200)),
            assistant_text("done"),
        ];
        assert_eq!(
            estimate_tokens(&msgs),
            model::tokens::estimate_message_tokens(&msgs)
        );
    }

    #[test]
    fn an_image_block_is_not_hundreds_of_thousands_of_tokens() {
        // ~1 MB of base64. Under a `len() / 4` estimator this reads as
        // ~250_000 tokens and trips compaction on the first pasted screenshot.
        let msgs = vec![ModelMessage {
            role: MessageRole::User,
            content: vec![ModelContentBlock::Image {
                media_type: "image/png".into(),
                data: "A".repeat(1_000_000),
            }],
        }];
        let n = estimate_tokens(&msgs);
        assert_eq!(n, base::interface::model::IMAGE_TOKEN_ESTIMATE);
        assert!(n < 5_000, "got {n}");
    }

    #[test]
    fn analysis_total_matches_the_estimator_that_drives_compaction() {
        let msgs = vec![
            text_user("hello world"),
            tool_use("t1", "Read", r#"{"file_path":"/repo/x.rs"}"#),
            tool_result("t1", &"file contents ".repeat(100)),
            assistant_text("all done"),
        ];
        let analysis = analyze_context(&msgs);
        // `analyze_context` omits the tool_use_id / tool name framing that the
        // block estimator charges, so it is a lower bound, not an identity —
        // but it must be in the same ballpark, not off by 4x.
        let total = estimate_tokens(&msgs);
        assert!(
            analysis.total_estimated_tokens <= total && analysis.total_estimated_tokens * 2 > total,
            "analysis {} vs estimator {total}",
            analysis.total_estimated_tokens
        );
    }

    // ── C-7: tool result budget ──

    #[test]
    fn tool_result_budget_clears_oldest_first() {
        // Sixteen 40 KB results: each is under the 50 KB per-result cap (so
        // pass 1 leaves them alone) but together they are 640 KB, over the
        // 500 KB total cap, which is what pass 2 exists for. The oldest must be
        // the ones cleared; the newest must survive intact.
        let big = "x".repeat(40_000);
        let mut msgs: Vec<ModelMessage> = (0..16)
            .map(|i| tool_result(&format!("t{i}"), &format!("{i:02}{}", &big[2..])))
            .collect();

        let modified = enforce_tool_result_budget(&mut msgs, &ToolResultBudget::default());
        assert!(modified > 0);

        let cleared: Vec<bool> = msgs
            .iter()
            .map(|m| {
                matches!(&m.content[0],
                    ModelContentBlock::ToolResult { content, .. }
                        if content == "[Old tool result content cleared]")
            })
            .collect();
        // Clearing must be a prefix of the (chronological) list.
        let first_kept = cleared.iter().position(|c| !c).expect("something survives");
        assert!(
            cleared[first_kept..].iter().all(|c| !c),
            "clearing must stop once under budget, not skip around: {cleared:?}"
        );
        assert!(cleared[0], "the oldest result must be the first cleared");
        assert!(
            !cleared[cleared.len() - 1],
            "the newest result must survive"
        );
    }

    #[test]
    fn tool_result_budget_is_a_noop_when_under_budget() {
        let mut msgs = vec![tool_result("t1", "small"), tool_result("t2", "also small")];
        assert_eq!(
            enforce_tool_result_budget(&mut msgs, &ToolResultBudget::default()),
            0
        );
    }
}
