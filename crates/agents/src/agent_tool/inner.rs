//! `Inner`, the state shared by every `AgentTool` clone, with the slot/admission
//! accounting and the accessors a delegate session inherits from.

use super::*;

/// Excluded from the tool set a sub-agent inherits, so a "full access" type
/// (general-purpose/claude/any custom type with an empty `allowed_tools`)
/// doesn't hand delegation straight back to the child.
///
/// This filter is a convenience, not the bound: what actually stops a
/// delegation chain is `Inner::depth` counted against
/// `EngineConfig::max_agent_depth`, enforced in `spawn_guard` on every spawn
/// path and mirrored by `Builder::build()` withholding this tool at the
/// limit. Filtering alone cannot bound anything — `build()` creates a fresh
/// per-session registry and registers `Agent` into it regardless of what
/// `resolve_tools` handed over, and `RuntimeAgentSpawner` (Skill
/// `context: fork`, team members) bypasses `resolve_tools` entirely by
/// passing `sub_tools()` through unfiltered.
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

/// Leave to start one sub-agent: the depth it runs at and, when it counts
/// against the session's cap, the slot it holds until it ends.
pub(super) struct Admission {
    pub(super) depth: u32,
    pub(super) _slot: Option<tokio::sync::OwnedSemaphorePermit>,
}

/// Parent-session state wired after the `AgentTool` is built; every clone of
/// `Inner` sees the same value.
pub(super) struct SetLater<T>(Arc<std::sync::RwLock<T>>);

