//! Team coordinator using `dyn Model` trait.
//!
//! Also provides [`PermissionBridge`] — intercepts sub-agent permission prompts
//! and forwards them to the parent agent via the team mailbox (Bubble mode).

use async_trait::async_trait;
use base::context::EngineConfig;
use base::error::ToolError;
use base::interface::agent_spawner::AgentSpawner;
use base::interface::model::{Model, ModelEvent, StreamParams};
use base::interface::permission::{Permission, PermissionOutcome};
use base::interface::prompt::{BlockRole, PromptBlock};
use base::interface::settings::ThinkingMode;
use base::tool::InMemoryToolRegistry;
use base::tool::ToolContext;
use base::tool::ToolResult;
use base::tool::ToolResultContent;
use futures::stream::StreamExt;
use std::collections::HashMap;
use std::sync::Arc;

use crate::tool::{AggregateMode, TeamStage};

// ═══════════════════════════════════════════════════════════
// Permission bridge — Bubble mode
// ═══════════════════════════════════════════════════════════

/// A permission prompt forwarded from a sub-agent to the parent agent.
#[derive(Debug, Clone)]
pub struct PermissionPrompt {
    /// The tool_use_id of the original tool call (used as correlation key).
    pub tool_use_id: String,
    /// Name of the tool requesting permission.
    pub tool_name: String,
    /// Human-readable explanation of the permission request.
    pub message: String,
    /// File paths affected by the tool call, if any.
    pub paths: Vec<std::path::PathBuf>,
    /// The turn_id from the sub-agent's context.
    pub turn_id: String,
}

/// Bridges permission requests from sub-agents to the parent agent
/// via the team mailbox system. Implements the `Permission` trait
/// so it can be injected as the permission handler for sub-agents.
///
/// When a tool call requires permission, the bridge:
/// 1. For read-only / safe tools — auto-permits
/// 2. For other tools — creates a [`PermissionPrompt`], forwards it
///    to the parent agent via the mailbox, then blocks until the parent
///    responds (or a 120-second timeout elapses).
pub struct PermissionBridge {
    /// Team mailbox for forwarding requests to the parent.
    mailbox: Arc<crate::mailbox::MailboxStore>,
    /// This agent's label in the team.
    my_label: String,
    /// The parent agent's label (receives permission requests).
    parent_label: String,
    /// Pending permissions keyed by tool_use_id → prompt.
    pending_permissions: std::sync::Arc<std::sync::Mutex<HashMap<String, PermissionPrompt>>>,
    /// Oneshot channels for awaiting parent decisions.
    /// Each sender is consumed by `send()` when the parent responds.
    response_channels:
        std::sync::Arc<std::sync::Mutex<HashMap<String, tokio::sync::oneshot::Sender<bool>>>>,
}

