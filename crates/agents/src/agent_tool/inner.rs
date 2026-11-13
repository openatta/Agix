//! `Inner`: what every `AgentTool` clone shares — the parent it delegates for,
//! its agent types, tools, limits and members — and what a delegate session
//! is resolved from.

use super::*;

pub(super) struct Inner {
    pub(super) env: crate::delegate::DelegateEnv,
    pub(super) catalog: AgentCatalog,
    pub(super) pool: crate::delegate::ToolPool,
    pub(super) limits: crate::delegate::Limits,
    pub(super) members: crate::delegate::PersistentMembers,
}

impl Inner {
    pub(super) async fn fire_subagent_hook(&self, event: hooks::HookEvent, sub: &SubagentPayload) {
        if let Some(runner) = self.env.hooks() {
            sub.fire(&runner, event).await;
        }
    }

    /// The model instance a sub-agent spawn should use. Every spawn point is
    /// the same task type — `task_models` in settings.json is keyed by a
    /// small fixed taxonomy (`main`/`subagent`/`compact`/`memory`/
    /// `web_search`), not by `subagent_type` (which agent *kind* — "general-purpose",
    /// "code-reviewer", etc. — a different axis entirely), so the lookup
    /// key here is always the literal `"subagent"` task type regardless of
    /// which `subagent_type` was requested.
    pub(super) fn model_for_subagent(&self) -> Arc<dyn Model> {
        match &self.env.host.task_router {
            Some(router) => router.model_for("subagent"),
            None => self.env.model.clone(),
        }
    }

    pub(super) fn agent_type_def(&self, name: &str) -> Option<AgentTypeDefinition> {
        self.catalog.get(name)
    }

    /// The MCP servers a delegate of type `def` is granted: the parent's named
    /// in the type's `mcp_servers`, narrowed further by the run's tool lists.
    pub(super) fn mcp_granted_to(
        &self,
        def: Option<&AgentTypeDefinition>,
        run: &SubRun,
    ) -> Option<Box<dyn base::interface::mcp_servers::McpServers>> {
        let def = def.filter(|d| !d.mcp_servers.is_empty())?;
        let Some(parent) = &self.env.mcp else {
            tracing::warn!(
                agent_type = %def.name,
                "agent type names mcp_servers, but this session has no MCP servers to grant"
            );
            return None;
        };
        let granted = parent.granted(Some(&def.mcp_servers));
        if run.allowed_tools.is_empty() && run.disallowed_tools.is_empty() {
            return Some(granted);
        }
        Some(Box::new(ListedMcp {
            servers: granted,
            allowed: run.allowed_tools.clone(),
            disallowed: run.disallowed_tools.clone(),
        }))
    }

    /// See `AgentTool::permission_handler` — same resolution, reachable from
    /// the static `run_sub_inner` path (background spawns).
    pub(super) fn permission_handler(&self) -> PermissionSet {
        self.permission_handler_for(None)
    }

    /// `permission_handler` for a sub-agent running as `task`, which shows as
    /// waiting while a question of the sub-agent's waits on the host.
    pub(super) fn permission_handler_for(
        &self,
        task: Option<Arc<base::context::RunningTask>>,
    ) -> PermissionSet {
        self.permission_handler_asking(task, true)
    }

    /// `permission_handler_for`, where `can_ask: false` refuses what the
    /// parent's rules leave to a question instead of taking it to the host.
    /// A team member's is this one too, judged in the member's own mode.
    pub(super) fn permission_handler_asking(
        &self,
        task: Option<Arc<base::context::RunningTask>>,
        can_ask: bool,
    ) -> PermissionSet {
        let env = &self.env;
        let policy = env.host.permission.policy.clone();
        PermissionSet {
            policy: Arc::new(ParentPermission {
                parent: if can_ask {
                    policy
                } else {
                    Arc::new(NoPrompt(policy))
                },
                parent_grants: env.host.permission.grants.clone(),
                parent_session: env.parent.state.clone(),
                event_tx: env.parent.events.clone(),
                pending: env.parent.pending_permissions.clone(),
                timeout_secs: env.config.permission_prompt_timeout_secs,
                task,
                parent_exec: env.host.exec.clone(),
            }),
            grants: Arc::new(ParentGrants(env.host.permission.grants.clone())),
        }
    }