impl<T: Clone> SetLater<T> {
    pub(super) fn get(&self) -> T {
        self.0
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(super) fn set(&self, value: T) {
        *self
            .0
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = value;
    }
}

impl<T: Default> Default for SetLater<T> {
    fn default() -> Self {
        Self(Default::default())
    }
}

impl<T> Clone for SetLater<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

#[derive(Clone)]
pub(super) struct Inner {
    pub(super) model: Arc<dyn Model>,
    pub(super) config: Arc<EngineConfig>,
    pub(super) fallback_tools: Arc<InMemoryToolRegistry>,
    pub(super) parent_tools: Arc<InMemoryToolRegistry>,
    /// Delegation depth of the agent that *owns* this tool, not of the
    /// children it spawns — those get `depth + 1` (see `spawn_guard`).
    pub(super) depth: u32,
    /// Sub-agents running anywhere under the root session; shared by every
    /// delegate's `AgentTool` (see `AgentsModule::for_delegates`).
    pub(super) subagent_slots: SubagentSlots,
    /// Built-in types merged with any disk-loaded custom types (see
    /// `merge_agent_types`). Keyed by `subagent_type` name. `RwLock<Arc<..>>`
    /// (not a plain `Arc<..>`) so `agent_type_watcher::AgentTypeWatcher` can
    /// swap the whole map in place when a `.agix/agents/*.md` file changes —
    /// readers clone the inner `Arc` out under a brief read lock (see
    /// `Inner::agent_type_def`) rather than holding the guard, so a lookup
    /// never blocks a concurrent reload or vice versa.
    pub(super) agent_types: SharedAgentTypeMap,
    /// Kept alive only so the watcher's background thread keeps running —
    /// dropped (and thus stopped) only once every clone of this `Inner` is
    /// gone. `None` when watching wasn't set up (no watched dirs existed, or
    /// `notify` setup failed) — degrades to the pre-watcher behavior of a
    /// build-time-only catalog, same as `SkillManager::enable_watching`'s
    /// failure mode.
    pub(super) _agent_type_watcher: Option<Arc<crate::type_watcher::AgentTypeWatcher>>,
    /// Precomputed from the catalog at construction time — see
    /// `describe_agent_types`. Deliberately **not** kept live-reloaded like
    /// `agent_types` itself: `Tool::description(&self) -> &str` returns a
    /// borrowed `&str`, which can't be produced from data read out from
    /// behind a lock inside the function body (nothing to borrow it from).
    /// Net effect: a subagent_type added on disk after this `AgentTool` was
    /// built becomes *usable* immediately (via `agent_type_def`), but won't
    /// appear in the model-facing catalog text (`description()`) until the
    /// next full rebuild.
    pub(super) description: Arc<str>,
    /// The parent `Agent`'s own settings, when known (set via
    /// `.with_settings(...)`) — `sub_settings()`/`run_sub_inner()` clone this
    /// (overriding only the model fields) instead of a hardcoded
    /// `scope: "code"` stand-in, so a sub-agent's skills/hooks/agents
    /// lookups resolve against the parent's *real* scope, not a guess.
    /// `None` when unset (e.g. in tests, or the generic `AgentSpawner`
    /// bridge) falls back to a hardcoded `"code"` scope.
    pub(super) parent_settings: Option<Arc<Settings>>,
    /// Multi-provider per-task-type model routing — see
    /// `Builder::task_router` / `Inner::model_for_subagent`. `None` (the
    /// default) means every sub-agent spawn inherits `model` unchanged,
    /// exactly matching behavior before multi-provider routing existed.
    pub(super) task_router: Option<Arc<base::provider::TaskRouter>>,
    /// The plugins every delegate is built with, as its parent was — the
    /// QuickJS layer among them, which decides itself what follows a delegate.
    pub(super) delegate_plugins: Vec<Arc<dyn runtime::plugin::Plugin>>,
    /// The tools those plugins registered in this session. A delegate is not
    /// handed these copies: its own plugins register its own.
    pub(super) plugin_tools: Arc<std::collections::HashSet<String>>,
    /// Agent types this session's plugins declare, over the shared catalog:
    /// above built-in and package types, below the ones on disk.
    pub(super) session_agent_types: Arc<std::collections::HashMap<String, AgentTypeDefinition>>,
    pub(super) process_groups: Option<Arc<platform::GroupLedger>>,
    /// What every delegate is assembled with: the parent session's modules.
    pub(super) modules: Vec<Arc<dyn runtime::module::Module>>,
    /// The parent session's `SkillManager`, when known — used to resolve
    /// the `skills:` preload field on `AgentTypeDefinition`. Set via
    /// `AgentTool::set_skill_catalog` *after* construction (interior
    /// mutability, `RwLock` not a plain field) because the catalog is set
    /// later in the same assembly (`AgentsModule::on_session` in
    /// `agents/src/lib.rs`). `None` (tests,
    /// the generic `AgentSpawner` bridge) means `skills:` preload is
    /// silently skipped, matching how `parent_settings`/`task_router`
    /// absence degrades elsewhere in this struct.
    pub(super) skill_manager:
        SetLater<Option<Arc<dyn base::interface::skill_catalog::SkillCatalog>>>,
    /// A snapshot of the parent session's connected MCP tools, used to
    /// resolve `AgentTypeDefinition.mcp_servers` by reusing already-
    /// connected tool instances (not reconnecting). Same interior-
    /// mutability / set-after-construction pattern as `skill_manager` and
    /// for the same reason (`Builder::build()` connects MCP after
    /// `AgentTool` exists) — but a `Vec` snapshot rather than a live
    /// `Arc<McpManager>` reference: `McpManager` isn't `Clone` and `refresh_tools()` needs
    /// `&mut self`, which would conflict with `AgentTool` also holding a
    /// shared reference to the same live instance. A subagent therefore
    /// sees the MCP tools connected *as of when this was set* — a later
    /// `config.reload` reconnect in the parent session doesn't retroactively
    /// update it, an accepted scope boundary (matches `description`'s own
    /// "not kept live" doc comment on this same struct).
    pub(super) mcp_tool_adapters: SetLater<Vec<Arc<dyn base::tool::Tool>>>,
    /// The shared `TeamRegistry` `AgentsModule::on_session` also hands to
    /// `TeamCreate`/`TeamList`/`TeamDelete` — so persistent team members
    /// (spawned here, not through those tools) update the same state
    /// `TeamList` reads. Same interior-mutability / set-after-construction
    /// pattern as `skill_manager`/`mcp_tool_adapters`: `None` when team
    /// coordination isn't enabled for this session (`settings.execution.
    /// team_enabled == false`) or in tests that don't wire it — persistent
    /// team members just don't update `TeamList`'s view in that case (they
    /// still work; `TeamList` isn't reachable either when team_enabled is
    /// off, since it isn't registered).
    pub(super) team_registry: SetLater<Option<Arc<team::registry::TeamRegistry>>>,
    /// Live persistent team members, keyed by `(team_name, member_name)`.
    /// See `AgentTool::spawn_or_message_team_member`'s doc comment for the
    /// full mechanism. Scoped to this `AgentTool` (i.e. this session) —
    /// members don't survive a daemon restart or migrate across sessions.
    pub(super) team_members:
        Arc<std::sync::RwLock<std::collections::HashMap<(String, String), PersistentMember>>>,
    /// Source of `PersistentMember::idle_seq` values — one counter shared
    /// by every member in this session, so "oldest idle" has a single,
    /// unambiguous, collision-free ordering across all of them.
    pub(super) idle_seq_counter: Arc<std::sync::atomic::AtomicU64>,
    /// Serializes `spawn_or_message_team_member`'s whole find-or-create
    /// decision. Without this, two concurrent calls for the same
    /// `(team_name, member_name)` key (the `Agent` tool is
    /// `is_concurrency_safe`, so this is reachable — e.g. the model issuing
    /// two parallel tool calls that both target the same not-yet-existing
    /// member) can both read "doesn't exist yet" before either has
    /// inserted, and both spawn — the second insert silently overwrites the
    /// first's map entry while its loop keeps running orphaned, unreachable
    /// and never reclaimed. One session-wide lock (not per-key) is enough:
    /// the section it guards is cheap (map lookup/insert, sync `Agent`
    /// construction, no real I/O), so serializing it isn't a meaningful
    /// bottleneck for something that isn't a hot path to begin with.
    pub(super) team_spawn_lock: Arc<tokio::sync::Mutex<()>>,
    /// The **parent session's** event channel. Every event a sub-agent emits
    /// is re-emitted here wrapped in `AgentEvent::SubagentProgress`, so the
    /// host can render sub-agent activity live instead of seeing a long
    /// pause followed by a wall of text. `None` (tests, embedders that
    /// never call `set_event_sender`): events are still collected internally
    /// into the returned text, they just aren't mirrored anywhere.
    ///
    /// Same interior-mutability, set-after-construction pattern as
    /// `skill_manager` — `Builder::build()` creates the `AgentTool` before it
    /// creates the session's event channel.
    pub(super) event_tx: SetLater<Option<runtime::agent::EventSender>>,
    /// The parent session's hook runner, so sub-agent spawns can fire
    /// `SubagentStart`/`SubagentStop`. Same set-after-construction pattern as
    /// `event_tx` — `Builder::build()` constructs the `AgentTool` before the
    /// `HookRunner` exists. `None` (tests, embedders that never wire it)
    /// fires nothing. Weak: the runner's `"agent"` hooks spawn through this
    /// tool, and a strong edge back would keep both alive past the session.
    pub(super) hooks: SetLater<Option<std::sync::Weak<hooks::HookRunner>>>,
    /// Cancelled when the parent session ends; every persistent team
    /// member's token is a child of it.
    pub(super) session_ended: SetLater<tokio_util::sync::CancellationToken>,
    /// The parent session's history store, so a sub-agent's transcript is
    /// persisted and a failed sub-agent can be inspected afterwards.
    /// The sub-agent gets its own fresh session id, so it lands in its own
    /// JSONL file — the parent's transcript is never written to.
    pub(super) history_store: SetLater<Option<Arc<dyn HistoryStore>>>,
    /// The **parent agent's** `Permission` impl. Sub-agent permission checks
    /// are routed here instead of being hard-permitted, so a session
    /// that opts into a real permission handler can't be bypassed just by
    /// spawning a sub-agent. `None` means `AlwaysPermit`, for `AgentTool`s
    /// built without wiring (tests).
    pub(super) parent_permission: SetLater<Option<Arc<dyn Permission>>>,
    /// The **parent agent's** pending-permission registry, so a sub-agent's
    /// `Prompt` outcome can be turned into a real, answerable question
    /// instead of an automatic refusal.
    ///
    /// A sub-agent has no channel of its own to a human; the parent does.
    /// Registering the prompt here and emitting it on `event_tx` puts it in
    /// front of the same host that answers the parent's own prompts, via the
    /// same `session.respondToPrompt` RPC. `None` (unwired embedders) refuses
    /// with an explanation.
    pub(super) parent_pending_permissions: SetLater<Option<runtime::agent::PendingPermissions>>,
    /// The parent agent's scene. Sub-agents inherit it rather than
    /// always being built as `CodeScene`. `None` falls
    /// back to resolving `Settings.paths.scope` via `resolve_scene_id`, then to
    /// `CodeScene`.
    pub(super) parent_scene: SetLater<Option<Arc<dyn Scene>>>,
    /// Every scene this process knows about, for resolving an agent type's
    /// `scene:` id. `None` falls back to [`builtin_scene_by_id`], which is
    /// what an embedder that never wires a registry (tests, library callers)
    /// gets — the built-in four. A host that registers
    /// scenes beyond those (the daemon, which also registers plugin scenes)
    /// wires its registry here so those ids resolve too.
    pub(super) scene_registry: SetLater<Option<Arc<scene::SceneRegistry>>>,
    /// The parent's resolved instruction file (AGENTS.md / CLAUDE.md), for
    /// the case where it came from `Builder::instruction_file(..)` and so
    /// isn't visible in `Settings` — see `settings_from_parent`.
    pub(super) instruction_file: SetLater<Option<std::path::PathBuf>>,
    /// The parent `Agent`'s own session id, so a freshly spawned sub-agent's
    /// first `Meta` line can record `parent_session_id` and thereby become
    /// discoverable via `HistoryStore::child_sessions`. Same set-after-
    /// construction pattern as `event_tx`/`hooks`/`history_store` —
    /// `Builder::build()` creates the `AgentTool` before its own session id
    /// is finalized. `None` (tests, embedders that never call
    /// `set_parent_session_id`) means spawned sub-agents' `Meta` lines are
    /// written with `parent_session_id: None` — they still persist, they're
    /// just not linkable back to a parent.
    pub(super) parent_session_id: SetLater<Option<String>>,
    /// The parent `Agent`'s own telemetry handle — sub-agents built through
    /// this `AgentTool` inherit it instead of each falling back to
    /// `Builder::build()`'s own noop default (a channel whose receiver is
    /// dropped immediately, silently discarding every event). Same set-
    /// after-construction pattern as the other parent-state fields on this
    /// struct. `None` (tests, embedders that never call
    /// `set_telemetry_handle`) means spawned sub-agents get `Builder`'s
    /// usual noop fallback.
    pub(super) telemetry_handle: SetLater<Option<TelemetryHandle>>,
}

impl Inner {
    /// Depth to build a child at, or the message to hand the model when the
    /// chain has run out of room.
    ///
    /// Every spawn path goes through here — the `Agent` tool, Skill
    /// `context: fork` and team members via `RuntimeAgentSpawner`, and
    /// background spawns — because that is the only place all of them share.
    /// Bounding them individually by tool-registry filtering does not work:
    /// `RuntimeAgentSpawner` hands `sub_tools()` over unfiltered, and
    /// `Builder::build()` registers `Agent` into the fresh per-session
    /// registry it creates for the child regardless.
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

