//! Binds installed plugins to the engine's [`PluginHost`] seam.
//!
//! This crate is the only place that knows both halves: `plugin`'s on-disk
//! manifests on one side, `runtime`'s engine types on the other. Keeping the
//! translation here is what lets `runtime` stay free of any plugin dependency
//! and lets the whole subsystem be an optional dependency of the host — see
//! `runtime::plugin_host`.

pub mod config;
pub mod events;
pub mod scene;

pub use events::WasmEvents;
pub use scene::PluginScene;

use runtime::agent_tool::{AgentTypeDefinition, AgentTypeSource};
use runtime::plugin_host::PluginHost;
use std::path::Path;
use std::sync::Arc;
use wasm_host::health::HealthRegistry;
use wasm_host::{WasmEngine, WasmToolAdapter};

/// How many plugins may be loaded at once.
///
/// Loading runs a plugin's `init`, so it is not merely I/O: a bounded fan-out
/// keeps a machine with many plugins from starting all of them at once, while
/// still not making the tenth plugin wait for the ninth.
pub const MAX_CONCURRENT_LOADS: usize = 4;

/// How long the whole load phase may take.
///
/// The daemon awaits this before it serves, so an unbounded wait is a daemon
/// that never comes up. A plugin still loading when this expires is dropped
/// with a warning: starting without it is recoverable, never starting is not.
pub const LOAD_BUDGET: std::time::Duration = std::time::Duration::from_secs(30);

/// Every enabled plugin this daemon discovered, presented to the engine as
/// one host.
pub struct InstalledPlugins {
    plugins: Vec<plugin::manifest::Plugin>,
    /// Adapters for the tools each plugin's components export. Empty until
    /// [`load_components`](Self::load_components) has run — a manifest says
    /// what a plugin claims, only the component says what it provides.
    tools: Vec<Arc<dyn base::tool::Tool>>,
    /// Scenes plugins own, built once at load. Constructed alongside the
    /// tools because a plugin's own scene lists its own tools in
    /// `extra_tools`, and that list is only known after the components have
    /// been asked.
    scenes: Vec<Arc<dyn base::interface::scene::AgentScene>>,
    /// Loaded components by plugin name, so the event executor can find the
    /// one a `HookConfig::Wasm` entry names.
    instances: std::collections::HashMap<String, Arc<wasm_host::PluginInstance>>,
    /// Model-visible tool text, captured while the `ToolDef` was still in
    /// hand.
    ///
    /// Kept separately rather than read back off the registered tools,
    /// because `Tool` is a trait object and the long guide is not on the
    /// trait. Adding a downcast to the engine's core tool trait to serve an
    /// installer's disclosure would be the wrong trade.
    tool_text: Vec<ToolText>,
    /// Components that could not be loaded, by plugin, with the reason.
    ///
    /// A warning on the daemon's stderr is not a report. A plugin whose only
    /// component will not load stays listed and stays enabled and contributes
    /// nothing, and every question asked over RPC answers "fine" — the
    /// breaker counts faults from calls, and a component that never loaded is
    /// never called. This is what `plugin.list` says instead.
    component_faults: Vec<(String, String)>,
}

/// One tool's model-visible text, for [`InstalledPlugins::disclose`].
struct ToolText {
    plugin: String,
    tool: String,
    description: String,
    doc: Option<String>,
}

impl InstalledPlugins {
    pub fn new(plugins: Vec<plugin::manifest::Plugin>) -> Self {
        Self {
            plugins,
            tools: Vec::new(),
            scenes: Vec::new(),
            instances: std::collections::HashMap::new(),
            tool_text: Vec::new(),
            component_faults: Vec::new(),
        }
    }

    /// Why each component that did not load did not load, by plugin.
    pub fn component_faults(&self) -> &[(String, String)] {
        &self.component_faults
    }

    /// Compile, link and interrogate every declared WASM component.
    ///
    /// Separate from construction because it is async and slow — components
    /// are compiled here (or read from the AOT cache) and asked for their
    /// tool list. A plugin that fails any of these steps is dropped with a
    /// warning and the others still load: one broken package in a
    /// marketplace install must not cost the user the rest.
    pub async fn load_components(&mut self, engine: &WasmEngine, workspace: &Path) {
        self.load_components_with_health(engine, workspace, &HealthRegistry::new())
            .await;
    }

