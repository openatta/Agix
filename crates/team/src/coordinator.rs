//! Team coordinator: runs a team's stages one after another, each stage's
//! members side by side as delegates of the lead's session, and keeps the
//! team's record on disk (`team.json`, `state.json`, `events.jsonl`).

use async_trait::async_trait;
use base::context::EngineConfig;
use base::error::ToolError;
use base::interface::delegate::{
    CallSite, DelegateEnd, DelegateMode, DelegateRequest, Delegated, Delegator, MemberLifetime,
    StopsWith, TeamSeat, Workdir,
};
use base::interface::model::{Model, ModelEvent, StreamParams};
use base::prompt::{BlockRole, PromptBlock};
use base::settings::ThinkingMode;
use base::tool::ToolContext;
use base::tool::ToolResult;
use base::tool::ToolResultContent;
use futures::stream::StreamExt;
use std::collections::HashMap;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use crate::tool::{AggregateMode, TeamStage};

/// The lead session's event channel, where a team's `TeamProgress` goes.
pub type TeamEventSender = tokio::sync::mpsc::UnboundedSender<base::event::AgentEvent>;

#[async_trait]
pub trait Coordinator: Send + Sync {
    async fn orchestrate(&self, request: OrchestrateRequest) -> Result<ToolResult, ToolError>;

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
    pub permission_mode: Option<base::settings::PermissionMode>,
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

/// How one stage member's run ended, as the team counts it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MemberEnd {
    Done,
    Failed,
    Cancelled,
}

/// Default coordinator implementation: its members are delegated through a
/// [`Delegator`] from `base`, which `agents` implements — `team` sits below
/// it and knows only the trait.
pub struct DefaultCoordinator {
    delegator: Option<Arc<dyn Delegator>>,
    registry: Option<Arc<crate::registry::TeamRegistry>>,
    events: Option<TeamEventSender>,
    telemetry: Option<telemetry::TelemetryHandle>,
    /// Stamped into `team.json`; `"unknown"` when not given. `team` does not
    /// depend on the `scene` crate, so whoever builds the coordinator says.
    scene_id: Option<String>,
}

impl DefaultCoordinator {
    /// A coordinator that cannot start members: every stage reports them
    /// failed.
    pub fn new() -> Self {
        Self {
            delegator: None,
            registry: None,
            events: None,
            telemetry: None,
            scene_id: None,
        }
    }

    pub fn with_delegator(delegator: Arc<dyn Delegator>) -> Self {
        Self {
            delegator: Some(delegator),
            ..Self::new()
        }
    }

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

    /// Where `TeamProgress` goes as stages start and finish, so a team run
    /// is not one long silence followed by an enormous tool result.
    pub fn with_events(mut self, events: TeamEventSender) -> Self {
        self.events = Some(events);
        self
    }

