//! Modules: what an assembly adds to the kernel, and the one place it adds it.
//!
//! A module owns a capability a session can run without — sub-agents, MCP,
//! skills — and adds it to every session it is assembled into. The kernel
//! does not know which modules exist: the host hands [`Builder::modules`] the
//! list, and `build()` calls each one's [`Module::on_session`] once the
//! session's registry and services exist, before the scene's tool policy is
//! applied, so whatever a module registers is subject to it like any built-in.
//!
//! [`Builder::modules`]: crate::agent::Builder::modules

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use base::context::{EngineConfig, SessionState};
use base::interface::agent_spawner::AgentSpawner;
use base::interface::model::Model;
use base::interface::permission::Permission;
use base::interface::scene::Scene;
use base::interface::settings::Settings;
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
    pub delegate_scripts: Option<&'a Arc<base::interface::script_adapters::BoundScripts>>,
    pub plugin_host: Option<&'a Arc<dyn crate::plugin_host::PluginHost>>,
    /// Every module this session is assembled with, this one included —
    /// what a module that spawns delegates assembles them with.
    pub modules: &'a [Arc<dyn Module>],
    pub mcp: &'a mcp::manager::McpManager,
    pub skills: &'a Arc<skills::manager::SkillManager>,
    pub(crate) agent_spawner: &'a LateAgentSpawner,
}

impl SessionContext<'_> {
    /// Makes `spawner` the session's way to start a sub-agent — for `"agent"`
    /// hooks, forked skills, and any module after this one.
    pub fn provide_agent_spawner(&mut self, spawner: Arc<dyn AgentSpawner>) {
        self.agent_spawner.set(spawner);
    }

    pub fn agent_spawner(&self) -> Option<Arc<dyn AgentSpawner>> {
        self.agent_spawner.get()
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
