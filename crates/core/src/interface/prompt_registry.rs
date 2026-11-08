//! `PromptRegistry` — contributing to the system prompt instead of replacing
//! it.
//!
//! Assembly was a function that took a scene and returned a list. Every block
//! in that list was decided in one place, so an extension wanting to add a
//! paragraph — or change one line of the skills inventory — had exactly one
//! move available: reimplement the whole scene. Registration is the other
//! shape. Contributions arrive from anywhere, each named, ordered and
//! revocable, and assembly becomes the act of merging them.
//!
//! # Ordering, and why it is a number
//!
//! Blocks sort by [`RegisteredBlock::order`], ascending, ties broken by
//! registration order. The kernel's stages sit at round hundreds (see
//! [`orders`]) so that "just before the skills inventory" and "after
//! everything" are both expressible without renumbering anything. Negative
//! orders come before the scene; anything past
//! [`orders::CONFIG_PROMPT_APPEND`] comes last.
//!
//! A named anchor would read better than a number and does not survive
//! contact with two extensions that both want to be immediately after the
//! same block. Numbers make that conflict visible and resolvable by whoever
//! is配置ing the system, rather than by whichever extension registered second.
//!
//! # Nothing registered is the identical prompt
//!
//! An empty registry assembles exactly what the hardcoded function did, byte
//! for byte, which is what makes this safe to land before anything registers.

use std::sync::{Arc, Mutex};

use crate::interface::prompt_assembly::{AssemblyHook, Authority};
use crate::interface::scene::ScenePromptContext;
use crate::prompt::{BlockOrigin, BlockRole, CacheStrategy};
use crate::tool::{Disposer, RegistrationId};

/// Produces content at assembly time, from the context the prompt is being
/// assembled for. `None` contributes nothing — the block is dropped, which is
/// how a contribution stays out of the prompt in the sessions it has nothing
/// to say about.
pub type ContextProvider =
    Arc<dyn for<'a> Fn(&ScenePromptContext<'a>) -> Option<String> + Send + Sync>;

/// Produces the value `{{name}}` expands to. `None` leaves the placeholder
/// alone rather than blanking it: a variable that could not be resolved is a
/// bug to see, not a hole to hide.
pub type VariableProvider =
    Arc<dyn for<'a> Fn(&ScenePromptContext<'a>) -> Option<String> + Send + Sync>;

/// Where a block's text comes from.
#[derive(Clone)]
pub enum PromptContent {
    /// Fixed at registration.
    Static(String),
    /// Computed per assembly.
    Provider(ContextProvider),
}

impl std::fmt::Debug for PromptContent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Static(s) => f.debug_tuple("Static").field(s).finish(),
            Self::Provider(_) => f.write_str("Provider(..)"),
        }
    }
}

/// A contribution to the system prompt.
#[derive(Clone, Debug)]
pub struct RegisteredBlock {
    /// How anything else refers to this block. See
    /// [`crate::prompt::names`] for the kernel's.
    pub name: String,
    pub role: BlockRole,
    /// Ascending. See the module docs.
    pub order: i32,
    pub content: PromptContent,
    pub cache_strategy: Option<CacheStrategy>,
    pub origin: BlockOrigin,
}

impl RegisteredBlock {
    /// A static system block at `order`, from the kernel.
    pub fn system(name: impl Into<String>, order: i32, content: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            role: BlockRole::System,
            order,
            content: PromptContent::Static(content.into()),
            cache_strategy: None,
            origin: BlockOrigin::Kernel,
        }
    }

    pub fn with_cache(mut self, strategy: CacheStrategy) -> Self {
        self.cache_strategy = Some(strategy);
        self
    }

    pub fn from_origin(mut self, origin: BlockOrigin) -> Self {
        self.origin = origin;
        self
    }
}

/// Where the kernel's own stages sit, so a contribution can be placed
/// relative to them.
///
/// Round hundreds, in the order assembly has always used. The gaps are the
/// point: an extension asking to sit between two kernel stages has ninety-nine
/// places to do it without anything being renumbered.
pub mod orders {
    /// The scene's own blocks, in the order the scene emitted them.
    pub const SCENE: i32 = 0;
    pub const SKILLS_CATALOG: i32 = 100;
    pub const MEMORY_SESSION: i32 = 200;
    pub const RULES: i32 = 300;
    pub const MCP_INSTRUCTIONS: i32 = 400;
    pub const CONFIG_PROMPT_APPEND: i32 = 500;
}