    /// As [`load_components`](Self::load_components), reusing fault records
    /// that outlive this host.
    ///
    /// The caller that rebuilds hosts — every install, uninstall, enable and
    /// disable does — passes the registry it keeps, so a plugin that
    /// disabled itself does not come back because the user touched another
    /// one.
    pub async fn load_components_with_health(
        &mut self,
        engine: &WasmEngine,
        workspace: &Path,
        health: &Arc<HealthRegistry>,
    ) {
        let loaded = match tokio::time::timeout(
            LOAD_BUDGET,
            load_all(&self.plugins, engine, workspace, health),
        )
        .await
        {
            Ok(loaded) => loaded,
            Err(_) => {
                tracing::warn!(
                    budget_secs = LOAD_BUDGET.as_secs(),
                    "plugin loading exceeded its budget; serving without the plugins that \
                     had not finished"
                );
                Vec::new()
            }
        };

        let mut tools: Vec<Arc<dyn base::tool::Tool>> = Vec::new();
        let mut scenes: Vec<Arc<dyn base::interface::scene::AgentScene>> = Vec::new();
        let mut instances = std::collections::HashMap::new();
        let mut tool_text: Vec<ToolText> = Vec::new();
        let mut component_faults: Vec<(String, String)> = Vec::new();

        for outcome in loaded {
            let Loaded {
                plugin,
                mut own_tools,
                mut text,
                instance,
                faults,
            } = outcome;
            for fault in faults {
                component_faults.push((plugin.name().to_string(), fault));
            }
            if let Some(instance) = instance {
                instances.insert(plugin.name().to_string(), instance);
            }
            // A plugin's own scene gets its own tools unconditionally: it is
            // the plugin's scene, so being able to name a tool it shipped is
            // not a privilege, it is the point.
            if let Some(scene) = crate::scene::PluginScene::from_plugin(&plugin, own_tools.clone())
            {
                scenes.push(Arc::new(scene));
            }
            tools.append(&mut own_tools);
            tool_text.append(&mut text);
        }

        self.tools = tools;
        self.scenes = scenes;
        self.instances = instances;
        self.tool_text = tool_text;
        self.component_faults = component_faults;
    }

    /// What one installed plugin will contribute, for an installer to put in
    /// front of the user.
    ///
    /// Enriched with the tool text only a loaded component can supply, which
    /// is why this lives here rather than in the manifest crate: the
    /// descriptions a plugin's tools carry into the model's context are not
    /// in the manifest at all.
    pub fn disclose(&self, name: &str) -> Option<Result<plugin::Disclosure, plugin::PluginError>> {
        let p = self.plugins.iter().find(|p| p.name() == name)?;
        let mut d = match plugin::Disclosure::from_plugin(p) {
            Ok(d) => d,
            Err(e) => return Some(Err(e)),
        };
        for text in self.tool_text.iter().filter(|t| t.plugin == name) {
            if let Err(e) = d.add_tool(&text.tool, &text.description, text.doc.as_deref()) {
                return Some(Err(e));
            }
        }
        Some(Ok(d))
    }

    /// Scene ids this host contributes, for the caller that has to register
    /// and later withdraw them.
    pub fn scene_ids(&self) -> Vec<String> {
        self.scenes.iter().map(|s| s.id().to_string()).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }

    pub fn names(&self) -> Vec<&str> {
        self.plugins
            .iter()
            .map(|p| p.manifest.plugin.name.as_str())
            .collect()
    }
}

impl PluginHost for InstalledPlugins {
    fn tools(&self) -> Vec<Arc<dyn base::tool::Tool>> {
        self.tools.clone()
    }

    /// Keyed `{plugin}-mcp-{declared name}` so two plugins declaring a
    /// server each don't collide when the host merges these into its own
    /// configured set.
    ///
    /// Only `kind = "native"` payloads produce a config here. A `dsh` payload
    /// names a JS entry module rather than a server, and reaches the host as
    /// an `agix-dsh-bridge` invocation — see that bridge's own docs.
    fn mcp_servers(&self) -> Vec<(String, serde_json::Value)> {
        self.plugins
            .iter()
            .flat_map(plugin::mcp::servers_for)
            .collect()
    }

    /// Only from plugins whose components actually loaded: a manifest that
    /// subscribes to an event but whose component failed to compile would
    /// otherwise register a hook that can never answer, and every event it
    /// claimed would pay a dispatch for nothing.
    fn hook_configs(&self) -> Vec<(hooks::config::HookEvent, hooks::config::HookConfig)> {
        self.plugins
            .iter()
            .filter(|p| self.instances.contains_key(p.name()))
            .flat_map(crate::events::hook_configs_for)
            .collect()
    }

    fn scenes(&self) -> Vec<Arc<dyn base::interface::scene::AgentScene>> {
        self.scenes.clone()
    }

    fn hook_executor(&self) -> Option<Arc<dyn hooks::runner::WasmHookExecutor>> {
        if self.instances.is_empty() {
            return None;
        }
        Some(Arc::new(crate::events::WasmEvents::new(
            self.instances.clone(),
        )))
    }

