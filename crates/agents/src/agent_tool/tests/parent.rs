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
    pub(super) scene: Arc<SceneSpec>,
    pub(super) id: String,
    pub(super) mcp: Option<Arc<dyn base::interface::mcp_servers::McpServers>>,
    pub(super) skills: Arc<dyn base::interface::skill_catalog::SkillCatalog>,
    pub(super) depth: u32,
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
            state: Arc::new(base::context::SessionState::for_tests(std::env::temp_dir())),
            events: None,
            pending: Arc::default(),
            ended: tokio_util::sync::CancellationToken::new(),
            hooks: std::sync::Weak::new(),
            scene: Arc::new(scene::code::spec(scene::code::Interaction::Interactive)),
            id: "test-parent".into(),
            mcp: None,
            skills: Arc::new(base::interface::skill_catalog::NoSkills),
            depth: 0,
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
            .unwrap_or_else(|| PermissionSet::without_grants(Arc::new(PermitsEverything)));
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
            under_hook: false,
        };
        let catalog = match self.shared_types {
            Some(map) => AgentCatalog::shared(map, &self.session_agent_types),
            None => AgentCatalog::watched(
                &self.agent_dirs,
                &self.plugin_agent_types,
                &env.config.prompt_files,
            ),
        };
        let limits =
            crate::delegate::Limits::new(&env.config, env.depth, self.slots, Default::default());
        let pool = crate::delegate::ToolPool::new(self.template, self.tools);
        let members = crate::delegate::PersistentMembers::new(self.team_registry);
        AgentTool::new(Delegator::new(env, catalog, pool, limits, members))
    }
}

/// What a test asks of an `AgentTool` it holds, in the terms of the calls it
/// makes, each one a `DelegateRequest` through the one dispatch.
impl AgentTool {
    pub(super) fn parent_state(&self) -> Arc<base::context::SessionState> {
        self.delegator.env.parent.state.clone()
    }

    fn assert_parent(&self, session: &Arc<base::context::SessionState>) {
        assert!(
            Arc::ptr_eq(session, &self.delegator.env.parent.state),
            "a delegate's task is the parent session's: pass `agent_tool.parent_state()`"
        );
    }

    /// A delegate's permission, put to this parent, asking its host.
    pub(super) fn sub_permission(&self) -> PermissionSet {
        self.sub_relay(None).permission
    }

    pub(super) fn sub_relay(&self, task: Option<Arc<base::context::RunningTask>>) -> Relay {
        self.delegator
            .relay("sub", task, AskRoute::Host, Vec::new())
    }

    pub(super) fn sub_settings(&self, model_name: Option<&str>) -> LoadedSettings {
        self.delegator.sub_settings(model_name)
    }

    /// What a delegate of `agent_type` is left of what it inherits, once its
    /// tool policy holds it to its type and to a caller's `allowed` and
    /// `disallowed`.
    pub(super) fn delegate_tools(
        &self,
        agent_type: Option<&str>,
        allowed: &[String],
        disallowed: &[String],
    ) -> Arc<InMemoryToolRegistry> {
        let def = agent_type.and_then(|t| self.delegator.agent_type_def(t));
        let caller = ToolSelector::from_lists(allowed, disallowed);
        let selector = delegate_selector(def.as_ref(), &caller, &[]);
        let left = InMemoryToolRegistry::new();
        for tool in self.delegator.pool.inherited().list() {
            if selector.admits(tool.name(), &tool.spec().origin) {
                left.register(tool);
            }
        }
        Arc::new(left)
    }

    pub(super) async fn delegate(&self, req: DelegateRequest) -> Result<Delegated, DelegateError> {
        self.delegator.delegate(req).await
    }

    /// `prompt` waited on, as `called_from`, in `cwd`.
    pub(super) async fn wait_for(
        &self,
        prompt: &str,
        cwd: std::path::PathBuf,
        cancel: tokio_util::sync::CancellationToken,
        agent_type: Option<&str>,
        called_from: Option<(u32, &str)>,
    ) -> Result<String, base::error::ToolError> {
        let mut req = DelegateRequest::new(prompt, DelegateMode::Wait, StopsWith::Call(cancel));
        req.workdir = Workdir::At(cwd);
        req.agent_type = agent_type.map(str::to_string);
        req.called_from = called_from.map(|(turn, id)| CallSite {
            turn,
            tool_use_id: id.to_string(),
        });
        finished_text(self.delegate(req).await)
    }

