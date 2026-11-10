//! `Plugin` — a Rust plugin: one named bundle of registrations and
//! interceptions, compiled into the host and registered once per session.
//!
//! QuickJS scripts arrive through this same interface: `script-host` provides
//! a plugin in [`Layer::QuickJs`]. Where the two layers contribute to the same
//! point, the order is fixed by layer rather than by who called the builder
//! first: Rust runs first in a chain and innermost around a tool call, QuickJS
//! after and outside it, and a QuickJS registration whose origin is local takes
//! a name a Rust one also claims. Within one layer a name is taken once. See
//! `docs/extension_points.md`, 「叠加顺序」.
//!
//! Contracts — replacing a subsystem — are not here: they stay single-slot
//! setters on [`crate::agent::Builder`].

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use base::agent_type::AgentTypeDefinition;
use base::interface::health::HealthCheck;
use base::interface::memory_contracts::RetrievalHook;
use base::interface::model_interceptor::ModelInterceptor;
use base::interface::prompt_assembly::{AssemblyHook, AsyncAssemblyHook, Authority};
use base::interface::prompt_middleware::PromptMiddleware;
use base::interface::prompt_registry::{
    ContextProvider, PromptContent, PromptRegistry, RegisteredBlock, VariableProvider,
};
use base::interface::tool_middleware::ToolMiddleware;
use base::interface::tool_result::ToolResultTransformer;
use base::prompt::{BlockOrigin, BlockRole};
use base::tool::Tool;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layer {
    Rust,
    QuickJs,
}

pub trait Plugin: Send + Sync {
    /// Unique within a session; contributions that collide are reported
    /// against it.
    fn name(&self) -> &str;

    fn layer(&self) -> Layer {
        Layer::Rust
    }

    /// Called for every session the plugin is added to, sub-agents included,
    /// each time that session is built.
    fn register(&self, session: &PluginSession<'_>, r: &mut Registrar);
}

pub struct PluginSession<'a> {
    pub session_id: &'a str,
    pub project_root: &'a Path,
    pub scene: &'a str,
    /// 0 for a session a host created, parent + 1 for a delegate.
    pub depth: u32,
}

#[derive(Default)]
pub struct Registrar {
    pub(crate) tools: Vec<Arc<dyn Tool>>,
    pub(crate) agent_types: Vec<AgentTypeDefinition>,
    pub(crate) tool_middleware: Vec<Arc<dyn ToolMiddleware>>,
    pub(crate) tool_results: Vec<Arc<dyn ToolResultTransformer>>,
    pub(crate) model_interceptors: Vec<Arc<dyn ModelInterceptor>>,
    pub(crate) retrieval_hooks: Vec<Arc<dyn RetrievalHook>>,
    pub(crate) prompt_middleware: Vec<Arc<dyn PromptMiddleware>>,
    pub(crate) prompt_blocks: Vec<RegisteredBlock>,
    pub(crate) prompt_variables: Vec<(String, VariableProvider, BlockOrigin)>,
    pub(crate) assembly_hooks: Vec<(Arc<dyn AssemblyHook>, Authority)>,
    pub(crate) async_assembly_hooks: Vec<(Arc<dyn AsyncAssemblyHook>, Authority)>,
    pub(crate) health_checks: Vec<Arc<dyn HealthCheck>>,
    pub(crate) turn_starts: Vec<TurnStart>,
    pub(crate) admit_patterns: Vec<String>,
}

pub type TurnStart = Arc<dyn Fn() + Send + Sync>;

impl Registrar {
    pub fn tool(&mut self, tool: Arc<dyn Tool>) -> &mut Self {
        self.tools.push(tool);
        self
    }

    /// Let this plugin's own tools matching `pattern` — an exact name, or a
    /// prefix ending in `*` — through a scene's `tools` whitelist. The scene's
    /// `disallowed_tools` still removes them, and an agent type's
    /// `allowed_tools` still narrows a sub-agent. A pattern never reaches a
    /// built-in tool or another plugin's: it is matched only against what
    /// this plugin registers with [`tool`](Self::tool).
    pub fn admit_tools(&mut self, pattern: impl Into<String>) -> &mut Self {
        self.admit_patterns.push(pattern.into());
        self
    }

    pub fn agent_type(&mut self, definition: AgentTypeDefinition) -> &mut Self {
        self.agent_types.push(definition);
        self
    }

    /// `tool.around`.
    pub fn tool_middleware(&mut self, m: Arc<dyn ToolMiddleware>) -> &mut Self {
        self.tool_middleware.push(m);
        self
    }

    /// `tool.result`.
    pub fn tool_result(&mut self, t: Arc<dyn ToolResultTransformer>) -> &mut Self {
        self.tool_results.push(t);
        self
    }

    /// `model.request` and `model.message`.
    pub fn model_interceptor(&mut self, i: Arc<dyn ModelInterceptor>) -> &mut Self {
        self.model_interceptors.push(i);
        self
    }