/// What a host, plugin or script contributes to the prompt.
pub trait PromptRegistry: Send + Sync {
    /// Add a block. Dispose to remove it.
    fn register_block(&self, block: RegisteredBlock) -> Disposer;

    /// Add a block whose text is computed at assembly time.
    ///
    /// Sugar for [`register_block`](Self::register_block) with
    /// [`PromptContent::Provider`] — the same thing, named after what it is
    /// usually for: context that only exists once the session is running.
    fn register_context(&self, name: &str, order: i32, provider: ContextProvider) -> Disposer {
        self.register_block(RegisteredBlock {
            name: name.to_string(),
            role: BlockRole::System,
            order,
            content: PromptContent::Provider(provider),
            cache_strategy: None,
            origin: BlockOrigin::Kernel,
        })
    }

    /// Make `{{name}}` expand to whatever `provider` returns, in every block.
    fn register_variable(&self, name: &str, provider: VariableProvider) -> Disposer;

    /// Add a pass over the whole assembled prompt — see
    /// [`crate::interface::prompt_assembly`]. `authority` is what that pass
    /// may do beyond adding, and is the registrar's to establish honestly:
    /// nothing downstream can tell a plugin apart from a script except by
    /// what it was registered as.
    fn register_assembly_hook(&self, hook: Arc<dyn AssemblyHook>, authority: Authority)
        -> Disposer;

    /// Everything registered, in registration order. Assembly sorts.
    fn blocks(&self) -> Vec<RegisteredBlock>;

    /// Every registered variable, by name.
    fn variables(&self) -> Vec<(String, VariableProvider)>;

    /// Every registered pass, in the order they run.
    fn assembly_hooks(&self) -> Vec<(Arc<dyn AssemblyHook>, Authority)> {
        Vec::new()
    }

    /// Add a pass that has to await something — a script carrier, most of
    /// all. Kept separate from [`register_assembly_hook`](Self::register_assembly_hook)
    /// so a Rust hook that only rearranges strings is not an async function
    /// for the sake of the one backend that has to be.
    fn register_async_assembly_hook(
        &self,
        _hook: Arc<dyn crate::interface::prompt_assembly::AsyncAssemblyHook>,
        _authority: Authority,
    ) -> Disposer {
        Disposer::inert(RegistrationId::next())
    }

    /// Every registered async pass, in the order they run.
    fn async_assembly_hooks(
        &self,
    ) -> Vec<(
        Arc<dyn crate::interface::prompt_assembly::AsyncAssemblyHook>,
        Authority,
    )> {
        Vec::new()
    }
}

/// One registered assembly pass and the authority it runs under.
type HookEntry = (RegistrationId, Arc<dyn AssemblyHook>, Authority);

/// The same, for a pass that has to await something.
type AsyncHookEntry = (
    RegistrationId,
    Arc<dyn crate::interface::prompt_assembly::AsyncAssemblyHook>,
    Authority,
);

/// A registry nothing has registered with, and nothing can.
///
/// The default: what assembly uses when the host supplied no registry, and
/// therefore the implementation that has to make "nothing registered assembles
/// the identical prompt" true by construction rather than by luck.
pub struct NoRegistrations;

impl PromptRegistry for NoRegistrations {
    fn register_block(&self, _block: RegisteredBlock) -> Disposer {
        Disposer::inert(RegistrationId::next())
    }

    fn register_variable(&self, _name: &str, _provider: VariableProvider) -> Disposer {
        Disposer::inert(RegistrationId::next())
    }

    fn register_assembly_hook(
        &self,
        _hook: Arc<dyn AssemblyHook>,
        _authority: Authority,
    ) -> Disposer {
        Disposer::inert(RegistrationId::next())
    }

    fn blocks(&self) -> Vec<RegisteredBlock> {
        Vec::new()
    }

    fn variables(&self) -> Vec<(String, VariableProvider)> {
        Vec::new()
    }
}

