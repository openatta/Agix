//! `AgentTool` — spawn sub-agents using the agent's own `Agent` engine.
//!
//! Implements `base::tool::Tool` for the `Agent` invocable by the model.
//! Each sub-agent is a session of its own built with `Builder::build()`; it
//! inherits the parent's model client but gets a restricted tool set.
//!
//! # Agent type registry
//!
//! The module defines built-in agent types (`builtin_agent_types()`) and can
//! load user-defined types from a directory (e.g. `~/.agix/scenes/<scope>/agents/*.md`
//! or `<project>/.agix/agents/*.md`) via `load_agent_types_from_dir()`. Each
//! type specifies a system prompt and an allowed tool set, which
//! `resolve_tools()` applies when spawning sub-agents.

use anyhow::anyhow;
use async_trait::async_trait;
use base::context::EngineConfig;
use base::interface::event::AgentEvent;
use base::interface::model::Model;
use base::interface::permission::Permission;
use base::interface::scene::Scene;
use base::interface::settings::{
    Effort, ExecutionSettings, ModelSettings, PathSettings, PermissionMode, SandboxConfig,
    Settings, ThinkingMode,
};
use base::tool::ProgressSender;
use base::tool::ToolContext;
use base::tool::ToolResultContent;
use base::tool::{InMemoryToolRegistry, ToolSpec};
use history::store::HistoryStore;
use runtime::agent::{Agent, Builder, EventReceiver, InputMessage, InputSender};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::path::Path;
use std::sync::Arc;
use telemetry::TelemetryHandle;
use tools::worktree::create_worktree;

pub use base::agent_type::{parse_permission_mode, AgentTypeDefinition, AgentTypeSource};

mod build;
mod catalog;
mod delegate;
mod inner;
mod members;
mod permission;
mod resolve;
mod settings;
mod spawn_meta;
mod task;
#[cfg(test)]
mod tests;
mod turn;

use build::*;
pub use catalog::*;
pub use inner::*;
use permission::*;
use settings::*;
use spawn_meta::*;
pub(crate) use task::*;
use turn::*;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AgentInput {
    pub prompt: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, alias = "subagentType")]
    pub subagent_type: Option<String>,
    #[serde(default)]
    pub worktree: Option<String>,
    #[serde(default, alias = "run_in_background", alias = "runInBackground")]
    pub background: bool,
    #[serde(default)]
    pub model: Option<String>,
    /// A JSON Schema the sub-agent's final answer must satisfy; the answer is
    /// then also returned as structured content.
    #[serde(default)]
    pub output_schema: Option<Value>,
    /// A team to join, together with `name`. If that member is alive, `prompt`
    /// is queued as its next message instead of spawning a new one. Needs
    /// team mode enabled for the session.
    // The schema cannot hide this per session, so `AgentTool::call` rejects it
    // when team mode is off.
    #[serde(default)]
    pub team_name: Option<String>,
    /// This member's name within `team_name`. Reuse the same pair to keep
    /// messaging the same member.
    #[serde(default)]
    pub name: Option<String>,
    /// With `team_name` + `name`: the permission mode a new member runs under
    /// for its whole lifetime (default `plan`, read-only). The first member
    /// spawned under a team fixes it for the team; messaging a live member
    /// ignores it. `bubble` is rejected.
    // Fixed at spawn because a persistent member's lead is not around to
    // answer per-call prompts (see `AgentTool::build_team_member_agent`), and
    // `bubble` needs that lead.
    #[serde(default)]
    pub permission_mode: Option<base::interface::settings::PermissionMode>,
}

pub struct AgentTool {
    inner: Arc<Inner>,
}

impl AgentTool {
    /// Construct with only the built-in agent types (no `.agix/agents/`
    /// scan) — convenience for callers that don't need custom types (e.g.
    /// tests, or embedders that manage their own catalog).
    pub fn new(
        model: Arc<dyn Model>,
        config: Arc<EngineConfig>,
        fallback_tools: Arc<InMemoryToolRegistry>,
    ) -> Self {
        Self::with_parent_tools(
            model,
            config,
            fallback_tools.clone(),
            fallback_tools,
            &[],
            &[],
        )
    }