    /// `memory.retrieval_hook`.
    pub fn retrieval_hook(&mut self, h: Arc<dyn RetrievalHook>) -> &mut Self {
        self.retrieval_hooks.push(h);
        self
    }

    /// `prompt.submit`.
    pub fn prompt_middleware(&mut self, m: Arc<dyn PromptMiddleware>) -> &mut Self {
        self.prompt_middleware.push(m);
        self
    }

    /// `prompt.block`.
    pub fn prompt_block(&mut self, block: RegisteredBlock) -> &mut Self {
        self.prompt_blocks.push(block);
        self
    }

    /// `prompt.context`.
    pub fn prompt_context(
        &mut self,
        name: impl Into<String>,
        order: i32,
        provider: ContextProvider,
    ) -> &mut Self {
        self.prompt_blocks.push(RegisteredBlock {
            name: name.into(),
            role: BlockRole::System,
            order,
            content: PromptContent::Provider(provider),
            cache_strategy: None,
            origin: BlockOrigin::Kernel,
        });
        self
    }

    /// `prompt.variable`.
    pub fn prompt_variable(
        &mut self,
        name: impl Into<String>,
        provider: VariableProvider,
    ) -> &mut Self {
        self.prompt_variable_from(name, provider, BlockOrigin::Kernel)
    }

    /// `prompt.variable`, answered on behalf of `origin`.
    pub fn prompt_variable_from(
        &mut self,
        name: impl Into<String>,
        provider: VariableProvider,
        origin: BlockOrigin,
    ) -> &mut Self {
        self.prompt_variables.push((name.into(), provider, origin));
        self
    }

    /// `prompt.assemble`, with the authority of the engine itself.
    pub fn assembly_hook(&mut self, hook: Arc<dyn AssemblyHook>) -> &mut Self {
        self.assembly_hooks
            .push((hook, Authority::local(BlockOrigin::Kernel)));
        self
    }

    /// `prompt.assemble`, for a pass that awaits something, under `authority`.
    pub fn async_assembly_hook(
        &mut self,
        hook: Arc<dyn AsyncAssemblyHook>,
        authority: Authority,
    ) -> &mut Self {
        self.async_assembly_hooks.push((hook, authority));
        self
    }

    /// `health.check`.
    pub fn health_check(&mut self, check: Arc<dyn HealthCheck>) -> &mut Self {
        self.health_checks.push(check);
        self
    }

    /// Called as each turn of the session begins.
    pub fn on_turn_start(&mut self, f: TurnStart) -> &mut Self {
        self.turn_starts.push(f);
        self
    }

    pub(crate) fn registers_on_prompt_registry(&self) -> bool {
        !self.prompt_blocks.is_empty()
            || !self.prompt_variables.is_empty()
            || !self.assembly_hooks.is_empty()
            || !self.async_assembly_hooks.is_empty()
    }

    pub(crate) fn append(&mut self, other: Registrar) {
        self.tools.extend(other.tools);
        self.agent_types.extend(other.agent_types);
        self.tool_middleware.extend(other.tool_middleware);
        self.tool_results.extend(other.tool_results);
        self.model_interceptors.extend(other.model_interceptors);
        self.retrieval_hooks.extend(other.retrieval_hooks);
        self.prompt_middleware.extend(other.prompt_middleware);
        self.prompt_blocks.extend(other.prompt_blocks);
        self.prompt_variables.extend(other.prompt_variables);
        self.assembly_hooks.extend(other.assembly_hooks);
        self.async_assembly_hooks.extend(other.async_assembly_hooks);
        self.health_checks.extend(other.health_checks);
        self.turn_starts.extend(other.turn_starts);
    }
}

pub(crate) struct Layers {
    pub rust: Registrar,
    pub quickjs: Registrar,
    /// Every tool a plugin registered, by name.
    pub tool_owners: HashMap<String, ToolOwner>,
}

pub(crate) struct ToolOwner {
    pub plugin: String,
    pub admitted: bool,
}

#[derive(Default)]
struct Claims {
    blocks: HashMap<String, String>,
    variables: HashMap<String, String>,
}

