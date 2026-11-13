//! `Inner`, the state shared by every `AgentTool` clone, with the slot/admission
//! accounting and the accessors a delegate session inherits from.

use super::*;

/// Never inherited: a delegate's own agents module registers its own `Agent`,
/// counted from the delegate's depth, or none at the limit. The parent's would
/// spawn at the parent's depth, past `EngineConfig::max_agent_depth`.
///
/// The bound itself is `Inner::depth` against `max_agent_depth`, enforced in
/// `spawn_guard` on every spawn path.
pub(super) const AGENT_TOOL_NAME: &str = "Agent";

/// A live persistent team member (`Agent` tool's `team_name`+`name` mode).
/// Its `Agent`/`event_rx`/`input_tx` live inside `run_persistent_member_loop`
/// (the spawned task `join` would represent, if kept — not kept here since
/// nothing currently awaits it; `cancel` is enough to stop it).
pub(super) struct PersistentMember {
    /// Internal-only identifier, never surfaced to the model — routing is
    /// always by the `(team_name, member_name)` map key (reusing the same
    /// name is *how* you keep messaging the same member; that's the point
    /// of persistence). This exists purely for logs/observability so two
    /// members that happened to reuse a name at different times (one
    /// stopped, a new one spawned under the same key later) are
    /// distinguishable in a trace.
    pub(super) id: String,
    pub(super) message_tx: tokio::sync::mpsc::UnboundedSender<PersistentMessage>,
    pub(super) cancel: tokio_util::sync::CancellationToken,
    /// Epoch seconds since this member last went idle (finished a turn with
    /// nothing queued after it), or `i64::MAX` while it's currently
    /// processing one — the sentinel keeps a mid-turn member permanently
    /// ineligible for the idle-timeout reclaim rule (b) without needing a
    /// separate `Active`/`Idle` flag alongside the timestamp. Wall-clock,
    /// so it's meaningful to compare against `team_member_idle_timeout_secs`
    /// and to show in `TeamList` — but *not* used to order which member is
    /// "oldest idle" for the total-count reclaim rule (a): see `idle_seq`.
    pub(super) idle_since: Arc<std::sync::atomic::AtomicI64>,
    /// Monotonically increasing on every idle transition (`u64::MAX` while
    /// `Active`, same sentinel pattern as `idle_since`) — reclaim rule (a)
    /// sorts by *this*, not by `idle_since`. Two members can easily go idle
    /// within the same wall-clock second (`idle_since` only has 1-second
    /// resolution), which would make a plain timestamp sort non-
    /// deterministically tie-break between them; a strictly-increasing
    /// counter can't tie, so "oldest idle" always means the same thing
    /// regardless of how fast members actually run.
    pub(super) idle_seq: Arc<std::sync::atomic::AtomicU64>,
}

pub(super) struct PersistentMessage {
    pub(super) prompt: String,
    pub(super) task: Arc<base::context::RunningTask>,
}

pub const DEFAULT_MAX_CONCURRENT_SUBAGENTS: usize = 6;

/// The cap on sub-agents running at once in one session tree.
#[derive(Clone, Debug)]
pub struct SubagentSlots {
    pub(super) slots: Arc<tokio::sync::Semaphore>,
    pub(super) cap: usize,
}

impl SubagentSlots {
    pub fn new(cap: usize) -> Self {
        let cap = cap.max(1);
        Self {
            slots: Arc::new(tokio::sync::Semaphore::new(cap)),
            cap,
        }
    }

    pub fn running(&self) -> usize {
        self.cap - self.slots.available_permits()
    }
}

/// The sessions of the agents running for `"type": "agent"` hooks. Their own
/// events start no agent hooks: a guard that reads a file would otherwise ask
/// another guard about it, and that one another, until the first times out.
#[derive(Clone, Default)]
pub(super) struct HookAgents(Arc<std::sync::Mutex<std::collections::HashSet<String>>>);