    /// A sub-agent's `Settings`: the parent's, with its own model.
    pub(super) fn sub_settings(&self, model_name: Option<&str>) -> LoadedSettings {
        let c = &self.env.config;
        let model_name = model_name.unwrap_or(&c.model).to_string();
        settings_from_parent(
            &self.env.settings,
            model_name,
            c.max_tokens,
            c.fallback_model.clone(),
        )
    }

    /// The scene a sub-agent should run in.
    ///
    /// The agent type's own `scene:` when it is no wider than the parent's,
    /// otherwise the parent's.
    pub(super) fn scene_for_subagent(&self, def: Option<&AgentTypeDefinition>) -> Arc<dyn Scene> {
        let parent = &self.env.scene;
        if let Some(name) = def.and_then(|d| d.scene.as_deref()) {
            match self.resolve_scene_id(name) {
                Some(requested) => {
                    // An agent type may *narrow* the scene, never widen it.
                    //
                    // Agent types are loaded from `.agix/agents/*.md`, which
                    // is repository content — on a cloned repo it is
                    // attacker-supplied. A scene's `tools()` whitelist is the
                    // enforcement point for "this session may not run shell
                    // commands" (the `harnix` and `mini` scenes rely on it), so
                    // letting a file in the repo pick the scene meant a narrow
                    // session could
                    // spawn `Agent(subagent_type: "x")` where `x.md` said
                    // `scene: code` and get `Bash` back. The boundary the
                    // parent scene draws has to hold across a spawn.
                    //
                    // "Narrower" is judged by tool surface: the requested
                    // scene is accepted only when everything it exposes is
                    // also exposed by the parent.
                    match parent {
                        p if !scene_is_narrower_or_equal(requested.as_ref(), p.as_ref()) => {
                            tracing::warn!(
                                scene = %name,
                                parent_scene = %p.name(),
                                agent_type = %def.map(|d| d.name.as_str()).unwrap_or(""),
                                "agent type requested a scene that grants tools the parent scene \
                                 does not; ignoring it and inheriting the parent's scene"
                            );
                        }
                        _ => return requested,
                    }
                }
                None => {
                    tracing::warn!(
                        scene = %name,
                        agent_type = %def.map(|d| d.name.as_str()).unwrap_or(""),
                        "unrecognized scene in agent type definition, inheriting parent's scene instead"
                    );
                }
            }
        }
        parent.clone()
    }

    /// A scene id resolved against the host's registry when one was wired,
    /// otherwise against the built-in set. Plugin scenes (`plugin:<name>`)
    /// only exist in the registry, so without this an agent type naming one
    /// would silently fall through to "unrecognized scene" and inherit the
    /// parent's instead.
    pub(super) fn resolve_scene_id(&self, id: &str) -> Option<Arc<dyn Scene>> {
        if let Some(registry) = &self.env.host.scene_registry {
            return registry.resolve(id);
        }
        builtin_scene_by_id(id)
    }
}

/// Does `candidate` expose a subset of `parent`'s tools?
///
/// Used to decide whether an agent type's `scene:` override may be honored —
/// see `scene_for_subagent`. A scene's `tools()` is an allow-list, with the
/// empty list meaning "everything registered is allowed"; `disallowed_tools()`
/// subtracts from it.
///
/// So: a parent with an empty allow-list (CodeScene) permits any child. A
/// parent with a real allow-list permits only a child whose own allow-list is
/// non-empty and contained in it, minus anything the parent disallows.
pub(super) fn scene_is_narrower_or_equal(candidate: &dyn Scene, parent: &dyn Scene) -> bool {
    let parent_allow = parent.tools();
    let parent_deny = parent.disallowed_tools();
    if parent_allow.is_empty() && parent_deny.is_empty() {
        return true;
    }
    let candidate_allow = candidate.tools();
    if candidate_allow.is_empty() {
        // "Everything registered" is never a subset of a real allow-list.
        return false;
    }
    candidate_allow
        .iter()
        .all(|t| !parent_deny.contains(t) && (parent_allow.is_empty() || parent_allow.contains(t)))
}
