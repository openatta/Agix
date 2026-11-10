//! Per-spawn bookkeeping: telemetry pair, hook payloads, the event tag and label,
//! and preloaded skill text.

use super::*;

/// The `agent_spawned` / `agent_completed` pair for one sub-agent run, in
/// the telemetry of the session that spawned it. The sub-agent's own events
/// go to the same handle under its own session id, which is `agent_id`.
pub(super) struct SpawnTelemetry {
    pub(super) handle: TelemetryHandle,
    pub(super) parent_session_id: String,
    pub(super) parent_turn: u32,
    pub(super) agent_id: String,
    pub(super) role: String,
    pub(super) started: std::time::Instant,
}

impl SpawnTelemetry {
    pub(super) fn start(
        inner: &Inner,
        parent: Option<(&str, u32, &str)>,
        agent_session_id: &str,
        agent_type: Option<&str>,
        model: &str,
        background: bool,
    ) -> Option<Self> {
        let handle = inner.telemetry_handle()?;
        let (parent_session_id, parent_turn) = match parent {
            Some((sid, turn, _)) => (sid.to_string(), turn),
            None => (inner.parent_session_id().unwrap_or_default(), 0),
        };
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
        Some(Self {
            handle,
            parent_session_id,
            parent_turn,
            agent_id: agent_session_id.to_string(),
            role,
            started: std::time::Instant::now(),
        })
    }

    pub(super) fn finish(self, had_error: bool) {
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
pub(super) struct SubagentPayload {
    pub(super) session_id: String,
    pub(super) cwd: std::path::PathBuf,
    pub(super) permission_mode: base::interface::settings::PermissionMode,
    pub(super) agent_type: Option<String>,
    pub(super) stop_reason: Option<String>,
}

impl SubagentPayload {
    pub(super) fn of(
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

    pub(super) fn stopped(mut self, reason: &str) -> Self {
        self.stop_reason = Some(reason.to_string());
        self
    }

    /// Fire one sub-agent lifecycle hook about this sub-agent. Notification
    /// only — the response is discarded, so a hook cannot veto a spawn or a
    /// completion.
    pub(super) async fn fire(&self, runner: &hooks::HookRunner, event: hooks::HookEvent) {
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
pub(super) fn stop_reason_of(
    result: &Result<String, base::error::ToolError>,
    cancelled: bool,
) -> &'static str {
    match (result, cancelled) {
        (_, true) => "cancelled",
        (Ok(_), _) => "end_turn",
        (Err(_), _) => "error",
    }
}

/// Everything needed to re-emit one sub-agent's events on the parent's
/// channel. Built once per spawn; cloned into the collector task.
#[derive(Clone)]
pub(super) struct SubagentTag {
    pub(super) parent_tx: runtime::agent::EventSender,
    pub(super) agent_label: String,
    pub(super) agent_session_id: String,
    pub(super) agent_type: Option<String>,
    pub(super) parent_session_id: String,
    pub(super) parent_turn: u32,
    pub(super) parent_tool_use_id: Option<String>,
    pub(super) task_id: Option<String>,
}

impl SubagentTag {
    /// Build a tag, or `None` when there's no parent channel to forward to
    /// (in which case the collector degrades to pure text accumulation).
    pub(super) fn new(
        inner: &Inner,
        agent_session_id: &str,
        agent_type: Option<&str>,
        parent: Option<(&str, u32, &str)>,
    ) -> Option<Self> {
        let parent_tx = inner.event_tx()?;
        let (parent_session_id, parent_turn, parent_tool_use_id) = parent.unwrap_or(("", 0, ""));
        Some(Self {
            parent_tx,
            agent_label: subagent_label(agent_type, agent_session_id),
            agent_session_id: agent_session_id.to_string(),
            agent_type: agent_type.map(str::to_string),
            parent_session_id: parent_session_id.to_string(),
            parent_turn,
            parent_tool_use_id: (!parent_tool_use_id.is_empty())
                .then(|| parent_tool_use_id.to_string()),
            task_id: None,
        })
    }

    pub(super) fn for_task(mut self, task: Option<&Arc<base::context::RunningTask>>) -> Self {
        self.task_id = task.map(|t| t.task_id.clone());
        self
    }

    pub(super) fn forward(&self, event: AgentEvent) {
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
    pub(super) fn spawned(&self) {
        let _ = self.parent_tx.send(AgentEvent::AgentSpawned {
            agent_id: self.agent_label.clone(),
            parent_turn: self.parent_turn,
            turn_id: self.agent_session_id.clone(),
        });
    }

    pub(super) fn completed(&self, outcome: &str) {
        let _ = self.parent_tx.send(AgentEvent::AgentCompleted {
            agent_id: self.agent_label.clone(),
            outcome: outcome.to_string(),
            turn_id: self.agent_session_id.clone(),
        });
    }
}

/// Stable, human-readable label for one sub-agent run — identical on every
/// event that run emits, so a host can fold them under a single node.
pub(super) fn subagent_label(agent_type: Option<&str>, session_id: &str) -> String {
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
pub(super) fn preload_skills_text(
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