    /// `agent_dirs` are scanned low → high priority (see `merge_agent_types`)
    /// for `.agix/agents/*.md` custom type definitions, on top of the 6
    /// built-in types and any `plugin_agent_types`. Pass `&[]` for either to
    /// skip that tier.
    pub fn with_parent_tools(
        model: Arc<dyn Model>,
        config: Arc<EngineConfig>,
        parent_tools: Arc<InMemoryToolRegistry>,
        fallback_tools: Arc<InMemoryToolRegistry>,
        agent_dirs: &[(std::path::PathBuf, AgentTypeSource)],
        plugin_agent_types: &[AgentTypeDefinition],
    ) -> Self {
        let agent_types = merge_agent_types(agent_dirs, plugin_agent_types);
        let description: Arc<str> = describe_agent_types(&agent_types).into();
        let agent_types = Arc::new(std::sync::RwLock::new(Arc::new(agent_types)));

        // Live reload: watch the same directories for `.md` changes and keep
        // `agent_types` current without a session rebuild — see
        // `agent_type_watcher`'s module doc for why a full re-merge (not a
        // per-file patch) is the correct reload strategy here. `notify`
        // setup failing (permissions, inotify/kqueue limits) degrades to the
        // pre-watcher, build-time-only catalog rather than failing
        // construction — same non-fatal handling as
        // `SkillManager::enable_watching`.
        let _agent_type_watcher = crate::type_watcher::AgentTypeWatcher::watch(
            agent_dirs.to_vec(),
            plugin_agent_types.to_vec(),
            agent_types.clone(),
        )
        .map(Arc::new)
        .map_err(|e| {
            tracing::warn!(error = %e, "failed to enable agent-type file watching; live reload disabled for this session");
        })
        .ok();

        Self {
            inner: Arc::new(Inner {
                model,
                config,
                fallback_tools,
                parent_tools,
                depth: 0,
                subagent_slots: SubagentSlots::new(DEFAULT_MAX_CONCURRENT_SUBAGENTS),
                agent_types,
                _agent_type_watcher,
                description,
                parent_settings: None,
                task_router: None,
                delegate_plugins: Vec::new(),
                plugin_tools: Default::default(),
                session_agent_types: Default::default(),
                process_groups: None,
                modules: Vec::new(),
                skill_manager: Default::default(),
                mcp_tool_adapters: Default::default(),
                team_registry: Default::default(),
                team_members: Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
                idle_seq_counter: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                team_spawn_lock: Arc::new(tokio::sync::Mutex::new(())),
                event_tx: Default::default(),
                hooks: Default::default(),
                session_ended: Default::default(),
                history_store: Default::default(),
                parent_permission: Default::default(),
                parent_pending_permissions: Default::default(),
                parent_scene: Default::default(),
                scene_registry: Default::default(),
                instruction_file: Default::default(),
                parent_session_id: Default::default(),
                telemetry_handle: Default::default(),
            }),
        }
    }