    /// TeamCreate members are bounded by `team_stage_concurrency` instead.
    pub(super) fn admit_team_member(&self) -> Result<Admission, String> {
        Ok(Admission {
            depth: self.spawn_guard()?,
            _slot: None,
        })
    }

    pub(super) fn spawn_guard(&self) -> Result<u32, String> {
        let max = self.config.max_agent_depth;
        if self.depth >= max {
            return Err(format!(
                "Delegation depth limit reached ({max}). This agent is already {} \
                 level(s) deep and cannot spawn another sub-agent. Complete the \
                 task directly instead of delegating it.",
                self.depth
            ));
        }
        Ok(self.depth + 1)
    }

    /// Everything a delegated session inherits from the one that spawned it.
    ///
    /// Written once because the list is not derivable from anything: each item
    /// is on it because somebody noticed it was missing, and the noticing
    /// happened once per item — a transcript that was not persisted, telemetry
    /// dropped on the floor, a policy that stopped at the first delegation. A
    /// fifth spawn site copying three of the four is the shape every one of
    /// those had.
    ///
    /// Deliberately not here: what a delegate decides for itself — its scene,
    /// its model, its tool set, its depth, its own settings. Those differ per
    /// spawn site; `build_delegate` resolves them from its `DelegatePlan`.
    pub(super) fn inherited_by_a_delegate(&self, mut builder: Builder) -> Builder {
        // A policy that stops at the first delegation is not a policy.
        for plugin in &self.delegate_plugins {
            builder = builder.plugin(plugin.clone());
        }
        // Its own session id keeps a delegate's transcript in a file of its
        // own rather than mixed into the parent's.
        if let Some(store) = self.history_store() {
            builder = builder.history_store(store);
        }
        // Without this, every event a delegate produces — tool timings,
        // permission decisions, cost — goes to `Builder::build()`'s noop.
        if let Some(handle) = self.telemetry_handle() {
            builder = builder.telemetry_handle(handle);
        }
        if let Some(ledger) = &self.process_groups {
            builder = builder.process_groups(ledger.clone());
        }
        // One runner for the whole tree: `once` holds across it, and the
        // root's file watcher is the only one.
        if let Some(runner) = self.hooks() {
            builder = builder.hooks(runner);
        }
        builder.modules(self.modules.iter().cloned())
    }