/// Holds registrations in memory for the life of a session.
#[derive(Default)]
pub struct InMemoryPromptRegistry {
    // `Arc`-shared with every disposer this hands out, so a disposer stays
    // valid — and inert rather than dangling — after the registry itself is
    // dropped.
    blocks: Arc<Mutex<Vec<(RegistrationId, RegisteredBlock)>>>,
    variables: Arc<Mutex<Vec<(RegistrationId, String, VariableProvider)>>>,
    hooks: Arc<Mutex<Vec<HookEntry>>>,
    async_hooks: Arc<Mutex<Vec<AsyncHookEntry>>>,
}

impl InMemoryPromptRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
}

impl PromptRegistry for InMemoryPromptRegistry {
    fn register_block(&self, block: RegisteredBlock) -> Disposer {
        let id = RegistrationId::next();
        self.blocks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((id, block));
        let blocks = self.blocks.clone();
        Disposer::new(id, move |id| {
            blocks
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|(entry, _)| *entry != id);
        })
    }

    fn register_variable(&self, name: &str, provider: VariableProvider) -> Disposer {
        let id = RegistrationId::next();
        self.variables
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((id, name.to_string(), provider));
        let variables = self.variables.clone();
        Disposer::new(id, move |id| {
            variables
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|(entry, _, _)| *entry != id);
        })
    }

    fn register_assembly_hook(
        &self,
        hook: Arc<dyn AssemblyHook>,
        authority: Authority,
    ) -> Disposer {
        let id = RegistrationId::next();
        self.hooks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((id, hook, authority));
        let hooks = self.hooks.clone();
        Disposer::new(id, move |id| {
            hooks
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|(entry, _, _)| *entry != id);
        })
    }

    fn blocks(&self) -> Vec<RegisteredBlock> {
        self.blocks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(_, b)| b.clone())
            .collect()
    }

    fn assembly_hooks(&self) -> Vec<(Arc<dyn AssemblyHook>, Authority)> {
        self.hooks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(_, h, a)| (h.clone(), a.clone()))
            .collect()
    }

    fn register_async_assembly_hook(
        &self,
        hook: Arc<dyn crate::interface::prompt_assembly::AsyncAssemblyHook>,
        authority: Authority,
    ) -> Disposer {
        let id = RegistrationId::next();
        self.async_hooks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((id, hook, authority));
        let hooks = self.async_hooks.clone();
        Disposer::new(id, move |id| {
            hooks
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|(entry, _, _)| *entry != id);
        })
    }

    fn async_assembly_hooks(
        &self,
    ) -> Vec<(
        Arc<dyn crate::interface::prompt_assembly::AsyncAssemblyHook>,
        Authority,
    )> {
        self.async_hooks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(_, h, a)| (h.clone(), a.clone()))
            .collect()
    }

    fn variables(&self) -> Vec<(String, VariableProvider)> {
        self.variables
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(_, n, p)| (n.clone(), p.clone()))
            .collect()
    }
}

/// Expand `{{name}}` for every registered variable.
///
/// Deliberately not a template language. An unregistered placeholder is left
/// exactly as written — a prompt that contains `{{` for its own reasons must
/// come out unchanged, and a variable that failed to resolve should be visible
/// rather than silently blank.
/// What a session's QuickJS scripts contribute to the prompt, fixed when the
/// session is built.
#[derive(Default)]
pub struct ScriptContributions {
    pub blocks: Vec<RegisteredBlock>,
    pub variables: Vec<(String, VariableProvider, BlockOrigin)>,
    pub async_assembly_hooks: Vec<(
        Arc<dyn crate::interface::prompt_assembly::AsyncAssemblyHook>,
        Authority,
    )>,
}

/// The Rust registrations with the QuickJS ones above them.
///
/// A name both layers claim goes to the script when the script's origin is
/// local; a packaged script may only add, so its claim on a name a Rust
/// registration holds is dropped. Assembly passes run Rust first. New
/// registrations land in the Rust layer.
pub struct LayeredPromptRegistry {
    native: Vec<Arc<dyn PromptRegistry>>,
    top: Arc<dyn PromptRegistry>,
    script: ScriptContributions,
}

impl LayeredPromptRegistry {
    /// The Rust layer is `host`, when there is one, read before `own`, which
    /// is where registrations made through this registry go.
    pub fn new(
        host: Option<Arc<dyn PromptRegistry>>,
        own: Arc<dyn PromptRegistry>,
        script: ScriptContributions,
    ) -> Self {
        let native = host.into_iter().chain([own.clone()]).collect();
        Self {
            native,
            top: own,
            script,
        }
    }