    fn agent_types(&self) -> Vec<AgentTypeDefinition> {
        self.plugins
            .iter()
            .flat_map(|p| {
                p.manifest
                    .agent
                    .iter()
                    .filter_map(|def| agent_def_to_type(def, &p.root))
            })
            .collect()
    }
}

/// Compile every component a freshly installed plugin declares.
///
/// Runs at install, not at first load, and a failure fails the install:
/// installing a plugin that cannot be loaded is worse than not installing
/// it, because the failure surfaces later and somewhere else.
///
/// With the compiler linked in this is a direct call. Without it — the
/// hardened build, where the daemon has no Cranelift at all — the work goes
/// to `agix-plugin-compile`, located the same way the DSH bridge is.
pub async fn precompile_plugin(dir: &Path) -> anyhow::Result<()> {
    // What the package declares decides whether a compiler is needed at all.
    // Asking the manifest first is what keeps a script-only or MCP-only
    // package installable on a build that carries no compiler: going looking
    // for `agix-plugin-compile` on its behalf failed the install over
    // components the package never had.
    let p = plugin::manifest::Plugin::load(dir, &dir.join("plugin.toml"))?;
    if p.manifest.wasm.is_empty() {
        return Ok(());
    }

    #[cfg(feature = "compile")]
    {
        let compiled = compile_in_process(&p)?;
        tracing::info!(
            dir = %dir.display(),
            components = compiled,
            "precompiled plugin components"
        );
        Ok(())
    }
    #[cfg(not(feature = "compile"))]
    {
        compile_out_of_process(dir).await
    }
}

#[cfg(feature = "compile")]
fn compile_in_process(p: &plugin::manifest::Plugin) -> anyhow::Result<usize> {
    let engine = WasmEngine::new()?;
    let mut n = 0;
    for payload in &p.manifest.wasm {
        engine.precompile(&p.path(&payload.component), &p.root)?;
        n += 1;
    }
    Ok(n)
}