impl PermissionBridge {
    /// Create a new bridge that forwards permission requests from `my_label`
    /// to `parent_label` via the shared team mailbox.
    pub fn new(
        mailbox: Arc<crate::mailbox::MailboxStore>,
        my_label: impl Into<String>,
        parent_label: impl Into<String>,
    ) -> Self {
        Self {
            mailbox,
            my_label: my_label.into(),
            parent_label: parent_label.into(),
            pending_permissions: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
            response_channels: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    /// Returns a snapshot of currently pending permission prompts.
    pub fn pending(&self) -> Vec<PermissionPrompt> {
        let guard = self.pending_permissions.lock().unwrap();
        guard.values().cloned().collect()
    }

    /// Forward a permission prompt to the parent agent via the team mailbox.
    ///
    /// The prompt is stored in `pending_permissions` keyed by `tool_use_id`
    /// so that [`receive_decision`] can correlate the parent's response.
    pub fn forward_to_parent(&self, prompt: PermissionPrompt) {
        let tool_use_id = prompt.tool_use_id.clone();

        let message = serde_json::json!({
            "type": "permission_request",
            "tool_use_id": tool_use_id,
            "tool_name": prompt.tool_name,
            "message": prompt.message,
            "paths": prompt.paths,
            "turn_id": prompt.turn_id,
        });

        {
            let mut pending = self.pending_permissions.lock().unwrap();
            pending.insert(tool_use_id, prompt);
        }
        self.mailbox
            .send(&self.my_label, &self.parent_label, &message.to_string());
    }

    /// Receive a parent's decision for a pending permission request.
    ///
    /// The decision is applied by completing the oneshot channel that the
    /// sub-agent is blocked on. If `allowed` is true the tool call proceeds;
    /// otherwise the sub-agent receives a denial.
    pub fn receive_decision(&self, tool_use_id: &str, allowed: bool) {
        {
            let mut pending = self.pending_permissions.lock().unwrap();
            pending.remove(tool_use_id);
        }
        let sender = {
            let mut channels = self.response_channels.lock().unwrap();
            channels.remove(tool_use_id)
        };
        if let Some(tx) = sender {
            let _ = tx.send(allowed);
        }
    }
}

#[async_trait]
impl Permission for PermissionBridge {
    async fn check(
        &self,
        tool_name: &str,
        _tool_input: &serde_json::Value,
        _cwd: &std::path::Path,
        _session_id: &str,
    ) -> PermissionOutcome {
        // Auto-permit read-only / safe tools to avoid unnecessary parent
        // interaction for tools that cannot cause side effects.
        let safe_tools: &[&str] = &[
            "Read",
            "Grep",
            "Glob",
            "WebSearch",
            "WebFetch",
            "ListPeers",
            "ReadMail",
            "SendMessage",
            "Agent",
        ];
        if safe_tools.contains(&tool_name) {
            return PermissionOutcome::Permit;
        }

        // Generate a unique ID for this permission request.
        let prompt_id = format!(
            "{:x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );

        let (tx, rx) = tokio::sync::oneshot::channel::<bool>();

        // Store the oneshot sender so receive_decision can complete it.
        {
            let mut channels = self.response_channels.lock().unwrap();
            channels.insert(prompt_id.clone(), tx);
        }

        // Forward the permission prompt to the parent.
        let prompt = PermissionPrompt {
            tool_use_id: prompt_id.clone(),
            tool_name: tool_name.to_string(),
            message: format!(
                "Sub-agent '{}' requests permission to use tool '{}'",
                self.my_label, tool_name
            ),
            paths: vec![],
            turn_id: String::new(),
        };
        self.forward_to_parent(prompt);

        // Wait for the parent's decision (with timeout).
        match tokio::time::timeout(std::time::Duration::from_secs(120), rx).await {
            Ok(Ok(true)) => PermissionOutcome::Permit,
            Ok(Ok(false)) => PermissionOutcome::Deny {
                reason: "Parent denied permission".into(),
            },
            Ok(Err(_)) => PermissionOutcome::Deny {
                reason: "Permission response channel closed unexpectedly".into(),
            },
            Err(_) => PermissionOutcome::Deny {
                reason: "Permission request timed out after 120s".into(),
            },
        }
    }
}

/// Create [`PermissionBridge`] instances for every agent in a team's stages.
///
/// Each bridge maps the agent's label → agent so callers can look up the
/// bridge for a given label when spawning team members.
pub fn create_permission_bridges(
    mailbox: Arc<crate::mailbox::MailboxStore>,
    stages: &[TeamStage],
    team_name: &str,
) -> HashMap<String, Arc<PermissionBridge>> {
    let mut bridges = HashMap::new();
    for stage in stages {
        for agent in &stage.agents {
            if bridges.contains_key(&agent.label) {
                continue;
            }
            let bridge = Arc::new(PermissionBridge::new(
                mailbox.clone(),
                agent.label.clone(),
                team_name,
            ));
            bridges.insert(agent.label.clone(), bridge);
        }
    }
    bridges
}

// ═══════════════════════════════════════════════════════════
// Coordinator trait + DefaultCoordinator
// ═══════════════════════════════════════════════════════════

/// The parent session's event channel, when the composition root wired one
/// in — see [`Coordinator::set_event_sender`].
pub type TeamEventSender = tokio::sync::mpsc::UnboundedSender<base::interface::event::AgentEvent>;

#[async_trait]
pub trait Coordinator: Send + Sync {
    async fn orchestrate(&self, request: OrchestrateRequest) -> Result<ToolResult, ToolError>;

    /// Attach the parent session's event channel so the coordinator can emit
    /// `AgentEvent::TeamProgress` as stages start and finish (T-3), instead
    /// of the whole team run being one long silence followed by an enormous
    /// tool result. Default implementation is a no-op; coordinators that
    /// don't emit progress simply ignore it.
    ///
    /// Takes `&self` (interior mutability) because the tool holding the
    /// coordinator is registered before the session's event channel exists.
    fn set_event_sender(&self, _tx: TeamEventSender) {}

    /// Attach a telemetry handle so team lifecycle moments (member spawned/
    /// completed, stage completed, resume) are also recorded into the
    /// structured event stream, not just `events.jsonl`'s per-team audit
    /// log. Default implementation is a no-op.
    fn set_telemetry_handle(&self, _handle: telemetry::TelemetryHandle) {}

    /// Clean up a team's resources. Default implementation is a no-op;
    /// override for actual resource cleanup (task lists, scratchpad, etc.).
    async fn cleanup_team(&self, _name: &str) -> Result<(), String> {
        Ok(())
    }
    /// Resume a previously-interrupted coordinator workflow from its last
    /// checkpoint. Default returns an error — override for teams that
    /// persist checkpoint state (e.g. scratchpad files).
    async fn resume_coordinator(
        &self,
        task_id: &str,
        ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let _ = (task_id, ctx);
        Err(ToolError::Execution(anyhow::anyhow!(
            "resume not supported by this coordinator"
        )))
    }
}

pub struct OrchestrateRequest {
    pub model: Arc<dyn Model>,
    pub config: Arc<EngineConfig>,
    pub parent_tools: Arc<InMemoryToolRegistry>,
    pub sub_tools: Arc<InMemoryToolRegistry>,
    pub stages: Vec<TeamStage>,
    pub name: String,
    pub scratchpad: Option<String>,
    pub ctx: ToolContext,
    /// The permission grant every agent in this call runs under — "one
    /// team, one authorization" (see `TeamCreateInput::permission_mode`'s
    /// doc comment). `None` falls back to `PermissionMode::Plan` (read-only
    /// tools only) at the spawn site — a safe default that doesn't require
    /// a caller to think about permissions to get *something* better than
    /// unrestricted access, but is never silently more permissive than
    /// that without an explicit choice.
    pub permission_mode: Option<base::interface::settings::PermissionMode>,
}

/// Lifecycle states for team members.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeammateLifecycle {
    /// Agent registered but not yet started.
    Idle,
    /// Agent actively executing.
    Active,
    /// Agent completed execution (success or failure).
    Completed,
    /// Agent shut down (cleanup complete).
    Shutdown,
}

/// Default coordinator implementation.
///
/// Orchestrates sub-agents across stages using an optional [`AgentSpawner`].
/// The spawner is injected at construction time via [`DefaultCoordinator::with_agent_spawner`]
/// to break the circular dependency between the `team` and `runtime` crates:
/// `team` knows only the trait from `base`, while `runtime` (which depends on `team`)
/// provides the concrete implementation.
pub struct DefaultCoordinator {
    spawner: Option<Arc<dyn AgentSpawner>>,
    registry: Option<Arc<crate::registry::TeamRegistry>>,
    /// Parent session's event channel — see [`Coordinator::set_event_sender`].
    events: Arc<std::sync::RwLock<Option<TeamEventSender>>>,
    /// See [`Coordinator::set_telemetry_handle`].
    telemetry: Arc<std::sync::RwLock<Option<telemetry::TelemetryHandle>>>,
    /// The scene this coordinator's session runs under — `team` doesn't
    /// depend on the `scene` crate (same reason `tools`' sandbox policy
    /// takes an override rather than a dependency), so this is threaded in
    /// by whoever constructs the coordinator (`runtime::agent::Builder`,
    /// which does have it) rather than looked up here. Used to stamp
    /// `team.json`'s `scene` field; `"unknown"` if never set (e.g. tests
    /// that build a bare `DefaultCoordinator`).
    scene_id: Option<String>,
}

impl DefaultCoordinator {
    /// Create a coordinator with no spawner. Agents will not execute
    /// until a spawner is provided via [`with_agent_spawner`].
    pub fn new() -> Self {
        Self {
            spawner: None,
            registry: None,
            events: Arc::new(std::sync::RwLock::new(None)),
            telemetry: Arc::new(std::sync::RwLock::new(None)),
            scene_id: None,
        }
    }

    /// Create a coordinator with an [`AgentSpawner`] for executing sub-agents.
    ///
    /// The spawner wraps the runtime's `AgentTool` logic and is safe to pass
    /// across crate boundaries because both sides depend only on the trait in `base`.
    pub fn with_agent_spawner(spawner: Arc<dyn AgentSpawner>) -> Self {
        Self {
            spawner: Some(spawner),
            registry: None,
            events: Arc::new(std::sync::RwLock::new(None)),
            telemetry: Arc::new(std::sync::RwLock::new(None)),
            scene_id: None,
        }
    }

    /// See `scene_id`'s doc comment.
    pub fn with_scene_id(mut self, scene_id: String) -> Self {
        self.scene_id = Some(scene_id);
        self
    }

    /// Attach the shared team registry so `orchestrate()` records what it
    /// created and `cleanup_team()` can find it again — see
    /// `crate::registry`'s module doc for why this can't just live inside
    /// `DefaultCoordinator` itself (`TeamCreate`/`TeamList`/`TeamDelete` each
    /// wrap their own separate coordinator instance).
    pub fn with_registry(mut self, registry: Arc<crate::registry::TeamRegistry>) -> Self {
        self.registry = Some(registry);
        self
    }

    /// The wired-in telemetry handle, if [`Coordinator::set_telemetry_handle`]
    /// was ever called — `None` returns a noop handle's worth of nothing
    /// rather than every call site checking `Option` separately.
    fn telemetry(&self) -> Option<telemetry::TelemetryHandle> {
        self.telemetry.read().unwrap().clone()
    }

    /// Emit one `TeamProgress` event, if a channel was wired in.
    #[allow(clippy::too_many_arguments)]
    fn emit_progress(
        &self,
        team: &str,
        team_id: &str,
        stage: &str,
        stage_index: usize,
        stage_count: usize,
        status: base::interface::event::TeamStageStatus,
        members: Vec<String>,
        failed: Vec<String>,
    ) {
        let Some(tx) = self.events.read().unwrap().clone() else {
            return;
        };
        let _ = tx.send(base::interface::event::AgentEvent::TeamProgress {
            team: team.to_string(),
            team_id: team_id.to_string(),
            stage: stage.to_string(),
            stage_index,
            stage_count,
            status,
            members,
            failed,
        });
    }
}

impl Default for DefaultCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Coordinator for DefaultCoordinator {
    fn set_event_sender(&self, tx: TeamEventSender) {
        *self.events.write().unwrap() = Some(tx);
    }

    fn set_telemetry_handle(&self, handle: telemetry::TelemetryHandle) {
        *self.telemetry.write().unwrap() = Some(handle);
    }

    async fn orchestrate(&self, req: OrchestrateRequest) -> Result<ToolResult, ToolError> {
        let OrchestrateRequest {
            model,
            config,
            parent_tools: _,
            sub_tools: _,
            stages,
            name,
            scratchpad,
            ctx,
            permission_mode,
        } = req;
        let permission_mode =
            permission_mode.unwrap_or(base::interface::settings::PermissionMode::Plan);

        let all_labels: Vec<String> = stages
            .iter()
            .flat_map(|s| s.agents.iter().map(|a| a.label.clone()))
            .collect();
        let team_id = format!("team-{}-{}", name, chrono_id());
        let team_dir = ctx.session.cwd.join(".agix/teams").join(&team_id);
        let sp_path = team_dir.join("SCRATCHPAD.md");
        if let Some(p) = sp_path.parent() {
            let _ = tokio::fs::create_dir_all(p).await;
        }

        // §6.2/§6.3: team.json (write-once declaration) + events.jsonl
        // (team_created) supersede the old config.json — same discoverable
        // metadata, in the shape a future project-level `TeamStore` reads.
        let declaration = crate::persist::TeamDeclaration {
            schema_version: crate::persist::CURRENT_TEAM_SCHEMA_VERSION,
            team_id: team_id.clone(),
            name: name.clone(),
            scene: self
                .scene_id
                .clone()
                .unwrap_or_else(|| "unknown".to_string()),
            project_root: ctx.session.cwd.display().to_string(),
            owner_session_id: ctx.session_id.clone(),
            created_at: crate::persist::iso_now(),
            mode: crate::persist::TeamMode::Batch,
            permission_mode: serde_json::to_value(permission_mode)
                .ok()
                .and_then(|v| v.as_str().map(|s| s.to_string())),
            stages: stages
                .iter()
                .enumerate()
                .map(|(index, s)| crate::persist::StageDeclaration {
                    index,
                    name: s.name.clone(),
                    aggregate: s.aggregate.and_then(|m| {
                        serde_json::to_value(m)
                            .ok()
                            .and_then(|v| v.as_str().map(|s| s.to_string()))
                    }),
                    members: s
                        .agents
                        .iter()
                        .map(|a| crate::persist::MemberDeclaration {
                            label: a.label.clone(),
                            agent_type: a.agent_type.clone(),
                            prompt: a.prompt.clone(),
                        })
                        .collect(),
                })
                .collect(),
        };
        // `team.json` is the foundation every later read of this team
        // (index rebuild, `TeamList`, a future `TeamStore`) assumes exists —
        // unlike `state.json`/`events.jsonl`, there's no snapshot to fall
        // back to if it never landed, so a failure here must fail team
        // creation outright rather than continue orchestrating into a team
        // directory that doesn't even have its declaration.
        if let Err(e) = crate::persist::write_team_declaration(&team_dir, &declaration).await {
            tracing::error!(team_id = %team_id, error = %e, "failed to write team.json — aborting team creation");
            return Err(ToolError::Execution(anyhow::anyhow!(
                "failed to create team '{name}': could not write team.json: {e}"
            )));
        }
        if let Err(e) = crate::persist::append_team_event(
            &team_dir,
            crate::persist::TeamEvent::TeamCreated {
                team_id: team_id.clone(),
            },
            crate::persist::EventsRetention::default(),
        )
        .await
        {
            tracing::warn!(team_id = %team_id, event = "team_created", error = %e, "failed to append team event");
        }

        let mailbox = Arc::new(crate::mailbox::MailboxStore::with_persistence(
            all_labels,
            team_dir.join("mailbox"),
        ));

        // Create permission bridges for each agent in the team.
        // These bridges are used when spawning team members to bubble
        // permission decisions up to the coordinator.
        let _permission_bridges = create_permission_bridges(mailbox.clone(), &stages, &name);

        // Inject coordinator system prompt.
        let stage_names: Vec<String> = stages.iter().map(|s| s.name.clone()).collect();
        let coordinator_prompt = crate::prompt::build_coordinator_prompt(&name, &stage_names);
        let mut scratch = format!(
            "# Team `{name}`\n\nTeam id: `{team_id}`\nStages: {}\n\n{coordinator_prompt}\n",
            stages.len()
        );
        if let Some(s) = scratchpad {
            scratch.push_str("\n## 0_initial_context\n\n");
            scratch.push_str(&s);
            scratch.push('\n');
        }
        let _ = tokio::fs::write(&sp_path, &scratch).await;

        // P1-8: Teammate lifecycle tracking.
        let mut lifecycles: std::collections::HashMap<String, TeammateLifecycle> = stages
            .iter()
            .flat_map(|s| s.agents.iter())
            .map(|a| (a.label.clone(), TeammateLifecycle::Idle))
            .collect();

        let stage_count = stages.len();
        let all_stage_names: Vec<String> = stages.iter().map(|s| s.name.clone()).collect();
        let sp_display = sp_path.display().to_string();

        let mut any_err = false;
        for (si, stage) in stages.iter().enumerate() {
            let stage_start = std::time::Instant::now();
            let mut sections: Vec<(String, String, bool)> = Vec::new();
            let stage_members: Vec<String> = stage.agents.iter().map(|a| a.label.clone()).collect();

            // T-3: tell the host the stage is starting, so a large team
            // streams progress instead of going silent until the very end.
            self.emit_progress(
                &name,
                &team_id,
                &stage.name,
                si,
                stage_count,
                base::interface::event::TeamStageStatus::Started,
                stage_members.clone(),
                Vec::new(),
            );
            if let Err(e) = crate::persist::append_team_event(
                &team_dir,
                crate::persist::TeamEvent::StageStarted {
                    index: si,
                    name: stage.name.clone(),
                    members: stage_members.clone(),
                },
                crate::persist::EventsRetention::default(),
            )
            .await
            {
                tracing::warn!(team_id = %team_id, event = "stage_started", stage = %stage.name, error = %e, "failed to append team event");
            }

            // Spawn agents in this stage using the AgentSpawner (if provided)
            if let Some(ref spawner) = self.spawner {
                // Transition every agent in this stage to Active up front —
                // the concurrent futures below don't need to mutate `lifecycles`.
                for agent_spec in &stage.agents {
                    lifecycles.insert(agent_spec.label.clone(), TeammateLifecycle::Active);
                    if let Err(e) = crate::persist::append_team_event(
                        &team_dir,
                        crate::persist::TeamEvent::MemberSpawned {
                            label: agent_spec.label.clone(),
                            agent_id: None,
                            session_id: None,
                        },
                        crate::persist::EventsRetention::default(),
                    )
                    .await
                    {
                        tracing::warn!(team_id = %team_id, event = "member_spawned", label = %agent_spec.label, error = %e, "failed to append team event");
                    }
                    if let Some(handle) = self.telemetry() {
                        let _ = handle.record(telemetry::TelemetryEvent::agent_spawned(
                            &team_id,
                            0,
                            None,
                            telemetry::AgentSpawnedPayload {
                                agent_id: agent_spec.label.clone(),
                                role: agent_spec
                                    .agent_type
                                    .clone()
                                    .unwrap_or_else(|| "default".to_string()),
                                model: String::new(),
                                parent_agent_id: None,
                                stage_name: stage.name.clone(),
                            },
                        ));
                    }
                }

                let stage_start_snapshot =
                    build_state_snapshot(si, crate::persist::TeamStatus::Running, &lifecycles);
                if let Err(e) =
                    crate::persist::write_team_state_atomic(&team_dir, &stage_start_snapshot).await
                {
                    tracing::warn!(team_id = %team_id, stage = %stage.name, error = %e, "failed to write state.json snapshot (stage start)");
                }

                // `.max(1)`: a misconfigured `0` would make every `acquire_owned()`
                // below block forever (a zero-permit semaphore never grants).
                let permits = Arc::new(tokio::sync::Semaphore::new(
                    config.team_stage_concurrency.max(1),
                ));
                let later_stages: Vec<String> =
                    all_stage_names[(si + 1).min(stage_count)..].to_vec();
                let futs = stage.agents.iter().map(|agent_spec| {
                    let permits = permits.clone();
                    let spawner = spawner.clone();
                    let label = agent_spec.label.clone();
                    // T-1: workers used to get `agent_spec.prompt` verbatim
                    // with no idea they were in a team at all. Prepend a
                    // short orientation block — team, stage, role, peers,
                    // scratchpad — rather than leaning on the coordinator
                    // prompt to nag the lead into repeating it every time.
                    let peers: Vec<String> = stage_members
                        .iter()
                        .filter(|l| *l != &label)
                        .cloned()
                        .collect();
                    let prompt = crate::prompt::build_worker_prompt(
                        &crate::prompt::WorkerContext {
                            team: &name,
                            team_id: &team_id,
                            label: &label,
                            stage: &stage.name,
                            stage_index: si,
                            stage_count,
                            peers: &peers,
                            later_stages: &later_stages,
                            scratchpad: &sp_display,
                        },
                        &agent_spec.prompt,
                    );
                    // A member's `agent_type` was parsed off the tool input
                    // and then thrown away here; pass it through so the
                    // runtime's agent catalog can actually apply it.
                    let agent_type = agent_spec.agent_type.clone();
                    let cwd = ctx.cwd.clone();
                    let cancel = ctx.cancel.child_token();
                    // Mailbox tools scoped to this member's own label — lets
                    // it coordinate with whichever siblings are concurrently
                    // running in the same stage (see `AgentSpawner::
                    // spawn_agent_with_tools`'s doc comment for why the lead
                    // itself can't participate here, only siblings can).
                    let extra_tools: Vec<Arc<dyn base::tool::Tool>> = vec![
                        Arc::new(crate::mailbox::SendMessageTool::new(
                            mailbox.clone(),
                            label.clone(),
                        )),
                        Arc::new(crate::mailbox::ReadMailTool::new(
                            mailbox.clone(),
                            label.clone(),
                        )),
                        Arc::new(crate::mailbox::ListPeersTool::new(mailbox.clone())),
                    ];
                    let role = agent_type.clone().unwrap_or_else(|| "default".to_string());
                    async move {
                        let member_start = std::time::Instant::now();
                        let _permit = permits.acquire_owned().await.expect("semaphore closed");
                        let result = spawner
                            .spawn_agent_with_tools(
                                prompt,
                                vec![], // allowed_tools: empty = all tools
                                extra_tools,
                                cwd,
                                cancel,
                                // A member's `agent_type` used to be parsed off
                                // the tool input and then dropped here, so a
                                // team member never got the type it asked for.
                                agent_type,
                                Some(permission_mode),
                            )
                            .await;
                        drop(_permit);
                        let duration_ms = member_start.elapsed().as_millis() as u64;
                        match result {
                            Ok(text) => (label, text, false, duration_ms, role),
                            Err(e) => (label, format!("ERROR: {e}"), true, duration_ms, role),
                        }
                    }
                });
                let results = futures::future::join_all(futs).await;

                // Bulk-mark every agent in this stage Completed after the join.
                for agent_spec in &stage.agents {
                    lifecycles.insert(agent_spec.label.clone(), TeammateLifecycle::Completed);
                }

                for (label, text, is_error, duration_ms, role) in results {
                    if is_error {
                        any_err = true;
                    }
                    if let Err(e) = crate::persist::append_team_event(
                        &team_dir,
                        crate::persist::TeamEvent::MemberCompleted {
                            label: label.clone(),
                            is_error,
                        },
                        crate::persist::EventsRetention::default(),
                    )
                    .await
                    {
                        tracing::warn!(team_id = %team_id, event = "member_completed", label = %label, error = %e, "failed to append team event");
                    }
                    if let Some(handle) = self.telemetry() {
                        let _ = handle.record(telemetry::TelemetryEvent::agent_completed(
                            &team_id,
                            0,
                            None,
                            telemetry::AgentCompletedPayload {
                                agent_id: label.clone(),
                                role,
                                turn_count: 0,
                                tool_call_count: 0,
                                had_error: is_error,
                                duration_ms,
                            },
                        ));
                    }
                    sections.push((label, text, is_error));
                }
            } else {
                // No spawner available — log a warning that agents cannot be spawned.
                // This happens when TeamCreate is used standalone without a daemon wiring.
                tracing::warn!(
                    "TeamCreate: no AgentSpawner provided — stage '{}' agents will not execute. \
                     Wire RuntimeAgentSpawner at the composition root to enable team coordination.",
                    stage.name
                );
                for agent_spec in &stage.agents {
                    sections.push((
                        agent_spec.label.clone(),
                        "[AgentSpawner not available — team coordination requires daemon wiring]"
                            .to_string(),
                        true,
                    ));
                    any_err = true;
                }
            }

            if let Some(mode) = stage.aggregate {
                if !sections.is_empty() {
                    sections = aggregate(
                        &*model,
                        &config.model,
                        mode,
                        &stage.name,
                        &sections,
                        Some(ctx.session_id.as_str()),
                    )
                    .await;
                }
            }

            let mut md = format!("\n## {}_{}\n\n", si + 1, stage.name);
            for (l, b, e) in &sections {
                md.push_str(&format!(
                    "### {}{}\n{b}\n\n",
                    l,
                    if *e { " (ERROR)" } else { "" }
                ));
            }
            scratch.push_str(&md);
            let _ = tokio::fs::write(&sp_path, &scratch).await;

            // T-3: stage finished — report which members failed so the host
            // can surface a problem before the whole team run is over.
            let failed: Vec<String> = sections
                .iter()
                .filter(|(_, _, e)| *e)
                .map(|(l, _, _)| l.clone())
                .collect();
            self.emit_progress(
                &name,
                &team_id,
                &stage.name,
                si,
                stage_count,
                base::interface::event::TeamStageStatus::Completed,
                stage_members,
                failed.clone(),
            );
            if let Some(handle) = self.telemetry() {
                let _ = handle.record(telemetry::TelemetryEvent::team_stage_complete(
                    &team_id,
                    0,
                    None,
                    telemetry::TeamStageCompletePayload {
                        stage_name: stage.name.clone(),
                        agent_count: stage.agents.len(),
                        duration_ms: stage_start.elapsed().as_millis() as u64,
                        success: failed.is_empty(),
                        agents_with_errors: failed.len(),
                    },
                ));
            }
            if let Err(e) = crate::persist::append_team_event(
                &team_dir,
                crate::persist::TeamEvent::StageCompleted { index: si, failed },
                crate::persist::EventsRetention::default(),
            )
            .await
            {
                tracing::warn!(team_id = %team_id, event = "stage_completed", stage = %stage.name, error = %e, "failed to append team event");
            }

            // A snapshot per stage completion — so a mid-run crash leaves a
            // `state.json` no more than one stage stale, not zero snapshots
            // for the whole run's duration.
            let stage_end_snapshot =
                build_state_snapshot(si, crate::persist::TeamStatus::Running, &lifecycles);
            if let Err(e) =
                crate::persist::write_team_state_atomic(&team_dir, &stage_end_snapshot).await
            {
                tracing::warn!(team_id = %team_id, stage = %stage.name, error = %e, "failed to write state.json snapshot (stage completion)");
            }
        }

        let final_status = if any_err {
            crate::persist::TeamStatus::Failed
        } else {
            crate::persist::TeamStatus::Completed
        };
        let state = build_state_snapshot(stage_count.saturating_sub(1), final_status, &lifecycles);
        if let Err(e) = crate::persist::write_team_state_atomic(&team_dir, &state).await {
            tracing::warn!(team_id = %team_id, error = %e, "failed to write final state.json");
        }
        if let Err(e) = crate::persist::append_team_event(
            &team_dir,
            crate::persist::TeamEvent::TeamCompleted {
                status: final_status,
            },
            crate::persist::EventsRetention::default(),
        )
        .await
        {
            tracing::warn!(team_id = %team_id, event = "team_completed", error = %e, "failed to append team event");
        }

        if let Some(registry) = &self.registry {
            registry.upsert(crate::registry::TeamInfo {
                team_id: team_id.clone(),
                name: name.clone(),
                created_at_secs: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
                team_dir: team_dir.clone(),
                members: lifecycles
                    .iter()
                    .map(|(label, lifecycle)| crate::registry::TeamMemberInfo {
                        label: label.clone(),
                        lifecycle: *lifecycle,
                        idle_since_secs: None,
                    })
                    .collect(),
                // Not stored for batch mode — this call's `permission_mode`
                // only applies to this one call, not persisted for reuse.
                // See `TeamInfo::permission_mode`'s doc comment.
                permission_mode: None,
            });
        }

        Ok(ToolResult {
            content: ToolResultContent::Text(format!(
                "{}\n\n_(scratchpad: {})_",
                scratch,
                sp_path.display()
            )),
            is_error: any_err,
            structured_content: None,
            mcp_meta: None,
            new_messages: None,
        })
    }

    /// Real cleanup: removes the team from the shared registry and deletes
    /// its `.agix/teams/{team_id}/` directory. Returns an error (rather than
    /// silently succeeding) when `name` isn't a team this registry knows
    /// about — surfacing that to the model instead of letting it believe a
    /// nonexistent/mistyped team was cleaned up. `orchestrate()`'s batch
    /// members are always already `Completed` by the time this runs (it's a
    /// synchronous call, nothing is left `Active` to cancel by the time the
    /// model can even call `TeamDelete`) — but persistent members (`Agent`
    /// tool's `team_name`+`name` mode) genuinely can still be alive, so this
    /// calls `AgentSpawner::stop_team_member` for every member on record;
    /// it's a no-op for the ones that were never persistent in the first
    /// place (nothing to find, matches `stop_team_member`'s own tolerance
    /// for unknown names).
    ///
    /// No registry wired (a bare `DefaultCoordinator::new()` without
    /// `.with_registry(...)`, e.g. in tests that don't care about this) is
    /// treated as a no-op success, not an error — that's a wiring choice by
    /// the caller, not something the model did wrong.
    async fn cleanup_team(&self, name: &str) -> Result<(), String> {
        let Some(registry) = &self.registry else {
            return Ok(());
        };
        let Some(info) = registry.remove(name) else {
            return Err(format!(
                "no such team: '{name}' — nothing was created under this name \
                 (or it was already deleted). Use TeamList to see active teams."
            ));
        };
        if let Some(spawner) = &self.spawner {
            for member in &info.members {
                spawner.stop_team_member(name, &member.label).await;
            }
        }
        if !info.team_dir.as_os_str().is_empty() {
            if let Err(e) = tokio::fs::remove_dir_all(&info.team_dir).await {
                if e.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(
                        team = %name,
                        path = %info.team_dir.display(),
                        error = %e,
                        "team cleanup: failed to remove team directory"
                    );
                }
            }
        }
        Ok(())
    }

    /// Resume a DefaultCoordinator workflow from its scratchpad checkpoint.
    ///
    /// Reads the team's SCRATCHPAD.md from the `.agix/teams/<task_id>/`
    /// directory, identifies the last completed stage, and returns a prompt
    /// that can be fed to a sub-agent to continue coordination.
    async fn resume_coordinator(
        &self,
        task_id: &str,
        ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let start = std::time::Instant::now();

        let team_dir = ctx.session.cwd.join(".agix/teams").join(task_id);
        let sp_path = team_dir.join("SCRATCHPAD.md");

        let scratchpad = match tokio::fs::read_to_string(&sp_path).await {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(ToolError::Execution(anyhow::anyhow!(
                    "team scratchpad not found for task_id: {task_id}"
                )));
            }
            Err(e) => {
                return Err(ToolError::Execution(anyhow::anyhow!(
                    "read scratchpad: {e}"
                )));
            }
        };

        // Identify the last completed stage heading
        let last_stage: &str = scratchpad
            .lines()
            .rfind(|l| l.starts_with("## "))
            .map(|l| l.trim_start_matches("## ").trim())
            .unwrap_or("(none)");

        let resume_prompt = format!(
            "Resuming team coordinator for task `{task_id}`.\n\
             Last checkpoint: {last_stage}\n\n\
             Scratchpad:\n{scratchpad}\n\n\
             Continue coordinating the remaining stages."
        );

        // This used to be `tracing::info!(target: "telemetry", ...)` — no
        // subscriber anywhere in the workspace listens on that target, so it
        // never actually reached the telemetry pipeline. Now that
        // `set_telemetry_handle` gives this coordinator a real handle, record
        // it properly.
        let latency_ms = start.elapsed().as_millis() as u64;
        if let Some(handle) = self.telemetry() {
            let _ = handle.record(telemetry::TelemetryEvent::resume_action(
                task_id,
                0,
                None,
                telemetry::ResumeActionPayload {
                    outcome: telemetry::ResumeOutcome::Succeeded,
                    source: "team_scratchpad".into(),
                    entry_count: 0,
                    projected_message_count: 0,
                    compact_boundary_count: 0,
                    sidechain_entry_count: 0,
                    warning_kind: None,
                    latency_ms,
                },
            ));
        }

        Ok(ToolResult {
            content: ToolResultContent::Text(resume_prompt),
            is_error: false,
            structured_content: None,
            mcp_meta: None,
            new_messages: None,
        })
    }
}