    pub(super) fn hooks(&self) -> Option<Arc<hooks::HookRunner>> {
        self.hooks.get().and_then(|runner| runner.upgrade())
    }

    pub(super) fn session_ended(&self) -> tokio_util::sync::CancellationToken {
        self.session_ended.get()
    }

    pub(super) async fn fire_subagent_hook(&self, event: hooks::HookEvent, sub: &SubagentPayload) {
        if let Some(runner) = self.hooks() {
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
        match &self.task_router {
            Some(router) => router.model_for("subagent"),
            None => self.model.clone(),
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

    pub(super) fn skill_manager(
        &self,
    ) -> Option<Arc<dyn base::interface::skill_catalog::SkillCatalog>> {
        self.skill_manager.get()
    }

    pub(super) fn mcp_tool_adapters(&self) -> Vec<Arc<dyn base::tool::Tool>> {
        self.mcp_tool_adapters.get()
    }

    pub(super) fn event_tx(&self) -> Option<runtime::agent::EventSender> {
        self.event_tx.get()
    }

    pub(super) fn history_store(&self) -> Option<Arc<dyn HistoryStore>> {
        self.history_store.get()
    }

    pub(super) fn instruction_file(&self) -> Option<std::path::PathBuf> {
        self.instruction_file.get()
    }

    pub(super) fn parent_session_id(&self) -> Option<String> {
        self.parent_session_id.get()
    }

    pub(super) fn telemetry_handle(&self) -> Option<TelemetryHandle> {
        self.telemetry_handle.get()
    }

    /// See `AgentTool::permission_handler` — same resolution, reachable from
    /// the static `run_sub_inner` path (background spawns).
    pub(super) fn permission_handler(&self) -> Arc<dyn Permission> {
        self.permission_handler_for(None)
    }

    /// `permission_handler` for a sub-agent running as `task`, which shows as
    /// waiting while a question of the sub-agent's waits on the host.
    pub(super) fn permission_handler_for(
        &self,
        task: Option<Arc<base::context::RunningTask>>,
    ) -> Arc<dyn Permission> {
        self.permission_handler_asking(task, true)
    }

    /// `permission_handler_for`, where `can_ask: false` refuses what the
    /// parent's rules leave to a question instead of taking it to the host.
    pub(super) fn permission_handler_asking(
        &self,
        task: Option<Arc<base::context::RunningTask>>,
        can_ask: bool,
    ) -> Arc<dyn Permission> {
        if let Some(parent) = self.parent_permission.get() {
            return Arc::new(ParentPermission {
                inner: if can_ask {
                    parent
                } else {
                    Arc::new(NoPrompt(parent))
                },
                event_tx: self.event_tx(),
                pending: self.parent_pending_permissions.get(),
                timeout_secs: self.config.permission_prompt_timeout_secs,
                own_tools: Default::default(),
                task,
                session: Default::default(),
            });
        }
        Arc::new(AlwaysPermit)
    }

    /// A team member's permission: the parent's, judged in the member's own
    /// mode, as any sub-agent's is — the parent's deny rules and its host's
    /// answers reach the member. With no parent wired (an `AgentTool` built
    /// on its own) there are no rules to inherit, and the member is judged by
    /// its mode alone.
    pub(super) fn team_member_permission(
        &self,
        tools: &Arc<InMemoryToolRegistry>,
        mode: base::interface::settings::PermissionMode,
    ) -> Arc<dyn Permission> {
        let wired = self.parent_permission.get().is_some();
        if wired {
            return self.permission_handler();
        }
        Arc::new(permissions::rule_set_permission::RuleSetPermission::new(
            Arc::new(permissions::gate::PermissionGate::empty()),
            tools.clone(),
            mode.into(),
        ))
    }

    /// Build a sub-agent's `Settings`, inheriting the parent's when known.
    pub(super) fn sub_settings(
        inner: &Inner,
        model_name: Option<&str>,
        cwd: std::path::PathBuf,
    ) -> Settings {
        let c = &inner.config;
        let model_name = model_name.unwrap_or(&c.model).to_string();
        let instruction_file = inner.instruction_file();
        match &inner.parent_settings {
            Some(parent) => settings_from_parent(
                parent,
                model_name,
                c.max_tokens,
                c.fallback_model.clone(),
                instruction_file.as_ref(),
            ),
            None => fallback_settings(
                model_name,
                c.max_tokens,
                c.fallback_model.clone(),
                cwd,
                instruction_file.as_ref(),
            ),
        }
    }

    /// The parent's tools a delegate starts from: all of them but the ones
    /// the parent's scene contributed. Those belong to the scene, and the
    /// delegate's own scene contributes its tools again when it is built —
    /// inheriting them as well would register each twice.
    pub(super) fn inherited_tools(&self) -> Vec<Arc<dyn base::tool::Tool>> {
        let parent_scene = self.parent_scene.get();
        let scene_tools: std::collections::HashSet<String> = parent_scene
            .map(|scene| {
                scene
                    .extra_tools()
                    .iter()
                    .map(|t| t.name().to_string())
                    .collect()
            })
            .unwrap_or_default();
        let mut out: Vec<Arc<dyn base::tool::Tool>> = Vec::new();
        for tool in self
            .parent_tools
            .all()
            .into_iter()
            .chain(self.fallback_tools.all())
        {
            if !scene_tools.contains(tool.name())
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
    /// Precedence: the agent type's own `scene:` override > the parent's
    /// scene (wired via `AgentTool::set_scene`) > the scene named by
    /// `Settings.paths.scope` > `CodeScene`. The last two steps mean a
    /// daemon session inherits correctly even without explicit wiring: the
    /// daemon sets `scope` to `scene.id()`.
    pub(super) fn scene_for_subagent(
        &self,
        def: Option<&AgentTypeDefinition>,
        settings: &Settings,
    ) -> Arc<dyn Scene> {
        let parent = self.parent_scene.get();
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
                    match parent.as_ref() {
                        Some(p) if !scene_is_narrower_or_equal(requested.as_ref(), p.as_ref()) => {
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
        if let Some(parent) = parent {
            return parent;
        }
        self.resolve_scene_id(&settings.paths.scope)
            .unwrap_or_else(|| Arc::new(scene::code::CodeScene::default()))
    }

    /// A scene id resolved against the host's registry when one was wired,
    /// otherwise against the built-in set. Plugin scenes (`plugin:<name>`)
    /// only exist in the registry, so without this an agent type naming one
    /// would silently fall through to "unrecognized scene" and inherit the
    /// parent's instead.
    pub(super) fn resolve_scene_id(&self, id: &str) -> Option<Arc<dyn Scene>> {
        if let Some(registry) = self.scene_registry.get() {
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