    fn top(&self) -> &Arc<dyn PromptRegistry> {
        &self.top
    }
}

impl PromptRegistry for LayeredPromptRegistry {
    fn register_block(&self, block: RegisteredBlock) -> Disposer {
        self.top().register_block(block)
    }

    fn register_variable(&self, name: &str, provider: VariableProvider) -> Disposer {
        self.top().register_variable(name, provider)
    }

    fn register_assembly_hook(
        &self,
        hook: Arc<dyn AssemblyHook>,
        authority: Authority,
    ) -> Disposer {
        self.top().register_assembly_hook(hook, authority)
    }

    fn register_async_assembly_hook(
        &self,
        hook: Arc<dyn crate::interface::prompt_assembly::AsyncAssemblyHook>,
        authority: Authority,
    ) -> Disposer {
        self.top().register_async_assembly_hook(hook, authority)
    }

    fn blocks(&self) -> Vec<RegisteredBlock> {
        let native: Vec<RegisteredBlock> = self.native.iter().flat_map(|r| r.blocks()).collect();
        let taken_by_script: std::collections::HashSet<&str> = self
            .script
            .blocks
            .iter()
            .filter(|b| b.origin.is_local())
            .map(|b| b.name.as_str())
            .collect();
        let native_names: std::collections::HashSet<String> =
            native.iter().map(|b| b.name.clone()).collect();
        let mut out: Vec<RegisteredBlock> = native
            .into_iter()
            .filter(|b| !taken_by_script.contains(b.name.as_str()))
            .collect();
        for block in &self.script.blocks {
            if !block.origin.is_local() && native_names.contains(&block.name) {
                tracing::warn!(
                    block = %block.name,
                    origin = ?block.origin,
                    "a packaged script may only add to the prompt; a Rust plugin already \
                     registers this block name, so the script's block is dropped"
                );
                continue;
            }
            out.push(block.clone());
        }
        out
    }

    fn variables(&self) -> Vec<(String, VariableProvider)> {
        // `interpolate` takes the first provider for a name, so order is precedence.
        let native: Vec<(String, VariableProvider)> =
            self.native.iter().flat_map(|r| r.variables()).collect();
        let mut out: Vec<(String, VariableProvider)> = self
            .script
            .variables
            .iter()
            .filter(|(_, _, origin)| origin.is_local())
            .map(|(n, p, _)| (n.clone(), p.clone()))
            .collect();
        for (name, provider, origin) in &self.script.variables {
            if origin.is_local() {
                continue;
            }
            if native.iter().any(|(n, _)| n == name) {
                tracing::warn!(
                    variable = %name,
                    origin = ?origin,
                    "a packaged script may only add to the prompt; a Rust plugin already \
                     provides this variable, so the script's is dropped"
                );
                continue;
            }
            out.push((name.clone(), provider.clone()));
        }
        out.extend(native);
        out
    }

    fn assembly_hooks(&self) -> Vec<(Arc<dyn AssemblyHook>, Authority)> {
        self.native
            .iter()
            .flat_map(|r| r.assembly_hooks())
            .collect()
    }

    fn async_assembly_hooks(
        &self,
    ) -> Vec<(
        Arc<dyn crate::interface::prompt_assembly::AsyncAssemblyHook>,
        Authority,
    )> {
        let mut out: Vec<_> = self
            .native
            .iter()
            .flat_map(|r| r.async_assembly_hooks())
            .collect();
        out.extend(self.script.async_assembly_hooks.iter().cloned());
        out
    }
}

