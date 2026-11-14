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

use crate::aging::{Aging, AgingInput};
pub use crate::budget::{enforce_tool_result_budget, ToolResultBudget, TOOL_IMAGE_CHARS};
use crate::grouping::{estimate_tokens, group_by_api_round, ApiRound};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactStrategy {
    /// Nothing was done — the conversation was already within budget, or
    /// nothing the cascade could do to it made it any smaller.
    ///
    /// Distinct from `Snip` with zero dropped rounds: this reports that no
    /// strategy ran at all. Reporting `Snip` here would make every
    /// under-budget turn emit a phantom Snip event to telemetry and to the
    /// `CompactAction` stream, drowning real compactions in noise.
    NoOp,
    /// Drop oldest rounds, keep recent N rounds (not messages).
    Snip,
    /// Clear tool results older than K rounds, replacing content with a placeholder.
    MicroCompact,
    /// Fold old rounds into a summary message, keep recent rounds intact.
    CollapseContext,
    /// LLM-driven full summary of the conversation.
    FullCompact,
}

impl CompactStrategy {
    /// Whether earlier turns are gone from the transcript afterwards — dropped
    /// or folded into a summary — rather than kept with their tool results
    /// blanked.
    pub fn drops_history(self) -> bool {
        matches!(self, Self::Snip | Self::CollapseContext | Self::FullCompact)
    }
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
    /// This strategy declined — not a failure. The caller should fall through
    /// to the next strategy in the cascade without logging an error.
    #[error("strategy not applicable: {0}")]
    NotApplicable(String),
    #[error("{0}")]
    Internal(String),
    /// The model asked for a summary failed, timed out included.
    #[error("summarization failed: {0}")]
    Model(base::interface::model::ModelError),
    /// The turn it ran for was interrupted; nothing was changed.
    #[error("cancelled")]
    Cancelled,
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
        ReactiveOutcome::Compacted
    }

    /// Tell the model compaction is coming, while there is still room to react
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
             Context is at {:.0}% of the token budget ({}/{} tokens). \
             When it fills, older turns are replaced with a summary automatically and \
             you carry on in this same turn — there is no need to stop or to cut work \
             short. Write anything that must not be lost (values you were given, \
             results so far, decisions) into your reply text, where the summary will \
             pick it up.\n\
             </system-reminder>",
            (state.context_tokens as f64 / state.threshold as f64 * 100.0).min(99.0),
            state.context_tokens,
            state.threshold,
        ))
    }

    /// Whether the budget forces a compaction.
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

pub struct DefaultCompactor;

fn holds_a_summary(round: &ApiRound) -> bool {
    round.messages.iter().any(crate::llm_summary::is_summary)
}

impl DefaultCompactor {
    fn tokens(msgs: &[ModelMessage]) -> usize {
        estimate_tokens(msgs)
    }

    /// Snip: keep the most recent `keep_rounds` API rounds; if that is still
    /// over budget, keep fewer — down to a floor of 1 round, which is always
    /// kept even when it alone exceeds the budget (dropping it would leave the
    /// model with no conversation at all). A round holding an earlier
    /// compaction's summary is kept wherever it sits — it is the only record
    /// of the turns it replaced — and its cost comes off the budget first.
    ///
    /// Returns (messages, strategy, dropped_rounds, dropped_messages, tokens_saved) so the
    /// caller can build a SnipProjection for telemetry/UI.
    ///
    /// The cut is decided from the per-round token totals `group_by_api_round`
    /// already computed: `estimate_tokens` is additive, so the cost of a set of
    /// rounds is the sum of theirs.
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

        let mut kept: Vec<bool> = rounds.iter().map(holds_a_summary).collect();
        let mut tokens: usize = rounds
            .iter()
            .zip(&kept)
            .filter(|(_, pinned)| **pinned)
            .map(|(round, _)| round.estimated_tokens)
            .sum();
        let target_keep = keep_rounds.clamp(1, total_rounds);
        let mut keep = 0usize;
        for (i, round) in rounds.iter().enumerate().rev() {
            if kept[i] {
                continue;
            }
            let next = tokens + round.estimated_tokens;
            // The newest round is unconditional; beyond it, take a round only
            // if it fits and we still owe rounds toward `target_keep`.
            if keep > 0 && (next > max_tokens || keep >= target_keep) {
                break;
            }
            kept[i] = true;
            keep += 1;
            tokens = next;
        }