    pub(super) async fn start_in_background_for_test(
        &self,
        prompt: &str,
        session: &Arc<base::context::SessionState>,
    ) -> Result<String, base::error::ToolError> {
        self.assert_parent(session);
        let mode = DelegateMode::Background {
            notify_parent: true,
        };
        let req = DelegateRequest::new(prompt, mode, StopsWith::Session);
        match self.delegate(req).await.map_err(tool_error)? {
            Delegated::Started(started) => Ok(started.task_id),
            Delegated::Finished(_) => panic!("a background delegate finished at once"),
        }
    }

    pub(super) async fn run_stage_member(
        &self,
        prompt: &str,
        cancel: tokio_util::sync::CancellationToken,
        permission_mode: Option<base::settings::PermissionMode>,
        extra_tools: Vec<Arc<dyn base::tool::Tool>>,
    ) -> Result<String, base::error::ToolError> {
        let mode = DelegateMode::TeamMember {
            seat: TeamSeat {
                team: "team".into(),
                member: "member".into(),
            },
            permission_mode,
            lifetime: MemberLifetime::Stage {
                stage: "stage".into(),
                extra_tools,
            },
        };
        finished_text(
            self.delegate(DelegateRequest::new(prompt, mode, StopsWith::Call(cancel)))
                .await,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn spawn_or_message_team_member(
        &self,
        team: String,
        member: String,
        prompt: String,
        agent_type: Option<String>,
        cwd: std::path::PathBuf,
        session: Arc<base::context::SessionState>,
        permission_mode: Option<base::settings::PermissionMode>,
    ) -> Result<String, base::error::ToolError> {
        self.assert_parent(&session);
        let mode = DelegateMode::TeamMember {
            seat: TeamSeat { team, member },
            permission_mode,
            lifetime: MemberLifetime::Persistent,
        };
        let mut req = DelegateRequest::new(prompt, mode, StopsWith::Session);
        req.agent_type = agent_type;
        req.workdir = Workdir::At(cwd);
        match self.delegate(req).await.map_err(tool_error)? {
            Delegated::Started(started) => Ok(started.task_id),
            Delegated::Finished(_) => panic!("a member answered instead of queuing"),
        }
    }

    pub(super) async fn stop_team_member(&self, team: &str, member: &str) {
        self.delegator
            .stop_member(&TeamSeat {
                team: team.into(),
                member: member.into(),
            })
            .await;
    }

    /// A persistent member's session, built and not yet run.
    pub(super) async fn build_team_member_agent(
        &self,
        cwd: std::path::PathBuf,
        agent_type: Option<&str>,
        permission_mode: base::settings::PermissionMode,
        seat: Option<TeamSeat>,
    ) -> Result<(Agent, EventReceiver, InputSender, SubagentPayload), base::error::ToolError> {
        let d = &self.delegator;
        let depth = d.limits.spawn_guard().map_err(tool_error)?;
        let spec = Spec {
            session_id: base::id::Id::new().to_string(),
            agent_type: agent_type.map(str::to_string),
            model: None,
            tools: ToolSelector::all(),
            output_schema: None,
            permission_mode: Some(permission_mode),
            seat,
            cwd,
            called_from: None,
        };
        let built = d
            .build_delegate(
                &spec,
                depth,
                Vec::new(),
                d.relay(&spec.session_id, None, AskRoute::Host, Vec::new()),
                true,
            )
            .await?;
        Ok((built.agent, built.event_rx, built.input_tx, built.payload))
    }
}

/// A finished delegate's answer, a failed one's error.
pub(super) fn finished_text(
    delegated: Result<Delegated, DelegateError>,
) -> Result<String, base::error::ToolError> {
    match delegated.map_err(tool_error)? {
        Delegated::Finished(outcome) => match outcome.end {
            DelegateEnd::Failed(e) => Err(base::error::ToolError::Execution(anyhow!(e))),
            _ => Ok(outcome.text),
        },
        Delegated::Started(_) => panic!("a waited delegate was started in the background"),
    }
}

struct PermitsEverything;

#[async_trait]
impl PermissionPolicy for PermitsEverything {
    async fn decide(
        &self,
        _: &base::interface::permission::PermissionRequest<'_>,
    ) -> base::interface::permission::PermissionOutcome {
        base::interface::permission::PermissionOutcome::Permit {
            basis: base::interface::permission::Basis::Bypass,
            profile: Default::default(),
            obligations: Default::default(),
        }
    }
}
