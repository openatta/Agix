//! Modules: what an assembly adds to the kernel, and the one place it adds it.
//!
//! A module owns a capability a session can run without — sub-agents, MCP,
//! skills — and adds it to every session it is assembled into. The kernel
//! does not know which modules exist: the host hands [`Builder::modules`] the
//! list, and `build()` calls each one's [`Module::on_session`] once the
//! session's registry and services exist, before the scene's tool policy is
//! applied, so whatever a module registers is subject to it like any built-in.
//! A scene can narrow the list (`Scene::modules` / `Scene::disabled_modules`);
//! a module it does not permit is not run for that session at all.
//!
//! [`Builder::modules`]: crate::agent::Builder::modules

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use base::context::{EngineConfig, SessionState};
use base::frozen::SkillEntry;
use base::interface::agent_spawner::AgentSpawner;
use base::interface::mcp_servers::McpServers;
use base::interface::model::Model;
use base::interface::permission::Permission;
use base::interface::scene::Scene;
use base::interface::settings::Settings;
use base::interface::skill_catalog::{NoSkills, SkillCatalog};
use base::tool::InMemoryToolRegistry;
use tokio_util::sync::CancellationToken;

use crate::agent::PendingPermissions;

pub trait Module: Send + Sync {
    fn id(&self) -> &'static str;

    /// Modules that must come earlier in the list. Checked by `build()`,
    /// which refuses a list that names one missing or later.
    fn requires(&self) -> &'static [&'static str] {
        &[]
    }

    fn on_session(&self, cx: &mut SessionContext<'_>) -> Result<(), ModuleError>;
}

#[derive(Debug, thiserror::Error)]
#[error("module `{module}`: {message}")]
pub struct ModuleError {
    pub module: &'static str,
    pub message: String,
}

impl ModuleError {
    pub fn new(module: &'static str, message: impl Into<String>) -> Self {
        Self {
            module,
            message: message.into(),
        }
    }
}

/// The session as a module sees it while it is being built.
pub struct SessionContext<'a> {
    pub tools: &'a Arc<InMemoryToolRegistry>,
    pub settings: &'a Arc<Settings>,
    pub config: &'a Arc<EngineConfig>,
    pub scene: &'a Arc<dyn Scene>,
    pub scene_registry: Option<&'a Arc<scene::SceneRegistry>>,
    /// Already wrapped for telemetry: what a module calls through it is
    /// accounted to this session.
    pub model: &'a Arc<dyn Model>,
    pub permission: &'a Arc<dyn Permission>,
    pub pending_permissions: &'a PendingPermissions,
    pub hooks: &'a Arc<hooks::HookRunner>,
    pub events: &'a crate::event_bus::EventBus,
    pub telemetry: &'a telemetry::TelemetryHandle,
    pub session_id: &'a str,
    pub session_state: &'a Arc<SessionState>,
    pub history_store: Option<&'a Arc<dyn history::store::HistoryStore>>,
    pub instruction_file: Option<&'a PathBuf>,
    /// How many agents deep this session is: 0 for one a host created.
    pub depth: u32,
    pub task_router: Option<&'a Arc<base::provider::TaskRouter>>,
    pub process_groups: Option<&'a Arc<base::interface::exec::local::GroupLedger>>,
    /// The Rust plugins this session was built with; a delegate is built
    /// with them too.
    pub delegate_plugins: &'a [Arc<dyn crate::plugin::Plugin>],
    /// The tools those plugins registered here. A delegate gets its own from
    /// the plugins, not this session's copies.
    pub plugin_tools: &'a [String],
    /// Agent types the session's plugins declare.
    pub plugin_agent_types: &'a [base::agent_type::AgentTypeDefinition],
    /// Every module this session runs, this one included: the assembled
    /// list narrowed by the scene. What a module that spawns delegates
    /// assembles them with.
    pub modules: &'a [Arc<dyn Module>],
    pub(crate) mcp: &'a mut Option<Box<dyn McpServers>>,
    pub(crate) agent_spawner: &'a LateAgentSpawner,
    pub(crate) skill_catalog: &'a LateSkillCatalog,
}

impl SessionContext<'_> {
    /// Makes `spawner` the session's way to start a sub-agent — for `"agent"`
    /// hooks, forked skills, and any module after this one.
    pub fn provide_agent_spawner(&mut self, spawner: Arc<dyn AgentSpawner>) {
        self.agent_spawner.set(spawner);
    }

    /// The session's spawner if a module earlier in the list provided one.
    pub fn agent_spawner(&self) -> Option<Arc<dyn AgentSpawner>> {
        self.agent_spawner.get()
    }

    /// Makes `servers` the MCP servers the session is connected to.
    pub fn provide_mcp(&mut self, servers: Box<dyn McpServers>) {
        *self.mcp = Some(servers);
    }

    /// The tools of the MCP servers a module earlier in the list provided.
    pub fn mcp_tools(&self) -> Vec<Arc<dyn base::tool::Tool>> {
        self.mcp.as_ref().map(|m| m.tools()).unwrap_or_default()
    }

    /// Makes `catalog` the skills the session lists, expands and reminds the
    /// model of.
    pub fn provide_skill_catalog(&mut self, catalog: Arc<dyn SkillCatalog>) {
        self.skill_catalog.set(catalog);
    }

    /// The session's skill catalog — whichever module provides it, before or
    /// after this one; empty if none does.
    pub fn skill_catalog(&self) -> Arc<dyn SkillCatalog> {
        Arc::new(self.skill_catalog.clone())
    }
}