impl HookAgents {
    pub(super) fn includes(&self, session_id: &str) -> bool {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(session_id)
    }

    pub(super) fn enter(&self, session_id: String) -> HookAgentRunning {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(session_id.clone());
        HookAgentRunning {
            agents: self.clone(),
            session_id,
        }
    }
}

pub(super) struct HookAgentRunning {
    agents: HookAgents,
    session_id: String,
}

impl Drop for HookAgentRunning {
    fn drop(&mut self) {
        self.agents
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.session_id);
    }
}

/// Leave to start one sub-agent: the depth it runs at and, when it counts
/// against the session's cap, the slot it holds until it ends.
pub(super) struct Admission {
    pub(super) depth: u32,
    pub(super) _slot: Option<tokio::sync::OwnedSemaphorePermit>,
}

pub(super) struct Inner {
    pub(super) env: crate::delegate::DelegateEnv,
    /// What the parent session was built from.
    pub(super) fallback_tools: Arc<InMemoryToolRegistry>,
    /// The parent session's own registry, as it stands when a delegate spawns.
    pub(super) parent_tools: Arc<InMemoryToolRegistry>,
    /// Sub-agents running anywhere under the root session; shared by every
    /// delegate's `AgentTool` (see `AgentsModule::for_delegates`).
    pub(super) subagent_slots: SubagentSlots,
    pub(super) hook_agents: HookAgents,
    /// Swapped in place by the watcher; readers clone the inner `Arc` out
    /// rather than hold the guard (see `Inner::agent_type_def`).
    pub(super) agent_types: SharedAgentTypeMap,
    /// Kept alive only so the watcher's thread keeps running.
    pub(super) _agent_type_watcher: Option<Arc<crate::type_watcher::AgentTypeWatcher>>,
    /// Computed once: `Tool::description` hands out a borrowed `&str`, which
    /// cannot come from behind the catalog's lock. A type added on disk later
    /// is usable at once but not listed until the next rebuild.
    pub(super) description: Arc<str>,
    /// The tools the parent's plugins registered in it. A delegate is not
    /// handed these copies: its own plugins register its own.
    pub(super) plugin_tools: Arc<std::collections::HashSet<String>>,
    /// Agent types this session's plugins declare, over the shared catalog:
    /// above built-in and package types, below the ones on disk.
    pub(super) session_agent_types: Arc<std::collections::HashMap<String, AgentTypeDefinition>>,
    /// Shared with `TeamCreate`/`TeamList`/`TeamDelete`; `None` when team
    /// coordination is off for this session.
    pub(super) team_registry: Option<Arc<team::registry::TeamRegistry>>,
    /// Live persistent team members, keyed by `(team_name, member_name)`.
    pub(super) team_members:
        Arc<std::sync::RwLock<std::collections::HashMap<(String, String), PersistentMember>>>,
    /// Source of `PersistentMember::idle_seq`: one counter for every member
    /// of the session, so "oldest idle" has a single ordering.
    pub(super) idle_seq_counter: Arc<std::sync::atomic::AtomicU64>,
    /// Serializes `spawn_or_message_team_member`'s find-or-create: two
    /// concurrent calls for one new key would otherwise both spawn, and the
    /// second insert would orphan the first member's loop.
    pub(super) team_spawn_lock: Arc<tokio::sync::Mutex<()>>,
}