#[cfg(not(feature = "compile"))]
async fn compile_out_of_process(dir: &Path) -> anyhow::Result<()> {
    let exe = plugin::locate::locate_tool("agix-plugin-compile", "AGIX_PLUGIN_COMPILE")
        .ok_or_else(|| {
            anyhow::anyhow!(
                "this build cannot compile plugin components and `agix-plugin-compile` was not \
             found; set AGIX_PLUGIN_COMPILE to its path"
            )
        })?;
    let out = tokio::process::Command::new(&exe)
        .arg(dir)
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("running {}: {e}", exe.display()))?;
    if !out.status.success() {
        anyhow::bail!(
            "compiling this plugin's components failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// What loading one plugin produced.
struct Loaded {
    plugin: plugin::manifest::Plugin,
    own_tools: Vec<Arc<dyn base::tool::Tool>>,
    text: Vec<ToolText>,
    instance: Option<Arc<wasm_host::PluginInstance>>,
    faults: Vec<String>,
}

/// Load every plugin, at most [`MAX_CONCURRENT_LOADS`] at a time.
///
/// Results come back in the order the plugins were declared rather than the
/// order they happened to finish, so what a daemon serves does not depend on
/// which plugin's `init` was slow today.
async fn load_all(
    plugins: &[plugin::manifest::Plugin],
    engine: &WasmEngine,
    workspace: &Path,
    health: &Arc<HealthRegistry>,
) -> Vec<Loaded> {
    use futures::stream::StreamExt;

    futures::stream::iter(plugins.iter().cloned())
        .map(|p| async move {
            let mut own_tools: Vec<Arc<dyn base::tool::Tool>> = Vec::new();
            let mut text: Vec<ToolText> = Vec::new();
            let mut instance = None;
            let mut faults: Vec<String> = Vec::new();
            for payload in &p.manifest.wasm {
                match load_payload(engine, &p, payload, workspace, health).await {
                    Ok((inst, mut loaded, mut t)) => {
                        own_tools.append(&mut loaded);
                        text.append(&mut t);
                        instance = Some(inst);
                    }
                    Err(e) => {
                        tracing::warn!(
                            plugin = %p.name(),
                            component = %payload.component.display(),
                            error = %e,
                            "plugin component could not be loaded; skipping it"
                        );
                        // Named by component, because a plugin may declare
                        // more than one and "it did not load" is not enough
                        // to act on when only one of them did not.
                        faults.push(format!("{}: {e:#}", payload.component.display()));
                    }
                }
            }
            Loaded {
                plugin: p,
                own_tools,
                text,
                instance,
                faults,
            }
        })
        .buffered(MAX_CONCURRENT_LOADS)
        .collect()
        .await
}

/// Load one `[[wasm]]` payload and adapt each tool it exports.
async fn load_payload(
    engine: &WasmEngine,
    plugin: &plugin::manifest::Plugin,
    payload: &plugin::manifest::WasmPayload,
    workspace: &Path,
    health: &Arc<HealthRegistry>,
) -> anyhow::Result<(
    Arc<wasm_host::PluginInstance>,
    Vec<Arc<dyn base::tool::Tool>>,
    Vec<ToolText>,
)> {
    let caps = Arc::new(wasm_host::capabilities::resolve(
        &payload.capabilities,
        workspace,
        &plugin.root,
    )?);
    // Before anything is compiled: a configuration the plugin cannot accept
    // is a reason not to load it, and finding that out first saves the work.
    let config = crate::config::load_config(plugin)?;

    let component = engine.load(&plugin.path(&payload.component), &plugin.root)?;
    let instance = Arc::new(wasm_host::PluginInstance::link_with_health(
        engine,
        &component,
        plugin.name().to_string(),
        caps,
        health.for_plugin(plugin.name()),
    )?);

    // Nothing has cancelled anything yet — this runs at load, before any
    // session exists to withdraw the request.
    let cancel = tokio_util::sync::CancellationToken::new();

    // The plugin gets the last word on its own configuration: the schema
    // says what is well-formed, only the plugin knows what is workable.
    instance
        .init(&config.to_string(), &cancel)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let defs = instance
        .list_tools(&cancel)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let tools = defs
        .iter()
        .map(|def| {
            Arc::new(WasmToolAdapter::new(instance.clone(), plugin.name(), def))
                as Arc<dyn base::tool::Tool>
        })
        .collect();
    let text = defs
        .iter()
        .map(|def| ToolText {
            plugin: plugin.name().to_string(),
            tool: def.name.clone(),
            description: def.description.clone(),
            doc: def.doc.clone(),
        })
        .collect();
    Ok((instance, tools, text))
}

/// Convert a plugin-declared `AgentDef` into a runtime [`AgentTypeDefinition`],
/// reading its system prompt file relative to the plugin's root.
///
/// Returns `None` (with a `tracing::warn!`) when the prompt file can't be
/// read, so one bad definition doesn't break agent type resolution for
/// everyone else.
///
/// The result carries [`AgentTypeSource::Plugin`], which is what clamps its
/// `permission_mode` / `max_turns` overrides — a definition that arrived over
/// the network must not be able to hand its sub-agent more than the session
/// already had.
pub fn agent_def_to_type(
    def: &plugin::manifest::AgentDef,
    plugin_root: &Path,
) -> Option<AgentTypeDefinition> {
    let prompt_path = plugin_root.join(&def.prompt);
    let system_prompt = match std::fs::read_to_string(&prompt_path) {
        Ok(body) => body,
        Err(e) => {
            tracing::warn!(
                agent = %def.name,
                path = %prompt_path.display(),
                error = %e,
                "plugin agent type: failed to read system prompt, skipping"
            );
            return None;
        }
    };

    let permission_mode = def.permission_mode.as_deref().and_then(|raw| {
        let parsed = runtime::agent_tool::parse_permission_mode(raw);
        if parsed.is_none() {
            tracing::warn!(
                agent = %def.name,
                permission_mode = %raw,
                "plugin agent type: unrecognized permission_mode, ignoring"
            );
        }
        parsed
    });

    Some(AgentTypeDefinition {
        name: def.name.clone(),
        source: AgentTypeSource::Plugin,
        description: def.description.clone(),
        allowed_tools: def.allowed_tools.clone(),
        disallowed_tools: def.disallowed_tools.clone(),
        model: def.model.clone(),
        permission_mode,
        effort: def.effort.clone(),
        max_turns: def.max_turns,
        scene: def.scene.clone(),
        system_prompt,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEAD: &str = "[plugin]\nname = \"p\"\nversion = \"1.0.0\"\napi_version = \"0.1\"\n";

    fn load(root: &Path, body: &str) -> plugin::manifest::Plugin {
        std::fs::write(root.join("plugin.toml"), format!("{HEAD}{body}")).unwrap();
        plugin::manifest::Plugin::load(root, &root.join("plugin.toml")).unwrap()
    }

    fn agent_body(prompt_rel: &str) -> String {
        format!(
            r#"
[[agent]]
name = "helper"
description = "A plugin-declared agent"
prompt = "{prompt_rel}"
allowed_tools = ["Read"]
disallowed_tools = ["Bash"]
model = "claude-opus-5"
permission_mode = "plan"
effort = "high"
max_turns = 30
scene = "plugin:p"
"#
        )
    }

    /// Every field the manifest offers has to arrive in the engine type. A
    /// field that parses but is never mapped is worse than one that doesn't
    /// exist: the author sees it accepted and assumes it took effect.
    #[test]
    fn every_declared_agent_field_reaches_the_engine_type() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("prompt.md"), "You are a plugin agent.").unwrap();
        let p = load(dir.path(), &agent_body("prompt.md"));

        let types = InstalledPlugins::new(vec![p]).agent_types();
        assert_eq!(types.len(), 1);
        let t = &types[0];

        assert_eq!(t.name, "helper");
        assert_eq!(t.system_prompt, "You are a plugin agent.");
        assert_eq!(t.allowed_tools, ["Read"]);
        assert_eq!(t.disallowed_tools, ["Bash"]);
        assert_eq!(t.model.as_deref(), Some("claude-opus-5"));
        assert_eq!(
            t.permission_mode,
            Some(base::interface::settings::PermissionMode::Plan)
        );
        assert_eq!(t.effort.as_deref(), Some("high"));
        assert_eq!(t.max_turns, Some(30));
        assert_eq!(t.scene.as_deref(), Some("plugin:p"));
        assert_eq!(
            t.source,
            AgentTypeSource::Plugin,
            "provenance is what clamps this definition's permission overrides"
        );
    }

    #[test]
    fn an_unreadable_prompt_skips_that_definition_rather_than_failing() {
        let dir = tempfile::tempdir().unwrap();
        let p = load(dir.path(), &agent_body("does-not-exist.md"));
        assert!(InstalledPlugins::new(vec![p]).agent_types().is_empty());
    }

    /// An unrecognized mode is dropped, not guessed at — and dropping it
    /// leaves the sub-agent on the session's inherited mode, which is the
    /// safe direction.
    #[test]
    fn an_unrecognized_permission_mode_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("prompt.md"), "body").unwrap();
        let p = load(
            dir.path(),
            r#"
[[agent]]
name = "helper"
description = "d"
prompt = "prompt.md"
permission_mode = "make-me-root"
"#,
        );
        let types = InstalledPlugins::new(vec![p]).agent_types();
        assert_eq!(types.len(), 1);
        assert_eq!(types[0].permission_mode, None);
    }

    /// A DSH payload becomes a stdio server that runs the bridge, so the JS
    /// runtime stays outside the host — the reason DSH plugins arrive over
    /// MCP at all.
    #[test]
    fn a_dsh_payload_becomes_a_bridge_invocation() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("dist")).unwrap();
        std::fs::write(
            dir.path().join("dist/index.js"),
            "export function apply() {}",
        )
        .unwrap();
        std::env::set_var("AGIX_TEST_DSH_TOKEN", "secret");
        let p = load(
            dir.path(),
            r#"
[[mcp]]
name = "pr-helper"
kind = "dsh"
entry = "dist/index.js"
env = ["AGIX_TEST_DSH_TOKEN", "AGIX_TEST_UNSET"]
"#,
        );

        let servers = InstalledPlugins::new(vec![p]).mcp_servers();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].0, "p-mcp-pr-helper");

        let cfg = &servers[0].1;
        assert_eq!(cfg["type"], "stdio");
        assert_eq!(cfg["command"], "node");
        let args = cfg["args"].as_array().unwrap();
        assert!(
            args[0].as_str().unwrap().ends_with("main.js"),
            "the bridge runs first: {args:?}"
        );
        assert!(
            args[1].as_str().unwrap().ends_with("dist/index.js"),
            "the plugin entry is its argument: {args:?}"
        );
        assert_eq!(cfg["env"]["AGIX_TEST_DSH_TOKEN"], "secret");
        assert!(
            cfg["env"].get("AGIX_TEST_UNSET").is_none(),
            "a variable that isn't set is simply absent, not empty"
        );
    }

    /// An entry module that isn't there would produce a server that fails to
    /// start on every session. Better to notice at load.
    #[test]
    fn a_dsh_payload_with_no_entry_module_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let p = load(
            dir.path(),
            "\n[[mcp]]\nname = \"gone\"\nkind = \"dsh\"\nentry = \"dist/missing.js\"\n",
        );
        assert!(InstalledPlugins::new(vec![p]).mcp_servers().is_empty());
    }

    #[test]
    fn native_mcp_payloads_are_namespaced_and_dsh_ones_are_not_servers() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("mcp")).unwrap();
        std::fs::write(
            dir.path().join("mcp/server.json"),
            r#"{"command":"echo","args":[]}"#,
        )
        .unwrap();
        let p = load(
            dir.path(),
            r#"
[[mcp]]
name = "github"
kind = "native"
config = "mcp/server.json"

[[mcp]]
name = "pr-helper"
kind = "dsh"
entry = "dist/index.js"
"#,
        );

        let servers = InstalledPlugins::new(vec![p]).mcp_servers();
        // The dsh payload's entry module does not exist here, so only the
        // native one produces a server.
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].0, "p-mcp-github");
        assert_eq!(servers[0].1["command"], "echo");
    }

    /// A host with nothing installed must be indistinguishable from having no
    /// host at all — that equivalence is what makes the subsystem removable.
    #[test]
    fn an_empty_host_contributes_nothing() {
        let host = InstalledPlugins::new(Vec::new());
        assert!(host.is_empty());
        assert!(host.agent_types().is_empty());
        assert!(host.mcp_servers().is_empty());
        assert!(host.tools().is_empty());
        assert!(host.scenes().is_empty());
        assert!(host.hook_configs().is_empty());
    }

    /// The manifest names events as strings because `plugin` has no hooks
    /// dependency. This is the check that the two lists have not drifted.
    #[test]
    fn every_subscribable_event_name_resolves_to_a_real_hook_event() {
        for name in plugin::manifest::SUBSCRIBABLE_EVENTS {
            let parsed: Result<hooks::config::HookEvent, _> =
                serde_json::from_value(serde_json::Value::String((*name).to_string()));
            assert!(parsed.is_ok(), "`{name}` is not a real HookEvent");
        }
    }
}

