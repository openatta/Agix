//! `PluginHost` — the entire surface installed plugins present to the engine.
//!
//! Everything a plugin can contribute arrives through this one trait, and the
//! host holds it as an `Option`. That is what lets the plugin subsystem be
//! compiled out (`daemon`'s `plugins` feature): with no implementation linked
//! in, the option is `None` and every call site is an `if let Some(..)` that
//! does nothing — no `#[cfg]` scattered through the engine, and no behavior
//! difference to reason about beyond "there are no plugins".
//!
//! There are five contribution points — tools, MCP servers, hook
//! subscriptions, scenes, agent types — plus two supporting methods that
//! serve them rather than adding new ones ([`PluginHost::hook_executor`] is
//! the backend for the hook subscriptions; [`PluginHost::permission_rules`]
//! is data the host merges). Each contribution point is a seam the engine
//! has to keep working across refactors, so adding a sixth is a decision to
//! argue for, not a detail to slip in.

use base::interface::scene::Scene;
use base::tool::Tool;
use std::sync::Arc;

/// What installed plugins contribute to a session.
///
/// Implementations are built once per daemon instance and shared; every
/// method may be called repeatedly and must be cheap enough for that.
pub trait PluginHost: Send + Sync {
    /// Tools to register into the session's tool registry, already wrapped as
    /// ordinary [`Tool`]s (a WASM component's exports, typically).
    fn tools(&self) -> Vec<Arc<dyn Tool>>;

    /// MCP server declarations, as `(name, raw config JSON)`.
    ///
    /// Unparsed on purpose: turning these into `mcp::config::McpServerConfig`
    /// here would make this crate's plugin seam depend on the MCP crate for a
    /// step the host already performs on its own configured servers. The host
    /// parses both through the same path.
    fn mcp_servers(&self) -> Vec<(String, serde_json::Value)>;

    /// Hook subscriptions, merged into the session's `HookRunner`.
    ///
    /// Plugins reach the engine's lifecycle through the hook dispatcher that
    /// already exists rather than through new call sites in the turn loop —
    /// the events they may subscribe to are a whitelist, enforced when the
    /// manifest is parsed.
    fn hook_configs(&self) -> Vec<(hooks::config::HookEvent, hooks::config::HookConfig)>;

    /// Scenes contributed by plugins, registered under `plugin:<name>` ids.
    ///
    /// A plugin that only adds tools contributes none; this is for the ones
    /// that want their own system prompt, tool surface and budgets, which
    /// they get by owning a scene instead of altering someone else's.
    fn scenes(&self) -> Vec<Arc<dyn Scene>>;

    /// Agent types plugins declare, discoverable through `AgentTool`'s
    /// `subagent_type`.
    ///
    /// These carry [`AgentTypeSource::Plugin`](crate::agent_tool::AgentTypeSource::Plugin),
    /// which is what clamps their `permission_mode` / `max_turns` overrides —
    /// see `agent_tool::apply_agent_type_overrides`.
    fn agent_types(&self) -> Vec<crate::agent_tool::AgentTypeDefinition>;

    /// The backend that runs `HookConfig::Wasm` entries, when this host has
    /// components loaded. `None` means the entries from
    /// [`hook_configs`](Self::hook_configs) would have nothing to answer
    /// them, and the dispatcher skips them with an explanation.
    fn hook_executor(&self) -> Option<Arc<dyn hooks::runner::WasmHookExecutor>> {
        None
    }

    /// Permission rules plugins contribute, tagged
    /// [`RuleSource::Plugin`](base::permission::RuleSource::Plugin) so that
    /// unloading a plugin can withdraw them in one call.
    fn permission_rules(&self) -> Vec<base::permission::PermissionRule> {
        Vec::new()
    }
}

/// A host that contributes nothing.
///
/// For tests and for embedders that want the seam present without wiring a
/// real implementation. Behaviorally identical to holding `None`.
pub struct NoPlugins;

impl PluginHost for NoPlugins {
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        Vec::new()
    }
    fn mcp_servers(&self) -> Vec<(String, serde_json::Value)> {
        Vec::new()
    }
    fn hook_configs(&self) -> Vec<(hooks::config::HookEvent, hooks::config::HookConfig)> {
        Vec::new()
    }
    fn scenes(&self) -> Vec<Arc<dyn Scene>> {
        Vec::new()
    }
    fn agent_types(&self) -> Vec<crate::agent_tool::AgentTypeDefinition> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_plugins_contributes_nothing_on_every_seam() {
        let h = NoPlugins;
        assert!(h.tools().is_empty());
        assert!(h.mcp_servers().is_empty());
        assert!(h.hook_configs().is_empty());
        assert!(h.scenes().is_empty());
        assert!(h.agent_types().is_empty());
        assert!(h.permission_rules().is_empty());
    }
}