    /// Like [`with_parent_tools`](Self::with_parent_tools), but for a daemon
    /// session that shares one `SessionPool`-level agent-type catalog +
    /// watcher instead of building (and watching) its own.
    ///
    /// `with_parent_tools` spawns a dedicated `AgentTypeWatcher` — its own
    /// `notify` background thread — per call, i.e. per session. In a daemon
    /// with many concurrent sessions all watching the exact same three
    /// `agents/` directories, that's N redundant threads for one set of
    /// directories. Here, `shared_agent_types` is expected to already be
    /// live-reloaded by a watcher the caller owns (see
    /// `SessionPool::agent_types` / `SessionPool::_agent_type_watcher`) — no
    /// watcher is started by this constructor, and `Inner::_agent_type_watcher`
    /// is `None`. `description` is computed from a one-time read of
    /// `shared_agent_types`'s *current* contents at construction time — same
    /// documented "functional reload without cosmetic reload" boundary as
    /// `with_parent_tools` has, but shared across sessions rather than reset
    /// per session.
    pub fn with_shared_agent_types(
        model: Arc<dyn Model>,
        config: Arc<EngineConfig>,
        parent_tools: Arc<InMemoryToolRegistry>,
        fallback_tools: Arc<InMemoryToolRegistry>,
        shared_agent_types: SharedAgentTypeMap,
    ) -> Self {
        let description: Arc<str> = {
            let snapshot = shared_agent_types
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            describe_agent_types(&snapshot).into()
        };
        Self {
            inner: Arc::new(Inner {
                model,
                config,
                fallback_tools,
                parent_tools,
                depth: 0,
                subagent_slots: SubagentSlots::new(DEFAULT_MAX_CONCURRENT_SUBAGENTS),
                agent_types: shared_agent_types,
                _agent_type_watcher: None,
                description,
                parent_settings: None,
                task_router: None,
                delegate_plugins: Vec::new(),
                plugin_tools: Default::default(),
                session_agent_types: Default::default(),
                process_groups: None,
                modules: Vec::new(),
                skill_manager: Default::default(),
                mcp_tool_adapters: Default::default(),
                team_registry: Default::default(),
                team_members: Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
                idle_seq_counter: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                team_spawn_lock: Arc::new(tokio::sync::Mutex::new(())),
                event_tx: Default::default(),
                hooks: Default::default(),
                session_ended: Default::default(),
                history_store: Default::default(),
                parent_permission: Default::default(),
                parent_pending_permissions: Default::default(),
                parent_scene: Default::default(),
                scene_registry: Default::default(),
                instruction_file: Default::default(),
                parent_session_id: Default::default(),
                telemetry_handle: Default::default(),
            }),
        }
    }

    /// Attach the parent `Agent`'s own settings so sub-agents inherit its
    /// real scope/paths. See the
    /// `Inner::parent_settings` doc comment.
    pub fn with_settings(mut self, settings: Arc<Settings>) -> Self {
        let mut inner = (*self.inner).clone();
        inner.parent_settings = Some(settings);
        self.inner = Arc::new(inner);
        self
    }

    /// Record how deep the owning agent already sits in the delegation
    /// chain, so `spawn_guard` can refuse to go past
    /// `EngineConfig::max_agent_depth`. Set by `Builder::build()` from
    /// `Builder::agent_depth`; `0` (a root session) otherwise.
    pub fn with_modules(mut self, modules: Vec<Arc<dyn runtime::module::Module>>) -> Self {
        let mut inner = (*self.inner).clone();
        inner.modules = modules;
        self.inner = Arc::new(inner);
        self
    }

    pub fn with_subagent_slots(mut self, slots: SubagentSlots) -> Self {
        let mut inner = (*self.inner).clone();
        inner.subagent_slots = slots;
        self.inner = Arc::new(inner);
        self
    }

    pub fn with_depth(mut self, depth: u32) -> Self {
        let mut inner = (*self.inner).clone();
        inner.depth = depth;
        self.inner = Arc::new(inner);
        self
    }

    /// The plugins every agent this tool spawns is built with, and the tools
    /// they registered in this session.
    pub fn with_delegate_plugins(
        mut self,
        plugins: Vec<Arc<dyn runtime::plugin::Plugin>>,
        their_tools: &[String],
    ) -> Self {
        let mut inner = (*self.inner).clone();
        inner.delegate_plugins = plugins;
        inner.plugin_tools = Arc::new(their_tools.iter().cloned().collect());
        self.inner = Arc::new(inner);
        self
    }

    /// Agent types this session's plugins declare, on top of the catalog this
    /// tool was built with. Only those whose source may shadow
    /// ([`AgentTypeSource::may_shadow`]) outrank them.
    pub fn with_session_agent_types(mut self, types: &[AgentTypeDefinition]) -> Self {
        if types.is_empty() {
            return self;
        }
        let mut inner = (*self.inner).clone();
        inner.session_agent_types =
            Arc::new(types.iter().map(|d| (d.name.clone(), d.clone())).collect());
        let mut visible = inner
            .agent_types
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .clone();
        for (name, definition) in inner.session_agent_types.iter() {
            if visible.get(name).is_none_or(|d| !d.source.may_shadow()) {
                visible.insert(name.clone(), definition.clone());
            }
        }
        inner.description = describe_agent_types(&visible).into();
        self.inner = Arc::new(inner);
        self
    }