/// The echo fixture's component, built if it is not there yet.
///
/// Built rather than skipped when absent, which is what this used to do. The
/// argument for skipping was that a unit test suite should not shell out to
/// cargo, and what it bought was five tests that drive a real component
/// reporting green in CI without running: `cargo test --workspace` reaches
/// this crate long before `wasm-host`, whose integration test is what builds
/// the fixture. Forty-two tests in two hundredths of a second is what that
/// looks like from the outside, which is to say it looks like nothing.
#[cfg(test)]
fn fixture_component() -> std::path::PathBuf {
    static BUILT: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| {
            let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../tests/fixtures/wasm_echo_plugin");
            let out = dir.join("target/wasm32-wasip2/release/wasm_echo_plugin.wasm");
            if out.exists() {
                return out;
            }
            let status = std::process::Command::new(env!("CARGO"))
                .args(["build", "--release", "--target", "wasm32-wasip2"])
                .current_dir(&dir)
                .status()
                .expect("cargo should be runnable");
            assert!(
                status.success(),
                "could not build the fixture component. If the target is missing: \
                 rustup target add wasm32-wasip2"
            );
            out
        })
        .clone()
}

#[cfg(test)]
mod component_tests {
    use super::*;

    /// The whole point of the seam: a manifest on disk turns into tools the
    /// engine can register, named the way permission rules expect.
    /// The long guide is model-visible too — ToolSearch fetches it — so an
    /// installer that showed only the one-liner would be reviewing half of
    /// what reaches the model.
    #[tokio::test]
    async fn disclosure_covers_the_long_guide_not_just_the_one_liner() {
        let component = fixture_component();
        let dir = tempfile::tempdir().unwrap();
        std::fs::copy(&component, dir.path().join("echo.wasm")).unwrap();
        std::fs::write(
            dir.path().join("plugin.toml"),
            "[plugin]\nname = \"echo-plugin\"\nversion = \"1.0.0\"\napi_version = \"0.1\"\n\n[[wasm]]\ncomponent = \"echo.wasm\"\n",
        )
        .unwrap();
        let p =
            plugin::manifest::Plugin::load(dir.path(), &dir.path().join("plugin.toml")).unwrap();

        let mut host = InstalledPlugins::new(vec![p]);
        host.load_components(&WasmEngine::new().unwrap(), dir.path())
            .await;

        let d = host.disclose("echo-plugin").unwrap().unwrap();
        let origins: Vec<&str> = d.model_visible.iter().map(|v| v.origin.as_str()).collect();
        assert!(
            origins.iter().any(|o| o.contains("description")),
            "{origins:?}"
        );
        assert!(
            origins.iter().any(|o| o.contains("guide")),
            "the long guide must be disclosed too: {origins:?}"
        );
        assert!(
            d.model_visible.iter().any(|v| v.text.contains("verbatim")),
            "the guide's actual text is what a reviewer needs to see"
        );
    }

