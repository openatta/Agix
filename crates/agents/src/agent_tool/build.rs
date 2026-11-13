//! The one place a delegated session is assembled — foreground and background
//! one-shot sub-agents and persistent team members all come through
//! `Inner::build_delegate` — and the one-shot run around it.

use super::*;

/// Where a delegate's permission comes from.
pub(super) enum Grant {
    /// The caller's, under the run's deny rules.
    Given(PermissionSet),
    /// A team member's, judged in the member's final mode.
    TeamMember,
}

/// What differs between spawn sites; everything else about a delegate is
/// decided in `build_delegate`.
pub(super) struct DelegatePlan<'a> {
    pub(super) tools: Arc<InMemoryToolRegistry>,
    pub(super) cwd: std::path::PathBuf,
    pub(super) depth: u32,
    pub(super) subagent_type: Option<&'a str>,
    pub(super) grant: Grant,
    pub(super) parent: Option<(&'a str, u32, &'a str)>,
    pub(super) run: &'a SubRun,
    pub(super) extracts_memories: bool,
}

pub(super) struct Delegate {
    pub(super) agent: Agent,
    pub(super) event_rx: EventReceiver,
    pub(super) input_tx: InputSender,
    pub(super) def: Option<AgentTypeDefinition>,
    pub(super) settings: Arc<LoadedSettings>,
    pub(super) payload: SubagentPayload,
    pub(super) history_store: Option<Arc<dyn HistoryStore>>,
}

/// How a one-shot run is driven.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum RunsOn {
    /// The caller awaits it, on a task of its own.
    Foreground,
    /// Already on its own task; its steering inbox is open.
    Background,
}

/// A one-shot sub-agent run: one prompt, one turn, then it ends.
pub(super) struct OneShot<'a> {
    pub(super) prompt: String,
    pub(super) tools: Arc<InMemoryToolRegistry>,
    pub(super) cwd: std::path::PathBuf,
    pub(super) cancel: tokio_util::sync::CancellationToken,
    pub(super) subagent_type: Option<&'a str>,
    pub(super) grant: Grant,
    pub(super) parent: Option<(&'a str, u32, &'a str)>,
    pub(super) run: SubRun,
    pub(super) runs_on: RunsOn,
}

