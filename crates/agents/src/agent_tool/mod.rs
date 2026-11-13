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
//! `delegate_tools()` applies when spawning sub-agents.

use anyhow::anyhow;
use async_trait::async_trait;
#[cfg(test)]
use base::context::EngineConfig;
use base::event::AgentEvent;
use base::interface::model::Model;
use base::interface::permission::{PermissionFacts, PermissionSet, Reach, Subject};
use base::interface::scene::Scene;
use base::paths::PromptFiles;
use base::settings::{Effort, LoadedSettings, Settings};
#[cfg(test)]
use base::settings::{
    ExecutionSettings, ModelSettings, PathSettings, PermissionMode, SandboxConfig, ThinkingMode,
};
use base::tool::ProgressSender;
use base::tool::ToolContext;
use base::tool::ToolResultContent;
use base::tool::{InMemoryToolRegistry, PathStyle, ToolSpec};
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
mod mcp_grant;
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
use mcp_grant::*;
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
    pub permission_mode: Option<base::settings::PermissionMode>,
}

pub struct AgentTool {
    inner: Arc<Inner>,
}

/// Where a session's agent types come from.
pub(crate) enum TypeCatalog<'a> {
    /// Scanned from `dirs` (low → high priority) over the built-in types and
    /// `plugin_types`, and watched for changes.
    Watched {
        dirs: &'a [(std::path::PathBuf, AgentTypeSource)],
        plugin_types: &'a [AgentTypeDefinition],
    },
    /// A catalog the host keeps and watches for all its sessions, with this
    /// session's plugins' types over it.
    Shared {
        map: SharedAgentTypeMap,
        session_types: &'a [AgentTypeDefinition],
    },
}

/// What an `AgentTool` is built from besides its parent session.
pub(crate) struct Parts<'a> {
    pub(crate) types: TypeCatalog<'a>,
    /// What the parent session was built from.
    pub(crate) template: Arc<InMemoryToolRegistry>,
    /// The parent session's own registry.
    pub(crate) parent_tools: Arc<InMemoryToolRegistry>,
    /// The tools the parent's plugins registered in it.
    pub(crate) plugin_tools: &'a [String],
    pub(crate) slots: SubagentSlots,
    pub(crate) team_registry: Option<Arc<team::registry::TeamRegistry>>,
}

impl AgentTool {
    pub(crate) fn new(env: crate::delegate::DelegateEnv, parts: Parts<'_>) -> Self {
        let Parts {
            types,
            template,
            parent_tools,
            plugin_tools,
            slots,
            team_registry,
        } = parts;
        let (agent_types, _agent_type_watcher, session_agent_types) = match types {
            TypeCatalog::Watched { dirs, plugin_types } => {
                let merged = merge_agent_types(dirs, plugin_types, &env.config.prompt_files);
                let map: SharedAgentTypeMap = Arc::new(std::sync::RwLock::new(Arc::new(merged)));
                // A watcher that cannot start leaves the catalog as it was
                // built, as `SkillManager::enable_watching` does.
                let watcher = crate::type_watcher::AgentTypeWatcher::watch(
                    dirs.to_vec(),
                    plugin_types.to_vec(),
                    env.config.prompt_files.clone(),
                    map.clone(),
                )
                .map(Arc::new)
                .map_err(|e| {
                    tracing::warn!(error = %e, "failed to enable agent-type file watching; live reload disabled for this session");
                })
                .ok();
                (map, watcher, std::collections::HashMap::new())
            }
            TypeCatalog::Shared { map, session_types } => (
                map,
                None,
                session_types
                    .iter()
                    .map(|d| (d.name.clone(), d.clone()))
                    .collect(),
            ),
        };
        let description: Arc<str> = {
            let mut visible = agent_types
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
                .clone();
            for (name, definition) in &session_agent_types {
                if visible.get(name).is_none_or(|d| !d.source.may_shadow()) {
                    visible.insert(name.clone(), definition.clone());
                }
            }
            describe_agent_types(&visible).into()
        };
        Self {
            inner: Arc::new(Inner {
                env,
                fallback_tools: template,
                parent_tools,
                subagent_slots: slots,
                hook_agents: Default::default(),
                agent_types,
                _agent_type_watcher,
                description,
                plugin_tools: Arc::new(plugin_tools.iter().cloned().collect()),
                session_agent_types: Arc::new(session_agent_types),
                team_registry,
                team_members: Default::default(),
                idle_seq_counter: Default::default(),
                team_spawn_lock: Default::default(),
            }),
        }
    }
}

#[async_trait]
impl base::tool::Tool for AgentTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new()
            .callable_from_code(true)
            .code_result("the sub-agent's final text; an object when output_schema is given")
            .concurrency_safe(true)
    }
    async fn permission_facts(&self, _: &Value, _: &ToolContext) -> PermissionFacts {
        PermissionFacts::new(Subject::None, Reach::Delegates)
    }
    fn name(&self) -> &str {
        AGENT_TOOL_NAME
    }
    fn description(&self) -> &str {
        &self.inner.description
    }
    fn input_schema(&self, _: PathStyle) -> Value {
        serde_json::to_value(schemars::schema_for!(AgentInput)).unwrap_or(Value::Null)
    }
    async fn guide(&self, _: &base::tool::PromptContext) -> Option<String> {
        Some(include_str!("agent_tool.prompt.md").to_string())
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

        // The schema offers `team_name`/`name` whether or not this session
        // has team coordination on; with it off they are refused here, as
        // `TeamCreate` is by not being registered.
        match (&inp.team_name, &inp.name) {
            (Some(team_name), Some(name)) => {
                if self.inner.team_registry.is_none() {
                    return Err(base::error::ToolError::Validation(
                        "team_name/name require team coordination to be enabled for this \
                         session (settings.execution.team_enabled)"
                            .into(),
                    ));
                }
                if matches!(
                    inp.permission_mode,
                    Some(base::settings::PermissionMode::Bubble)
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
                &tools::bash::engine_exec(&ctx),
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
        let tools = self.delegate_tools(inp.subagent_type.as_deref(), &[], &[]);
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