// ═══════════════════════════════════════════════════════════
// Aggregation helpers
// ═══════════════════════════════════════════════════════════

pub async fn aggregate(
    model: &dyn Model,
    model_name: &str,
    mode: AggregateMode,
    stage_name: &str,
    sections: &[(String, String, bool)],
    session_id: Option<&str>,
) -> Vec<(String, String, bool)> {
    match mode {
        AggregateMode::Concat => sections.to_vec(),
        AggregateMode::Best => {
            if let Some(label) =
                pick_best(model, model_name, stage_name, sections, session_id).await
            {
                sections
                    .iter()
                    .filter(|(l, _, _)| *l == label)
                    .cloned()
                    .collect()
            } else {
                sections.to_vec()
            }
        }
        AggregateMode::Aggregate => {
            let text = merge(model, model_name, stage_name, sections, session_id)
                .await
                .unwrap_or_else(|| {
                    let mut a = String::new();
                    for (l, b, _) in sections {
                        a.push_str(&format!("### {l}\n{b}\n\n"));
                    }
                    a
                });
            vec![("(aggregated)".into(), text, false)]
        }
    }
}

pub async fn aggregate_stage_results(
    model: &dyn Model,
    model_name: &str,
    mode: AggregateMode,
    stage_name: &str,
    sections: &[(String, String, bool)],
    session_id: Option<&str>,
) -> Vec<(String, String, bool)> {
    aggregate(model, model_name, mode, stage_name, sections, session_id).await
}