impl Inner {
    /// Depth to build a child at, or the message to hand the model when the
    /// chain has run out of room.
    ///
    /// Every spawn path goes through here — the `Agent` tool, Skill
    /// `context: fork` and team members via `RuntimeAgentSpawner`, and
    /// background spawns — because that is the only place all of them share.
    /// Tool filtering cannot bound them: a delegate's own agents module
    /// registers an `Agent` of its own below the limit.
    ///
    /// Refusing with a normal tool error rather than a panic or a silent
    /// no-op is the point: the model sees "you cannot delegate further" and
    /// can do the work itself, which is what a runaway retry loop needs in
    /// order to terminate instead of recursing until the process dies.
    /// Refuses rather than waits when the session is at its cap: an agent
    /// holding a slot while it waits for one for its own child could
    /// deadlock a tree whose every slot is held that way.
    pub(super) fn admit(&self) -> Result<Admission, String> {
        let depth = self.spawn_guard()?;
        let slot = self
            .subagent_slots
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| {
                format!(
                    "{} sub-agents are already running in this session, the most it allows \
                     (execution.max_concurrent_subagents). Wait for one to finish (TaskWait), \
                     or do this part yourself.",
                    self.subagent_slots.cap
                )
            })?;
        Ok(Admission {
            depth,
            _slot: Some(slot),
        })
    }

    /// [`admit`](Self::admit) for a program's fan-out: at the cap it waits
    /// for a slot instead of refusing. Only a root session runs programs and
    /// a root session holds no slot, so what it waits on is never itself.
    pub(super) async fn admit_waiting(
        &self,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<Admission, String> {
        let depth = self.spawn_guard()?;
        let slot = tokio::select! {
            slot = self.subagent_slots.slots.clone().acquire_owned() => {
                slot.map_err(|_| "the session's sub-agent slots are closed".to_string())?
            }
            _ = cancel.cancelled() => {
                return Err("stopped while waiting for a sub-agent slot".into());
            }
        };
        Ok(Admission {
            depth,
            _slot: Some(slot),
        })
    }

    /// Leave that takes no slot: for `TeamCreate` members, bounded by
    /// `team_stage_concurrency` instead, and for hook agents, which are the
    /// session's configuration rather than the model's to start.
    pub(super) fn admit_unslotted(&self) -> Result<Admission, String> {
        Ok(Admission {
            depth: self.spawn_guard()?,
            _slot: None,
        })
    }

    pub(super) fn spawn_guard(&self) -> Result<u32, String> {
        let max = self.env.config.max_agent_depth;
        if self.env.depth >= max {
            return Err(format!(
                "Delegation depth limit reached ({max}). This agent is already {} \
                 level(s) deep and cannot spawn another sub-agent. Complete the \
                 task directly instead of delegating it.",
                self.env.depth
            ));
        }
        Ok(self.env.depth + 1)
    }

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

    /// Look up one agent type by name, cloning it out from behind the
    /// `agent_types` lock. Always returns an owned value (never a borrow
    /// tied to the read guard) — `agent_types` can be swapped in place by
    /// the watcher, and a borrow into the replaced map would
    /// dangle the instant a reload swaps it out. The clone itself is cheap:
    /// a handful of strings and a short tool-name list, not a hot-path
    /// concern at sub-agent-spawn frequency.
    pub(super) fn agent_type_def(&self, name: &str) -> Option<AgentTypeDefinition> {
        let shared = self
            .agent_types
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(name)
            .cloned();
        match (shared, self.session_agent_types.get(name)) {
            (Some(on_disk), _) if on_disk.source.may_shadow() => Some(on_disk),
            (_, Some(declared)) => Some(declared.clone()),
            (shared, None) => shared,
        }
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

    /// The tools a delegate starts from: the ones its parent was built from
    /// (`fallback_tools`, the template) that the parent still has
    /// (`parent_tools`, its session registry) — so nothing its scene took away.
    /// What the parent's engine, modules, scene and plugins registered on top
    /// is not among them; the delegate's own build registers its own of each.
    pub(super) fn inherited_tools(&self) -> Vec<Arc<dyn base::tool::Tool>> {
        let mut out: Vec<Arc<dyn base::tool::Tool>> = Vec::new();
        for tool in self.fallback_tools.list() {
            if tool.name() != AGENT_TOOL_NAME
                && self.parent_tools.get(tool.name()).is_some()
                && !self.plugin_tools.contains(tool.name())
                && !out.iter().any(|t| t.name() == tool.name())
            {
                out.push(tool);
            }
        }
        out
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
