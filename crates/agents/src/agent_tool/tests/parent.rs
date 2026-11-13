//! `TestParent`: the parent session an `AgentTool` under test delegates for,
//! with everything a test does not set left at a quiet default.

use super::*;

pub(super) struct TestParent {
    pub(super) model: Arc<dyn Model>,
    pub(super) config: Arc<EngineConfig>,
    /// `None` is `fallback_settings` for the model in `config`.
    pub(super) settings: Option<LoadedSettings>,
    pub(super) tools: Arc<InMemoryToolRegistry>,
    pub(super) template: Arc<InMemoryToolRegistry>,
    pub(super) agent_dirs: Vec<(std::path::PathBuf, AgentTypeSource)>,
    pub(super) plugin_agent_types: Vec<AgentTypeDefinition>,
    /// With `session_agent_types` over it, in place of `agent_dirs` and
    /// `plugin_agent_types`.
    pub(super) shared_types: Option<SharedAgentTypeMap>,
    pub(super) session_agent_types: Vec<AgentTypeDefinition>,
    /// `None` is `host_contracts()`.
    pub(super) host: Option<runtime::agent::HostContracts>,
    /// Over `host`'s; `None` permits everything.
    pub(super) permission: Option<PermissionSet>,
    pub(super) state: Arc<base::context::SessionState>,
    /// `None` is a channel nobody reads.
    pub(super) events: Option<runtime::agent::EventSender>,
    pub(super) pending: runtime::agent::PendingPermissions,
    pub(super) ended: tokio_util::sync::CancellationToken,
    pub(super) hooks: std::sync::Weak<hooks::HookRunner>,
    pub(super) scene: Arc<dyn Scene>,
    pub(super) id: String,
    pub(super) mcp: Option<Arc<dyn base::interface::mcp_servers::McpServers>>,
    pub(super) skills: Arc<dyn base::interface::skill_catalog::SkillCatalog>,
    pub(super) depth: u32,
    pub(super) plugin_tools: Vec<String>,
    pub(super) slots: SubagentSlots,
    pub(super) team_registry: Option<Arc<team::registry::TeamRegistry>>,
}

impl TestParent {
    pub(super) fn new(model: Arc<dyn Model>) -> Self {
        let tools = Arc::new(InMemoryToolRegistry::new());
        Self {
            model,
            config: Arc::new(EngineConfig::defaults_for("test-model")),
            settings: None,
            tools: tools.clone(),
            template: tools,
            agent_dirs: Vec::new(),
            plugin_agent_types: Vec::new(),
            shared_types: None,
            session_agent_types: Vec::new(),
            host: None,
            permission: None,
            state: Arc::new(base::context::SessionState::new(std::env::temp_dir())),
            events: None,
            pending: Arc::default(),
            ended: tokio_util::sync::CancellationToken::new(),
            hooks: std::sync::Weak::new(),
            scene: Arc::new(scene::code::CodeScene::default()),
            id: "test-parent".into(),
            mcp: None,
            skills: Arc::new(base::interface::skill_catalog::NoSkills),
            depth: 0,
            plugin_tools: Vec::new(),
            slots: SubagentSlots::new(DEFAULT_MAX_CONCURRENT_SUBAGENTS),
            team_registry: None,
        }
    }

    /// On `tools`, as both the parent's registry and what it was built from.
    pub(super) fn on(model: Arc<dyn Model>, tools: Arc<InMemoryToolRegistry>) -> Self {
        Self {
            tools: tools.clone(),
            template: tools,
            ..Self::new(model)
        }
    }

    pub(super) fn with_team(mut self) -> Self {
        self.team_registry = Some(Arc::new(team::registry::TeamRegistry::new()));
        self
    }

    pub(super) fn agent_tool(self) -> AgentTool {
        let settings = self.settings.unwrap_or_else(|| {
            fallback_settings(
                self.config.model.clone(),
                self.config.max_tokens,
                self.config.fallback_model.clone(),
                std::env::temp_dir(),
            )
        });
        let mut host = self.host.unwrap_or_else(host_contracts);
        host.permission = self
            .permission
            .unwrap_or_else(|| PermissionSet::without_grants(Arc::new(AlwaysPermit)));
        let events = self.events.unwrap_or_else(|| {
            let (tx, _) = tokio::sync::mpsc::unbounded_channel();
            runtime::event_bus::EventBus::new(tx)
        });
        let env = crate::delegate::DelegateEnv {
            host,
            parent: crate::delegate::ParentSession {
                id: self.id,
                state: self.state,
                events,
                pending_permissions: self.pending,
                ended: self.ended,
            },
            hooks: self.hooks,
            scene: self.scene,
            settings: Arc::new(settings),
            config: self.config,
            model: self.model,
            mcp: self.mcp,
            skills: self.skills,
            depth: self.depth,
        };
        let types = match self.shared_types {
            Some(map) => TypeCatalog::Shared {
                map,
                session_types: &self.session_agent_types,
            },
            None => TypeCatalog::Watched {
                dirs: &self.agent_dirs,
                plugin_types: &self.plugin_agent_types,
            },
        };
        AgentTool::new(
            env,
            Parts {
                types,
                template: self.template,
                parent_tools: self.tools,
                plugin_tools: &self.plugin_tools,
                slots: self.slots,
                team_registry: self.team_registry,
            },
        )
    }
}