async fn pick_best(
    model: &dyn Model,
    model_name: &str,
    stage_name: &str,
    sections: &[(String, String, bool)],
    session_id: Option<&str>,
) -> Option<String> {
    let formatted: Vec<String> = sections
        .iter()
        .map(|(l, b, e)| {
            let s = if *e { " (ERROR)" } else { "" };
            format!("<agent label=\"{l}{s}\">\n{b}\n</agent>")
        })
        .collect();
    let prompt = format!(
        "You are evaluating results of a team of AI agents on stage \"{stage_name}\".\n\n\
         Results:\n{}\n\n\
         Pick the single best result. Return ONLY the agent label, nothing else.",
        formatted.join("\n\n"),
    );
    let label = drain(model, model_name, prompt, 100, session_id).await?;
    let label = label.trim().to_string();
    if sections.iter().any(|(l, _, _)| l == &label) {
        Some(label)
    } else {
        None
    }
}

async fn merge(
    model: &dyn Model,
    model_name: &str,
    stage_name: &str,
    sections: &[(String, String, bool)],
    session_id: Option<&str>,
) -> Option<String> {
    let formatted: Vec<String> = sections
        .iter()
        .map(|(l, b, e)| {
            let s = if *e { " (ERROR)" } else { "" };
            format!("<agent label=\"{l}{s}\">\n{b}\n</agent>")
        })
        .collect();
    let prompt = format!(
        "Synthesize results of AI agents on stage \"{stage_name}\".\n\nResults:\n{}\n\n\
         Combine into one document. Capture best insights, remove redundancy, preserve facts.",
        formatted.join("\n\n"),
    );
    drain(model, model_name, prompt, 4096, session_id).await
}