/// A [`SkillCatalog`] provided after the things reading it were built: the
/// command registry, the agent and other modules hold this and see the real
/// catalog once a module provides it.
#[derive(Default, Clone)]
pub(crate) struct LateSkillCatalog(Arc<OnceLock<Arc<dyn SkillCatalog>>>);

impl LateSkillCatalog {
    fn set(&self, catalog: Arc<dyn SkillCatalog>) {
        let _ = self.0.set(catalog);
    }

    fn inner(&self) -> &dyn SkillCatalog {
        match self.0.get() {
            Some(c) => c.as_ref(),
            None => &NoSkills,
        }
    }
}

impl SkillCatalog for LateSkillCatalog {
    fn list(&self) -> Vec<SkillEntry> {
        self.inner().list()
    }
    fn get(&self, name: &str) -> Option<SkillEntry> {
        self.inner().get(name)
    }
    fn body(&self, name: &str) -> Option<String> {
        self.inner().body(name)
    }
    fn reload_changed(&self) -> usize {
        self.inner().reload_changed()
    }
    fn discover_for_paths(&self, paths: &[PathBuf]) -> Vec<SkillEntry> {
        self.inner().discover_for_paths(paths)
    }
    fn activate_for_paths(&self, files: &[PathBuf]) -> Vec<SkillEntry> {
        self.inner().activate_for_paths(files)
    }
    fn invocation_count(&self, name: &str) -> u32 {
        self.inner().invocation_count(name)
    }
    fn last_invoked_seq(&self, name: &str) -> u64 {
        self.inner().last_invoked_seq(name)
    }
}

/// An [`AgentSpawner`] whose implementation arrives after the things holding
/// it were built: hooks and skills are constructed before the module that
/// knows how to spawn agents has run. Until one is provided — or in a session
/// assembled without one — every spawn is refused.
#[derive(Default, Clone)]
pub(crate) struct LateAgentSpawner(Arc<OnceLock<Arc<dyn AgentSpawner>>>);

impl LateAgentSpawner {
    pub(crate) fn set(&self, spawner: Arc<dyn AgentSpawner>) {
        let _ = self.0.set(spawner);
    }

    fn get(&self) -> Option<Arc<dyn AgentSpawner>> {
        self.0.get().cloned()
    }

    fn require(&self) -> Result<&Arc<dyn AgentSpawner>, Box<dyn std::error::Error + Send>> {
        self.0.get().ok_or_else(|| {
            Box::new(std::io::Error::other(
                "this session was assembled without sub-agents",
            )) as Box<dyn std::error::Error + Send>
        })
    }
}

#[async_trait]
impl AgentSpawner for LateAgentSpawner {
    async fn spawn_agent(
        &self,
        prompt: String,
        allowed_tools: Vec<String>,
        cwd: PathBuf,
        cancel: CancellationToken,
        agent_type: Option<String>,
    ) -> Result<String, Box<dyn std::error::Error + Send>> {
        self.require()?
            .spawn_agent(prompt, allowed_tools, cwd, cancel, agent_type)
            .await
    }

    async fn spawn_agent_background(
        &self,
        prompt: String,
        allowed_tools: Vec<String>,
        cwd: PathBuf,
        cancel: CancellationToken,
        agent_type: Option<String>,
        session: Arc<SessionState>,
    ) -> Result<String, Box<dyn std::error::Error + Send>> {
        self.require()?
            .spawn_agent_background(prompt, allowed_tools, cwd, cancel, agent_type, session)
            .await
    }

    async fn start_agent(
        &self,
        request: base::interface::agent_spawner::AgentRequest,
        session: Arc<SessionState>,
        cancel: CancellationToken,
    ) -> Result<
        base::interface::agent_spawner::StartedAgent,
        base::interface::agent_spawner::StartAgentError,
    > {
        match self.0.get() {
            Some(spawner) => spawner.start_agent(request, session, cancel).await,
            None => Err(
                base::interface::agent_spawner::StartAgentError::Unsupported(
                    "this session was assembled without sub-agents".into(),
                ),
            ),
        }
    }

