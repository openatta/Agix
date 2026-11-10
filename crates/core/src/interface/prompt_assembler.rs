//! `PromptAssembler` — the algorithm that turns every contribution into the
//! blocks one request carries.
//!
//! Contributing a block and rewriting one are already open: a registration
//! adds, an `AssemblyHook` gets a last pass over the result. What was not open
//! is the assembly itself — the order stages are placed in, how a
//! contribution's order merges with the kernel's, where the cache boundaries
//! fall, whether two blocks stay two blocks. Those are decisions
//! [`assemble_prompt_with`](crate::prompt::assemble_prompt_with) makes, and a
//! deployment that wants different ones had to make them by bolting a hook on
//! the end and undoing the arrangement it had just been handed.
//!
//! # The inputs are a value
//!
//! Assembly reads seven things. Passing them as a struct is not a style
//! preference: an implementation that wants only two should not have to
//! restate the other five, and a new input should not break every
//! implementation that never asked for one.

use crate::interface::prompt_registry::PromptRegistry;
use crate::interface::scene::{Scene, ScenePromptContext};
use crate::memory::MemoryStore;
use crate::prompt::{BlockRole, CacheStrategy, PromptBlock};
use crate::settings::Settings;

/// Everything one assembly reads.
pub struct AssemblyRequest<'a> {
    /// What else asked to be in this prompt.
    pub registry: &'a dyn PromptRegistry,
    pub scene: &'a dyn Scene,
    pub settings: &'a Settings,
    pub memory_store: &'a MemoryStore,
    pub ctx: &'a ScenePromptContext<'a>,
    /// The rule documents the model is told exist, already merged.
    pub rules: &'a [crate::rules::RuleEntry],
    /// The skills inventory, already rendered.
    pub skills_text: Option<&'a str>,
    /// Usage guidance from the connected MCP servers, already rendered.
    pub mcp_instructions: Option<&'a str>,
}

/// How a request's system prompt gets built.
pub trait PromptAssembler: Send + Sync {
    fn assemble(&self, request: &AssemblyRequest<'_>) -> Vec<PromptBlock>;
}

/// The engine's own assembly, unchanged.
pub struct DefaultAssembler;

impl PromptAssembler for DefaultAssembler {
    fn assemble(&self, request: &AssemblyRequest<'_>) -> Vec<PromptBlock> {
        crate::prompt::assemble_prompt_with(request)
    }
}

/// Assembles as `inner` does, then folds the system blocks into one.
///
/// A real configuration rather than a stub. Anthropic allows four
/// `cache_control` breakpoints per request; the number of system blocks is not
/// bounded by anything, because plugins, scripts and config each contribute
/// their own. A deployment that has more contributors than breakpoints wants
/// one cached prefix rather than an arbitrary four of them, and providers that
/// take a single system string are going to concatenate these anyway — better
/// to decide where the joins are than to let an adapter decide.
///
/// The fold is order-preserving and touches nothing else: the blocks arrive
/// from `inner` already sorted, interpolated and hooked, and non-system blocks
/// keep their positions relative to the merged one.
pub struct MergedSystemPrompt {
    inner: Box<dyn PromptAssembler>,
    separator: String,
    /// The name the merged block carries.
    name: String,
}

impl MergedSystemPrompt {
    /// Two newlines between blocks, which is the join a reader of the
    /// resulting prompt would expect between sections.
    pub const DEFAULT_SEPARATOR: &'static str = "\n\n";
    pub const MERGED_NAME: &'static str = "system.merged";

    pub fn new(inner: Box<dyn PromptAssembler>) -> Self {
        Self {
            inner,
            separator: Self::DEFAULT_SEPARATOR.to_string(),
            name: Self::MERGED_NAME.to_string(),
        }
    }

    pub fn with_separator(mut self, separator: impl Into<String>) -> Self {
        self.separator = separator.into();
        self
    }
}

impl Default for MergedSystemPrompt {
    fn default() -> Self {
        Self::new(Box::new(DefaultAssembler))
    }
}

impl PromptAssembler for MergedSystemPrompt {
    fn assemble(&self, request: &AssemblyRequest<'_>) -> Vec<PromptBlock> {
        fold_system_blocks(self.inner.assemble(request), &self.separator, &self.name)
    }
}

fn fold_system_blocks(blocks: Vec<PromptBlock>, separator: &str, name: &str) -> Vec<PromptBlock> {
    let mut systems: Vec<String> = Vec::new();
    // The one breakpoint the merged block gets goes at its end, so it covers
    // every stage that asked to be cached rather than the first of them.
    let mut cache: Option<CacheStrategy> = None;
    let mut merged_at: Option<usize> = None;
    let mut out: Vec<PromptBlock> = Vec::new();

    for block in blocks {
        if block.role != BlockRole::System {
            out.push(block);
            continue;
        }
        if let Some(strategy) = block.cache_strategy {
            cache = Some(strategy);
        }
        if merged_at.is_none() {
            merged_at = Some(out.len());
            out.push(PromptBlock::system(String::new()));
        }
        systems.push(block.content);
    }

    if let Some(at) = merged_at {
        out[at] = PromptBlock {
            role: BlockRole::System,
            content: systems.join(separator),
            cache_strategy: cache,
            name: Some(name.to_string()),
            origin: crate::prompt::BlockOrigin::Kernel,
        };
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folded(blocks: Vec<PromptBlock>) -> Vec<PromptBlock> {
        fold_system_blocks(
            blocks,
            MergedSystemPrompt::DEFAULT_SEPARATOR,
            MergedSystemPrompt::MERGED_NAME,
        )
    }

    #[test]
    fn every_system_block_becomes_one_block_in_the_same_order() {
        let out = folded(vec![
            PromptBlock::system("first").named("a"),
            PromptBlock::system("second").named("b"),
            PromptBlock::system("third").named("c"),
        ]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].content, "first\n\nsecond\n\nthird");
        assert_eq!(
            out[0].name.as_deref(),
            Some(MergedSystemPrompt::MERGED_NAME)
        );
    }

    /// One breakpoint, at the end of everything that asked to be cached —
    /// putting it at the first such block would leave the rest of the merged
    /// prefix outside the cache it was asking for.
    #[test]
    fn the_merged_block_keeps_a_cache_breakpoint_if_any_stage_wanted_one() {
        let out = folded(vec![
            PromptBlock::system_cached("cached"),
            PromptBlock::system("plain"),
        ]);
        assert_eq!(out[0].cache_strategy, Some(CacheStrategy::Ephemeral));

        let out = folded(vec![PromptBlock::system("a"), PromptBlock::system("b")]);
        assert_eq!(out[0].cache_strategy, None);
    }

    #[test]
    fn a_block_that_is_not_a_system_block_is_left_where_it_was() {
        let out = folded(vec![
            PromptBlock::system("s1"),
            PromptBlock::user("u"),
            PromptBlock::system("s2"),
        ]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].content, "s1\n\ns2");
        assert_eq!(out[1].role, BlockRole::User);
    }

    #[test]
    fn nothing_to_merge_produces_nothing() {
        assert!(folded(Vec::new()).is_empty());
        let out = folded(vec![PromptBlock::user("u")]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].role, BlockRole::User);
    }
}