    #[tokio::test]
    async fn a_declared_component_becomes_registered_tools() {
        let component = fixture_component();
        let dir = tempfile::tempdir().unwrap();
        std::fs::copy(&component, dir.path().join("echo.wasm")).unwrap();
        std::fs::write(
            dir.path().join("plugin.toml"),
            r#"
[plugin]
name = "echo-plugin"
version = "1.0.0"
api_version = "0.1"

[[wasm]]
component = "echo.wasm"
"#,
        )
        .unwrap();
        let p =
            plugin::manifest::Plugin::load(dir.path(), &dir.path().join("plugin.toml")).unwrap();

        let mut host = InstalledPlugins::new(vec![p]);
        assert!(
            host.tools().is_empty(),
            "a manifest alone says nothing about what a component provides"
        );

        let engine = WasmEngine::new().unwrap();
        host.load_components(&engine, dir.path()).await;

        let names: Vec<String> = host.tools().iter().map(|t| t.name().to_string()).collect();
        assert!(
            names.contains(&"plugin__echo-plugin__echo".to_string()),
            "{names:?}"
        );
        assert!(
            host.tools().iter().all(|t| t.is_deferred()),
            "plugin tools default to deferred so their schemas cost nothing per turn"
        );
    }

    /// The plugin gets the last word on its own configuration: a schema says
    /// what is well-formed, only the plugin knows what is workable. The
    /// fixture refuses `{"fail":true}`, and refusing means not loading.
    #[tokio::test]
    async fn a_plugin_that_rejects_its_configuration_does_not_load() {
        let component = fixture_component();
        let dir = tempfile::tempdir().unwrap();
        std::fs::copy(&component, dir.path().join("echo.wasm")).unwrap();
        std::fs::write(
            dir.path().join("plugin.toml"),
            "[plugin]\nname = \"echo-plugin\"\nversion = \"1.0.0\"\napi_version = \"0.1\"\n\n[[wasm]]\ncomponent = \"echo.wasm\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join(crate::config::CONFIG_FILE),
            r#"{"fail":true}"#,
        )
        .unwrap();
        let p =
            plugin::manifest::Plugin::load(dir.path(), &dir.path().join("plugin.toml")).unwrap();

        let mut host = InstalledPlugins::new(vec![p]);
        host.load_components(&WasmEngine::new().unwrap(), dir.path())
            .await;

        assert!(
            host.tools().is_empty(),
            "a plugin that refused its configuration must contribute nothing"
        );
    }