async fn drain(
    model: &dyn Model,
    model_name: &str,
    prompt: String,
    max_tokens: u32,
    session_id: Option<&str>,
) -> Option<String> {
    let blocks = vec![PromptBlock {
        role: BlockRole::System,
        content: "You are a strict judge. Output only the requested text, nothing else.".into(),
        cache_strategy: None,
        name: Some("team.judge".into()),
        origin: base::prompt::BlockOrigin::Kernel,
    }];
    let messages = vec![base::interface::model::ModelMessage {
        role: base::interface::model::MessageRole::User,
        content: vec![base::interface::model::ModelContentBlock::Text { text: prompt }],
    }];
    let params = StreamParams {
        model: model_name.to_string(),
        max_tokens,
        thinking_mode: ThinkingMode::Off,
        fallback_model: None,
        cache_edits: vec![],
        origin: session_id.map(|id| {
            base::interface::model::CallOrigin::auxiliary(
                id,
                base::interface::model::call_purpose::TEAM_JUDGE,
            )
        }),
        input_map: None,
    };
    let cancel = tokio_util::sync::CancellationToken::new();
    let mut stream = model
        .stream(blocks, vec![], messages, params, cancel)
        .await
        .ok()?;
    let mut text = String::new();
    while let Some(ev) = stream.next().await {
        match ev.ok()? {
            ModelEvent::TextDelta { text: t } => text.push_str(&t),
            ModelEvent::EndTurn { .. } => break,
            _ => {}
        }
    }
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

/// Build a `state.json` snapshot from the coordinator's in-memory lifecycle
/// map — shared by the per-stage checkpoint writes and the final write so
/// they can't drift apart in shape.
fn build_state_snapshot(
    current_stage: usize,
    status: crate::persist::TeamStatus,
    lifecycles: &HashMap<String, TeammateLifecycle>,
) -> crate::persist::TeamStateFile {
    crate::persist::TeamStateFile {
        schema_version: crate::persist::CURRENT_STATE_SCHEMA_VERSION,
        updated_at: crate::persist::iso_now(),
        current_stage,
        status,
        members: lifecycles
            .iter()
            .map(|(label, lifecycle)| crate::persist::MemberState {
                label: label.clone(),
                lifecycle: *lifecycle,
                agent_id: None,
                session_id: None,
                idle_since: None,
                last_error: None,
            })
            .collect(),
    }
}

fn chrono_id() -> String {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{n:x}")
}

// ═══════════════════════════════════════════════════════════
// Tests — stage concurrency (bounded parallel dispatch)
// ═══════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::TeamAgentSpec;
    use base::interface::model::{ModelError, ModelStream};
    use base::provider::ApiType;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// A `Model` stub that is never actually invoked by these tests (all
    /// test stages use `aggregate: None`, so `aggregate()` never calls it).
    struct UnusedModel;

    #[async_trait]
    impl Model for UnusedModel {
        fn api_type(&self) -> ApiType {
            ApiType::Anthropic
        }
        async fn stream(
            &self,
            _prompt_blocks: Vec<PromptBlock>,
            _tools: Vec<base::interface::model::ToolDef>,
            _messages: Vec<base::interface::model::ModelMessage>,
            _params: StreamParams,
            _cancel: tokio_util::sync::CancellationToken,
        ) -> Result<ModelStream, ModelError> {
            panic!("UnusedModel::stream should not be called by these tests");
        }
    }

    /// A fake `AgentSpawner` whose `spawn_agent` sleeps for a duration
    /// encoded in the agent's task text (as a plain millisecond count),
    /// tracking in-flight concurrency via an `AtomicUsize`. Records every
    /// prompt it was handed so tests can inspect the injected team context.
    struct FakeSpawner {
        in_flight: AtomicUsize,
        peak_in_flight: AtomicUsize,
        prompts: std::sync::Mutex<Vec<String>>,
        agent_types: std::sync::Mutex<Vec<Option<String>>>,
    }

    impl FakeSpawner {
        fn new() -> Self {
            Self {
                in_flight: AtomicUsize::new(0),
                peak_in_flight: AtomicUsize::new(0),
                prompts: std::sync::Mutex::new(Vec::new()),
                agent_types: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    /// The coordinator now prepends a `<team-context>` block, so the task
    /// text a test encoded is the last line of what the spawner receives.
    fn task_text(prompt: &str) -> String {
        prompt
            .trim_end()
            .rsplit('\n')
            .next()
            .unwrap_or("")
            .trim()
            .to_string()
    }

    #[async_trait]
    impl AgentSpawner for FakeSpawner {
        async fn spawn_agent(
            &self,
            prompt: String,
            _allowed_tools: Vec<String>,
            _cwd: std::path::PathBuf,
            _cancel: tokio_util::sync::CancellationToken,
            agent_type: Option<String>,
        ) -> Result<String, Box<dyn std::error::Error + Send>> {
            let cur = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak_in_flight.fetch_max(cur, Ordering::SeqCst);
            self.prompts
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(prompt.clone());
            self.agent_types
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(agent_type);

            let task = task_text(&prompt);
            let delay_ms: u64 = task.parse().unwrap_or(0);
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;

            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            Ok(format!("done-{task}"))
        }

        async fn spawn_agent_background(
            &self,
            _prompt: String,
            _allowed_tools: Vec<String>,
            _cwd: std::path::PathBuf,
            _cancel: tokio_util::sync::CancellationToken,
            _agent_type: Option<String>,
            _session: Arc<base::context::SessionState>,
        ) -> Result<String, Box<dyn std::error::Error + Send>> {
            unimplemented!("not exercised by these tests")
        }
    }

    fn make_ctx() -> (tempfile::TempDir, ToolContext) {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = ToolContext::for_test(dir.path().to_path_buf());
        (dir, ctx)
    }

    fn make_request(ctx: ToolContext, stages: Vec<TeamStage>) -> OrchestrateRequest {
        OrchestrateRequest {
            model: Arc::new(UnusedModel),
            config: Arc::new(EngineConfig::defaults_for("test")),
            parent_tools: Arc::new(InMemoryToolRegistry::new()),
            sub_tools: Arc::new(InMemoryToolRegistry::new()),
            stages,
            name: "test-team".to_string(),
            scratchpad: None,
            ctx,
            permission_mode: None,
        }
    }

    /// (a) A stage with N agents, each sleeping D ms, should complete in
    /// wall-clock time much closer to D than to N*D — proving the agents
    /// run concurrently rather than sequentially.
    #[tokio::test]
    async fn stage_agents_run_concurrently() {
        let n = 6usize;
        let delay_ms: u64 = 100;

        let agents: Vec<TeamAgentSpec> = (0..n)
            .map(|i| TeamAgentSpec {
                label: format!("agent{i}"),
                prompt: delay_ms.to_string(),
                agent_type: None,
            })
            .collect();
        let stages = vec![TeamStage {
            name: "stage0".into(),
            agents,
            aggregate: None,
        }];

        let (_dir, ctx) = make_ctx();
        let spawner = Arc::new(FakeSpawner::new());
        let coordinator = DefaultCoordinator::with_agent_spawner(spawner);
        let req = make_request(ctx, stages);

        let start = std::time::Instant::now();
        let result = coordinator.orchestrate(req).await.expect("orchestrate ok");
        let elapsed = start.elapsed();

        assert!(!result.is_error, "no agent should have errored");
        let sequential_upper_bound = Duration::from_millis(delay_ms * n as u64);
        assert!(
            elapsed < sequential_upper_bound.mul_f64(0.6),
            "expected concurrent dispatch (~{delay_ms}ms), got {elapsed:?} \
             (sequential would be ~{sequential_upper_bound:?})"
        );
    }

    /// (b) The section order in the coordinator's output must match the
    /// declaration order of `stage.agents`, even when later-declared agents
    /// finish first (i.e. output order is NOT completion order).
    #[tokio::test]
    async fn stage_output_preserves_declaration_order_not_completion_order() {
        // Agent 0 sleeps longest, last agent sleeps shortest — so completion
        // order is the reverse of declaration order.
        let delays_ms: [u64; 4] = [150, 100, 50, 10];
        let agents: Vec<TeamAgentSpec> = delays_ms
            .iter()
            .enumerate()
            .map(|(i, d)| TeamAgentSpec {
                label: format!("agent{i}"),
                prompt: d.to_string(),
                agent_type: None,
            })
            .collect();
        let stages = vec![TeamStage {
            name: "stage0".into(),
            agents,
            aggregate: None,
        }];

        let (_dir, ctx) = make_ctx();
        let spawner = Arc::new(FakeSpawner::new());
        let coordinator = DefaultCoordinator::with_agent_spawner(spawner);
        let req = make_request(ctx, stages);

        let result = coordinator.orchestrate(req).await.expect("orchestrate ok");
        let text = match &result.content {
            ToolResultContent::Text(t) => t.clone(),
            ToolResultContent::Blocks(_) => panic!("expected text content"),
        };

        // Assert each label's heading appears in the text in declaration
        // order (agent0 before agent1 before agent2 before agent3), which
        // is the reverse of completion order.
        let positions: Vec<usize> = (0..delays_ms.len())
            .map(|i| {
                text.find(&format!("### agent{i}"))
                    .unwrap_or_else(|| panic!("missing section for agent{i} in:\n{text}"))
            })
            .collect();
        let mut sorted = positions.clone();
        sorted.sort();
        assert_eq!(
            positions, sorted,
            "expected output sections in declaration order agent0..agent3, got positions {positions:?}"
        );
    }

    /// (c) With more agents than the semaphore has permits, the observed
    /// peak in-flight concurrency must never exceed the permit count (6).
    #[tokio::test]
    async fn stage_dispatch_is_bounded_by_semaphore() {
        let n = 10usize;
        let delay_ms: u64 = 40;

        let agents: Vec<TeamAgentSpec> = (0..n)
            .map(|i| TeamAgentSpec {
                label: format!("agent{i}"),
                prompt: delay_ms.to_string(),
                agent_type: None,
            })
            .collect();
        let stages = vec![TeamStage {
            name: "stage0".into(),
            agents,
            aggregate: None,
        }];

        let (_dir, ctx) = make_ctx();
        let spawner = Arc::new(FakeSpawner::new());
        let coordinator = DefaultCoordinator::with_agent_spawner(spawner.clone());
        let req = make_request(ctx, stages);

        let result = coordinator.orchestrate(req).await.expect("orchestrate ok");
        assert!(!result.is_error, "no agent should have errored");

        let peak = spawner.peak_in_flight.load(Ordering::SeqCst);
        assert!(
            peak <= 6,
            "expected peak in-flight <= 6 permits, observed {peak}"
        );
        // Sanity: with 10 agents and a limit of 6 we expect the bound to
        // actually be exercised (otherwise the test proves nothing).
        assert!(
            peak >= 2,
            "test setup produced no real concurrency (peak={peak})"
        );
    }

    /// A fake `AgentSpawner` that actually *uses* `extra_tools` (unlike
    /// `FakeSpawner`, which never overrides `spawn_agent_with_tools` and so
    /// silently falls through to the trait's ignore-and-delegate default) —
    /// it calls the `ListPeers` tool it was handed and returns its result as
    /// the "agent's own output", proving the tool it received is real and
    /// wired to the right mailbox, not just present-but-inert.
    struct MailboxAwareSpawner;

    #[async_trait]
    impl AgentSpawner for MailboxAwareSpawner {
        async fn spawn_agent(
            &self,
            _prompt: String,
            _allowed_tools: Vec<String>,
            _cwd: std::path::PathBuf,
            _cancel: tokio_util::sync::CancellationToken,
            _agent_type: Option<String>,
        ) -> Result<String, Box<dyn std::error::Error + Send>> {
            panic!("MailboxAwareSpawner test only expects spawn_agent_with_tools to be called");
        }

        async fn spawn_agent_background(
            &self,
            _prompt: String,
            _allowed_tools: Vec<String>,
            _cwd: std::path::PathBuf,
            _cancel: tokio_util::sync::CancellationToken,
            _agent_type: Option<String>,
            _session: Arc<base::context::SessionState>,
        ) -> Result<String, Box<dyn std::error::Error + Send>> {
            panic!("not used by this test");
        }

        async fn spawn_agent_with_tools(
            &self,
            _prompt: String,
            _allowed_tools: Vec<String>,
            extra_tools: Vec<Arc<dyn base::tool::Tool>>,
            cwd: std::path::PathBuf,
            _cancel: tokio_util::sync::CancellationToken,
            _agent_type: Option<String>,
            _permission_mode: Option<base::interface::settings::PermissionMode>,
        ) -> Result<String, Box<dyn std::error::Error + Send>> {
            let list_peers = extra_tools
                .iter()
                .find(|t| t.name() == "ListPeers")
                .expect("coordinator must hand each team member a ListPeers tool");
            let ctx = ToolContext::for_test(cwd);
            let result = list_peers
                .call(
                    serde_json::json!({}),
                    ctx,
                    base::tool::ProgressSender::noop("test"),
                )
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send>)?;
            match result.content {
                ToolResultContent::Text(t) => Ok(t),
                ToolResultContent::Blocks(_) => Ok("(blocks)".into()),
            }
        }
    }

    /// (e) Phase-2 regression: team members must receive *working*
    /// `SendMessage`/`ReadMail`/`ListPeers` tools scoped to the team's own
    /// mailbox — not just have the plumbing exist unused. Each of the two
    /// members here calls `ListPeers` for real and returns the result as
    /// its output; both should see both labels.
    #[tokio::test]
    async fn orchestrate_gives_team_members_a_working_listpeers_tool() {
        let agents = vec![
            TeamAgentSpec {
                label: "alice".into(),
                prompt: "irrelevant".into(),
                agent_type: None,
            },
            TeamAgentSpec {
                label: "bob".into(),
                prompt: "irrelevant".into(),
                agent_type: None,
            },
        ];
        let stages = vec![TeamStage {
            name: "stage0".into(),
            agents,
            aggregate: None,
        }];

        let (_dir, ctx) = make_ctx();
        let spawner = Arc::new(MailboxAwareSpawner);
        let coordinator = DefaultCoordinator::with_agent_spawner(spawner);
        let req = make_request(ctx, stages);

        let result = coordinator.orchestrate(req).await.expect("orchestrate ok");
        assert!(!result.is_error, "no agent should have errored");
        let text = match &result.content {
            ToolResultContent::Text(t) => t.clone(),
            ToolResultContent::Blocks(_) => panic!("expected text content"),
        };
        assert!(
            text.contains("alice"),
            "expected 'alice' to appear via a real ListPeers call in:\n{text}"
        );
        assert!(
            text.contains("bob"),
            "expected 'bob' to appear via a real ListPeers call in:\n{text}"
        );
    }

    /// (f) Phase-1 regression: `orchestrate()` records the team in the
    /// shared registry (so `TeamList` can see it), and `cleanup_team()`
    /// actually removes both the registry entry and the on-disk
    /// `.agix/teams/{id}/` directory — not the old no-op that claimed
    /// success unconditionally. A second `cleanup_team()` call for the same
    /// name must error instead of silently "succeeding" again.
    #[tokio::test]
    async fn orchestrate_records_team_and_cleanup_team_removes_it() {
        let agents = vec![TeamAgentSpec {
            label: "worker".into(),
            prompt: "0".into(),
            agent_type: None,
        }];
        let stages = vec![TeamStage {
            name: "stage0".into(),
            agents,
            aggregate: None,
        }];

        let (_dir, ctx) = make_ctx();
        let spawner = Arc::new(FakeSpawner::new());
        let registry = Arc::new(crate::registry::TeamRegistry::new());
        let coordinator =
            DefaultCoordinator::with_agent_spawner(spawner).with_registry(registry.clone());
        let req = make_request(ctx, stages);

        coordinator.orchestrate(req).await.expect("orchestrate ok");

        let teams = registry.list();
        assert_eq!(teams.len(), 1, "expected exactly one team recorded");
        let team = teams[0].clone();
        assert_eq!(team.name, "test-team");
        assert_eq!(team.members.len(), 1);
        assert_eq!(team.members[0].label, "worker");
        assert_eq!(team.members[0].lifecycle, TeammateLifecycle::Completed);
        assert!(
            team.team_dir.exists(),
            "team_dir should exist on disk after orchestrate(): {:?}",
            team.team_dir
        );

        coordinator
            .cleanup_team("test-team")
            .await
            .expect("cleanup_team should succeed for a team that exists");
        assert!(
            registry.list().is_empty(),
            "team should be gone from the registry after cleanup_team"
        );
        assert!(
            !team.team_dir.exists(),
            "team_dir should be deleted after cleanup_team: {:?}",
            team.team_dir
        );

        let err = coordinator
            .cleanup_team("test-team")
            .await
            .expect_err("cleanup_team on an already-removed team must error, not fake-succeed");
        assert!(
            err.contains("no such team"),
            "expected a clear 'no such team' error, got: {err}"
        );
    }

    /// §6.2/§6.3: `orchestrate()` must land `team.json`/`state.json`/
    /// `events.jsonl` in the shapes `crate::persist` defines, not just the
    /// in-memory `TeamRegistry` entry — a future project-level `TeamStore`
    /// reads these files, not the per-session registry.
    #[tokio::test]
    async fn orchestrate_writes_team_json_state_json_and_events_jsonl() {
        let agents = vec![TeamAgentSpec {
            label: "worker".into(),
            prompt: "0".into(),
            agent_type: None,
        }];
        let stages = vec![TeamStage {
            name: "stage0".into(),
            agents,
            aggregate: None,
        }];

        let (_dir, ctx) = make_ctx();
        let spawner = Arc::new(FakeSpawner::new());
        let registry = Arc::new(crate::registry::TeamRegistry::new());
        let coordinator = DefaultCoordinator::with_agent_spawner(spawner)
            .with_registry(registry.clone())
            .with_scene_id("coding".to_string());
        let req = make_request(ctx, stages);

        coordinator.orchestrate(req).await.expect("orchestrate ok");
        let team_dir = registry.list()[0].team_dir.clone();

        let decl = crate::persist::read_team_declaration(&team_dir)
            .unwrap()
            .expect("team.json should exist");
        assert_eq!(decl.name, "test-team");
        assert_eq!(decl.scene, "coding");
        assert_eq!(decl.mode, crate::persist::TeamMode::Batch);
        assert_eq!(decl.stages.len(), 1);
        assert_eq!(decl.stages[0].members[0].label, "worker");
        assert!(
            !team_dir.join("config.json").exists(),
            "config.json should be superseded by team.json, not written alongside it"
        );

        let state = crate::persist::read_team_state(&team_dir)
            .unwrap()
            .expect("state.json should exist");
        assert_eq!(state.status, crate::persist::TeamStatus::Completed);
        assert_eq!(state.members[0].lifecycle, TeammateLifecycle::Completed);

        let events = crate::persist::read_events(&team_dir).unwrap();
        let kinds: Vec<&str> = events
            .iter()
            .map(|e| match &e.event {
                crate::persist::TeamEvent::TeamCreated { .. } => "team_created",
                crate::persist::TeamEvent::StageStarted { .. } => "stage_started",
                crate::persist::TeamEvent::MemberSpawned { .. } => "member_spawned",
                crate::persist::TeamEvent::MemberCompleted { .. } => "member_completed",
                crate::persist::TeamEvent::StageCompleted { .. } => "stage_completed",
                crate::persist::TeamEvent::TeamCompleted { .. } => "team_completed",
                crate::persist::TeamEvent::Truncated { .. } => "truncated",
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                "team_created",
                "stage_started",
                "member_spawned",
                "member_completed",
                "stage_completed",
                "team_completed",
            ]
        );
    }
    /// T-1: a worker used to receive its raw task text and nothing else —
    /// no idea it was in a team, which stage, who else was running, or where
    /// the scratchpad was. The coordinator prompt papered over that by
    /// telling the lead to repeat it in every prompt; this asserts the
    /// plumbing does it instead.
    #[tokio::test]
    async fn worker_prompt_carries_the_team_context_block() {
        let stages = vec![
            TeamStage {
                name: "research".into(),
                agents: vec![
                    TeamAgentSpec {
                        label: "sources".into(),
                        prompt: "0".into(),
                        agent_type: Some("explore".into()),
                    },
                    TeamAgentSpec {
                        label: "prior-art".into(),
                        prompt: "0".into(),
                        agent_type: None,
                    },
                ],
                aggregate: None,
            },
            TeamStage {
                name: "synthesis".into(),
                agents: vec![TeamAgentSpec {
                    label: "writer".into(),
                    prompt: "0".into(),
                    agent_type: None,
                }],
                aggregate: None,
            },
        ];

        let (_dir, ctx) = make_ctx();
        let spawner = Arc::new(FakeSpawner::new());
        let coordinator = DefaultCoordinator::with_agent_spawner(spawner.clone());
        coordinator
            .orchestrate(make_request(ctx, stages))
            .await
            .expect("orchestrate ok");

        let prompts = spawner.prompts.lock().unwrap().clone();
        assert_eq!(prompts.len(), 3);

        let sources = prompts
            .iter()
            .find(|p| p.contains("You are: sources"))
            .expect("no prompt for the `sources` worker");
        assert!(sources.starts_with("<team-context>"));
        assert!(sources.contains("Team: test-team"));
        assert!(sources.contains("Stage 1/2: research"));
        assert!(
            sources.contains("prior-art"),
            "a worker should know who is running alongside it: {sources}"
        );
        assert!(
            sources.contains("synthesis"),
            "a worker should know its output feeds a later stage: {sources}"
        );
        assert!(
            sources.contains("SCRATCHPAD.md"),
            "a worker should be told where the shared scratchpad is: {sources}"
        );
        assert!(
            sources.ends_with("0"),
            "the worker's own task text must still be the tail of the prompt"
        );

        // The last stage has no later stages and a single member.
        let writer = prompts
            .iter()
            .find(|p| p.contains("You are: writer"))
            .expect("no prompt for the `writer` worker");
        assert!(writer.contains("Stage 2/2: synthesis"));
        assert!(writer.contains("you are the only worker in this stage"));

        // A member's declared `agent_type` used to be parsed and dropped.
        let types = spawner.agent_types.lock().unwrap().clone();
        assert!(
            types.contains(&Some("explore".to_string())),
            "a member's agent_type must reach the spawner, got {types:?}"
        );
    }

    /// T-3: `TeamCreate` used to return nothing until every member of every
    /// stage had finished. Progress must now be observable while the run is
    /// still going, and must arrive before the final tool result.
    #[tokio::test]
    async fn team_progress_is_emitted_per_stage_before_the_final_result() {
        use base::interface::event::{AgentEvent, TeamStageStatus};

        let stages = vec![
            TeamStage {
                name: "first".into(),
                agents: vec![TeamAgentSpec {
                    label: "a".into(),
                    prompt: "0".into(),
                    agent_type: None,
                }],
                aggregate: None,
            },
            TeamStage {
                name: "second".into(),
                agents: vec![TeamAgentSpec {
                    label: "b".into(),
                    prompt: "0".into(),
                    agent_type: None,
                }],
                aggregate: None,
            },
        ];

        let (_dir, ctx) = make_ctx();
        let spawner = Arc::new(FakeSpawner::new());
        let coordinator = DefaultCoordinator::with_agent_spawner(spawner);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        coordinator.set_event_sender(tx);

        let result = coordinator
            .orchestrate(make_request(ctx, stages))
            .await
            .expect("orchestrate ok");

        let mut seen: Vec<(String, TeamStageStatus)> = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            match ev {
                AgentEvent::TeamProgress {
                    team,
                    stage,
                    stage_index,
                    stage_count,
                    status,
                    members,
                    ..
                } => {
                    assert_eq!(team, "test-team");
                    assert_eq!(stage_count, 2);
                    assert_eq!(members.len(), 1);
                    assert_eq!(stage_index, if stage == "first" { 0 } else { 1 });
                    seen.push((stage, status));
                }
                other => panic!("unexpected event {other:?}"),
            }
        }

        assert_eq!(
            seen,
            vec![
                ("first".to_string(), TeamStageStatus::Started),
                ("first".to_string(), TeamStageStatus::Completed),
                ("second".to_string(), TeamStageStatus::Started),
                ("second".to_string(), TeamStageStatus::Completed),
            ],
            "each stage must report start and completion, in order"
        );
        assert!(!result.is_error);
    }

    #[tokio::test]
    async fn set_telemetry_handle_records_agent_and_stage_lifecycle_events() {
        let stages = vec![TeamStage {
            name: "only".into(),
            agents: vec![TeamAgentSpec {
                label: "a".into(),
                prompt: "0".into(),
                agent_type: Some("researcher".into()),
            }],
            aggregate: None,
        }];

        let (_dir, ctx) = make_ctx();
        let spawner = Arc::new(FakeSpawner::new());
        let coordinator = DefaultCoordinator::with_agent_spawner(spawner);
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        coordinator.set_telemetry_handle(telemetry::TelemetryHandle::new(tx));

        let result = coordinator
            .orchestrate(make_request(ctx, stages))
            .await
            .expect("orchestrate ok");
        assert!(!result.is_error);

        let mut kinds = Vec::new();
        while let Ok(event) = rx.try_recv() {
            kinds.push(event.kind().to_string());
        }
        assert!(
            kinds.contains(&"agent_spawned".to_string()),
            "got: {kinds:?}"
        );
        assert!(
            kinds.contains(&"agent_completed".to_string()),
            "got: {kinds:?}"
        );
        assert!(
            kinds.contains(&"team_stage_complete".to_string()),
            "got: {kinds:?}"
        );
    }

    #[tokio::test]
    async fn resume_coordinator_records_resume_action() {
        let (dir, ctx) = make_ctx();
        let team_dir = dir.path().join(".agix/teams/task-1");
        tokio::fs::create_dir_all(&team_dir).await.unwrap();
        tokio::fs::write(team_dir.join("SCRATCHPAD.md"), "## stage-one\n\nnotes\n")
            .await
            .unwrap();

        let coordinator = DefaultCoordinator::new();
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        coordinator.set_telemetry_handle(telemetry::TelemetryHandle::new(tx));

        let result = coordinator
            .resume_coordinator("task-1", &ctx)
            .await
            .expect("resume should find the scratchpad");
        assert!(!result.is_error);

        let event = rx
            .try_recv()
            .expect("resume_action should have been recorded");
        assert_eq!(event.kind(), "resume_action");
    }

    /// A failing member is named in the stage's completion event, so the
    /// host can surface the problem without waiting for the whole run.
    #[tokio::test]
    async fn team_progress_reports_failed_members() {
        use base::interface::event::{AgentEvent, TeamStageStatus};

        struct AlwaysFails;
        #[async_trait]
        impl AgentSpawner for AlwaysFails {
            async fn spawn_agent(
                &self,
                _prompt: String,
                _allowed_tools: Vec<String>,
                _cwd: std::path::PathBuf,
                _cancel: tokio_util::sync::CancellationToken,
                _agent_type: Option<String>,
            ) -> Result<String, Box<dyn std::error::Error + Send>> {
                #[derive(Debug)]
                struct E;
                impl std::fmt::Display for E {
                    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                        write!(f, "boom")
                    }
                }
                impl std::error::Error for E {}
                Err(Box::new(E))
            }
            async fn spawn_agent_background(
                &self,
                _prompt: String,
                _allowed_tools: Vec<String>,
                _cwd: std::path::PathBuf,
                _cancel: tokio_util::sync::CancellationToken,
                _agent_type: Option<String>,
                _session: Arc<base::context::SessionState>,
            ) -> Result<String, Box<dyn std::error::Error + Send>> {
                unimplemented!()
            }
        }

        let stages = vec![TeamStage {
            name: "only".into(),
            agents: vec![TeamAgentSpec {
                label: "doomed".into(),
                prompt: "task".into(),
                agent_type: None,
            }],
            aggregate: None,
        }];
        let (_dir, ctx) = make_ctx();
        let coordinator = DefaultCoordinator::with_agent_spawner(Arc::new(AlwaysFails));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        coordinator.set_event_sender(tx);

        let result = coordinator
            .orchestrate(make_request(ctx, stages))
            .await
            .expect("orchestrate returns Ok with is_error set");
        assert!(result.is_error);

        let mut failed_seen = None;
        while let Ok(ev) = rx.try_recv() {
            if let AgentEvent::TeamProgress {
                status: TeamStageStatus::Completed,
                failed,
                ..
            } = ev
            {
                failed_seen = Some(failed);
            }
        }
        assert_eq!(failed_seen, Some(vec!["doomed".to_string()]));
    }

    /// Without a wired channel nothing is emitted and orchestration is
    /// unaffected — the pre-existing behavior for embedders that never call
    /// `set_event_sender`.
    #[tokio::test]
    async fn team_progress_is_a_no_op_without_a_wired_channel() {
        let stages = vec![TeamStage {
            name: "only".into(),
            agents: vec![TeamAgentSpec {
                label: "a".into(),
                prompt: "0".into(),
                agent_type: None,
            }],
            aggregate: None,
        }];
        let (_dir, ctx) = make_ctx();
        let coordinator = DefaultCoordinator::with_agent_spawner(Arc::new(FakeSpawner::new()));
        let result = coordinator
            .orchestrate(make_request(ctx, stages))
            .await
            .expect("orchestrate ok");
        assert!(!result.is_error);
    }
}
