//! What a set of script bindings produced, sorted by where each piece has to
//! be installed.
//!
//! A carrier turns files into adapters and has no way to install them, because
//! they belong in three different places: a prompt registry, the session
//! builder, and the carriers a turn resets. This is that answer as a value.
//!
//! It lives beside the adapters rather than with the carrier so the
//! *installing* can be written once. It was written twice for a day — in the
//! daemon and in the test that proves a script reaches the model — and the
//! copy in the test went stale the moment two more kinds of adapter existed.
//! A comment saying "mirrors the daemon" does not prevent that; having one
//! place does.

use std::sync::Arc;

use crate::interface::prompt_assembly::Authority;
use crate::interface::prompt_registry::PromptRegistry;

/// What a set of bindings produced, sorted by where each piece has to be
/// installed.
///
/// The pieces go to different places: a tool-result transformer and a model
/// interceptor go on the session builder, and a prompt hook goes on the
/// registry. The caller owns those three places and puts each pile where it
/// belongs.
#[derive(Default)]
pub struct BoundScripts {
    /// What every script call did, in the order the points made them.
    ///
    /// Kept here rather than left to the caller because a script cannot keep
    /// a record of its own — a fresh runtime per call, nothing carried over —
    /// so whether one ran, and why it did not, is only answerable from
    /// outside. Every carrier below writes into this one.
    pub ledger: Arc<crate::interface::script::ScriptLedger>,
    /// Passes over the assembled prompt, with the authority each was
    /// granted by where its file lives.
    pub assembly_hooks: Vec<(
        Arc<dyn crate::interface::prompt_assembly::AsyncAssemblyHook>,
        Authority,
    )>,
    /// Blocks the scripts contribute, each already carrying the name and
    /// the order its script asked for.
    pub prompt_blocks: Vec<crate::interface::prompt_registry::RegisteredBlock>,
    /// What `{{name}}` expands to, by name, with whose script answers it.
    pub prompt_variables: Vec<(
        String,
        crate::interface::prompt_registry::VariableProvider,
        crate::prompt::BlockOrigin,
    )>,
    /// What a tool result may look like.
    pub tool_results: Vec<Arc<dyn crate::interface::tool_result::ToolResultTransformer>>,
    /// Both ends of memory recall.
    pub retrieval_hooks: Vec<Arc<dyn crate::interface::memory_contracts::RetrievalHook>>,
    /// A ring in front of every tool call.
    pub tool_middleware: Vec<Arc<dyn crate::interface::tool_middleware::ToolMiddleware>>,
    /// The request on its way out and the message on its way back.
    pub model_interceptors: Vec<Arc<dyn crate::interface::model_interceptor::ModelInterceptor>>,
    /// Every carrier that was built, so a turn can reset their quotas.
    ///
    /// Without this the per-turn budget is a per-session one: nothing
    /// would ever call `begin_turn`, and a script bound to a per-tool-call
    /// point would go quiet partway through a long session and stay quiet.
    pub carriers: Vec<Arc<crate::interface::script::ScriptCarrier>>,
}

impl std::fmt::Debug for BoundScripts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoundScripts")
            .field("assembly_hooks", &self.assembly_hooks.len())
            .field("prompt_blocks", &self.prompt_blocks.len())
            .field("prompt_variables", &self.prompt_variables.len())
            .field("tool_results", &self.tool_results.len())
            .field("retrieval_hooks", &self.retrieval_hooks.len())
            .field("tool_middleware", &self.tool_middleware.len())
            .field("model_interceptors", &self.model_interceptors.len())
            .field("ledger", &self.ledger.records().len())
            .finish()
    }
}

impl BoundScripts {
    pub fn is_empty(&self) -> bool {
        !self.registers_on_prompt_registry()
            && self.tool_results.is_empty()
            && self.retrieval_hooks.is_empty()
            && self.tool_middleware.is_empty()
            && self.model_interceptors.is_empty()
    }

    /// Whether [`apply_to_registry`](Self::apply_to_registry) has anything
    /// to install, and therefore whether the session needs a registry of
    /// its own at all.
    pub fn registers_on_prompt_registry(&self) -> bool {
        !self.assembly_hooks.is_empty()
            || !self.prompt_blocks.is_empty()
            || !self.prompt_variables.is_empty()
    }

    /// The half of this that a delegated agent inherits.
    ///
    /// A sub-agent, a team member and a background task each run their own
    /// session, and the question is which of the operator's scripts follow
    /// them there. The line is drawn between what *constrains* the engine and
    /// what *writes* a prompt:
    ///
    /// - the rings around tool calls and model calls travel, because a policy
    ///   that stops at the first delegation is not a policy. A script that
    ///   refuses `Bash` in this project, or redacts what a tool returns, would
    ///   otherwise be one `Agent` call away from being bypassed — by the
    ///   model, without anyone deciding it.
    /// - the prompt contributions stay, because they were written against the
    ///   prompt of the session that bound them. A delegate has its own scene
    ///   and its own prompt, and a block or an assembly pass aimed at one is
    ///   not aimed at the other.
    ///
    /// The carriers are left behind too, which is what makes the delegate's
    /// calls count against the *parent turn's* quota rather than getting a
    /// budget of their own: the delegate runs inside that turn, and
    /// `calls_per_turn` is supposed to bound the whole of it, delegated work
    /// included. Nothing calls `begin_turn` on a carrier the delegate does not
    /// hold, so its turns cannot reset a budget the parent is still spending.
    pub fn for_delegate(&self) -> Self {
        Self {
            assembly_hooks: Vec::new(),
            prompt_blocks: Vec::new(),
            prompt_variables: Vec::new(),
            tool_results: self.tool_results.clone(),
            retrieval_hooks: self.retrieval_hooks.clone(),
            tool_middleware: self.tool_middleware.clone(),
            model_interceptors: self.model_interceptors.clone(),
            carriers: Vec::new(),
            ledger: self.ledger.clone(),
        }
    }

    /// Install the prompt-side pieces on a registry.
    pub fn apply_to_registry(&self, registry: &dyn PromptRegistry) {
        for (hook, authority) in &self.assembly_hooks {
            registry.register_async_assembly_hook(hook.clone(), authority.clone());
        }
        for block in &self.prompt_blocks {
            registry.register_block(block.clone());
        }
        for (name, provider, _) in &self.prompt_variables {
            registry.register_variable(name, provider.clone());
        }
    }

    /// The prompt-side pieces, for a registry that layers them over the
    /// session's Rust registrations.
    pub fn prompt_contributions(&self) -> crate::interface::prompt_registry::ScriptContributions {
        crate::interface::prompt_registry::ScriptContributions {
            blocks: self.prompt_blocks.clone(),
            variables: self.prompt_variables.clone(),
            assembly_hooks: Vec::new(),
            async_assembly_hooks: self.assembly_hooks.clone(),
        }
    }
}