    /// Order comes from the manifest list, not from which plugin's `init`
    /// happened to finish first — otherwise what a daemon serves varies run
    /// to run.
    #[tokio::test]
    async fn loading_is_concurrent_but_results_keep_their_order() {
        let component = fixture_component();
        let mut dirs = Vec::new();
        let mut plugins = Vec::new();
        for name in ["a-plugin", "b-plugin", "c-plugin"] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::copy(&component, dir.path().join("echo.wasm")).unwrap();
            std::fs::write(
                dir.path().join("plugin.toml"),
                format!(
                    "[plugin]\nname = \"{name}\"\nversion = \"1.0.0\"\napi_version = \"0.1\"\n\n[[wasm]]\ncomponent = \"echo.wasm\"\n"
                ),
            )
            .unwrap();
            plugins.push(
                plugin::manifest::Plugin::load(dir.path(), &dir.path().join("plugin.toml"))
                    .unwrap(),
            );
            dirs.push(dir);
        }

        let mut host = InstalledPlugins::new(plugins);
        host.load_components(&WasmEngine::new().unwrap(), dirs[0].path())
            .await;

        let names: Vec<String> = host
            .tools()
            .iter()
            .map(|t| t.name().split("__").nth(1).unwrap().to_string())
            .collect();
        let first_of_each: Vec<&String> = {
            let mut seen = std::collections::HashSet::new();
            names.iter().filter(|n| seen.insert((*n).clone())).collect()
        };
        assert_eq!(
            first_of_each,
            ["a-plugin", "b-plugin", "c-plugin"],
            "results must follow the declared order: {names:?}"
        );
    }

    /// One unloadable component must not cost the user the plugins that work.
    #[tokio::test]
    async fn a_broken_component_is_skipped_and_the_rest_still_load() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("broken.wasm"), b"not a component").unwrap();
        std::fs::write(
            dir.path().join("plugin.toml"),
            r#"
[plugin]
name = "broken"
version = "1.0.0"
api_version = "0.1"

[[wasm]]
component = "broken.wasm"
"#,
        )
        .unwrap();
        let p =
            plugin::manifest::Plugin::load(dir.path(), &dir.path().join("plugin.toml")).unwrap();

        let mut host = InstalledPlugins::new(vec![p]);
        let engine = WasmEngine::new().unwrap();
        host.load_components(&engine, dir.path()).await;

        assert!(host.tools().is_empty());
        assert_eq!(
            host.names(),
            ["broken"],
            "the plugin is still known; only its tools are missing"
        );
    }
}

