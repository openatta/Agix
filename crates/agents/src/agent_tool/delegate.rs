//! Foreground and background delegate runs on `AgentTool`: `run_sub*`, background
//! spawn and host-started runs.

use super::*;

impl AgentTool {
    /// The parent session's state, once wired: where a delegate's task is
    /// registered and whose directory it inherits.
    pub(crate) fn parent_session_state(&self) -> Arc<base::context::SessionState> {
        self.inner.env.parent.state.clone()
    }

    /// Cancelled when the parent session ends.
    pub(crate) fn parent_session_ended(&self) -> tokio_util::sync::CancellationToken {
        self.inner.env.parent.ended.clone()
    }

    /// The parent's permission, refusing what it would ask about.
    pub(crate) fn sub_permission_refusing_questions(&self) -> PermissionSet {
        self.inner.permission_handler_asking(None, false)
    }

    /// Core: run sub-agent and collect text output.
    ///
    /// `subagent_type` — when it names a type in the merged catalog (built-in
    /// or `.agix/agents/*.md`) that declares a `model` override, that model
    /// is used instead of the parent's. `None` (or an unknown/type-less
    /// call, e.g. from the generic `AgentSpawner` bridge used by team/skill-fork)
    /// keeps the parent's model. `model`, when given, is used over both.
    /// `allowed_tools` narrows what the delegate's own modules contribute, as
    /// it already narrowed `tools`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn run_sub(
        &self,
        prompt: String,
        tools: Arc<InMemoryToolRegistry>,
        cwd: std::path::PathBuf,
        cancel: tokio_util::sync::CancellationToken,
        perm: PermissionSet,
        subagent_type: Option<&str>,
        model: Option<String>,
        allowed_tools: Vec<String>,
    ) -> Result<String, base::error::ToolError> {
        self.run_sub_tagged(
            prompt,
            tools,
            cwd,
            cancel,
            perm,
            subagent_type,
            None,
            SubRun {
                model,
                allowed_tools,
                ..SubRun::default()
            },
        )
        .await
    }

    /// The agent behind a `"type": "agent"` hook on an event of
    /// `judged_session`, run to its verdict. It takes none of the session's
    /// sub-agent slots: those are the model's to fill, and a guard the model
    /// can crowd out by holding them all is no guard — its hook would fail
    /// and, failing open, let the call through. What bounds hook agents
    /// instead is that one's own events start none (`HookAgents`).
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn run_hook_agent(
        &self,
        judged_session: &str,
        prompt: String,
        tools: Arc<InMemoryToolRegistry>,
        cwd: std::path::PathBuf,
        cancel: tokio_util::sync::CancellationToken,
        subagent_type: Option<&str>,
        model: Option<String>,
        allowed_tools: Vec<String>,
    ) -> Result<String, base::error::ToolError> {
        if self.inner.limits.hook_agents.includes(judged_session) {
            return Err(base::error::ToolError::Denied(
                "an agent hook does not judge the agent running for another agent hook".into(),
            ));
        }
        let admission = self
            .inner
            .limits
            .admit_unslotted()
            .map_err(base::error::ToolError::Denied)?;
        let sid = base::id::Id::new().to_string();
        let _running = self.inner.limits.hook_agents.enter(sid.clone());
        let perm = self.sub_permission();
        self.run_sub_admitted(
            admission,
            prompt,
            tools,
            cwd,
            cancel,
            perm,
            subagent_type,
            None,
            SubRun {
                session_id: Some(sid),
                model,
                allowed_tools,
                ..SubRun::default()
            },
        )
        .await
    }

    /// `run_sub` plus the parent's `(session_id, turn_no)` for event
    /// attribution. Split out rather than folded into `run_sub`'s signature
    /// so the cross-crate `AgentSpawner` bridge (which has no `ToolContext`,
    /// and lives behind a trait in `base`) keeps working unchanged — it just
    /// forwards events with an empty parent id, which still carries the
    /// stable per-run `agent_label`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn run_sub_tagged(
        &self,
        prompt: String,
        tools: Arc<InMemoryToolRegistry>,
        cwd: std::path::PathBuf,
        cancel: tokio_util::sync::CancellationToken,
        perm: PermissionSet,
        subagent_type: Option<&str>,
        parent: Option<(&str, u32, &str)>,
        run: SubRun,
    ) -> Result<String, base::error::ToolError> {
        let admission = self
            .inner
            .limits
            .admit()
            .map_err(base::error::ToolError::Denied)?;
        self.run_sub_admitted(
            admission,
            prompt,
            tools,
            cwd,
            cancel,
            perm,
            subagent_type,
            parent,
            run,
        )
        .await
    }

    /// `run_sub` for a `TeamCreate` member, in the team's `permission_mode`
    /// held to the parent's.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn run_team_member(
        &self,
        prompt: String,
        tools: Arc<InMemoryToolRegistry>,
        cwd: std::path::PathBuf,
        cancel: tokio_util::sync::CancellationToken,
        subagent_type: Option<&str>,
        model: Option<String>,
        permission_mode: Option<base::settings::PermissionMode>,
        seat: Option<base::interface::agent_spawner::TeamSeat>,
    ) -> Result<String, base::error::ToolError> {
        let admission = self
            .inner
            .limits
            .admit_unslotted()
            .map_err(base::error::ToolError::Denied)?;
        let grant = match permission_mode {
            Some(_) => Grant::TeamMember,
            None => Grant::Given(self.sub_permission()),
        };
        self.inner
            .run_delegate(
                admission,
                OneShot {
                    prompt,
                    tools,
                    cwd,
                    cancel,
                    subagent_type,
                    grant,
                    parent: None,
                    run: SubRun {
                        model,
                        permission_mode,
                        team_member: true,
                        seat,
                        ..SubRun::default()
                    },
                    runs_on: RunsOn::Foreground,
                },
            )
            .await
            .0
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn run_sub_admitted(
        &self,
        admission: Admission,
        prompt: String,
        tools: Arc<InMemoryToolRegistry>,
        cwd: std::path::PathBuf,
        cancel: tokio_util::sync::CancellationToken,
        perm: PermissionSet,
        subagent_type: Option<&str>,
        parent: Option<(&str, u32, &str)>,
        run: SubRun,
    ) -> Result<String, base::error::ToolError> {
        self.inner
            .run_delegate(
                admission,
                OneShot {
                    prompt,
                    tools,
                    cwd,
                    cancel,
                    subagent_type,
                    grant: Grant::Given(perm),
                    parent,
                    run,
                    runs_on: RunsOn::Foreground,
                },
            )
            .await
            .0
    }

    /// Background counterpart to `run_sub`, for `AgentSpawner::spawn` in `SpawnMode::Background`
    /// (the `context: fork` + `background: true` skill path) — same shape as
    /// `launch_bg` (task registration + `tokio::spawn(run_sub_inner(..))` +
    /// status/output bookkeeping on completion), minus the `worktree`
    /// handling `launch_bg` does from `AgentInput.worktree`: skill fork
    /// doesn't have an equivalent frontmatter field, so there's nothing to
    /// branch on here. Returns the task id immediately; the caller (the
    /// `Skill` tool) hands that id back to the model to poll via
    /// `TaskOutput`/`TaskStop` — the same mechanism `Agent`'s own
    /// `background: true` argument already uses.
    ///
    /// The task runs under a child of `cancel`: stopping the task must not
    /// cancel the turn that started it.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn spawn_background(
        &self,
        prompt: String,
        tools: Arc<InMemoryToolRegistry>,
        cwd: std::path::PathBuf,
        cancel: tokio_util::sync::CancellationToken,
        subagent_type: Option<&str>,
        model: Option<String>,
        allowed_tools: Vec<String>,
        session: Arc<base::context::SessionState>,
    ) -> Result<String, base::error::ToolError> {
        let admission = self
            .inner
            .limits
            .admit()
            .map_err(base::error::ToolError::Denied)?;
        let cancel = cancel.child_token();
        let tid = bg_task_id();
        let sid = base::id::Id::new().to_string();
        let task = session.register_task(
            tid.clone(),
            base::context::TaskType::Agent,
            cancel.clone(),
            serde_json::json!({
                "agent_type": subagent_type,
                "background": true,
                "agent_session_id": sid,
            }),
        );
        let subagent_type = subagent_type.map(str::to_string);
        let inner = self.inner.clone();
        let run = SubRun {
            model,
            allowed_tools,
            ..SubRun::for_task(&task, &sid)
        };
        let session_c = session.clone();

        tokio::spawn(async move {
            let (r, cancelled) = without_panics(async move {
                Self::run_sub_inner(
                    &inner,
                    prompt,
                    tools,
                    cwd,
                    cancel,
                    subagent_type.as_deref(),
                    None,
                    run,
                    admission,
                )
                .await
            })
            .await;
            conclude_background_task(&session_c, &task, &r, cancelled, true);
        });
        Ok(tid)
    }

    pub(super) async fn launch_bg(
        &self,
        input: &AgentInput,
        ctx: &ToolContext,
    ) -> Result<base::tool::ToolResult, base::error::ToolError> {
        let admission = self
            .inner
            .limits
            .admit()
            .map_err(base::error::ToolError::Denied)?;
        let tid = bg_task_id();
        let sid = base::id::Id::new().to_string();
        let outer_cancel = ctx.cancel.child_token();
        let task = ctx.session.register_task(
            tid.clone(),
            base::context::TaskType::Agent,
            outer_cancel.clone(),
            serde_json::json!({
                "agent_type": input.subagent_type,
                "description": input.description,
                "background": true,
                "tool_use_id": ctx.tool_use_id,
                "agent_session_id": sid,
            }),
        );
        let mut worktree_handle = match &input.worktree {
            Some(s) => match create_worktree(
                &ctx.exec,
                &tools::bash::engine_exec(ctx),
                &ctx.session.cwd,
                s,
            )
            .await
            {
                Ok(h) => Some(h),
                Err(e) => {
                    ctx.session.finish_task(
                        &tid,
                        base::context::RunningStatus::Failed(format!("worktree: {e}")),
                    );
                    return Ok(bg_result(&tid, "worktree failed"));
                }
            },
            None => None,
        };
        let cwd = worktree_handle
            .as_ref()
            .map(|h| h.path().to_path_buf())
            .unwrap_or_else(|| ctx.session.cwd.clone());
        let tools = self.delegate_tools(input.subagent_type.as_deref(), &[], &[]);
        let prompt = self.build_prompt(input.subagent_type.as_deref(), &input.prompt);
        let subagent_type = input.subagent_type.clone();
        let inner = self.inner.clone();
        let run = SubRun {
            model: input.model.clone(),
            output_schema: input.output_schema.clone(),
            ..SubRun::for_task(&task, &sid)
        };
        let session = ctx.session.clone();
        let (parent_session_id, parent_turn, parent_tool_use_id) =
            (ctx.session_id.clone(), ctx.turn_no, ctx.tool_use_id.clone());

        tokio::spawn(async move {
            let (r, cancelled) = without_panics(async move {
                Self::run_sub_inner(
                    &inner,
                    prompt,
                    tools,
                    cwd,
                    outer_cancel,
                    subagent_type.as_deref(),
                    Some((&parent_session_id, parent_turn, &parent_tool_use_id)),
                    run,
                    admission,
                )
                .await
            })
            .await;
            if let Some(h) = worktree_handle.as_mut() {
                h.cleanup().await;
            }
            conclude_background_task(&session, &task, &r, cancelled, true);
        });
        Ok(bg_result(&tid, "spawned"))
    }

    /// A sub-agent a host asked for (`session.startAgent`): in the
    /// background, as a task of `session`, under the same limits as the
    /// model's own `Agent` calls and never with more than they would get.
    /// It stops when `session_cancel` does.
    pub(crate) fn start_for_host(
        &self,
        request: base::interface::agent_spawner::AgentRequest,
        session: Arc<base::context::SessionState>,
        session_cancel: tokio_util::sync::CancellationToken,
    ) -> Result<
        base::interface::agent_spawner::StartedAgent,
        base::interface::agent_spawner::StartAgentError,
    > {
        use base::interface::agent_spawner::{StartAgentError, StartedAgent};
        let admission = self.inner.limits.admit().map_err(StartAgentError::Limit)?;
        let tid = bg_task_id();
        let sid = base::id::Id::new().to_string();
        let cancel = session_cancel.child_token();
        let task = session.register_task(
            tid.clone(),
            base::context::TaskType::Agent,
            cancel.clone(),
            serde_json::json!({
                "agent_type": request.subagent_type,
                "description": request.description,
                "background": true,
                "agent_session_id": sid,
                "tag": request.tag,
            }),
        );
        let tools = self.delegate_tools(
            request.subagent_type.as_deref(),
            &request.allowed_tools,
            &request.disallowed_tools,
        );
        let prompt = self.build_prompt(request.subagent_type.as_deref(), &request.prompt);
        let run = SubRun {
            model: request.model,
            allowed_tools: request.allowed_tools,
            disallowed_tools: request.disallowed_tools,
            deny_rules: request
                .permission_rules
                .into_iter()
                .filter(|r| r.behavior == base::permission::RuleBehavior::Deny)
                .collect(),
            output_schema: request.output_schema,
            ..SubRun::for_task(&task, &sid)
        };
        let inner = self.inner.clone();
        let subagent_type = request.subagent_type;
        let notify_parent = request.notify_parent;
        let cwd = session.cwd.clone();
        tokio::spawn(async move {
            let (r, cancelled) = without_panics(async move {
                Self::run_sub_inner(
                    &inner,
                    prompt,
                    tools,
                    cwd,
                    cancel,
                    subagent_type.as_deref(),
                    None,
                    run,
                    admission,
                )
                .await
            })
            .await;
            conclude_background_task(&session, &task, &r, cancelled, notify_parent);
        });
        Ok(StartedAgent {
            task_id: tid,
            agent_session_id: sid,
        })
    }
}

impl AgentTool {
    /// A background run: the sub-agent's questions reach the host, and it
    /// shows as waiting on `run.task` while one does. Returns the answer and
    /// whether `cancel` stopped the run.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn run_sub_inner(
        inner: &Inner,
        prompt: String,
        tools: Arc<InMemoryToolRegistry>,
        cwd: std::path::PathBuf,
        cancel: tokio_util::sync::CancellationToken,
        subagent_type: Option<&str>,
        parent: Option<(&str, u32, &str)>,
        run: SubRun,
        admission: Admission,
    ) -> (Result<String, base::error::ToolError>, bool) {
        let permission = inner.permission_handler_for(run.task.clone());
        inner
            .run_delegate(
                admission,
                OneShot {
                    prompt,
                    tools,
                    cwd,
                    cancel,
                    subagent_type,
                    grant: Grant::Given(permission),
                    parent,
                    run,
                    runs_on: RunsOn::Background,
                },
            )
            .await
    }
}
