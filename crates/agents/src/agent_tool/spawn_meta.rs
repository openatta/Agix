//! Per-spawn bookkeeping: telemetry pair, hook payloads, the event tag and label,
//! and preloaded skill text.

use super::*;

/// The `agent_spawned` / `agent_completed` pair for one sub-agent run, in
/// the telemetry of the session that spawned it. The sub-agent's own events
/// go to the same handle under its own session id, which is `agent_id`.
pub(crate) struct SpawnTelemetry {
    pub(crate) handle: TelemetryHandle,
    pub(crate) parent_session_id: String,
    pub(crate) parent_turn: u32,
    pub(crate) agent_id: String,
    pub(crate) role: String,
    pub(crate) started: std::time::Instant,
}

impl SpawnTelemetry {
    pub(crate) fn start(
        delegator: &Delegator,
        called_from: Option<&CallSite>,
        agent_session_id: &str,
        agent_type: Option<&str>,
        model: &str,
        background: bool,
    ) -> Self {
        let handle = delegator.env.host.telemetry.clone();
        let parent_session_id = delegator.env.parent.id.clone();
        let parent_turn = called_from.map_or(0, |c| c.turn);
        let role = agent_type.unwrap_or("default").to_string();
        let _ = handle.record(telemetry::TelemetryEvent::agent_spawned(
            &parent_session_id,
            parent_turn,
            None,
            telemetry::AgentSpawnedPayload {
                agent_id: agent_session_id.to_string(),
                role: role.clone(),
                model: (!model.is_empty()).then(|| model.to_string()),
                stage_name: None,
                background,
            },
        ));
        Self {
            handle,
            parent_session_id,
            parent_turn,
            agent_id: agent_session_id.to_string(),
            role,
            started: std::time::Instant::now(),
        }
    }

    pub(crate) fn finish(self, had_error: bool) {
        let _ = self
            .handle
            .record(telemetry::TelemetryEvent::agent_completed(
                &self.parent_session_id,
                self.parent_turn,
                None,
                telemetry::AgentCompletedPayload {
                    agent_id: self.agent_id,
                    role: self.role,
                    had_error,
                    duration_ms: self.started.elapsed().as_millis() as u64,
                },
            ));
    }
}

/// What a sub-agent's `SubagentStart` / `SubagentStop` payload says about it.
#[derive(Clone)]
pub(crate) struct SubagentPayload {
    pub(crate) session_id: String,
    pub(crate) cwd: std::path::PathBuf,
    pub(crate) permission_mode: base::settings::PermissionMode,
    pub(crate) agent_type: Option<String>,
    pub(crate) stop_reason: Option<String>,
}

impl SubagentPayload {
    pub(crate) fn of(
        session_id: &str,
        cwd: &std::path::Path,
        settings: &Settings,
        agent_type: Option<&str>,
    ) -> Self {
        Self {
            session_id: session_id.to_string(),
            cwd: cwd.to_path_buf(),
            permission_mode: settings.permission_mode,
            agent_type: agent_type.map(str::to_string),
            stop_reason: None,
        }
    }

    pub(crate) fn stopped(mut self, reason: &str) -> Self {
        self.stop_reason = Some(reason.to_string());
        self
    }

    /// Fire one sub-agent lifecycle hook about this sub-agent. Notification
    /// only — the response is discarded, so a hook cannot veto a spawn or a
    /// completion.
    pub(crate) async fn fire(&self, runner: &hooks::HookRunner, event: hooks::HookEvent) {
        if !runner.has_hooks_for(event) {
            return;
        }
        let mut input = hooks::HookInput::lifecycle(
            format!("{event:?}"),
            self.session_id.clone(),
            self.cwd.display().to_string(),
            serde_json::to_value(self.permission_mode)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_else(|| "default".into()),
        )
        .with_agent_type(self.agent_type.clone());
        if let (hooks::HookEvent::SubagentStop, Some(reason)) = (event, &self.stop_reason) {
            input = input.with_stop_reason(reason.clone());
        }
        let _ = runner.run(event, &input).await;
    }
}

/// How a one-shot sub-agent's run ended, as `SubagentStop` reports it.
pub(crate) fn stop_reason_of(end: &DelegateEnd) -> &'static str {
    match end {
        DelegateEnd::Cancelled => "cancelled",
        DelegateEnd::Failed(_) => "error",
        DelegateEnd::Completed | DelegateEnd::CutShort(_) => "end_turn",
    }
}

/// Everything needed to re-emit one sub-agent's events on the parent's
/// channel. Built once per spawn; cloned into the collector task.
#[derive(Clone)]
pub(crate) struct SubagentTag {
    pub(crate) parent_tx: runtime::agent::EventSender,
    pub(crate) agent_label: String,
    pub(crate) agent_session_id: String,
    pub(crate) agent_type: Option<String>,
    pub(crate) parent_session_id: String,
    pub(crate) parent_turn: u32,
    pub(crate) parent_tool_use_id: Option<String>,
    pub(crate) task_id: Option<String>,
}

