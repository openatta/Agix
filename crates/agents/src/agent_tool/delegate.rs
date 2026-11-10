//! Foreground and background delegate runs on `AgentTool`: `run_sub*`, background
//! spawn and host-started runs.

use super::*;

impl AgentTool {
    /// Core: run sub-agent and collect text output.
    ///
    /// `subagent_type` — when it names a type in the merged catalog (built-in
    /// or `.agix/agents/*.md`) that declares a `model` override, that model
    /// is used instead of the parent's. `None` (or an unknown/type-less
    /// call, e.g. from the generic `AgentSpawner` bridge used by team/skill-fork)
    /// keeps the parent's model.
    pub(crate) async fn run_sub(
        &self,
        prompt: String,
        tools: Arc<InMemoryToolRegistry>,
        cwd: std::path::PathBuf,
        cancel: tokio_util::sync::CancellationToken,
        perm: Arc<dyn Permission>,
        subagent_type: Option<&str>,
    ) -> Result<String, base::error::ToolError> {
        self.run_sub_tagged(
            prompt,
            tools,
            cwd,
            cancel,
            perm,
            subagent_type,
            None,
            SubRun::default(),
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
        perm: Arc<dyn Permission>,
        subagent_type: Option<&str>,
        parent: Option<(&str, u32, &str)>,
        run: SubRun,
    ) -> Result<String, base::error::ToolError> {
        let admission = self.inner.admit().map_err(base::error::ToolError::Denied)?;
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

    /// `run_sub` without an agent type, on `model`.
    pub(crate) async fn run_sub_on_model(
        &self,
        prompt: String,
        tools: Arc<InMemoryToolRegistry>,
        cwd: std::path::PathBuf,
        cancel: tokio_util::sync::CancellationToken,
        perm: Arc<dyn Permission>,
        model: String,
    ) -> Result<String, base::error::ToolError> {
        self.run_sub_tagged(
            prompt,
            tools,
            cwd,
            cancel,
            perm,
            None,
            None,
            SubRun {
                model: Some(model),
                ..SubRun::default()
            },
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
        perm: Arc<dyn Permission>,
        subagent_type: Option<&str>,
        permission_mode: Option<base::interface::settings::PermissionMode>,
        seat: Option<base::interface::agent_spawner::TeamSeat>,
    ) -> Result<String, base::error::ToolError> {
        let admission = self
            .inner
            .admit_team_member()
            .map_err(base::error::ToolError::Denied)?;
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
                permission_mode,
                team_member: true,
                seat,
                ..SubRun::default()
            },
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn run_sub_admitted(
        &self,
        admission: Admission,
        prompt: String,
        tools: Arc<InMemoryToolRegistry>,
        cwd: std::path::PathBuf,
        cancel: tokio_util::sync::CancellationToken,
        perm: Arc<dyn Permission>,
        subagent_type: Option<&str>,
        parent: Option<(&str, u32, &str)>,
        run: SubRun,
    ) -> Result<String, base::error::ToolError> {
        let Admission {
            depth: child_depth,
            _slot,
        } = admission;
        let cwd_for_meta = cwd.clone();
        let def = subagent_type.and_then(|t| self.inner.agent_type_def(t));
        let model_override = run
            .model
            .clone()
            .or_else(|| def.as_ref().and_then(|d| d.model.clone()));
        let mut sub_settings = self.sub_settings(model_override.as_deref(), cwd);
        if let Some(d) = &def {
            apply_agent_type_overrides(&mut sub_settings, d);
        }
        if let Some(mode) = run.permission_mode {
            sub_settings.permission_mode = mode.min_permissive(sub_settings.permission_mode);
        }
        let scene = self.inner.scene_for_subagent(def.as_ref(), &sub_settings);
        let scene_id = scene.id().to_string();
        let settings = Arc::new(sub_settings);

        let prompt = match (&def, self.inner.skill_manager()) {
            (Some(d), Some(mgr)) if !d.skills.is_empty() => {
                format!("{}{prompt}", preload_skills_text(mgr.as_ref(), &d.skills))
            }
            _ => prompt,
        };

        let sid = run
            .session_id
            .clone()
            .unwrap_or_else(|| base::id::Id::new().to_string());
        let tag = SubagentTag::new(&self.inner, &sid, subagent_type, parent)
            .map(|tag| tag.for_task(run.task.as_ref()));
        let sid_for_hook = sid.clone();
        let sid_for_meta = sid.clone();
        // Resolved once and shared by the sub-agent's own lineage and its `Meta`
        // line — two answers to "who spawned this" that must not be able to
        // disagree.
        let parent_session_id = parent
            .map(|(sid, _, _)| sid.to_string())
            .or_else(|| self.inner.parent_session_id());
        let mut builder = Builder::new()
            .session_id(sid)
            .lineage(parent_session_id.clone(), subagent_type)
            .scene(scene)
            .contributed_tool_filter(contributed_tool_filter(def.as_ref(), &run))
            .model(self.inner.model_for_subagent())
            .tools(tools)
            .settings(settings.clone())
            .agent_depth(child_depth)
            .permission(run.permission(perm));
        if let Some(schema) = &run.output_schema {
            builder = builder.output_schema(schema.clone());
        }
        if let Some(seat) = run.seat.clone() {
            builder = builder.team_seat(seat);
        }
        let history_store = self.inner.history_store();
        builder = self.inner.inherited_by_a_delegate(builder);
        let (agent, event_rx, input_tx) = builder
            .build()
            .map_err(|e| base::error::ToolError::Execution(anyhow!("build: {e}")))?;
        if let Some(store) = &history_store {
            write_sidechain_meta(
                store,
                &sid_for_meta,
                parent_session_id,
                &scene_id,
                &cwd_for_meta,
                &settings,
            )
            .await;
        }

        // SubagentStart / SubagentStop bracket the sub-agent's whole run.
        // `Stop` fires on both success and failure — a hook that cleans up
        // after a sub-agent must not be skipped just because it errored — with
        // the outcome carried in `stop_reason`.
        let payload = SubagentPayload::of(&sid_for_hook, &cwd_for_meta, &settings, subagent_type);
        self.inner
            .fire_subagent_hook(hooks::HookEvent::SubagentStart, &payload)
            .await;
        let spawned = (!run.team_member).then(|| {
            SpawnTelemetry::start(
                &self.inner,
                parent,
                &sid_for_hook,
                subagent_type,
                &settings.model.model_name,
                false,
            )
        });
        let (result, cancelled) = run_one_turn_detached(
            &sid_for_hook,
            agent,
            event_rx,
            input_tx,
            prompt,
            cancel,
            tag,
            run.task.clone(),
        )
        .await;
        if let Some(spawned) = spawned.flatten() {
            spawned.finish(result.is_err());
        }
        self.inner
            .fire_subagent_hook(
                hooks::HookEvent::SubagentStop,
                &payload.stopped(stop_reason_of(&result, cancelled)),
            )
            .await;
        // Marked terminal on failure too, so a stale `session.resume` against
        // it is rejected instead of silently continuing a "finished" sub-agent.
        mark_sidechain_terminal(history_store.as_ref(), &sid_for_hook, &result, cancelled).await;
        result
    }

    /// Background counterpart to `run_sub`, for `AgentSpawner::spawn_agent_background`
    /// (the `context: fork` + `background: true` skill path) — same shape as
    /// `launch_bg` (task registration + `tokio::spawn(run_sub_inner(..))` +
    /// status/output bookkeeping on completion), minus the `worktree`
    /// handling `launch_bg` does from `AgentInput.worktree`: skill fork
    /// doesn't have an equivalent frontmatter field, so there's nothing to
    /// branch on here. Returns the task id immediately; the caller (the
    /// `Skill` tool) hands that id back to the model to poll via
    /// `TaskOutput`/`TaskStop` — the same mechanism `Agent`'s own
    /// `background: true` argument already uses.
    pub(crate) async fn spawn_background(
        &self,
        prompt: String,
        tools: Arc<InMemoryToolRegistry>,
        cwd: std::path::PathBuf,
        cancel: tokio_util::sync::CancellationToken,
        subagent_type: Option<&str>,
        session: Arc<base::context::SessionState>,
    ) -> Result<String, base::error::ToolError> {
        let admission = self.inner.admit().map_err(base::error::ToolError::Denied)?;
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
        let run = SubRun::for_task(&task, &sid);
        let session_c = session.clone();

        tokio::spawn(async move {
            let r = without_panics(async move {
                Self::run_sub_inner(
                    &inner,
                    prompt,
                    tools,
                    cwd,
                    cancel,
                    subagent_type.as_deref(),
                    run,
                    admission,
                )
                .await
            })
            .await;
            conclude_background_task(&session_c, &task, &r, true);
        });
        Ok(tid)
    }

    pub(super) async fn launch_bg(
        &self,
        input: &AgentInput,
        ctx: &ToolContext,
    ) -> Result<base::tool::ToolResult, base::error::ToolError> {
        let admission = self.inner.admit().map_err(base::error::ToolError::Denied)?;
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
                &tools::bash::engine_git(ctx),
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
        let tools = self.resolve_tools(input.subagent_type.as_deref());
        let prompt = self.build_prompt(input.subagent_type.as_deref(), &input.prompt);
        let subagent_type = input.subagent_type.clone();
        let inner = self.inner.clone();
        let run = SubRun {
            model: input.model.clone(),
            output_schema: input.output_schema.clone(),
            ..SubRun::for_task(&task, &sid)
        };
        let session = ctx.session.clone();

        tokio::spawn(async move {
            let r = without_panics(async move {
                Self::run_sub_inner(
                    &inner,
                    prompt,
                    tools,
                    cwd,
                    outer_cancel,
                    subagent_type.as_deref(),
                    run,
                    admission,
                )
                .await
            })
            .await;
            if let Some(h) = worktree_handle.as_mut() {
                h.cleanup().await;
            }
            conclude_background_task(&session, &task, &r, true);
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
        let admission = self.inner.admit().map_err(StartAgentError::Limit)?;
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
        let tools = InMemoryToolRegistry::new();
        for tool in self.resolve_tools(request.subagent_type.as_deref()).all() {
            if lists_admit_tool(
                &request.allowed_tools,
                &request.disallowed_tools,
                tool.name(),
            ) {
                tools.register(tool);
            }
        }
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
            let r = without_panics(async move {
                Self::run_sub_inner(
                    &inner,
                    prompt,
                    Arc::new(tools),
                    cwd,
                    cancel,
                    subagent_type.as_deref(),
                    run,
                    admission,
                )
                .await
            })
            .await;
            conclude_background_task(&session, &task, &r, notify_parent);
        });
        Ok(StartedAgent {
            task_id: tid,
            agent_session_id: sid,
        })
    }
}

impl AgentTool {
    /// Static helper for background execution. `subagent_type` — see
    /// `run_sub`'s doc comment; resolved the same way against `inner.agent_types`.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn run_sub_inner(
        inner: &Inner,
        prompt: String,
        tools: Arc<InMemoryToolRegistry>,
        cwd: std::path::PathBuf,
        cancel: tokio_util::sync::CancellationToken,
        subagent_type: Option<&str>,
        run: SubRun,
        admission: Admission,
    ) -> Result<String, base::error::ToolError> {
        let Admission {
            depth: child_depth,
            _slot,
        } = admission;
        let perm: Arc<dyn Permission> =
            run.permission(inner.permission_handler_for(run.task.clone()));
        let cwd_for_meta = cwd.clone();
        let def = subagent_type.and_then(|t| inner.agent_type_def(t));
        let model_name = run
            .model
            .clone()
            .or_else(|| def.as_ref().and_then(|d| d.model.clone()));
        let mut sub_settings = Inner::sub_settings(inner, model_name.as_deref(), cwd);
        if let Some(d) = &def {
            apply_agent_type_overrides(&mut sub_settings, d);
        }
        let scene = inner.scene_for_subagent(def.as_ref(), &sub_settings);
        let scene_id = scene.id().to_string();
        let settings = Arc::new(sub_settings);

        let prompt = match (&def, inner.skill_manager()) {
            (Some(d), Some(mgr)) if !d.skills.is_empty() => {
                format!("{}{prompt}", preload_skills_text(mgr.as_ref(), &d.skills))
            }
            _ => prompt,
        };

        let sid = run
            .session_id
            .clone()
            .unwrap_or_else(|| base::id::Id::new().to_string());
        let sid_for_meta = sid.clone();
        let tag = SubagentTag::new(inner, &sid, subagent_type, None)
            .map(|tag| tag.for_task(run.task.as_ref()));
        let mut builder = Builder::new()
            .session_id(sid)
            .scene(scene)
            .contributed_tool_filter(contributed_tool_filter(def.as_ref(), &run))
            .model(inner.model_for_subagent())
            .tools(tools)
            .settings(settings.clone())
            .agent_depth(child_depth)
            .permission(perm);
        if let Some(schema) = &run.output_schema {
            builder = builder.output_schema(schema.clone());
        }
        let history_store = inner.history_store();
        builder = inner.inherited_by_a_delegate(builder);
        let (mut agent, mut event_rx, input_tx) = builder
            .build()
            .map_err(|e| base::error::ToolError::Execution(anyhow!("build: {e}")))?;
        if let Some(store) = &history_store {
            write_sidechain_meta(
                store,
                &sid_for_meta,
                inner.parent_session_id(),
                &scene_id,
                &cwd_for_meta,
                &settings,
            )
            .await;
        }

        let payload = SubagentPayload::of(&sid_for_meta, &cwd_for_meta, &settings, subagent_type);
        inner
            .fire_subagent_hook(hooks::HookEvent::SubagentStart, &payload)
            .await;
        open_inbox(run.task.as_deref(), &agent);
        let spawned = SpawnTelemetry::start(
            inner,
            None,
            &sid_for_meta,
            subagent_type,
            &settings.model.model_name,
            true,
        );
        let (result, cancelled) = run_one_turn(
            &mut agent,
            &mut event_rx,
            &input_tx,
            prompt,
            cancel,
            tag,
            run.task.as_deref(),
        )
        .await;
        if let Some(spawned) = spawned {
            spawned.finish(result.is_err());
        }
        drop(input_tx);
        inner
            .fire_subagent_hook(
                hooks::HookEvent::SubagentStop,
                &payload.stopped(stop_reason_of(&result, cancelled)),
            )
            .await;
        mark_sidechain_terminal(history_store.as_ref(), &sid_for_meta, &result, cancelled).await;
        result
    }
}