    async fn spawn_agent_with_tools(
        &self,
        prompt: String,
        allowed_tools: Vec<String>,
        extra_tools: Vec<Arc<dyn base::tool::Tool>>,
        cwd: PathBuf,
        cancel: CancellationToken,
        agent_type: Option<String>,
        permission_mode: Option<base::interface::settings::PermissionMode>,
    ) -> Result<String, Box<dyn std::error::Error + Send>> {
        self.require()?
            .spawn_agent_with_tools(
                prompt,
                allowed_tools,
                extra_tools,
                cwd,
                cancel,
                agent_type,
                permission_mode,
            )
            .await
    }

    async fn stop_team_member(&self, team_name: &str, member_name: &str) {
        if let Some(spawner) = self.get() {
            spawner.stop_team_member(team_name, member_name).await;
        }
    }
}

/// The modules `scene` lets run, in the order they were assembled.
///
/// Refuses a kept module that requires one the scene left out: dropping the
/// dependent as well would quietly take away a capability the scene asked
/// for, and running it without its requirement is what `check_order`
/// exists to prevent.
pub(crate) fn permitted_by(
    modules: &[Arc<dyn Module>],
    scene: &dyn Scene,
) -> Result<Vec<Arc<dyn Module>>, ModuleError> {
    let (kept, left_out): (Vec<_>, Vec<_>) = modules
        .iter()
        .cloned()
        .partition(|m| scene.permits_module(m.id()));
    for m in &kept {
        for needed in m.requires() {
            if left_out.iter().any(|out| out.id() == *needed) {
                return Err(ModuleError::new(
                    m.id(),
                    format!(
                        "requires `{needed}`, which scene `{}` does not run",
                        scene.id()
                    ),
                ));
            }
        }
    }
    Ok(kept)
}

/// Refuses a module list whose `requires` cannot be met in order.
pub(crate) fn check_order(modules: &[Arc<dyn Module>]) -> Result<(), ModuleError> {
    for (i, m) in modules.iter().enumerate() {
        if modules[..i].iter().any(|earlier| earlier.id() == m.id()) {
            return Err(ModuleError::new(m.id(), "is assembled twice"));
        }
        for needed in m.requires() {
            if !modules[..i].iter().any(|earlier| earlier.id() == *needed) {
                return Err(ModuleError::new(
                    m.id(),
                    format!("requires `{needed}`, which is not assembled before it"),
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod fakes {
    //! An MCP server set for the kernel's own tests: tools only, and a module
    //! that installs them the way the mcp module does.

    use super::*;
    use base::context::McpServerInstruction;
    use base::error::ToolError;
    use base::interface::mcp_servers::{McpPromptCommand, McpPromptInvocation};
    use base::tool::{ProgressSender, Tool, ToolContext, ToolResult};
    use std::sync::Mutex;

    pub(crate) struct FakeMcpTool {
        name: String,
        source: String,
    }

    pub(crate) fn mcp_tool(server: &str, tool: &str) -> Arc<dyn Tool> {
        Arc::new(FakeMcpTool {
            name: format!("mcp__{server}__{tool}"),
            source: format!("mcp:{server}"),
        })
    }

    #[async_trait]
    impl Tool for FakeMcpTool {
        fn name(&self) -> &str {
            &self.name
        }
        fn description(&self) -> &str {
            "a tool an MCP server lists"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        fn source(&self) -> std::borrow::Cow<'_, str> {
            std::borrow::Cow::Borrowed(&self.source)
        }
        async fn call(
            &self,
            _: serde_json::Value,
            _: ToolContext,
            _: ProgressSender,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::text("ok"))
        }
    }

    pub(crate) struct FakeMcp(pub(crate) Vec<Arc<dyn Tool>>);

    #[async_trait]
    impl McpServers for FakeMcp {
        fn tools(&self) -> Vec<Arc<dyn Tool>> {
            self.0.clone()
        }
        async fn refresh_tools(&mut self) {}
        fn instructions(&self) -> Vec<McpServerInstruction> {
            Vec::new()
        }
        async fn resolve_resource_refs(&self, _: &str) -> Option<String> {
            None
        }
        fn prompt_commands(&self) -> Vec<McpPromptCommand> {
            Vec::new()
        }
        fn has_prompt_command(&self, _: &str) -> bool {
            false
        }
        async fn invoke_prompt_command(&self, _: &str, _: &str) -> McpPromptInvocation {
            McpPromptInvocation {
                text: String::new(),
                is_error: true,
            }
        }
    }

    pub(crate) struct FakeMcpModule(Mutex<Option<FakeMcp>>);

    pub(crate) fn mcp_module(tools: Vec<Arc<dyn Tool>>) -> Arc<dyn Module> {
        Arc::new(FakeMcpModule(Mutex::new(Some(FakeMcp(tools)))))
    }

    impl Module for FakeMcpModule {
        fn id(&self) -> &'static str {
            "mcp"
        }
        fn on_session(&self, cx: &mut SessionContext<'_>) -> Result<(), ModuleError> {
            if let Some(mcp) = self.0.lock().unwrap().take() {
                for t in mcp.tools() {
                    cx.tools.register(t);
                }
                cx.provide_mcp(Box::new(mcp));
            }
            Ok(())
        }
    }
}