    pub fn with_process_groups(mut self, ledger: Arc<platform::GroupLedger>) -> Self {
        let mut inner = (*self.inner).clone();
        inner.process_groups = Some(ledger);
        self.inner = Arc::new(inner);
        self
    }

    /// See `Inner::task_router` / `Inner::model_for_subagent`.
    pub fn with_task_router(mut self, router: Arc<base::provider::TaskRouter>) -> Self {
        let mut inner = (*self.inner).clone();
        inner.task_router = Some(router);
        self.inner = Arc::new(inner);
        self
    }

    /// Attach the session's skill catalog so `AgentTypeDefinition.skills`
    /// (preload) can resolve.
    pub fn set_skill_catalog(&self, mgr: Arc<dyn base::interface::skill_catalog::SkillCatalog>) {
        self.inner.skill_manager.set(Some(mgr));
    }

    /// Attach a snapshot of the parent session's connected MCP tools — same
    /// interior-mutability, set-after-construction pattern as
    /// `set_skill_catalog`. Resolves `AgentTypeDefinition.mcp_servers` when
    /// a subagent is spawned. Pass `mcp_manager.tool_adapters().to_vec()`;
    /// see `Inner::mcp_tool_adapters`'s doc comment for why this takes a
    /// snapshot `Vec` rather than the live `McpManager`.
    pub fn set_mcp_tool_adapters(&self, adapters: Vec<Arc<dyn base::tool::Tool>>) {
        self.inner.mcp_tool_adapters.set(adapters);
    }

    /// Attach the same shared `TeamRegistry` `AgentsModule::on_session` hands
    /// to `TeamCreate`/`TeamList`/`TeamDelete`, so persistent team members
    /// spawned through this `AgentTool` (see `spawn_or_message_team_member`)
    /// update state those tools can see. Same interior-mutability,
    /// set-after-construction pattern as `set_skill_catalog`/
    /// `set_mcp_tool_adapters` — only called when `settings.execution.
    /// team_enabled` is true and the scene supports teams (see
    /// `AgentsModule::on_session` in `agents/src/lib.rs`).
    pub fn set_team_registry(&self, registry: Arc<team::registry::TeamRegistry>) {
        self.inner.team_registry.set(Some(registry));
    }

    /// Attach the parent session's event channel so sub-agent activity is
    /// mirrored onto it — see `Inner::event_tx`. Set-after-construction
    /// (`&self`, not the `with_X(self) -> Self` builder form) because
    /// `Builder::build()` creates this `AgentTool` before it creates the
    /// session's event channel.
    pub fn set_event_sender(&self, tx: runtime::agent::EventSender) {
        self.inner.event_tx.set(Some(tx));
    }

    /// Attach the parent session's hook runner: every delegate runs its hooks
    /// on it, and sub-agent spawns fire `SubagentStart`/`SubagentStop` there
    /// — see `Inner::hooks`.
    pub fn set_hooks(&self, runner: Arc<hooks::HookRunner>) {
        self.inner.hooks.set(Some(Arc::downgrade(&runner)));
    }

    /// The token the parent session cancels when it ends: persistent team
    /// members stop on it, mid-turn or idle.
    pub fn set_session_ended(&self, ended: tokio_util::sync::CancellationToken) {
        self.inner.session_ended.set(ended);
    }

    /// Attach the parent session's history store so sub-agent transcripts are
    /// persisted — see `Inner::history_store`.
    pub fn set_history_store(&self, store: Arc<dyn HistoryStore>) {
        self.inner.history_store.set(Some(store));
    }

    /// Record the parent `Agent`'s own session id, so sub-agents spawned
    /// from here can stamp `parent_session_id` on their own `Meta` line —
    /// see `Inner::parent_session_id`.
    pub fn set_parent_session_id(&self, id: String) {
        self.inner.parent_session_id.set(Some(id));
    }

    /// Record the parent `Agent`'s own telemetry handle, so sub-agents
    /// spawned from here inherit it instead of getting `Builder::build()`'s
    /// noop default — see `Inner::telemetry_handle`.
    pub fn set_telemetry_handle(&self, handle: TelemetryHandle) {
        self.inner.telemetry_handle.set(Some(handle));
    }