#[cfg(test)]
mod precompile_tests {
    /// A package with no components must install on a build that carries no
    /// compiler. This used to go looking for `agix-plugin-compile` on behalf
    /// of components the package never declared, and fail the install when it
    /// was not there — which is what kept a script-only or MCP-only package
    /// off every build but the one with the compiler linked in.
    ///
    /// The claim is only load-bearing without the `compile` feature, so
    /// `cargo test -p plugin-host --no-default-features` is where it bites;
    /// CI runs that configuration for exactly this reason.
    #[tokio::test]
    async fn a_package_with_no_components_needs_no_compiler() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("plugin.toml"),
            "[plugin]\nname = \"scripts-only\"\nversion = \"1.0.0\"\napi_version = \"0.1\"\n\n\
             [[script]]\npoint = \"tool.result\"\nentry = \"annotate.js:onResult\"\n",
        )
        .unwrap();
        super::precompile_plugin(dir.path())
            .await
            .expect("nothing to compile is not a compile failure");
    }

    /// The manifest is read before anything is compiled, so a package that
    /// cannot be parsed says so rather than reporting a compiler problem.
    #[tokio::test]
    async fn an_unreadable_manifest_fails_as_a_manifest() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("plugin.toml"), "not toml at all {{{").unwrap();
        let err = format!(
            "{:#}",
            super::precompile_plugin(dir.path()).await.unwrap_err()
        );
        assert!(err.contains("toml"), "{err}");
    }
}

#[cfg(test)]
mod unload_tests {
    use super::*;

    fn write_plugin(dir: &Path, component: &std::path::Path) {
        std::fs::copy(component, dir.join("echo.wasm")).unwrap();
        std::fs::write(
            dir.join("plugin.toml"),
            r#"
[plugin]
name = "echo-plugin"
version = "1.0.0"
api_version = "0.1"

[[wasm]]
component = "echo.wasm"
events = ["PreToolUse"]
"#,
        )
        .unwrap();
    }

    /// Unloading is dropping the host: the instances go, and with them the
    /// key-value namespace a plugin accumulated. Nothing has to be
    /// remembered and cleaned up separately, which is the point of keeping
    /// that state on the host side in the first place.
    #[tokio::test]
    async fn replacing_the_host_takes_a_plugins_state_with_it() {
        let component = fixture_component();
        let dir = tempfile::tempdir().unwrap();
        write_plugin(dir.path(), &component);
        let p =
            plugin::manifest::Plugin::load(dir.path(), &dir.path().join("plugin.toml")).unwrap();
        let engine = WasmEngine::new().unwrap();

        let mut host = InstalledPlugins::new(vec![p.clone()]);
        host.load_components(&engine, dir.path()).await;

        // `remember` reports the value the *previous* call stored, which
        // makes it the tool that can tell whether state survived a reload.
        let remember = |h: &InstalledPlugins| {
            h.tools()
                .into_iter()
                .find(|t| t.name() == "plugin__echo-plugin__remember")
                .expect("the fixture exports `remember`")
        };
        let call = |tool: Arc<dyn base::tool::Tool>, value: &str, root: &Path| {
            let input = serde_json::json!({ "value": value });
            let ctx = base::tool::ToolContext::for_test(root.to_path_buf());
            async move {
                let r = tool
                    .call(input, ctx, base::tool::ProgressSender::noop("t"))
                    .await
                    .expect("the fixture answers");
                match r.content {
                    base::tool::ToolResultContent::Text(s) => s,
                    other => panic!("expected text, got {other:?}"),
                }
            }
        };

        assert_eq!(
            call(remember(&host), "remembered", dir.path()).await,
            "(none)",
            "a freshly loaded plugin has nothing stored"
        );
        assert_eq!(
            call(remember(&host), "again", dir.path()).await,
            "remembered",
            "within one host, state carries between calls"
        );

        // A fresh host over the same plugin is what install/uninstall/enable
        // produces, and it starts with nothing.
        let mut reloaded = InstalledPlugins::new(vec![p]);
        reloaded.load_components(&engine, dir.path()).await;
        assert_eq!(
            call(remember(&reloaded), "after", dir.path()).await,
            "(none)",
            "a reloaded plugin must not inherit the previous one's state"
        );
    }

    /// A plugin whose component failed to load subscribes to nothing, so the
    /// dispatcher does not pay for events that can never be answered.
    #[tokio::test]
    async fn a_plugin_with_no_loaded_component_registers_no_hooks() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("broken.wasm"), b"not a component").unwrap();
        std::fs::write(
            dir.path().join("plugin.toml"),
            r#"
[plugin]
name = "broken"
version = "1.0.0"
api_version = "0.1"

[[wasm]]
component = "broken.wasm"
events = ["PreToolUse"]
"#,
        )
        .unwrap();
        let p =
            plugin::manifest::Plugin::load(dir.path(), &dir.path().join("plugin.toml")).unwrap();

        let mut host = InstalledPlugins::new(vec![p]);
        host.load_components(&WasmEngine::new().unwrap(), dir.path())
            .await;

        assert!(host.hook_configs().is_empty());
        assert!(host.hook_executor().is_none());
    }
}