impl SubagentTag {
    pub(crate) fn new(
        delegator: &Delegator,
        agent_session_id: &str,
        agent_type: Option<&str>,
        called_from: Option<&CallSite>,
    ) -> Self {
        Self {
            parent_tx: delegator.env.parent.events.clone(),
            agent_label: subagent_label(agent_type, agent_session_id),
            agent_session_id: agent_session_id.to_string(),
            agent_type: agent_type.map(str::to_string),
            parent_session_id: delegator.env.parent.id.clone(),
            parent_turn: called_from.map_or(0, |c| c.turn),
            parent_tool_use_id: called_from
                .map(|c| c.tool_use_id.clone())
                .filter(|id| !id.is_empty()),
            task_id: None,
        }
    }

    pub(crate) fn for_task(mut self, task: Option<&Arc<base::context::RunningTask>>) -> Self {
        self.task_id = task.map(|t| t.task_id.clone());
        self
    }

    pub(crate) fn forward(&self, event: AgentEvent) {
        let _ = self.parent_tx.send(AgentEvent::SubagentProgress {
            agent_label: self.agent_label.clone(),
            agent_session_id: self.agent_session_id.clone(),
            agent_type: self.agent_type.clone(),
            parent_session_id: self.parent_session_id.clone(),
            parent_turn: self.parent_turn,
            parent_tool_use_id: self.parent_tool_use_id.clone(),
            task_id: self.task_id.clone(),
            event: Box::new(event),
        });
    }

    /// Bracket the delegation with `AgentSpawned`/`AgentCompleted` on the
    /// parent's channel.
    ///
    /// Sent unwrapped, not through `forward` — these describe the *parent's*
    /// timeline ("a delegation started/ended here"), whereas
    /// `SubagentProgress` wraps events that happened inside the child. An
    /// embedder tracking sub-agent lifecycle wants the pair; it should not
    /// have to infer boundaries from the `agent_label` on progress events.
    ///
    /// `agent_id` is the same `agent_label` every `SubagentProgress` for
    /// this run carries, so the three correlate without extra bookkeeping.
    pub(crate) fn spawned(&self) {
        let _ = self.parent_tx.send(AgentEvent::AgentSpawned {
            agent_id: self.agent_label.clone(),
            parent_turn: self.parent_turn,
            turn_id: self.agent_session_id.clone(),
        });
    }

    pub(crate) fn completed(&self, outcome: &str) {
        let _ = self.parent_tx.send(AgentEvent::AgentCompleted {
            agent_id: self.agent_label.clone(),
            outcome: outcome.to_string(),
            turn_id: self.agent_session_id.clone(),
        });
    }
}

/// Stable, human-readable label for one sub-agent run — identical on every
/// event that run emits, so a host can fold them under a single node.
pub(crate) fn subagent_label(agent_type: Option<&str>, session_id: &str) -> String {
    let short: String = session_id.chars().filter(|c| *c != '-').take(8).collect();
    format!("{}#{short}", agent_type.unwrap_or("agent"))
}

/// Render the full content of every named skill for preloading into a
/// spawned subagent's initial context (`AgentTypeDefinition.skills`).
///
/// A name that doesn't resolve, or resolves to a `disable_model_invocation:
/// true` skill (preloading draws from the same invocable set the Skill tool
/// itself can call), is skipped with a `tracing::warn!` rather than failing
/// the whole spawn.
/// Returns an empty string when nothing resolved, so callers can
/// unconditionally prepend the result without an extra branch.
pub(crate) fn preload_skills_text(
    mgr: &dyn base::interface::skill_catalog::SkillCatalog,
    skill_names: &[String],
) -> String {
    let mut sections = Vec::new();
    for name in skill_names {
        let Some(info) = mgr.get(name) else {
            tracing::warn!(skill = %name, "agent skills: preload — skill not found, skipping");
            continue;
        };
        if info.disable_model_invocation {
            tracing::warn!(
                skill = %name,
                "agent skills: preload — skill has disable_model_invocation set, cannot be preloaded, skipping"
            );
            continue;
        }
        let Some(content) = mgr.body(name) else {
            tracing::warn!(skill = %name, "agent skills: preload — skill has no readable content, skipping");
            continue;
        };
        sections.push(format!("## Preloaded skill: {name}\n\n{content}"));
    }
    if sections.is_empty() {
        String::new()
    } else {
        format!(
            "<system-reminder>\nThe following skills were preloaded for this task — apply them without needing to call the Skill tool.\n\n{}\n</system-reminder>\n\n",
            sections.join("\n\n---\n\n")
        )
    }
}