/// Register every plugin and sort what they gave by layer.
///
/// A plugin name is taken once per session, and so is a tool name, an agent
/// type name, and — within one layer — a prompt block or variable name. The
/// host's own registry counts as part of the Rust layer.
pub(crate) fn register_all(
    plugins: &[Arc<dyn Plugin>],
    session: &PluginSession<'_>,
    host_registry: Option<&dyn PromptRegistry>,
) -> Result<Layers, String> {
    let mut layers = Layers {
        rust: Registrar::default(),
        quickjs: Registrar::default(),
        tool_owners: HashMap::new(),
    };
    let mut plugin_names = HashSet::new();
    let mut tools = HashMap::new();
    let mut agent_types = HashMap::new();
    let mut rust = Claims::default();
    let mut quickjs = Claims::default();
    if let Some(host) = host_registry {
        let owner = "the host's prompt registry";
        for block in host.blocks() {
            rust.blocks
                .entry(block.name)
                .or_insert_with(|| owner.into());
        }
        for (name, _) in host.variables() {
            rust.variables.entry(name).or_insert_with(|| owner.into());
        }
    }

    for plugin in plugins {
        if !plugin_names.insert(plugin.name().to_string()) {
            return Err(format!(
                "two plugins are named {:?}; a plugin's name is unique within a session",
                plugin.name()
            ));
        }
        let owner = format!("plugin {:?}", plugin.name());
        let mut r = Registrar::default();
        plugin.register(session, &mut r);

        for tool in &r.tools {
            claim(&mut tools, tool.name(), &owner, "tool")?;
            let admitted = r
                .admit_patterns
                .iter()
                .any(|pattern| crate::tool_policy::covers(pattern, tool.name()));
            layers.tool_owners.insert(
                tool.name().to_string(),
                ToolOwner {
                    plugin: plugin.name().to_string(),
                    admitted,
                },
            );
        }
        for pattern in &r.admit_patterns {
            if !r
                .tools
                .iter()
                .any(|tool| crate::tool_policy::covers(pattern, tool.name()))
            {
                tracing::debug!(
                    plugin = plugin.name(),
                    pattern = %pattern,
                    "admit_tools pattern matches none of the tools this plugin registers"
                );
            }
        }
        for definition in &r.agent_types {
            claim(&mut agent_types, &definition.name, &owner, "agent type")?;
        }
        let (claims, target) = match plugin.layer() {
            Layer::Rust => (&mut rust, &mut layers.rust),
            Layer::QuickJs => (&mut quickjs, &mut layers.quickjs),
        };
        for block in &r.prompt_blocks {
            claim(&mut claims.blocks, &block.name, &owner, "prompt block")?;
        }
        for (name, _, _) in &r.prompt_variables {
            claim(&mut claims.variables, name, &owner, "prompt variable")?;
        }
        target.append(r);
    }
    Ok(layers)
}

fn claim(
    taken: &mut HashMap<String, String>,
    name: &str,
    owner: &str,
    kind: &str,
) -> Result<(), String> {
    if let Some(previous) = taken.get(name) {
        return Err(format!(
            "{owner} registers the {kind} {name:?}, which {previous} already registers"
        ));
    }
    taken.insert(name.to_string(), owner.to_string());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::interface::prompt_registry::InMemoryPromptRegistry;

    struct Blocks {
        name: &'static str,
        layer: Layer,
        block: &'static str,
    }

    impl Plugin for Blocks {
        fn name(&self) -> &str {
            self.name
        }
        fn layer(&self) -> Layer {
            self.layer
        }
        fn register(&self, _: &PluginSession<'_>, r: &mut Registrar) {
            r.prompt_block(RegisteredBlock::system(self.block, 10, "text"));
        }
    }

    fn plugin(name: &'static str, layer: Layer, block: &'static str) -> Arc<dyn Plugin> {
        Arc::new(Blocks { name, layer, block })
    }

    fn register(
        plugins: &[Arc<dyn Plugin>],
        host: Option<&dyn PromptRegistry>,
    ) -> Result<Layers, String> {
        let session = PluginSession {
            session_id: "s",
            project_root: Path::new("/p"),
            scene: "code",
            depth: 0,
        };
        register_all(plugins, &session, host)
    }

    #[test]
    fn a_plugin_name_is_taken_once() {
        let e = register(
            &[plugin("a", Layer::Rust, "x"), plugin("a", Layer::Rust, "y")],
            None,
        )
        .err()
        .expect("refused");
        assert!(e.contains("two plugins are named \"a\""), "{e}");
    }

    #[test]
    fn a_name_is_taken_once_within_a_layer_and_the_error_names_both() {
        let e = register(
            &[plugin("a", Layer::Rust, "x"), plugin("b", Layer::Rust, "x")],
            None,
        )
        .err()
        .expect("refused");
        assert!(
            e.contains("plugin \"b\"") && e.contains("plugin \"a\""),
            "{e}"
        );

        let host = InMemoryPromptRegistry::new();
        host.register_block(RegisteredBlock::system("x", 10, "host"));
        let e = register(&[plugin("a", Layer::Rust, "x")], Some(host.as_ref()))
            .err()
            .expect("refused");
        assert!(e.contains("the host's prompt registry"), "{e}");
    }

    #[test]
    fn the_two_layers_may_share_a_name() {
        let layers = register(
            &[
                plugin("a", Layer::Rust, "x"),
                plugin("b", Layer::QuickJs, "x"),
            ],
            None,
        )
        .expect("registered");
        assert_eq!(layers.rust.prompt_blocks.len(), 1);
        assert_eq!(layers.quickjs.prompt_blocks.len(), 1);
    }
}