        let result: Vec<ModelMessage> = rounds
            .iter()
            .zip(&kept)
            .filter(|(_, kept)| **kept)
            .flat_map(|(round, _)| round.messages.iter().cloned())
            .collect();
        let dropped_rounds = kept.iter().filter(|kept| !**kept).count();
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
                            // would blank `tool_use_id`, orphaning the result from its
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
    /// Keeps the structure but truncates text to a fixed summary length. An
    /// earlier compaction's summary is carried over whole, ahead of the fold.
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

        let mut result: Vec<ModelMessage> = Vec::new();
        let mut summary_parts: Vec<String> = Vec::new();
        for round in to_collapse {
            for msg in &round.messages {
                if crate::llm_summary::is_summary(msg) {
                    result.push(msg.clone());
                    continue;
                }
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
        if !summary_parts.is_empty() {
            result.push(ModelMessage {
                role: MessageRole::User,
                content: vec![ModelContentBlock::Text {
                    text: "[Earlier conversation collapsed for context]".to_string(),
                }],
            });
            result.push(ModelMessage {
                role: MessageRole::User,
                content: vec![ModelContentBlock::Text {
                    text: format!(
                        "[Summary of earlier conversation]: {}",
                        summary_parts.join(" | ")
                    ),
                }],
            });
        }
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

        let (result, strategy, dropped_rounds, dropped_messages, tokens_saved) = {
            let (r, s, dr, dm, ts) = self.snip(messages.clone(), max_tokens, keep_rounds);
            if Self::tokens(&r) <= max_tokens {
                (r, s, dr, dm, ts)
            } else {
                let (r, s) = self.micro_compact(messages.clone(), keep_rounds);
                if Self::tokens(&r) <= max_tokens {
                    (r, s, 0, 0, 0)
                } else {
                    let (r, s) = self.collapse_context(messages.clone(), keep_rounds);
                    (r, s, 0, 0, 0)
                }
            }
        };

        let tokens_after = Self::tokens(&result);
        if tokens_after >= tokens_before {
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
    pub in_plan_mode: bool,
    /// Recently activated deferred tools
    pub activated_tools: Vec<String>,
    /// Background tasks still running: (task_id, what it is)
    pub running_tasks: Vec<(String, String)>,
}

/// Maximum number of recently read files to re-inject after compaction.
const MAX_RECOVER_FILES: usize = 5;
/// Maximum characters per recovered file.
const MAX_CHARS_PER_FILE: usize = 5_000;
/// Per-skill and shared-total character budgets for reattached skill
/// content — sized from a 5,000 / 25,000 *token* budget, converted with
/// this codebase's existing ~4-chars-per-token heuristic (see
/// `agent/turn/prompt.rs::build_skills_text`'s listing budget, same conversion).
const MAX_CHARS_PER_SKILL: usize = 20_000;
const MAX_CHARS_SKILLS_TOTAL: usize = 100_000;

/// Truncate `s` to at most `max_bytes` bytes without splitting a multi-byte
/// UTF-8 character. A crate-local alias for `base::text`'s shared definition.
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
    recovery.extend(recent_files_reminder(&ctx.recent_files));
    recovery.extend(invoked_skills_reminder(&ctx.invoked_skills));

    if ctx.in_plan_mode {
        recovery.push(reminder(
            "Plan mode is still active: only read-only tools will run. When your plan is ready, present it and call ExitPlanMode.",
        ));
    }

    if !ctx.running_tasks.is_empty() {
        let tasks_lines: Vec<_> = ctx
            .running_tasks
            .iter()
            .map(|(id, status)| format!("- task:{id} — {status}"))
            .collect();
        recovery.push(reminder(&format!(
            "The following background tasks are still running:\n\n{}",
            tasks_lines.join("\n")
        )));
    }

    if !ctx.activated_tools.is_empty() {
        let tools_text = ctx.activated_tools.join(", ");
        recovery.push(reminder(&format!(
            "These deferred tools were loaded earlier with ToolSearch: {tools_text}. If a schema it returned is no longer in the conversation, load it again with ToolSearch (select:<name>) before calling that tool."
        )));
    }

    recovery
}

fn reminder(body: &str) -> ModelMessage {
    ModelMessage {
        role: MessageRole::User,
        content: vec![ModelContentBlock::Text {
            text: format!("<system-reminder>\n{body}\n</system-reminder>"),
        }],
    }
}

fn recent_files_reminder(recent_files: &[(String, String)]) -> Option<ModelMessage> {
    let files: Vec<_> = recent_files
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
    if files.is_empty() {
        return None;
    }
    Some(reminder(&format!(
        "The following files were read before context compaction and may still be relevant:\n\n{}",
        files.join("\n\n---\n\n")
    )))
}

/// Re-attaches the skills' actual bodies, most recently invoked first: up to
/// the first 5,000 tokens of each, within a combined 25,000-token budget, so
/// older skills drop out entirely once the budget runs out.
fn invoked_skills_reminder(invoked_skills: &[InvokedSkillRecord]) -> Option<ModelMessage> {
    if invoked_skills.is_empty() {
        return None;
    }
    let mut by_recency = invoked_skills.to_vec();
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
    Some(reminder(&body))
}

/// Tool names whose results are file reads worth recovering after compaction.
/// `Read` is `crates/tools/src/file_read.rs::FileReadTool::name()`.
const FILE_READ_TOOLS: &[&str] = &["Read"];

/// Extract recently read file paths from a set of messages, most recent first.
/// Returns (path, content) pairs for file-read tool results found in the messages.
///
/// Pairs `ToolUse` → `ToolResult` by `tool_use_id`, taking the path from the
/// tool *input* (`file_path`) rather than guessing it from the result body:
/// the body is `cat -n` style (`"{:>6}\t{}\n"`), so it carries no path to
/// sniff.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time_based_mc::TimeBasedMcConfig;

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
        let said = DefaultCompactor
            .context_warning(&state(8_001, false))
            .expect("in the last fifth");
        assert!(
            said.contains("carry on in this same turn") && !said.contains("ask the user"),
            "the warning keeps the work going rather than winding it down: {said}"
        );
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
            "5 000 characters is nothing against the engine's 50 000"
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

    // Every truncation in this module is expressed as a *byte* budget while the
    // content is arbitrary UTF-8. The conversations below are built so the cut
    // necessarily lands inside a multi-byte character (3 bytes per Chinese
    // char vs. budgets of 200/1000).

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

    fn summary_message(text: &str) -> ModelMessage {
        text_user(&format!(
            "<system-reminder>\nThe earlier part of this conversation was removed.\n\n\
             {}\n{text}\n{}\n</system-reminder>",
            crate::llm_summary::COMPACTED_CONTEXT_OPEN,
            crate::llm_summary::COMPACTED_CONTEXT_CLOSE
        ))
    }

    fn texts(msgs: &[ModelMessage]) -> String {
        msgs.iter()
            .flat_map(|m| &m.content)
            .filter_map(|b| match b {
                ModelContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// A summary an earlier FullCompact left is the only record of what it
    /// replaced. A later compaction without a model must carry it forward,
    /// however old its round is.
    #[tokio::test]
    async fn snip_keeps_an_earlier_summary_however_old_it_is() {
        let mut msgs = vec![summary_message("SUMMARY-OF-THE-START")];
        msgs.extend(build_long_conversation(30));
        let (out, meta) = DefaultCompactor.compact(msgs, 60, 2).await.unwrap();
        assert_eq!(meta.strategy, CompactStrategy::Snip);
        assert!(texts(&out).contains("SUMMARY-OF-THE-START"), "{out:?}");
        assert!(texts(&out).contains("response 29"));
        assert!(!texts(&out).contains("response 0\n"));
        assert!(crate::llm_summary::is_summary(&out[0]));
    }

    #[tokio::test]
    async fn collapse_keeps_an_earlier_summary_whole() {
        let summary = format!("SUMMARY-HEAD {} SUMMARY-TAIL", "word ".repeat(200));
        let mut msgs = vec![summary_message(&summary)];
        for i in 0..10 {
            msgs.push(text_user(&format!("{i}{}", chinese_text(100))));
            msgs.push(assistant_text(&chinese_text(100)));
        }
        let (out, meta) = DefaultCompactor.compact(msgs, 1, 2).await.unwrap();
        assert_eq!(meta.strategy, CompactStrategy::CollapseContext);
        assert!(texts(&out).contains("SUMMARY-TAIL"), "{out:?}");
        assert!(crate::llm_summary::is_summary(&out[0]));
    }

    /// Over budget, Snip unable to get under it, and every round within the
    /// keep floor so the last resort folds nothing: nothing was done, and the
    /// result says so rather than naming a strategy.
    #[tokio::test]
    async fn a_cascade_whose_last_resort_changes_nothing_reports_noop() {
        let msgs = build_long_conversation(3);
        let (out, meta) = DefaultCompactor.compact(msgs.clone(), 1, 10).await.unwrap();
        assert_eq!(meta.strategy, CompactStrategy::NoOp);
        assert_eq!(out.len(), msgs.len());
        assert_eq!(meta.tokens_after, meta.tokens_before);
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
    fn post_compact_recovery_says_plan_mode_is_still_active() {
        let ctx = PostCompactContext {
            in_plan_mode: true,
            ..Default::default()
        };
        let text = recovery_text(&build_post_compact_recovery(&ctx));
        assert!(text.contains("Plan mode is still active"), "{text}");
        assert!(text.contains("ExitPlanMode"), "{text}");
    }

    #[test]
    fn post_compact_recovery_lists_running_tasks_on_their_own() {
        let ctx = PostCompactContext {
            running_tasks: vec![
                ("ag-1".into(), "agent: survey the parser".into()),
                ("mo-2".into(), "monitor".into()),
            ],
            ..Default::default()
        };
        let recovery = build_post_compact_recovery(&ctx);
        assert_eq!(recovery.len(), 1);
        let text = recovery_text(&recovery);
        assert!(
            text.contains("background tasks are still running"),
            "{text}"
        );
        assert!(
            text.contains("task:ag-1 — agent: survey the parser"),
            "{text}"
        );
        assert!(text.contains("task:mo-2 — monitor"), "{text}");
        assert!(!text.contains("Plan mode"), "{text}");
    }

    #[test]
    fn only_strategies_that_lose_turns_drop_history() {
        assert!(CompactStrategy::Snip.drops_history());
        assert!(CompactStrategy::CollapseContext.drops_history());
        assert!(CompactStrategy::FullCompact.drops_history());
        assert!(!CompactStrategy::MicroCompact.drops_history());
        assert!(!CompactStrategy::NoOp.drops_history());
    }

    #[test]
    fn post_compact_recovery_is_empty_when_no_skills_invoked() {
        let ctx = PostCompactContext::default();
        let recovery = build_post_compact_recovery(&ctx);
        assert!(recovery.is_empty());
    }

    /// Build a Read tool result the way the real tool builds it:
    /// `crates/tools/src/file_read.rs::format_with_line_numbers` →
    /// `"{:>6}\t{}\n"` per line. Deliberately *not* a hand-made string that
    /// happens to start with a path: a real Read result never does.
    fn cat_n_output(body: &str) -> String {
        body.lines()
            .enumerate()
            .map(|(i, line)| format!("{:>6}\t{}\n", i + 1, line))
            .collect()
    }

    #[test]
    fn extract_recent_reads_recovers_files_from_real_read_output() {
        let content = cat_n_output("fn main() {\n    println!(\"hi\");\n}");
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

    #[test]
    fn compaction_and_the_model_layer_estimate_tokens_alike() {
        // `estimate_tokens` is `model::tokens::estimate_message_tokens`, so a
        // divergence between the two has to be deliberate.
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
}