impl Inner {
    pub(super) async fn build_delegate(
        &self,
        plan: DelegatePlan<'_>,
    ) -> Result<Delegate, base::error::ToolError> {
        let DelegatePlan {
            tools,
            cwd,
            depth,
            subagent_type,
            grant,
            parent,
            run,
            extracts_memories,
        } = plan;
        let def = subagent_type.and_then(|t| self.agent_type_def(t));
        let model_override = run
            .model
            .clone()
            .or_else(|| def.as_ref().and_then(|d| d.model.clone()));
        let mut sub_settings = self.sub_settings(model_override.as_deref());
        if let Some(d) = &def {
            apply_agent_type_overrides(&mut sub_settings.values, d);
        }
        if let Some(mode) = run.permission_mode {
            sub_settings.values.permission_mode = mode.min_permissive(sub_settings.permission_mode);
        }
        let scene = self.scene_for_subagent(def.as_ref());
        let scene_id = scene.id().to_string();
        let permission = match grant {
            Grant::Given(perm) => run.permission(perm),
            Grant::TeamMember => self.permission_handler(),
        };
        let settings = Arc::new(sub_settings);

        let sid = run
            .session_id
            .clone()
            .unwrap_or_else(|| base::id::Id::new().to_string());
        // Resolved once and shared by the sub-agent's own lineage and its `Meta`
        // line — two answers to "who spawned this" that must not be able to
        // disagree.
        let parent_session_id = Some(
            parent
                .map(|(sid, _, _)| sid.to_string())
                .unwrap_or_else(|| self.env.parent.id.clone()),
        );
        // First, so that what the delegate decides for itself below — its
        // permission, narrowed from the parent's — replaces what it inherits.
        let builder = Builder::new().inherit(self.env.host.clone());
        let mut builder = match self.env.hooks() {
            Some(runner) => builder.hooks(runner),
            None => builder,
        }
        .session_id(sid.clone())
        .lineage(parent_session_id.clone(), subagent_type)
        .scene(scene)
        .contributed_tool_filter(contributed_tool_filter(def.as_ref(), run))
        .model(self.model_for_subagent())
        .tools(tools)
        .settings(settings.clone())
        .agent_depth(depth)
        .memory_extraction(extracts_memories)
        .permission(permission);
        if let Some(schema) = &run.output_schema {
            builder = builder.output_schema(schema.clone());
        }
        if let Some(seat) = run.seat.clone() {
            builder = builder.team_seat(seat);
        }
        if let Some(servers) = self.mcp_granted_to(def.as_ref(), run) {
            builder = builder.mcp_servers(servers);
        }
        let history_store = self.env.host.history_store.clone();
        let (agent, event_rx, input_tx) = builder
            .build()
            .map_err(|e| base::error::ToolError::Execution(anyhow!("build: {e}")))?;
        if let Some(store) = &history_store {
            write_sidechain_meta(store, &sid, parent_session_id, &scene_id, &cwd, &settings).await;
        }
        let payload = SubagentPayload::of(&sid, &cwd, &settings, subagent_type);
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
    /// held until the run ends. Returns the answer and whether `cancel`
    /// stopped the run.
    ///
    /// The delegate extracts no memories: it is dropped as soon as its turn
    /// returns, which would abandon an extraction already paid for. What it
    /// learned reaches the parent as its answer.
    pub(super) async fn run_delegate(
        &self,
        admission: Admission,
        spawn: OneShot<'_>,
    ) -> (Result<String, base::error::ToolError>, bool) {
        let Admission { depth, _slot } = admission;
        let OneShot {
            prompt,
            tools,
            cwd,
            cancel,
            subagent_type,
            grant,
            parent,
            run,
            runs_on,
        } = spawn;
        let Delegate {
            mut agent,
            mut event_rx,
            input_tx,
            def,
            settings,
            payload,
            history_store,
        } = match self
            .build_delegate(DelegatePlan {
                tools,
                cwd,
                depth,
                subagent_type,
                grant,
                parent,
                run: &run,
                extracts_memories: false,
            })
            .await
        {
            Ok(delegate) => delegate,
            Err(e) => return (Err(e), false),
        };
        let sid = payload.session_id.clone();

        let prompt = match &def {
            Some(d) if !d.skills.is_empty() => {
                preload_skills_text(self.env.skills.as_ref(), &d.skills) + &prompt
            }
            _ => prompt,
        };
        let tag =
            Some(SubagentTag::new(self, &sid, subagent_type, parent).for_task(run.task.as_ref()));

        // `Stop` fires on both success and failure — a hook that cleans up
        // after a sub-agent must not be skipped just because it errored.
        self.fire_subagent_hook(hooks::HookEvent::SubagentStart, &payload)
            .await;
        if runs_on == RunsOn::Background {
            open_inbox(run.task.as_deref(), &agent);
        }
        let spawned = (!run.team_member).then(|| {
            SpawnTelemetry::start(
                self,
                parent,
                &sid,
                subagent_type,
                &settings.model.model_name,
                runs_on == RunsOn::Background,
            )
        });
        let (result, cancelled) = match runs_on {
            RunsOn::Foreground => {
                run_one_turn_detached(
                    &sid,
                    agent,
                    event_rx,
                    input_tx,
                    prompt,
                    cancel,
                    tag,
                    run.task.clone(),
                )
                .await
            }
            RunsOn::Background => {
                let outcome = run_one_turn(
                    &mut agent,
                    &mut event_rx,
                    &input_tx,
                    prompt,
                    cancel,
                    tag,
                    run.task.as_deref(),
                )
                .await;
                drop(input_tx);
                outcome
            }
        };
        if let Some(spawned) = spawned {
            spawned.finish(result.is_err());
        }
        self.fire_subagent_hook(
            hooks::HookEvent::SubagentStop,
            &payload.stopped(stop_reason_of(&result, cancelled)),
        )
        .await;
        // Marked terminal on failure too, so a stale `session.resume` against
        // it is rejected instead of silently continuing a "finished" sub-agent.
        mark_sidechain_terminal(history_store.as_ref(), &sid, &result, cancelled).await;
        (result, cancelled)
    }
}