pub fn interpolate(
    text: &str,
    variables: &[(String, VariableProvider)],
    ctx: &ScenePromptContext<'_>,
) -> String {
    if variables.is_empty() || !text.contains("{{") {
        return text.to_string();
    }
    let mut out = text.to_string();
    for (name, provider) in variables {
        let placeholder = format!("{{{{{name}}}}}");
        if !out.contains(&placeholder) {
            continue;
        }
        if let Some(value) = provider(ctx) {
            out = out.replace(&placeholder, &value);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ScenePromptContext<'static> {
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

    #[test]
    fn disposing_a_registration_takes_it_back_out() {
        let registry = InMemoryPromptRegistry::new();
        let disposer = registry.register_block(RegisteredBlock::system("mine", 50, "hello"));
        assert_eq!(registry.blocks().len(), 1);
        disposer.dispose();
        assert!(
            registry.blocks().is_empty(),
            "a contribution that cannot be withdrawn is a permanent edit"
        );
    }

    #[test]
    fn a_disposer_outliving_its_registry_is_inert_rather_than_dangling() {
        let registry = InMemoryPromptRegistry::new();
        let disposer = registry.register_block(RegisteredBlock::system("mine", 50, "hello"));
        drop(registry);
        disposer.dispose();
    }

    #[test]
    fn an_unregistered_placeholder_survives_untouched() {
        let vars: Vec<(String, VariableProvider)> = vec![(
            "known".into(),
            Arc::new(|_: &ScenePromptContext<'_>| Some("resolved".to_string())),
        )];
        assert_eq!(
            interpolate("{{known}} and {{unknown}}", &vars, &ctx()),
            "resolved and {{unknown}}",
            "a placeholder nobody registered is text, not a hole"
        );
    }

    #[test]
    fn a_variable_that_declines_leaves_its_placeholder_alone() {
        let vars: Vec<(String, VariableProvider)> =
            vec![("maybe".into(), Arc::new(|_: &ScenePromptContext<'_>| None))];
        assert_eq!(interpolate("a {{maybe}} b", &vars, &ctx()), "a {{maybe}} b");
    }

    #[test]
    fn nothing_registered_means_nothing_to_interpolate() {
        assert_eq!(interpolate("{{anything}}", &[], &ctx()), "{{anything}}");
    }

    fn layered(script: ScriptContributions) -> LayeredPromptRegistry {
        let native = InMemoryPromptRegistry::new();
        native.register_block(RegisteredBlock::system("shared", 10, "NATIVE"));
        native.register_variable(
            "who",
            Arc::new(|_: &ScenePromptContext<'_>| Some("native".into())),
        );
        LayeredPromptRegistry::new(None, native, script)
    }

    fn texts(r: &LayeredPromptRegistry) -> Vec<String> {
        r.blocks()
            .into_iter()
            .map(|b| match b.content {
                PromptContent::Static(s) => s,
                PromptContent::Provider(_) => String::new(),
            })
            .collect()
    }

    fn who(r: &LayeredPromptRegistry) -> String {
        interpolate("{{who}}", &r.variables(), &ctx())
    }

    #[test]
    fn a_local_script_takes_a_name_the_rust_layer_also_holds() {
        let r = layered(ScriptContributions {
            blocks: vec![RegisteredBlock::system("shared", 10, "SCRIPT")
                .from_origin(BlockOrigin::Script("a.js".into()))],
            variables: vec![(
                "who".into(),
                Arc::new(|_: &ScenePromptContext<'_>| Some("script".into())),
                BlockOrigin::Script("a.js".into()),
            )],
            async_assembly_hooks: Vec::new(),
        });
        assert_eq!(texts(&r), ["SCRIPT"]);
        assert_eq!(who(&r), "script");
    }

    #[test]
    fn a_packaged_script_only_adds() {
        let r = layered(ScriptContributions {
            blocks: vec![
                RegisteredBlock::system("shared", 10, "PACKAGED")
                    .from_origin(BlockOrigin::Plugin("pkg".into())),
                RegisteredBlock::system("its.own", 20, "ADDED")
                    .from_origin(BlockOrigin::Plugin("pkg".into())),
            ],
            variables: vec![(
                "who".into(),
                Arc::new(|_: &ScenePromptContext<'_>| Some("packaged".into())),
                BlockOrigin::Plugin("pkg".into()),
            )],
            async_assembly_hooks: Vec::new(),
        });
        assert_eq!(texts(&r), ["NATIVE", "ADDED"]);
        assert_eq!(who(&r), "native");
    }

    #[test]
    fn registering_through_the_layers_lands_in_the_rust_layer() {
        let r = layered(ScriptContributions::default());
        r.register_block(RegisteredBlock::system("later", 30, "LATER"));
        assert_eq!(texts(&r), ["NATIVE", "LATER"]);
    }
}
