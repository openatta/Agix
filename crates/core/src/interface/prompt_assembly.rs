//! The assembly hook point: a last pass over the whole prompt, with authority
//! that depends on where the pass came from.
//!
//! Registration ([`crate::interface::prompt_registry`]) covers contributing.
//! It does not cover *editing* — reading what everything else produced and
//! changing it. Some of what an extension legitimately wants is only
//! expressible that way: strip a section that does not apply to this
//! deployment, rewrite a paragraph the model keeps misreading, move a block
//! the cache would rather see earlier.
//!
//! # Editing a prompt is not the same act as adding to one
//!
//! Adding is bounded: whatever a contribution says, everything else still
//! says what it said. Removal and rewriting are not — a block that silently
//! deletes `rules` turns off a safety mechanism its author never agreed to
//! turn off, and the result looks exactly like a system that has no rules
//! configured.
//!
//! So authority follows provenance. Something the operator wrote themselves —
//! a script in their project, their own configuration — may do anything they
//! could have done by editing the prompt by hand, because it *is* them. A
//! plugin they downloaded may add, and may do more only if it declared the
//! capability, which is what an install-time disclosure has to show them.
//!
//! # A refusal is visible, not silent
//!
//! A denied edit returns [`Denied`] and is counted on the assembly. A hook
//! that ignores the error still cannot make the change, and the count is what
//! lets a host tell "this plugin does nothing" apart from "this plugin is
//! being stopped from doing something".

use std::sync::Arc;

use crate::interface::scene::ScenePromptContext;
use crate::prompt::{BlockOrigin, PromptBlock};

/// What a hook is allowed to do beyond adding.
///
/// Every field defaults to off. A plugin that declares nothing gets the
/// smallest authority that is still useful, which is the only default that is
/// safe to apply to code the operator did not read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AssemblyCapabilities {
    /// Rewrite an existing block's content.
    pub modify: bool,
    /// Take an existing block out.
    pub remove: bool,
    /// Move blocks relative to one another.
    pub reorder: bool,
}

impl AssemblyCapabilities {
    /// Everything. What an operator's own script gets.
    pub fn all() -> Self {
        Self {
            modify: true,
            remove: true,
            reorder: true,
        }
    }
}

/// Who a hook is, and what it may therefore do.
#[derive(Debug, Clone)]
pub struct Authority {
    pub origin: BlockOrigin,
    pub capabilities: AssemblyCapabilities,
}

impl Authority {
    /// A hook from something the operator wrote or configured. Unrestricted:
    /// it can do what they could have done by hand, because it is them.
    pub fn local(origin: BlockOrigin) -> Self {
        debug_assert!(
            origin.is_local(),
            "local authority is for kernel, config and project scripts"
        );
        Self {
            origin,
            capabilities: AssemblyCapabilities::all(),
        }
    }

    /// A hook from an installed plugin, with whatever it declared at install
    /// time and nothing more.
    pub fn plugin(name: impl Into<String>, capabilities: AssemblyCapabilities) -> Self {
        Self {
            origin: BlockOrigin::Plugin(name.into()),
            capabilities,
        }
    }

    fn may(&self, want: Capability) -> bool {
        // Provenance first: something the operator wrote needs to declare
        // nothing, because there is nobody for it to declare to.
        if self.origin.is_local() {
            return true;
        }
        match want {
            Capability::Modify => self.capabilities.modify,
            Capability::Remove => self.capabilities.remove,
            Capability::Reorder => self.capabilities.reorder,
        }
    }

    pub(crate) fn describe(&self) -> String {
        match &self.origin {
            BlockOrigin::Plugin(name) => format!("plugin '{name}'"),
            BlockOrigin::Script(path) => format!("script '{path}'"),
            BlockOrigin::Config => "configuration".to_string(),
            BlockOrigin::Kernel => "the engine".to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Capability {
    Modify,
    Remove,
    Reorder,
}

impl Capability {
    fn name(self) -> &'static str {
        match self {
            Self::Modify => "modify",
            Self::Remove => "remove",
            Self::Reorder => "reorder",
        }
    }
}

/// An edit that was not permitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Denied {
    /// What was attempted, in the words a disclosure would use.
    pub capability: &'static str,
    /// The block it was attempted on.
    pub block: String,
    /// Who attempted it.
    pub by: String,
}

impl std::fmt::Display for Denied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} may not {} the prompt block '{}': it did not declare that capability at install",
            self.by, self.capability, self.block
        )
    }
}