    /// Route sub-agent permission checks to the parent agent's `Permission`
    /// impl — see `Inner::parent_permission`.
    pub fn set_parent_permission(&self, permission: Arc<dyn Permission>) {
        self.inner.parent_permission.set(Some(permission));
    }

    /// Route sub-agent permission *prompts* to the parent session's host
    /// — see `Inner::parent_pending_permissions`. Without this a
    /// sub-agent still inherits the parent's rules, but anything those rules
    /// leave undecided is refused rather than asked about.
    pub fn set_parent_pending_permissions(
        &self,
        pending: Arc<
            std::sync::Mutex<
                std::collections::HashMap<
                    String,
                    tokio::sync::oneshot::Sender<runtime::agent::PermissionDecision>,
                >,
            >,
        >,
    ) {
        self.inner.parent_pending_permissions.set(Some(pending));
    }

    /// Let sub-agents inherit the parent's scene — see
    /// `Inner::parent_scene`.
    pub fn set_scene(&self, scene: Arc<dyn Scene>) {
        self.inner.parent_scene.set(Some(scene));
    }

    /// Let an agent type's `scene:` id resolve against every scene the host
    /// registered, not just the built-in four — see `Inner::scene_registry`.
    pub fn set_scene_registry(&self, registry: Arc<scene::SceneRegistry>) {
        self.inner.scene_registry.set(Some(registry));
    }

    /// Let sub-agents inherit an instruction file that the parent got from
    /// `Builder::instruction_file(..)` rather than from settings.json
    /// — see `settings_from_parent`.
    pub fn set_instruction_file(&self, path: Option<std::path::PathBuf>) {
        self.inner.instruction_file.set(path);
    }
}

