//! The one place a delegated session is assembled — every mode's delegate
//! comes through `Delegator::build_delegate` — and the one-shot run around it.

use super::*;

/// What a delegate is, as its request and mode decided it; everything else
/// about it is decided in `build_delegate`.
pub(crate) struct Spec {
    pub(crate) session_id: String,
    pub(crate) agent_type: Option<String>,
    /// Over the agent type's model.
    pub(crate) model: Option<String>,
    pub(crate) tools: ToolSelector,
    pub(crate) output_schema: Option<Value>,
    /// A mode the caller asked for (a team's), held to the parent's.
    pub(crate) permission_mode: Option<base::settings::PermissionMode>,
    pub(crate) seat: Option<TeamSeat>,
    pub(crate) cwd: std::path::PathBuf,
    pub(crate) called_from: Option<CallSite>,
}

pub(crate) struct Delegate {
    pub(crate) agent: Agent,
    pub(crate) event_rx: EventReceiver,
    pub(crate) input_tx: InputSender,
    pub(crate) def: Option<AgentTypeDefinition>,
    pub(crate) settings: Arc<LoadedSettings>,
    pub(crate) payload: SubagentPayload,
    pub(crate) history_store: Option<Arc<dyn HistoryStore>>,
}

/// One prompt, one turn, then the delegate ends.
pub(crate) struct OneShot {
    pub(crate) spec: Spec,
    pub(crate) prompt: String,
    pub(crate) cancel: tokio_util::sync::CancellationToken,
    pub(crate) permission: PermissionSet,
    /// The task the run's cost, structured output and questions show on.
    pub(crate) task: Option<Arc<base::context::RunningTask>>,
    /// Registered into this delegate's own tool registry only.
    pub(crate) extra_tools: Vec<Arc<dyn base::tool::Tool>>,
    /// Already on a task of its own, with its steering inbox open; otherwise
    /// the caller awaits it and it runs on a task it is aborted with.
    pub(crate) background: bool,
    /// The team coordinator records a stage member's spawn itself.
    pub(crate) records_spawn: bool,
}

impl Delegator {
    pub(crate) async fn build_delegate(
        &self,
        spec: &Spec,
        depth: u32,
        tools: Arc<InMemoryToolRegistry>,
        permission: PermissionSet,
        extracts_memories: bool,
    ) -> Result<Delegate, base::error::ToolError> {
        let agent_type = spec.agent_type.as_deref();
        let def = agent_type.and_then(|t| self.agent_type_def(t));
        let model_override = spec
            .model
            .clone()
            .or_else(|| def.as_ref().and_then(|d| d.model.clone()));
        let mut sub_settings = self.sub_settings(model_override.as_deref());
        if let Some(d) = &def {
            apply_agent_type_overrides(&mut sub_settings.values, d);
        }
        if let Some(mode) = spec.permission_mode {
            sub_settings.values.permission_mode = mode.min_permissive(sub_settings.permission_mode);
        }
        let scene = self.scene_for_subagent(def.as_ref());
        let scene_id = scene.id().to_string();
        let settings = Arc::new(sub_settings);
        let sid = spec.session_id.clone();
        let parent_session_id = Some(self.env.parent.id.clone());
        // First, so that what the delegate decides for itself below — its
        // permission, narrowed from the parent's — replaces what it inherits.
        let builder = Builder::new().inherit(self.env.host.clone());
        let mut builder = match self.env.hooks() {
            Some(runner) => builder.hooks(runner),
            None => builder,
        }
        .session_id(sid.clone())
        .lineage(parent_session_id.clone(), agent_type)
        .scene(scene)
        .contributed_tool_filter(contributed_tool_filter(def.as_ref(), &spec.tools))
        .model(self.model_for_subagent())
        .tools(tools)
        .settings(settings.clone())
        .agent_depth(depth)
        .memory_extraction(extracts_memories)
        .permission(permission);
        if let Some(schema) = &spec.output_schema {
            builder = builder.output_schema(schema.clone());
        }
        if let Some(seat) = spec.seat.clone() {
            builder = builder.team_seat(seat);
        }
        if let Some(servers) = self.mcp_granted_to(def.as_ref(), &spec.tools) {
            builder = builder.mcp_servers(servers);
        }
        let history_store = self.env.host.history_store.clone();
        let (agent, event_rx, input_tx) = builder
            .build()
            .map_err(|e| base::error::ToolError::Execution(anyhow!("build: {e}")))?;
        let cwd = &spec.cwd;
        if let Some(store) = &history_store {
            write_sidechain_meta(store, &sid, parent_session_id, &scene_id, cwd, &settings).await;
        }
        let payload = SubagentPayload::of(&sid, cwd, &settings, agent_type);
        Ok(Delegate {
            agent,
            event_rx,
            input_tx,
            def,
            settings,
            payload,
            history_store,
        })
    }