    /// Where the team's stage and resume telemetry goes. Its members' own
    /// `agent_spawned`/`agent_completed` are recorded by the delegator.
    pub fn with_telemetry(mut self, handle: telemetry::TelemetryHandle) -> Self {
        self.telemetry = Some(handle);
        self
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_progress(
        &self,
        team: &str,
        team_id: &str,
        stage: &str,
        stage_index: usize,
        stage_count: usize,
        status: base::event::TeamStageStatus,
        members: Vec<String>,
        failed: Vec<String>,
    ) {
        let Some(tx) = &self.events else {
            return;
        };
        let _ = tx.send(base::event::AgentEvent::TeamProgress {
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

/// One stage member's seat and orders.
struct MemberCall {
    seat: TeamSeat,
    stage: String,
    prompt: String,
    agent_type: Option<String>,
    extra_tools: Vec<Arc<dyn base::tool::Tool>>,
    permission_mode: base::settings::PermissionMode,
}

/// One stage member, delegated and waited on. It stops with the lead's call.
async fn run_member(
    delegator: &dyn Delegator,
    call: MemberCall,
    ctx: &ToolContext,
) -> (String, MemberEnd) {
    let mode = DelegateMode::TeamMember {
        seat: call.seat,
        permission_mode: Some(call.permission_mode),
        lifetime: MemberLifetime::Stage {
            stage: call.stage,
            extra_tools: call.extra_tools,
        },
    };
    let mut req = DelegateRequest::new(call.prompt, mode, StopsWith::Call(ctx.cancel.clone()));
    req.agent_type = call.agent_type;
    req.workdir = Workdir::At(ctx.cwd.clone());
    req.called_from = Some(CallSite {
        turn: ctx.turn_no,
        tool_use_id: ctx.tool_use_id.clone(),
    });
    match delegator.delegate(req).await {
        Ok(Delegated::Finished(outcome)) => match outcome.end {
            DelegateEnd::Completed | DelegateEnd::CutShort(_) => (outcome.text, MemberEnd::Done),
            DelegateEnd::Cancelled => (outcome.text, MemberEnd::Cancelled),
            DelegateEnd::Failed(e) => (format!("ERROR: {e}"), MemberEnd::Failed),
        },
        Ok(Delegated::Started(_)) => (
            "ERROR: a stage member was started instead of waited on".into(),
            MemberEnd::Failed,
        ),
        Err(e) => (format!("ERROR: {e}"), MemberEnd::Failed),
    }
}

impl Default for DefaultCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Coordinator for DefaultCoordinator {
    async fn orchestrate(&self, req: OrchestrateRequest) -> Result<ToolResult, ToolError> {
        let OrchestrateRequest {
            model,
            config,
            stages,
            name,
            scratchpad,
            ctx,
            permission_mode,
        } = req;
        let permission_mode = permission_mode.unwrap_or(base::settings::PermissionMode::Plan);

        let all_labels: Vec<String> = stages
            .iter()
            .flat_map(|s| s.agents.iter().map(|a| a.label.clone()))
            .collect();
        let team_id = format!("team-{}-{}", name, chrono_id());
        let team_dir = ctx.session.env().cwd.join(".agix/teams").join(&team_id);
        let sp_path = team_dir.join("SCRATCHPAD.md");
        if let Some(p) = sp_path.parent() {
            let _ = tokio::fs::create_dir_all(p).await;
        }

        // team.json (write-once declaration) + events.jsonl (team_created),
        // in the shape a project-level `TeamStore` reads.
        let declaration = crate::persist::TeamDeclaration {
            schema_version: crate::persist::CURRENT_TEAM_SCHEMA_VERSION,
            team_id: team_id.clone(),
            name: name.clone(),
            scene: self
                .scene_id
                .clone()
                .unwrap_or_else(|| "unknown".to_string()),
            project_root: ctx.session.env().cwd.display().to_string(),
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
        let memory = Arc::new(crate::team_memory::TeamMemoryStore::for_team_dir(&team_dir));

        let mut scratch = format!(
            "# Team `{name}`\n\nTeam id: `{team_id}`\nStages: {}\n",
            stages.len()
        );
        if let Some(s) = scratchpad {
            scratch.push_str("\n## 0_initial_context\n\n");
            scratch.push_str(&s);
            scratch.push('\n');
        }
        let _ = tokio::fs::write(&sp_path, &scratch).await;

        // Teammate lifecycle tracking.
        let mut lifecycles: std::collections::HashMap<String, TeammateLifecycle> = stages
            .iter()
            .flat_map(|s| s.agents.iter())
            .map(|a| (a.label.clone(), TeammateLifecycle::Idle))
            .collect();

        let stage_count = stages.len();
        let all_stage_names: Vec<String> = stages.iter().map(|s| s.name.clone()).collect();
        let sp_display = sp_path.display().to_string();

        let mut any_err = false;
        let mut cancelled = false;
        let mut started = 0;
        for (si, stage) in stages.iter().enumerate() {
            if ctx.cancel.is_cancelled() {
                cancelled = true;
                break;
            }
            started = si + 1;
            let stage_start = std::time::Instant::now();
            let mut sections: Vec<(String, String, bool)> = Vec::new();
            let stage_members: Vec<String> = stage.agents.iter().map(|a| a.label.clone()).collect();

            // Tell the host the stage is starting, so a large team
            // streams progress instead of going silent until the very end.
            self.emit_progress(
                &name,
                &team_id,
                &stage.name,
                si,
                stage_count,
                base::event::TeamStageStatus::Started,
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

            if let Some(delegator) = &self.delegator {
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
                let memory_prompt = {
                    let memory = memory.clone();
                    tokio::task::spawn_blocking(move || memory.build_team_memory_prompt())
                        .await
                        .unwrap_or_default()
                };
                let ctx = &ctx;
                let futs = stage.agents.iter().map(|agent_spec| {
                    let permits = permits.clone();
                    let label = agent_spec.label.clone();
                    // Prepend a short orientation block — team, stage, role,
                    // peers, scratchpad — rather than leaning on the
                    // coordinator prompt to nag the lead into repeating it.
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
                            team_memory: &memory_prompt,
                        },
                        &agent_spec.prompt,
                    );
                    // Mailbox tools scoped to this member's own label — lets
                    // it coordinate with whichever siblings are concurrently
                    // running in the same stage. The lead is blocked awaiting
                    // the whole stage, so it can't take part, only siblings.
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
                        Arc::new(crate::team_memory::TeamMemorySaveTool::new(
                            memory.clone(),
                            format!("{team_id}/{label}"),
                        )),
                        Arc::new(crate::team_memory::TeamMemoryReadTool::new(memory.clone())),
                    ];
                    let call = MemberCall {
                        seat: TeamSeat {
                            team: name.clone(),
                            member: label.clone(),
                        },
                        stage: stage.name.clone(),
                        prompt,
                        agent_type: agent_spec.agent_type.clone(),
                        extra_tools,
                        permission_mode,
                    };
                    async move {
                        let _permit = permits.acquire_owned().await.expect("semaphore closed");
                        if ctx.cancel.is_cancelled() {
                            return (label, String::new(), MemberEnd::Cancelled);
                        }
                        let (text, end) = run_member(delegator.as_ref(), call, ctx).await;
                        (label, text, end)
                    }
                });
                let results = futures::future::join_all(futs).await;

                // Bulk-mark every agent in this stage Completed after the join.
                for agent_spec in &stage.agents {
                    lifecycles.insert(agent_spec.label.clone(), TeammateLifecycle::Completed);
                }

                for (label, text, end) in results {
                    let is_error = end != MemberEnd::Done;
                    match end {
                        MemberEnd::Done => {}
                        MemberEnd::Failed => any_err = true,
                        MemberEnd::Cancelled => cancelled = true,
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
                    sections.push((label, text, is_error));
                }
            } else {
                tracing::warn!(
                    "TeamCreate: no delegator — stage '{}' agents will not execute",
                    stage.name
                );
                for agent_spec in &stage.agents {
                    sections.push((
                        agent_spec.label.clone(),
                        "[no delegator — this session cannot start team members]".to_string(),
                        true,
                    ));
                    any_err = true;
                }
            }
            cancelled |= ctx.cancel.is_cancelled();

            if let Some(mode) = stage.aggregate.filter(|_| !cancelled) {
                if !sections.is_empty() {
                    sections = aggregate(
                        &*model,
                        &config.model,
                        mode,
                        &stage.name,
                        &sections,
                        Some(ctx.session_id.as_str()),
                        &ctx.cancel,
                    )
                    .await;
                }
            }
            cancelled |= ctx.cancel.is_cancelled();

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

            // Stage finished — report which members failed so the host
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
                base::event::TeamStageStatus::Completed,
                stage_members,
                failed.clone(),
            );
            if let Some(handle) = &self.telemetry {
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
            if cancelled {
                break;
            }
        }

        let final_status = if cancelled {
            crate::persist::TeamStatus::Cancelled
        } else if any_err {
            crate::persist::TeamStatus::Failed
        } else {
            crate::persist::TeamStatus::Completed
        };
        let state = build_state_snapshot(started.saturating_sub(1), final_status, &lifecycles);
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

        let mut text = format!("{}\n\n_(scratchpad: {})_", scratch, sp_path.display());
        if cancelled {
            text.push_str(&format!(
                "\n\nTeam `{name}` was cancelled: {started} of {stage_count} stages were started."
            ));
        }
        Ok(ToolResult {
            content: ToolResultContent::Text(text),
            is_error: any_err || cancelled,
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
    /// calls `Delegator::stop_member` for every member on record; it's a
    /// no-op for the ones that were never persistent in the first place.
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
        if let Some(delegator) = &self.delegator {
            for member in &info.members {
                delegator
                    .stop_member(&TeamSeat {
                        team: name.to_string(),
                        member: member.label.clone(),
                    })
                    .await;
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

        let team_dir = ctx.session.env().cwd.join(".agix/teams").join(task_id);
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

        let latency_ms = start.elapsed().as_millis() as u64;
        if let Some(handle) = &self.telemetry {
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

pub async fn aggregate(
    model: &dyn Model,
    model_name: &str,
    mode: AggregateMode,
    stage_name: &str,
    sections: &[(String, String, bool)],
    session_id: Option<&str>,
    cancel: &CancellationToken,
) -> Vec<(String, String, bool)> {
    match mode {
        AggregateMode::Concat => sections.to_vec(),
        AggregateMode::Best => {
            if let Some(label) =
                pick_best(model, model_name, stage_name, sections, session_id, cancel).await
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
            let text = merge(model, model_name, stage_name, sections, session_id, cancel)
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

async fn pick_best(
    model: &dyn Model,
    model_name: &str,
    stage_name: &str,
    sections: &[(String, String, bool)],
    session_id: Option<&str>,
    cancel: &CancellationToken,
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
    let label = drain(model, model_name, prompt, 100, session_id, cancel).await?;
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
    cancel: &CancellationToken,
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
    drain(model, model_name, prompt, 4096, session_id, cancel).await
}

async fn drain(
    model: &dyn Model,
    model_name: &str,
    prompt: String,
    max_tokens: u32,
    session_id: Option<&str>,
    cancel: &CancellationToken,
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
        effort: None,
        fallback_model: None,
        cache_edits: vec![],
        origin: session_id.map(|id| {
            base::interface::model::CallOrigin::auxiliary(
                id,
                base::interface::model::call_purpose::TEAM_JUDGE,
            )
        }),
        input_map: None,
        provider_search: None,
    };
    let mut stream = model
        .stream(blocks, vec![], messages, params, cancel.clone())
        .await
        .ok()?;
    let mut text = String::new();
    loop {
        let ev = tokio::select! {
            _ = cancel.cancelled() => return None,
            ev = stream.next() => ev,
        };
        let Some(ev) = ev else { break };
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::TeamAgentSpec;
    use base::interface::delegate::{DelegateError, DelegateOutcome};
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

    fn finished_as(text: impl Into<String>, end: DelegateEnd) -> Result<Delegated, DelegateError> {
        Ok(Delegated::Finished(DelegateOutcome {
            text: text.into(),
            end,
            structured: None,
            session_id: "member-session".into(),
        }))
    }

    fn finished(text: impl Into<String>) -> Result<Delegated, DelegateError> {
        finished_as(text, DelegateEnd::Completed)
    }

    fn cwd_of(req: &DelegateRequest) -> std::path::PathBuf {
        match &req.workdir {
            Workdir::At(dir) => dir.clone(),
            other => panic!("a stage member runs in the lead's cwd, not {other:?}"),
        }
    }

    fn stage_tools(req: DelegateRequest) -> Vec<Arc<dyn base::tool::Tool>> {
        match req.mode {
            DelegateMode::TeamMember {
                lifetime: MemberLifetime::Stage { extra_tools, .. },
                ..
            } => extra_tools,
            _ => panic!("only stage members are delegated here"),
        }
    }

    /// A fake `Delegator` whose `delegate` sleeps for a duration
    /// encoded in the agent's task text (as a plain millisecond count),
    /// tracking in-flight concurrency via an `AtomicUsize`. Records every
    /// prompt it was handed so tests can inspect the injected team context.
    struct FakeDelegator {
        in_flight: AtomicUsize,
        peak_in_flight: AtomicUsize,
        prompts: std::sync::Mutex<Vec<String>>,
        agent_types: std::sync::Mutex<Vec<Option<String>>>,
    }

    impl FakeDelegator {
        fn new() -> Self {
            Self {
                in_flight: AtomicUsize::new(0),
                peak_in_flight: AtomicUsize::new(0),
                prompts: std::sync::Mutex::new(Vec::new()),
                agent_types: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    /// The coordinator prepends a `<team-context>` block, so the task
    /// text a test encoded is the last line of what the delegator receives.
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
    impl Delegator for FakeDelegator {
        async fn stop_member(&self, _: &TeamSeat) {}

        async fn delegate(&self, request: DelegateRequest) -> Result<Delegated, DelegateError> {
            let prompt = request.prompt;
            let cur = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak_in_flight.fetch_max(cur, Ordering::SeqCst);
            self.prompts
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(prompt.clone());
            self.agent_types
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(request.agent_type);

            let task = task_text(&prompt);
            let delay_ms: u64 = task.parse().unwrap_or(0);
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;

            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            finished(format!("done-{task}"))
        }
    }

    fn make_ctx() -> (tempfile::TempDir, ToolContext) {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = platform::tool_context_for_test(dir.path().to_path_buf());
        (dir, ctx)
    }

    fn make_request(ctx: ToolContext, stages: Vec<TeamStage>) -> OrchestrateRequest {
        OrchestrateRequest {
            model: Arc::new(UnusedModel),
            config: Arc::new(EngineConfig::defaults_for("test")),
            stages,
            name: "test-team".to_string(),
            scratchpad: None,
            ctx,
            permission_mode: None,
        }
    }

    /// Each spawn waits until all `n` of a stage are in flight at once.
    struct GatheringDelegator {
        gathered: tokio::sync::Barrier,
    }

    #[async_trait]
    impl Delegator for GatheringDelegator {
        async fn stop_member(&self, _: &TeamSeat) {}

        async fn delegate(&self, _: DelegateRequest) -> Result<Delegated, DelegateError> {
            self.gathered.wait().await;
            finished("done")
        }
    }

    /// (a) The agents of a stage are all running at the same time: none of
    /// them can finish until every one has started.
    #[tokio::test]
    async fn stage_agents_run_concurrently() {
        let n = 6usize;
        let agents: Vec<TeamAgentSpec> = (0..n)
            .map(|i| TeamAgentSpec {
                label: format!("agent{i}"),
                prompt: "work".into(),
                agent_type: None,
            })
            .collect();
        let stages = vec![TeamStage {
            name: "stage0".into(),
            agents,
            aggregate: None,
        }];

        let (_dir, ctx) = make_ctx();
        let delegator = Arc::new(GatheringDelegator {
            gathered: tokio::sync::Barrier::new(n),
        });
        let coordinator = DefaultCoordinator::with_delegator(delegator);
        let req = make_request(ctx, stages);

        // Run one after the other, the first agent would wait for the rest
        // forever; the timeout only turns that into a failure.
        let result = tokio::time::timeout(Duration::from_secs(120), coordinator.orchestrate(req))
            .await
            .expect("the stage's agents never all ran at once")
            .expect("orchestrate ok");
        assert!(!result.is_error, "no agent should have errored");
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
        let delegator = Arc::new(FakeDelegator::new());
        let coordinator = DefaultCoordinator::with_delegator(delegator);
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

    #[tokio::test]
    async fn the_result_is_the_members_output_without_orchestration_instructions() {
        let stages = vec![TeamStage {
            name: "stage0".into(),
            agents: vec![TeamAgentSpec {
                label: "only".into(),
                prompt: "10".into(),
                agent_type: None,
            }],
            aggregate: None,
        }];
        let (_dir, ctx) = make_ctx();
        let coordinator = DefaultCoordinator::with_delegator(Arc::new(FakeDelegator::new()));
        let result = coordinator
            .orchestrate(make_request(ctx, stages))
            .await
            .expect("orchestrate ok");
        let ToolResultContent::Text(text) = &result.content else {
            panic!("expected text content");
        };
        assert!(text.contains("### only"), "{text}");
        for leaked in ["## Your Role", "## Your Tools", "## Workers", "SendMessage"] {
            assert!(!text.contains(leaked), "{leaked} in:\n{text}");
        }
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
        let delegator = Arc::new(FakeDelegator::new());
        let coordinator = DefaultCoordinator::with_delegator(delegator.clone());
        let req = make_request(ctx, stages);

        let result = coordinator.orchestrate(req).await.expect("orchestrate ok");
        assert!(!result.is_error, "no agent should have errored");

        let peak = delegator.peak_in_flight.load(Ordering::SeqCst);
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

    /// A fake `Delegator` that actually *uses* `extra_tools` (unlike
    /// `FakeDelegator`, which ignores them) — it calls the `ListPeers` tool it was handed and returns its result as
    /// the "agent's own output", proving the tool it received is real and
    /// wired to the right mailbox, not just present-but-inert.
    struct MailboxAwareDelegator;

    #[async_trait]
    impl Delegator for MailboxAwareDelegator {
        async fn stop_member(&self, _: &TeamSeat) {}

        async fn delegate(&self, request: DelegateRequest) -> Result<Delegated, DelegateError> {
            let ctx = platform::tool_context_for_test(cwd_of(&request));
            let extra_tools = stage_tools(request);
            let list_peers = extra_tools
                .iter()
                .find(|t| t.name() == "ListPeers")
                .expect("coordinator must hand each team member a ListPeers tool");
            let result = list_peers
                .call(
                    serde_json::json!({}),
                    ctx,
                    base::tool::ProgressSender::noop("test"),
                )
                .await
                .map_err(|e| DelegateError::Invalid(e.to_string()))?;
            match result.content {
                ToolResultContent::Text(t) => finished(t),
                ToolResultContent::Blocks(_) => finished("(blocks)"),
            }
        }
    }

    /// Team members must receive *working*
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
        let delegator = Arc::new(MailboxAwareDelegator);
        let coordinator = DefaultCoordinator::with_delegator(delegator);
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

    /// Saves a team memory when its task says "remember", otherwise answers
    /// with the prompt it was given.
    struct MemoryAwareDelegator;

    #[async_trait]
    impl Delegator for MemoryAwareDelegator {
        async fn stop_member(&self, _: &TeamSeat) {}

        async fn delegate(&self, request: DelegateRequest) -> Result<Delegated, DelegateError> {
            if !request.prompt.contains("remember") {
                return finished(request.prompt);
            }
            let ctx = platform::tool_context_for_test(cwd_of(&request));
            let extra_tools = stage_tools(request);
            let save = extra_tools
                .iter()
                .find(|t| t.name() == "TeamMemorySave")
                .expect("coordinator must hand each team member TeamMemorySave");
            let input = serde_json::json!({
                "name": "api-style",
                "description": "Handlers return typed errors",
                "content": "Every handler returns a typed error, never a string."
            });
            let result = save
                .call(input, ctx, base::tool::ProgressSender::noop("test"))
                .await
                .map_err(|e| DelegateError::Invalid(e.to_string()))?;
            assert!(!result.is_error);
            finished("saved")
        }
    }

    #[tokio::test]
    async fn a_later_stage_sees_what_an_earlier_stage_saved_to_team_memory() {
        let stage = |name: &str, label: &str, prompt: &str| TeamStage {
            name: name.into(),
            agents: vec![TeamAgentSpec {
                label: label.into(),
                prompt: prompt.into(),
                agent_type: None,
            }],
            aggregate: None,
        };
        let stages = vec![
            stage("design", "architect", "remember the API style"),
            stage("build", "builder", "implement it"),
        ];

        let (dir, ctx) = make_ctx();
        let registry = Arc::new(crate::registry::TeamRegistry::new());
        let coordinator = DefaultCoordinator::with_delegator(Arc::new(MemoryAwareDelegator))
            .with_registry(registry.clone());
        let mut req = make_request(ctx, stages);
        req.name = "mem".into();
        let result = coordinator.orchestrate(req).await.expect("orchestrate ok");
        let text = match &result.content {
            ToolResultContent::Text(t) => t.clone(),
            ToolResultContent::Blocks(_) => panic!("expected text content"),
        };
        assert!(
            text.contains("[api-style](api-style.md)"),
            "the build stage's prompt should list the saved entry:\n{text}"
        );

        let team_dir = registry.list()[0].team_dir.clone();
        assert!(team_dir.join("memory").join("api-style.md").is_file());
        assert!(team_dir.starts_with(dir.path()));
        coordinator.cleanup_team("mem").await.unwrap();
        assert!(!team_dir.join("memory").exists());
    }

    /// `orchestrate()` records the team in the shared registry (so
    /// `TeamList` can see it), and `cleanup_team()` actually removes both the
    /// registry entry and the on-disk `.agix/teams/{id}/` directory. A
    /// second `cleanup_team()` call for the same
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
        let delegator = Arc::new(FakeDelegator::new());
        let registry = Arc::new(crate::registry::TeamRegistry::new());
        let coordinator =
            DefaultCoordinator::with_delegator(delegator).with_registry(registry.clone());
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

    /// `orchestrate()` must land `team.json`/`state.json`/
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
        let delegator = Arc::new(FakeDelegator::new());
        let registry = Arc::new(crate::registry::TeamRegistry::new());
        let coordinator = DefaultCoordinator::with_delegator(delegator)
            .with_registry(registry.clone())
            .with_scene_id("code".to_string());
        let req = make_request(ctx, stages);

        coordinator.orchestrate(req).await.expect("orchestrate ok");
        let team_dir = registry.list()[0].team_dir.clone();

        let decl = crate::persist::read_team_declaration(&team_dir)
            .unwrap()
            .expect("team.json should exist");
        assert_eq!(decl.name, "test-team");
        assert_eq!(decl.scene, "code");
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
    /// A worker's prompt tells it it is in a team, which stage, who else is
    /// running, and where the scratchpad is — the plumbing does it, not the
    /// lead repeating it in every prompt.
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
        let delegator = Arc::new(FakeDelegator::new());
        let coordinator = DefaultCoordinator::with_delegator(delegator.clone());
        coordinator
            .orchestrate(make_request(ctx, stages))
            .await
            .expect("orchestrate ok");

        let prompts = delegator.prompts.lock().unwrap().clone();
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

        let types = delegator.agent_types.lock().unwrap().clone();
        assert!(
            types.contains(&Some("explore".to_string())),
            "a member's agent_type must reach the delegator, got {types:?}"
        );
    }

    /// `TeamCreate` progress must be observable while the run is still going,
    /// and must arrive before the final tool result.
    #[tokio::test]
    async fn team_progress_is_emitted_per_stage_before_the_final_result() {
        use base::event::{AgentEvent, TeamStageStatus};

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
        let delegator = Arc::new(FakeDelegator::new());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let coordinator = DefaultCoordinator::with_delegator(delegator).with_events(tx);

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
    async fn the_team_records_its_stages_and_leaves_its_members_to_the_delegator() {
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
        let delegator = Arc::new(FakeDelegator::new());
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let coordinator = DefaultCoordinator::with_delegator(delegator)
            .with_telemetry(telemetry::TelemetryHandle::new(tx));

        let result = coordinator
            .orchestrate(make_request(ctx, stages))
            .await
            .expect("orchestrate ok");
        assert!(!result.is_error);

        let mut kinds = Vec::new();
        while let Ok(event) = rx.try_recv() {
            kinds.push(event.kind().to_string());
        }
        assert_eq!(kinds, ["team_stage_complete"]);
    }

    #[tokio::test]
    async fn resume_coordinator_records_resume_action() {
        let (dir, ctx) = make_ctx();
        let team_dir = dir.path().join(".agix/teams/task-1");
        tokio::fs::create_dir_all(&team_dir).await.unwrap();
        tokio::fs::write(team_dir.join("SCRATCHPAD.md"), "## stage-one\n\nnotes\n")
            .await
            .unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let coordinator =
            DefaultCoordinator::new().with_telemetry(telemetry::TelemetryHandle::new(tx));

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
        use base::event::{AgentEvent, TeamStageStatus};

        struct AlwaysFails;
        #[async_trait]
        impl Delegator for AlwaysFails {
            async fn stop_member(&self, _: &TeamSeat) {}

            async fn delegate(&self, _: DelegateRequest) -> Result<Delegated, DelegateError> {
                finished_as("", DelegateEnd::Failed("boom".into()))
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
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let coordinator = DefaultCoordinator::with_delegator(Arc::new(AlwaysFails)).with_events(tx);

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
    /// unaffected: the case for embedders that give no `with_events`.
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
        let coordinator = DefaultCoordinator::with_delegator(Arc::new(FakeDelegator::new()));
        let result = coordinator
            .orchestrate(make_request(ctx, stages))
            .await
            .expect("orchestrate ok");
        assert!(!result.is_error);
    }

    /// Cancels the lead's turn from inside the first stage's member and
    /// hands back partial text, ending as `end` says.
    struct CancelsTheLead {
        lead: tokio_util::sync::CancellationToken,
        end: DelegateEnd,
        spawned: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait]
    impl Delegator for CancelsTheLead {
        async fn stop_member(&self, _: &TeamSeat) {}

        async fn delegate(&self, request: DelegateRequest) -> Result<Delegated, DelegateError> {
            let task = task_text(&request.prompt);
            self.spawned
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(task.clone());
            if task == "first" {
                self.lead.cancel();
            }
            finished_as(format!("partial-{task}"), self.end.clone())
        }
    }

    /// Whether the member says it was cancelled or not, the lead's call
    /// being cancelled ends the team there. The first stage aggregates on a
    /// model that panics if asked: a cancelled stage is not judged.
    #[tokio::test]
    async fn an_interrupted_team_starts_no_further_stage_and_ends_cancelled() {
        for end in [DelegateEnd::Completed, DelegateEnd::Cancelled] {
            let stage = |name: &str, prompt: &str| TeamStage {
                name: name.into(),
                agents: vec![TeamAgentSpec {
                    label: name.into(),
                    prompt: prompt.into(),
                    agent_type: None,
                }],
                aggregate: Some(AggregateMode::Best),
            };
            let (dir, ctx) = make_ctx();
            let delegator = Arc::new(CancelsTheLead {
                lead: ctx.cancel.clone(),
                end: end.clone(),
                spawned: Default::default(),
            });
            let registry = Arc::new(crate::registry::TeamRegistry::new());
            let coordinator = DefaultCoordinator::with_delegator(delegator.clone())
                .with_registry(registry.clone());
            let result = coordinator
                .orchestrate(make_request(
                    ctx,
                    vec![stage("one", "first"), stage("two", "second")],
                ))
                .await
                .expect("orchestrate returns a result");

            let spawned = delegator
                .spawned
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            assert_eq!(spawned, ["first"], "{end:?}: a later stage was started");
            assert!(
                result.is_error,
                "{end:?}: an interrupted team is not a success"
            );
            let team_dir = registry.list()[0].team_dir.clone();
            assert!(team_dir.starts_with(dir.path()));
            let state: serde_json::Value = serde_json::from_str(
                &std::fs::read_to_string(team_dir.join("state.json")).expect("state.json"),
            )
            .expect("state.json is json");
            assert_eq!(state["status"], "cancelled", "{end:?}: {state}");
            let log = std::fs::read_to_string(team_dir.join("events.jsonl")).unwrap();
            let last: serde_json::Value =
                serde_json::from_str(log.lines().last().unwrap()).unwrap();
            assert_eq!(last["kind"], "team_completed", "{end:?}: {log}");
            assert_eq!(last["status"], "cancelled", "{end:?}: {log}");
        }
    }

    /// A member still waiting for the stage's permit when the lead's call is
    /// cancelled is not started once the permit comes.
    #[tokio::test]
    async fn a_member_waiting_for_a_permit_is_not_started_after_a_cancel() {
        let (_dir, ctx) = make_ctx();
        let delegator = Arc::new(CancelsTheLead {
            lead: ctx.cancel.clone(),
            end: DelegateEnd::Cancelled,
            spawned: Default::default(),
        });
        let member = |label: &str| TeamAgentSpec {
            label: label.into(),
            prompt: label.into(),
            agent_type: None,
        };
        let stage = TeamStage {
            name: "one".into(),
            agents: vec![member("first"), member("second")],
            aggregate: None,
        };
        let mut config = EngineConfig::defaults_for("test");
        config.team_stage_concurrency = 1;
        let request = OrchestrateRequest {
            config: Arc::new(config),
            ..make_request(ctx, vec![stage])
        };
        let result = DefaultCoordinator::with_delegator(delegator.clone())
            .orchestrate(request)
            .await
            .expect("orchestrate returns a result");

        let spawned = delegator
            .spawned
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        assert_eq!(spawned, ["first"]);
        assert!(result.is_error);
    }

    /// Hangs until `cancel` fires, as a judge cut off mid-answer would.
    struct HangsUntilCancelled;

    #[async_trait]
    impl Model for HangsUntilCancelled {
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
            Ok(Box::new(futures::stream::pending()))
        }
    }

    /// Cancels the lead's call when asked to judge, then never answers.
    struct CancelsTheLeadWhenJudging {
        lead: tokio_util::sync::CancellationToken,
    }

    #[async_trait]
    impl Model for CancelsTheLeadWhenJudging {
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
            self.lead.cancel();
            Ok(Box::new(futures::stream::pending()))
        }
    }

    /// The last stage's members all finished but its judging was cut off by
    /// the lead's cancel: the team did not complete, and says so.
    #[tokio::test]
    async fn a_team_interrupted_while_its_last_stage_is_judged_ends_cancelled() {
        let (_dir, ctx) = make_ctx();
        let lead = ctx.cancel.clone();
        let stage = TeamStage {
            name: "only".into(),
            agents: ["a", "b"]
                .into_iter()
                .map(|label| TeamAgentSpec {
                    label: label.into(),
                    prompt: "0".into(),
                    agent_type: None,
                })
                .collect(),
            aggregate: Some(AggregateMode::Aggregate),
        };
        let registry = Arc::new(crate::registry::TeamRegistry::new());
        let request = OrchestrateRequest {
            model: Arc::new(CancelsTheLeadWhenJudging { lead }),
            ..make_request(ctx, vec![stage])
        };
        let result = DefaultCoordinator::with_delegator(Arc::new(FakeDelegator::new()))
            .with_registry(registry.clone())
            .orchestrate(request)
            .await
            .expect("orchestrate returns a result");

        assert!(result.is_error, "an interrupted team is not a success");
        let team_dir = registry.list()[0].team_dir.clone();
        let state: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(team_dir.join("state.json")).expect("state.json"),
        )
        .expect("state.json is json");
        assert_eq!(state["status"], "cancelled", "{state}");
    }

    /// A judge that would never answer stops with the lead's call, even when
    /// the model itself ignores the token.
    #[tokio::test]
    async fn aggregation_stops_with_the_leads_call() {
        let cancel = tokio_util::sync::CancellationToken::new();
        cancel.cancel();
        let sections = vec![
            ("a".to_string(), "one".to_string(), false),
            ("b".to_string(), "two".to_string(), false),
        ];
        let kept = aggregate(
            &HangsUntilCancelled,
            "m",
            AggregateMode::Best,
            "stage",
            &sections,
            None,
            &cancel,
        )
        .await;
        assert_eq!(kept, sections);
    }
}