#[async_trait]
impl base::tool::Tool for AgentTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            callable_from_code: true,
            code_result: Some("the sub-agent's final text; an object when output_schema is given"),
            concurrency_safe: true,
            ..ToolSpec::defaults_for(self)
        }
    }
    fn name(&self) -> &str {
        AGENT_TOOL_NAME
    }
    fn description(&self) -> &str {
        &self.inner.description
    }
    fn input_schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(AgentInput)).unwrap_or(Value::Null)
    }
    async fn prompt(&self, _: &base::tool::PromptContext) -> String {
        include_str!("agent_tool.prompt.md").to_string()
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolContext,
        _progress: ProgressSender,
    ) -> Result<base::tool::ToolResult, base::error::ToolError> {
        let inp: AgentInput = serde_json::from_value(input)
            .map_err(|e| base::error::ToolError::Validation(format!("{e}")))?;
        if ctx.from_code {
            if let Some(field) = [
                (inp.background, "background"),
                (
                    inp.team_name.is_some() || inp.name.is_some(),
                    "team_name/name",
                ),
                (inp.worktree.is_some(), "worktree"),
            ]
            .into_iter()
            .find_map(|(given, field)| given.then_some(field))
            {
                return Err(base::error::ToolError::Validation(format!(
                    "`{field}` is not available from a program: agent() runs a sub-agent and \
                     waits for it. Start that sub-agent with a direct Agent call instead."
                )));
            }
        }

        // Persistent team member — must give both team_name and name, or
        // neither; one without the other is almost certainly a mistake
        // (e.g. the model meant to join a team but dropped a field), not a
        // meaningful partial state to silently fall through on. Gated on
        // `team_registry` being set (`set_team_registry`, only called when
        // `settings.execution.team_enabled` is true — see
        // `AgentsModule::on_session` in `agents/src/lib.rs`):
        // `AgentInput`'s schema is generated once, unconditionally, so the
        // model can technically *set* `team_name`/`name` even when team
        // coordination is off for this session; without this check it would
        // silently spawn a persistent member nobody gated in via settings,
        // exactly the exposure `team_enabled` exists to prevent for
        // `TeamCreate`/`TeamList`/`TeamDelete` (see `ExecutionSettings::
        // team_enabled`'s doc comment) — this closes the same gap for the
        // `Agent` tool's own team-joining fields.
        match (&inp.team_name, &inp.name) {
            (Some(team_name), Some(name)) => {
                if self.inner.team_registry.get().is_none() {
                    return Err(base::error::ToolError::Validation(
                        "team_name/name require team coordination to be enabled for this \
                         session (settings.execution.team_enabled)"
                            .into(),
                    ));
                }
                if matches!(
                    inp.permission_mode,
                    Some(base::interface::settings::PermissionMode::Bubble)
                ) {
                    return Err(base::error::ToolError::Validation(
                        "permission_mode: \"bubble\" is not valid here — it needs an available \
                         lead, which a persistent member's lead (only runs code during its own \
                         turns) doesn't reliably have"
                            .into(),
                    ));
                }
                let tid = self
                    .spawn_or_message_team_member(
                        team_name.clone(),
                        name.clone(),
                        inp.prompt.clone(),
                        inp.subagent_type.clone(),
                        ctx.cwd.clone(),
                        ctx.session.clone(),
                        inp.permission_mode,
                    )
                    .await?;
                return Ok(bg_result(&tid, "queued for team member"));
            }
            (None, None) => {}
            _ => {
                return Err(base::error::ToolError::Validation(
                    "team_name and name must both be given together, or neither".into(),
                ));
            }
        }

        // Background
        if inp.background {
            return self.launch_bg(&inp, &ctx).await;
        }

        // Sync
        let mut worktree_handle = match &inp.worktree {
            Some(s) => match create_worktree(
                &ctx.exec,
                &tools::bash::engine_git(&ctx),
                &ctx.session.cwd,
                s,
            )
            .await
            {
                Ok(h) => Some(h),
                Err(e) => return Err(base::error::ToolError::Execution(anyhow!("worktree: {e}"))),
            },
            None => None,
        };
        let cwd = worktree_handle
            .as_ref()
            .map(|h| h.path().to_path_buf())
            .unwrap_or_else(|| ctx.session.cwd.clone());
        let tools = self.resolve_tools(inp.subagent_type.as_deref());
        let prompt = self.build_prompt(inp.subagent_type.as_deref(), &inp.prompt);
        let sid = base::id::Id::new().to_string();

        // Bound rather than matched inline so the worktree is cleaned up
        // before the result is turned into a ToolResult — the sub-agent may
        // have failed, and the temporary worktree must go either way.
        let run_cancel = ctx.cancel.child_token();
        let task_scope = base::context::TaskScope::new(
            ctx.session.clone(),
            ctx.tool_use_id.clone(),
            base::context::TaskType::Agent,
            run_cancel.clone(),
            serde_json::json!({
                "agent_type": inp.subagent_type,
                "description": inp.description,
                "background": false,
                "tool_use_id": ctx.tool_use_id,
                "agent_session_id": sid,
            }),
        );
        let perm = self
            .inner
            .permission_handler_asking(Some(task_scope.task().clone()), !ctx.from_code);
        let admission = if ctx.from_code {
            self.inner.admit_waiting(&run_cancel).await
        } else {
            self.inner.admit()
        };
        let result = match admission {
            Ok(admission) => {
                self.run_sub_admitted(
                    admission,
                    prompt,
                    tools,
                    cwd,
                    run_cancel,
                    perm,
                    inp.subagent_type.as_deref(),
                    Some((
                        ctx.session_id.as_str(),
                        ctx.turn_no,
                        ctx.tool_use_id.as_str(),
                    )),
                    SubRun {
                        model: inp.model.clone(),
                        output_schema: inp.output_schema.clone(),
                        ..SubRun::for_task(task_scope.task(), &sid)
                    },
                )
                .await
            }
            Err(refused) => Err(base::error::ToolError::Denied(refused)),
        };

        if let Some(h) = worktree_handle.as_mut() {
            h.cleanup().await;
        }
        if let Ok(text) = &result {
            task_scope.task().append_output(text);
        }
        let structured = task_scope.task().structured_output.get().cloned();

        match result {
            Ok(text) => Ok(base::tool::ToolResult {
                content: ToolResultContent::Text(text),
                is_error: false,
                structured_content: structured,
                mcp_meta: None,
                new_messages: None,
            }),
            Err(e) => Ok(base::tool::ToolResult {
                content: ToolResultContent::Text(format!("sub-agent error: {e}")),
                is_error: true,
                structured_content: None,
                mcp_meta: None,
                new_messages: None,
            }),
        }
    }
}