/// The prompt, mid-assembly, as one hook is allowed to see and change it.
pub struct PromptAssembly {
    blocks: Vec<PromptBlock>,
    authority: Authority,
    refusals: Vec<Denied>,
}

impl PromptAssembly {
    pub fn new(blocks: Vec<PromptBlock>, authority: Authority) -> Self {
        Self {
            blocks,
            authority,
            refusals: Vec::new(),
        }
    }

    pub fn blocks(&self) -> &[PromptBlock] {
        &self.blocks
    }

    /// Every edit this hook was not allowed to make.
    pub fn refusals(&self) -> &[Denied] {
        &self.refusals
    }

    pub fn into_blocks(self) -> Vec<PromptBlock> {
        self.blocks
    }

    /// Position of the block named `name`, if it is present.
    pub fn position(&self, name: &str) -> Option<usize> {
        self.blocks
            .iter()
            .position(|b| b.name.as_deref() == Some(name))
    }

    /// Add a block at the end. Always permitted — adding cannot unsay
    /// anything that was already said.
    pub fn push(&mut self, block: PromptBlock) {
        self.blocks.push(block);
    }

    /// Add a block directly after `name`, or at the end if there is no such
    /// block. Always permitted, for the same reason as [`push`](Self::push):
    /// nothing that was there stops being there.
    pub fn insert_after(&mut self, name: &str, block: PromptBlock) {
        match self.position(name) {
            Some(i) => self.blocks.insert(i + 1, block),
            None => self.blocks.push(block),
        }
    }

