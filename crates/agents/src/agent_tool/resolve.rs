//! What a delegate is resolved from its parent: its tools, prompt, model,
//! settings, scene, MCP grant and permission.

use super::*;

impl Delegator {
    /// `prompt` under the system prompt of `agent_type`, when it names one.
    pub(crate) fn build_prompt(&self, agent_type: Option<&str>, prompt: &str) -> String {
        match agent_type.and_then(|name| self.agent_type_def(name)) {
            Some(def) => format!("{}\n\nTask: {prompt}", def.system_prompt),
            None => prompt.to_string(),
        }
    }

    pub(crate) async fn fire_subagent_hook(
        &self,
        event: hooks::HookEvent,
        sub: &SubagentPayload,
        stop: &tokio_util::sync::CancellationToken,
    ) {
        if let Some(runner) = self.env.hooks() {
            sub.fire(&runner, event, stop).await;
        }
    }

    /// The model instance a sub-agent spawn should use. Every spawn point is
    /// the same task type — `task_models` in settings.json is keyed by a
    /// small fixed taxonomy (`main`/`subagent`/`compact`/`memory`/
    /// `web_search`), not by `subagent_type` (which agent *kind* — "general-purpose",
    /// "code-reviewer", etc. — a different axis entirely), so the lookup
    /// key here is always the literal `"subagent"` task type regardless of
    /// which `subagent_type` was requested.
    pub(crate) fn model_for_subagent(&self) -> Arc<dyn Model> {
        match &self.env.host.task_router {
            Some(router) => router.model_for("subagent"),
            None => self.env.model.clone(),
        }
    }

    pub(crate) fn agent_type_def(&self, name: &str) -> Option<AgentTypeDefinition> {
        self.catalog.get(name)
    }

    /// The MCP servers a delegate of type `def` is granted: the parent's named
    /// in the type's `mcp_servers`. Which of their tools it is offered is its
    /// tool policy's to decide, as for every other tool.
    pub(crate) fn mcp_granted_to(
        &self,
        def: Option<&AgentTypeDefinition>,
    ) -> Option<Box<dyn base::interface::mcp_servers::McpServers>> {
        let def = def.filter(|d| !d.mcp_servers.is_empty())?;
        let Some(parent) = &self.env.mcp else {
            tracing::warn!(
                agent_type = %def.name,
                "agent type names mcp_servers, but this session has no MCP servers to grant"
            );
            return None;
        };
        Some(parent.granted(Some(&def.mcp_servers)))
    }

    /// How the delegate `delegate_id` asks about its calls: of the parent's
    /// policy, with `rules` — its deny rules and the grants it starts with —
    /// besides; its questions go to the parent's host as `task`'s, or are
    /// refused when `asks` says nobody can be asked.
    pub(crate) fn relay(
        &self,
        delegate_id: &str,
        task: Option<Arc<base::context::RunningTask>>,
        asks: AskRoute,
        rules: Vec<base::permission::PermissionRule>,
    ) -> Relay {
        let env = &self.env;
        let turn_grants = Arc::new(TurnGrants::default());
        Relay {
            permission: PermissionSet {
                policy: Arc::new(DelegateRelay {
                    parent: env.host.permission.policy.clone(),
                    parent_session: env.parent.state.clone(),
                    asks,
                    rules,
                    turn_grants: turn_grants.clone(),
                }),
                grants: Arc::new(ParentGrants {
                    parent: env.host.permission.grants.clone(),
                    turn: turn_grants,
                }),
            },
            elicitation: Arc::new(ParentElicitation {
                delegate_id: delegate_id.to_string(),
                events: env.parent.events.clone(),
                pending: env.parent.pending_permissions.clone(),
                task,
            }),
        }
    }

    /// A sub-agent's `Settings`: the parent's, with its own model.
    pub(crate) fn sub_settings(&self, model_name: Option<&str>) -> LoadedSettings {
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
    pub(crate) fn scene_for_subagent(&self, def: Option<&AgentTypeDefinition>) -> Arc<SceneSpec> {
        let parent = &self.env.scene;
        if let Some(name) = def.and_then(|d| d.scene.as_deref()) {
            match self.resolve_scene_id(name) {
                Some(requested) => {
                    // An agent type may *narrow* the scene, never widen it.
                    //
                    // Agent types are loaded from `.agix/agents/*.md`, which
                    // is repository content — on a cloned repo it is
                    // attacker-supplied. A scene's allow list is the
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
                        p if !requested.tools.selector().within(&p.tools.selector()) => {
                            tracing::warn!(
                                scene = %name,
                                parent_scene = %p.name,
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
    /// otherwise against the built-in set (`scene::builtin`). Plugin scenes (`plugin:<name>`)
    /// only exist in the registry, so without this an agent type naming one
    /// would silently fall through to "unrecognized scene" and inherit the
    /// parent's instead.
    pub(crate) fn resolve_scene_id(&self, id: &str) -> Option<Arc<SceneSpec>> {
        if let Some(registry) = &self.env.host.scene_registry {
            return registry.resolve(id);
        }
        scene::builtin(id).map(Arc::new)
    }
}