    /// Build a one-shot delegate, run its turn between `SubagentStart` and
    /// `SubagentStop`, and mark its sidechain ended. `admission`'s slot is
    /// held until the run ends.
    ///
    /// The delegate extracts no memories: it is dropped as soon as its turn
    /// returns, which would abandon an extraction already paid for. What it
    /// learned reaches the parent as its answer.
    pub(crate) async fn run_delegate(
        &self,
        admission: Admission,
        shot: OneShot,
    ) -> (String, DelegateEnd) {
        let Admission { depth, _slot } = admission;
        let OneShot {
            spec,
            prompt,
            cancel,
            permission,
            task,
            extra_tools,
            background,
            records_spawn,
        } = shot;
        let tools = self.delegate_tools(spec.agent_type.as_deref(), &spec.tools);
        for t in extra_tools {
            tools.register(t);
        }
        let Delegate {
            mut agent,
            mut event_rx,
            input_tx,
            def,
            settings,
            payload,
            history_store,
        } = match self
            .build_delegate(&spec, depth, tools, permission, false)
            .await
        {
            Ok(delegate) => delegate,
            Err(e) => return (String::new(), DelegateEnd::Failed(e.to_string())),
        };
        let sid = payload.session_id.clone();
        let agent_type = spec.agent_type.as_deref();
        let prompt = match &def {
            Some(d) if !d.skills.is_empty() => {
                preload_skills_text(self.env.skills.as_ref(), &d.skills) + &prompt
            }
            _ => prompt,
        };
        let called_from = spec.called_from.as_ref();
        let tag = SubagentTag::new(self, &sid, agent_type, called_from).for_task(task.as_ref());

        // `Stop` fires on both success and failure — a hook that cleans up
        // after a sub-agent must not be skipped just because it errored.
        self.fire_subagent_hook(hooks::HookEvent::SubagentStart, &payload)
            .await;
        let spawned = records_spawn.then(|| {
            let model = &settings.model.model_name;
            SpawnTelemetry::start(self, called_from, &sid, agent_type, model, background)
        });
        let (text, end) = if background {
            open_inbox(task.as_deref(), &agent);
            let ran = run_one_turn(
                &mut agent,
                &mut event_rx,
                &input_tx,
                prompt,
                cancel,
                Some(tag),
                task.as_deref(),
            )
            .await;
            drop(input_tx);
            ran
        } else {
            run_one_turn_detached(
                &sid,
                agent,
                event_rx,
                input_tx,
                prompt,
                cancel,
                Some(tag),
                task,
            )
            .await
        };
        if let Some(spawned) = spawned {
            spawned.finish(matches!(end, DelegateEnd::Failed(_)));
        }
        let stopped = payload.stopped(stop_reason_of(&end));
        self.fire_subagent_hook(hooks::HookEvent::SubagentStop, &stopped)
            .await;
        // Marked terminal on failure too, so a stale `session.resume` against
        // it is rejected instead of silently continuing a "finished" sub-agent.
        mark_sidechain_terminal(history_store.as_ref(), &sid, &end).await;
        (text, end)
    }
}