    /// Rewrite a block's content. Requires `modify`.
    ///
    /// `Ok(false)` means there was no such block — a hook targeting something
    /// optional should not have to care whether this session has it.
    pub fn modify(&mut self, name: &str, content: String) -> Result<bool, Denied> {
        self.guard(Capability::Modify, name)?;
        match self.position(name) {
            Some(i) => {
                self.blocks[i].content = content;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Take a block out. Requires `remove`.
    pub fn remove(&mut self, name: &str) -> Result<bool, Denied> {
        self.guard(Capability::Remove, name)?;
        match self.position(name) {
            Some(i) => {
                self.blocks.remove(i);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Move a block so it sits directly before `before`. Requires `reorder`.
    pub fn move_before(&mut self, name: &str, before: &str) -> Result<bool, Denied> {
        self.guard(Capability::Reorder, name)?;
        let (Some(from), Some(_)) = (self.position(name), self.position(before)) else {
            return Ok(false);
        };
        let block = self.blocks.remove(from);
        // Recomputed after the removal: taking the block out shifts anything
        // that was behind it, so the index read before would be off by one.
        let to = self.position(before).unwrap_or(self.blocks.len());
        self.blocks.insert(to, block);
        Ok(true)
    }

    fn guard(&mut self, want: Capability, block: &str) -> Result<(), Denied> {
        if self.authority.may(want) {
            return Ok(());
        }
        let denied = Denied {
            capability: want.name(),
            block: block.to_string(),
            by: self.authority.describe(),
        };
        tracing::warn!(%denied, "a prompt assembly edit was refused");
        self.refusals.push(denied.clone());
        Err(denied)
    }
}

/// A pass over the assembled prompt.
///
/// Waterfall: hooks run in registration order and each sees what the previous
/// one left. An error stops that hook, not the assembly — a broken extension
/// must not be able to take a session down with it, and the prompt without its
/// edits is still a prompt.
pub trait AssemblyHook: Send + Sync {
    fn on_assemble(
        &self,
        assembly: &mut PromptAssembly,
        ctx: &ScenePromptContext<'_>,
    ) -> Result<(), String>;
}

/// A pass over the assembled prompt that has to await something.
///
/// The synchronous [`AssemblyHook`] is the common case and stays
/// synchronous — a Rust hook that only rearranges strings should not be an
/// async function for the sake of the one backend that needs to be. This is
/// the variant a script carrier implements, since running the operator's code
/// means awaiting an engine.
#[async_trait::async_trait]
pub trait AsyncAssemblyHook: Send + Sync {
    async fn on_assemble_async(
        &self,
        assembly: &mut PromptAssembly,
        ctx: &ScenePromptContext<'_>,
    ) -> Result<(), String>;
}

/// Run every async hook over `blocks`, each under its own authority.
///
/// Separate from [`run_assembly_hooks`] rather than replacing it: prompt
/// assembly is a synchronous function called from the turn, and making the
/// whole of it async to accommodate a backend nobody has configured would put
/// an await point in every session's hot path to serve none of them.
pub async fn run_async_assembly_hooks(
    blocks: Vec<PromptBlock>,
    hooks: &[(Arc<dyn AsyncAssemblyHook>, Authority)],
    ctx: &ScenePromptContext<'_>,
) -> Vec<PromptBlock> {
    let mut blocks = blocks;
    for (hook, authority) in hooks {
        let mut assembly = PromptAssembly::new(blocks, authority.clone());
        if let Err(e) = hook.on_assemble_async(&mut assembly, ctx).await {
            tracing::warn!(
                by = %authority.describe(),
                error = %e,
                "an async prompt assembly hook failed; its edits up to that point stand"
            );
        }
        blocks = assembly.into_blocks();
    }
    blocks
}

/// Run every hook over `blocks`, each under its own authority.
pub fn run_assembly_hooks(
    blocks: Vec<PromptBlock>,
    hooks: &[(Arc<dyn AssemblyHook>, Authority)],
    ctx: &ScenePromptContext<'_>,
) -> Vec<PromptBlock> {
    let mut blocks = blocks;
    for (hook, authority) in hooks {
        let mut assembly = PromptAssembly::new(blocks, authority.clone());
        if let Err(e) = hook.on_assemble(&mut assembly, ctx) {
            tracing::warn!(
                by = %authority.describe(),
                error = %e,
                "a prompt assembly hook failed; its edits up to that point stand"
            );
        }
        blocks = assembly.into_blocks();
    }
    blocks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt::names;

    fn prompt() -> Vec<PromptBlock> {
        vec![
            PromptBlock::system("you are an agent").named(names::SCENE_SKELETON),
            PromptBlock::system("skills: a, b").named(names::SKILLS_CATALOG),
            PromptBlock::system("rules: no rm -rf").named(names::RULES),
        ]
    }

    fn tests_ctx() -> ScenePromptContext<'static> {
        use std::borrow::Cow;
        ScenePromptContext {
            cwd: Cow::Borrowed("/tmp"),
            os: Cow::Borrowed("linux"),
            shell: Cow::Borrowed("bash"),
            home_dir: Cow::Borrowed("/home/user"),
            date: Cow::Borrowed("2026-06-10"),
            model_name: Cow::Borrowed("test-model"),
            skills_text: None,
            mcp_instructions: None,
            session_memory: None,
            is_git: false,
            git_branch: None,
            is_worktree: false,
            git_status: None,
            language: None,
            scratchpad_dir: None,
            output_style_content: None,
            available_tools: None,
            tool_results_ever_cleared: false,
        }
    }

    fn downloaded(caps: AssemblyCapabilities) -> Authority {
        Authority::plugin("example", caps)
    }

    fn own_script() -> Authority {
        Authority::local(BlockOrigin::Script("./.agix/scripts/prompt.js".into()))
    }

    /// The acceptance property: something downloaded, declaring nothing,
    /// cannot delete a kernel block.
    #[test]
    fn a_downloaded_extension_that_declared_nothing_cannot_remove_a_kernel_block() {
        let mut asm = PromptAssembly::new(prompt(), downloaded(AssemblyCapabilities::default()));

        let refused = asm.remove(names::RULES).unwrap_err();
        assert_eq!(refused.capability, "remove");
        assert_eq!(refused.block, names::RULES);
        assert!(refused.by.contains("example"), "{refused}");

        assert!(
            asm.position(names::RULES).is_some(),
            "the block must still be there — a refused edit that half-applies is worse than none"
        );
        assert_eq!(asm.refusals().len(), 1, "a refusal must be visible");
    }

    #[test]
    fn a_downloaded_extension_may_always_add() {
        let mut asm = PromptAssembly::new(prompt(), downloaded(AssemblyCapabilities::default()));
        asm.insert_after(
            names::SCENE_SKELETON,
            PromptBlock::system("also this").named("example.note"),
        );
        assert_eq!(asm.position("example.note"), Some(1));
        assert!(
            asm.refusals().is_empty(),
            "adding needs no capability: it cannot unsay anything"
        );
    }

    #[test]
    fn a_declared_capability_is_the_one_that_is_granted_and_no_other() {
        let caps = AssemblyCapabilities {
            modify: true,
            ..Default::default()
        };
        let mut asm = PromptAssembly::new(prompt(), downloaded(caps));

        assert_eq!(
            asm.modify(names::SKILLS_CATALOG, "skills: a".into()),
            Ok(true)
        );
        assert!(asm.remove(names::RULES).is_err(), "remove was not declared");
        assert!(
            asm.move_before(names::RULES, names::SCENE_SKELETON)
                .is_err(),
            "reorder was not declared"
        );
    }

    #[test]
    fn a_script_the_operator_wrote_may_rewrite_anything() {
        let mut asm = PromptAssembly::new(prompt(), own_script());
        assert_eq!(
            asm.modify(names::RULES, "rules: rewritten".into()),
            Ok(true)
        );
        assert_eq!(asm.remove(names::SKILLS_CATALOG), Ok(true));
        assert_eq!(
            asm.move_before(names::RULES, names::SCENE_SKELETON),
            Ok(true)
        );
        let names: Vec<&str> = asm
            .blocks()
            .iter()
            .filter_map(|b| b.name.as_deref())
            .collect();
        assert_eq!(names, [names::RULES, names::SCENE_SKELETON]);
        assert!(asm.refusals().is_empty());
    }

    #[test]
    fn editing_a_block_that_is_not_there_is_not_an_error() {
        let mut asm = PromptAssembly::new(prompt(), own_script());
        assert_eq!(asm.modify("mcp.instructions", "x".into()), Ok(false));
        assert_eq!(asm.remove("mcp.instructions"), Ok(false));
    }

    /// Waterfall, and containment: the second hook sees the first hook's
    /// result, and a hook that fails does not take the assembly with it.
    #[test]
    fn hooks_see_each_others_work_and_a_failure_is_contained() {
        struct Appends(&'static str);
        impl AssemblyHook for Appends {
            fn on_assemble(
                &self,
                asm: &mut PromptAssembly,
                _ctx: &ScenePromptContext<'_>,
            ) -> Result<(), String> {
                asm.push(PromptBlock::system("x").named(self.0));
                Ok(())
            }
        }
        struct AppendsThenFails;
        impl AssemblyHook for AppendsThenFails {
            fn on_assemble(
                &self,
                asm: &mut PromptAssembly,
                _ctx: &ScenePromptContext<'_>,
            ) -> Result<(), String> {
                asm.push(PromptBlock::system("y").named("second.partial"));
                Err("gave up halfway".into())
            }
        }

        let hooks: Vec<(Arc<dyn AssemblyHook>, Authority)> = vec![
            (Arc::new(Appends("first")), own_script()),
            (Arc::new(AppendsThenFails), own_script()),
            (Arc::new(Appends("third")), own_script()),
        ];
        let out = run_assembly_hooks(prompt(), &hooks, &tests_ctx());
        let names: Vec<&str> = out.iter().filter_map(|b| b.name.as_deref()).collect();
        assert_eq!(
            &names[3..],
            ["first", "second.partial", "third"],
            "each hook builds on the last, and the one that failed still ran"
        );
    }
}
