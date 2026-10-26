//! `TeamCreate` tool — spawn multiple sub-agents with staged parallel execution.
//! Uses agent's own Coordinator + AgentTool.

use crate::coordinator::{Coordinator, DefaultCoordinator, OrchestrateRequest};
use async_trait::async_trait;
use base::context::EngineConfig;
use base::error::ToolError;
use base::interface::model::Model;
use base::tool::InMemoryToolRegistry;
use base::tool::PromptContext;
use base::tool::ToolContext;
use base::tool::{PermissionDecision, ValidationResult};
use base::tool::{ProgressSender, ToolResult};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

/// Longest team `name` accepted — generous for a human-chosen label, small
/// enough to keep `team_id`/directory names sane.
const TEAM_NAME_MAX_LEN: usize = 128;

/// `name` is later spliced verbatim into `team_id` (`format!("team-{name}-…")`)
/// and then `join`ed onto `.agix/teams/` to form the team's directory — so it
/// must not be able to escape that directory. Allowlisting a safe character
/// set (rather than denylisting `/`, `\`, `..`) also rules out things like
/// embedded null bytes or leading/trailing whitespace without naming them
/// individually.
fn validate_team_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("name must not be empty".to_string());
    }
    if name.len() > TEAM_NAME_MAX_LEN {
        return Err(format!(
            "name must be {TEAM_NAME_MAX_LEN} characters or fewer (got {})",
            name.len()
        ));
    }
    if name == "." || name == ".." {
        return Err(format!("name must not be '.' or '..': {name:?}"));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(format!(
            "name must contain only ASCII letters, digits, '-', '_', or '.' \
             (rejecting anything that could escape the team's directory, e.g. '/', '\\', '..'); got {name:?}"
        ));
    }
    Ok(())
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TeamCreateInput {
    pub name: String,
    #[serde(default)]
    pub stages: Option<Vec<TeamStage>>,
    #[serde(default)]
    pub agents: Vec<TeamAgentSpec>,
    #[serde(default)]
    pub scratchpad: Option<String>,
    /// The permission grant every agent in this call runs under — chosen
    /// once, for the whole team, not negotiated per tool call. Omit for the
    /// safe default (`Plan`: read-only tools only). Use `BypassPermissions`
    /// to let the team actually make changes, `DontAsk` to allow nothing
    /// beyond structurally-safe read-only tools, etc. — the same
    /// `PermissionMode` values `settings.json`'s own `permission_mode`
    /// uses. `Auto` and `Bubble` aren't accepted here (rejected at
    /// validation) — `Auto` needs a transcript classifier this call site
    /// doesn't have, and `Bubble` needs a lead that's actually available to
    /// answer mid-call, which isn't true for `TeamCreate`'s synchronous
    /// batch mode (see this tool's own prompt for why).
    #[serde(default)]
    pub permission_mode: Option<base::interface::settings::PermissionMode>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AggregateMode {
    Concat,
    Best,
    Aggregate,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TeamStage {
    pub name: String,
    pub agents: Vec<TeamAgentSpec>,
    #[serde(default)]
    pub aggregate: Option<AggregateMode>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TeamAgentSpec {
    pub label: String,
    pub prompt: String,
    #[serde(default)]
    pub agent_type: Option<String>,
}

pub struct TeamCreateTool {
    model: Arc<dyn Model>,
    config: Arc<EngineConfig>,
    parent_tools: Arc<InMemoryToolRegistry>,
    sub_tools: Arc<InMemoryToolRegistry>,
    coordinator: Box<dyn Coordinator>,
}

impl TeamCreateTool {
    pub fn new(
        model: Arc<dyn Model>,
        config: Arc<EngineConfig>,
        parent_tools: Arc<InMemoryToolRegistry>,
        sub_tools: Arc<InMemoryToolRegistry>,
    ) -> Self {
        Self {
            model,
            config,
            parent_tools,
            sub_tools,
            coordinator: Box::new(DefaultCoordinator::new()),
        }
    }

    pub fn with_spawner(
        model: Arc<dyn Model>,
        config: Arc<EngineConfig>,
        parent_tools: Arc<InMemoryToolRegistry>,
        sub_tools: Arc<InMemoryToolRegistry>,
        spawner: Arc<dyn base::interface::agent_spawner::AgentSpawner>,
        registry: Arc<crate::registry::TeamRegistry>,
        scene_id: String,
    ) -> Self {
        Self {
            model,
            config,
            parent_tools,
            sub_tools,
            coordinator: Box::new(
                DefaultCoordinator::with_agent_spawner(spawner)
                    .with_registry(registry)
                    .with_scene_id(scene_id),
            ),
        }
    }

    /// Attach the parent session's event channel so team-stage progress is
    /// emitted as stages start/finish (T-3) rather than only at the very end.
    /// Takes `&self` — the tool is registered before the session's event
    /// channel exists; see `Coordinator::set_event_sender`.
    pub fn set_event_sender(&self, tx: crate::coordinator::TeamEventSender) {
        self.coordinator.set_event_sender(tx);
    }

    /// Attach a telemetry handle so team lifecycle events reach the
    /// structured event stream — see `Coordinator::set_telemetry_handle`.
    pub fn set_telemetry_handle(&self, handle: telemetry::TelemetryHandle) {
        self.coordinator.set_telemetry_handle(handle);
    }
}

#[async_trait]
impl base::tool::Tool for TeamCreateTool {
    fn name(&self) -> &str {
        "TeamCreate"
    }
    fn description(&self) -> &str {
        "Create a team of sub-agents for multi-stage parallel task execution"
    }
    fn input_schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(TeamCreateInput)).unwrap_or(Value::Null)
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        false
    }
    fn is_read_only(&self, _: &Value) -> bool {
        false
    }
    async fn prompt(&self, _: &PromptContext) -> String {
        include_str!("tool.prompt.md").to_string()
    }
    async fn validate_input(&self, input: &Value, _: &ToolContext) -> ValidationResult {
        match serde_json::from_value::<TeamCreateInput>(input.clone()) {
            Ok(p) => match validate_team_name(&p.name) {
                Ok(()) => ValidationResult::Ok,
                Err(msg) => ValidationResult::err(msg, 1),
            },
            Err(e) => ValidationResult::err(format!("invalid input: {e}"), 2),
        }
    }
    async fn check_permissions(&self, _: &Value, _: &ToolContext) -> PermissionDecision {
        PermissionDecision::allow()
    }
    async fn call(
        &self,
        input: Value,
        ctx: ToolContext,
        _: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let inp: TeamCreateInput =
            serde_json::from_value(input).map_err(|e| ToolError::Validation(format!("{e}")))?;
        // Defense in depth: `validate_input` above is the real enforcement
        // point (wired into `PermissionGate::check`), but `call()` can also
        // be invoked directly (tests, alternate call paths), so the check
        // is repeated here rather than assumed.
        if let Err(msg) = validate_team_name(&inp.name) {
            return Err(ToolError::Validation(msg));
        }
        if matches!(
            inp.permission_mode,
            Some(base::interface::settings::PermissionMode::Auto)
                | Some(base::interface::settings::PermissionMode::Bubble)
        ) {
            return Err(ToolError::Validation(
                "permission_mode: \"auto\" and \"bubble\" are not valid here — \"auto\" needs \
                 a transcript classifier this call site doesn't have, and \"bubble\" needs an \
                 available lead, which a synchronous TeamCreate call doesn't have either"
                    .into(),
            ));
        }
        let stages = inp.stages.unwrap_or_else(|| {
            vec![TeamStage {
                name: "main".into(),
                agents: inp.agents,
                aggregate: None,
            }]
        });
        self.coordinator
            .orchestrate(OrchestrateRequest {
                model: self.model.clone(),
                config: self.config.clone(),
                parent_tools: self.parent_tools.clone(),
                sub_tools: self.sub_tools.clone(),
                stages,
                name: inp.name,
                scratchpad: inp.scratchpad,
                ctx,
                permission_mode: inp.permission_mode,
            })
            .await
    }
}

// ── TeamDelete ──

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TeamDeleteInput {
    /// Name of the team to delete.
    pub name: String,
}

pub struct TeamDeleteTool {
    coordinator: Box<dyn Coordinator>,
}

impl TeamDeleteTool {
    pub fn new(coordinator: Box<dyn Coordinator>) -> Self {
        Self { coordinator }
    }
}

#[async_trait]
impl base::tool::Tool for TeamDeleteTool {
    fn name(&self) -> &str {
        "TeamDelete"
    }
    fn description(&self) -> &str {
        "Delete a previously created team and clean up its resources"
    }
    fn input_schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(TeamDeleteInput)).unwrap_or(Value::Null)
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        false
    }
    fn is_read_only(&self, _: &Value) -> bool {
        false
    }
    async fn prompt(&self, _: &PromptContext) -> String {
        "Delete a team by name. This cancels any running sub-agents and \
         cleans up team-scoped resources (task lists, scratchpads). \
         Use when the team's work is complete or the user asks to stop."
            .into()
    }
    async fn validate_input(&self, _: &Value, _: &ToolContext) -> ValidationResult {
        ValidationResult::Ok
    }
    async fn check_permissions(&self, _: &Value, _: &ToolContext) -> PermissionDecision {
        PermissionDecision::allow()
    }
    async fn call(
        &self,
        input: Value,
        _ctx: ToolContext,
        _: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let inp: TeamDeleteInput =
            serde_json::from_value(input).map_err(|e| ToolError::Validation(format!("{e}")))?;
        if let Err(e) = self.coordinator.cleanup_team(&inp.name).await {
            return Ok(ToolResult::error_text(format!(
                "Failed to delete team: {e}"
            )));
        }
        Ok(ToolResult::text(format!("Team '{}' deleted.", inp.name)))
    }
}

// ── TeamList ──

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TeamListInput {}

pub struct TeamListTool {
    registry: Arc<crate::registry::TeamRegistry>,
}

impl TeamListTool {
    pub fn new(registry: Arc<crate::registry::TeamRegistry>) -> Self {
        Self { registry }
    }
}

#[async_trait]
impl base::tool::Tool for TeamListTool {
    fn name(&self) -> &str {
        "TeamList"
    }
    fn description(&self) -> &str {
        "List currently active teams created via TeamCreate, with each member's status"
    }
    fn input_schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(TeamListInput)).unwrap_or(Value::Null)
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }
    fn is_read_only(&self, _: &Value) -> bool {
        true
    }
    async fn prompt(&self, _: &PromptContext) -> String {
        "List teams created via TeamCreate that are still tracked (i.e. haven't been \
         cleaned up with TeamDelete yet), along with each member's label and last-known \
         status. Call this before TeamDelete if you're not certain of the exact team \
         name — don't guess a name or create a new placeholder team instead of looking \
         one up.\n\n\
         For persistent members (the Agent tool's team_name+name mode): there is no \
         push notification when one goes idle or finishes — this list is how you check. \
         An idle duration next to a member is how long it's been waiting since its last \
         reply; a long idle time doesn't mean anything went wrong, it just means nobody \
         has messaged it since."
            .into()
    }
    async fn validate_input(&self, _: &Value, _: &ToolContext) -> ValidationResult {
        ValidationResult::Ok
    }
    async fn check_permissions(&self, _: &Value, _: &ToolContext) -> PermissionDecision {
        PermissionDecision::allow()
    }
    async fn call(
        &self,
        _input: Value,
        _ctx: ToolContext,
        _: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let teams = self.registry.list();
        if teams.is_empty() {
            return Ok(ToolResult::text("No active teams.".to_string()));
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let mut out = String::new();
        for t in &teams {
            out.push_str(&format!("- \"{}\" ({} members)\n", t.name, t.members.len()));
            for m in &t.members {
                let idle_tag = match m.idle_since_secs {
                    Some(since) => format!(
                        " (idle {})",
                        format_duration_secs(now.saturating_sub(since))
                    ),
                    None => String::new(),
                };
                out.push_str(&format!(
                    "  - {} [{:?}]{}\n",
                    m.label, m.lifecycle, idle_tag
                ));
            }
        }
        Ok(ToolResult::text(out))
    }
}

fn format_duration_secs(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    }
}
// Old Tool bridge impl removed — unified on base::tool::Tool.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordinator::TeammateLifecycle;
    use crate::registry::{TeamInfo, TeamMemberInfo, TeamRegistry};
    use base::tool::Tool as _;

    #[test]
    fn format_duration_secs_picks_the_coarsest_useful_unit() {
        assert_eq!(format_duration_secs(5), "5s");
        assert_eq!(format_duration_secs(90), "1m");
        assert_eq!(format_duration_secs(3600), "1h0m");
        assert_eq!(format_duration_secs(3660), "1h1m");
    }

    #[tokio::test]
    async fn team_list_shows_idle_duration_for_persistent_members() {
        let registry = Arc::new(TeamRegistry::new());
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        registry.upsert(TeamInfo {
            team_id: "t".into(),
            name: "t".into(),
            created_at_secs: now,
            team_dir: std::path::PathBuf::new(),
            members: vec![
                TeamMemberInfo {
                    label: "idle-worker".into(),
                    lifecycle: TeammateLifecycle::Idle,
                    idle_since_secs: Some(now.saturating_sub(90)),
                },
                TeamMemberInfo {
                    label: "busy-worker".into(),
                    lifecycle: TeammateLifecycle::Active,
                    idle_since_secs: None,
                },
            ],
            permission_mode: None,
        });

        let tool = TeamListTool::new(registry);
        let result = tool
            .call(
                serde_json::json!({}),
                ToolContext::for_test(std::path::PathBuf::from("/tmp")),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        let text = match result.content {
            base::tool::ToolResultContent::Text(t) => t,
            _ => panic!("expected text"),
        };
        assert!(
            text.contains("idle-worker") && text.contains("(idle 1m)"),
            "expected idle duration shown for the idle member, got:\n{text}"
        );
        assert!(
            text.contains("busy-worker") && !text.contains("busy-worker [Active] (idle"),
            "an Active member must not show a stale/absent idle duration, got:\n{text}"
        );
    }

    #[test]
    fn validate_team_name_accepts_simple_names() {
        assert!(validate_team_name("refactor").is_ok());
        assert!(validate_team_name("refactor-42").is_ok());
        assert!(validate_team_name("a_b.c").is_ok());
    }

    #[test]
    fn validate_team_name_rejects_path_traversal() {
        for bad in &["..", "../escape", "a/../b", "a/b", "a\\b", "/etc/passwd"] {
            assert!(
                validate_team_name(bad).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn validate_team_name_rejects_empty_and_dot() {
        assert!(validate_team_name("").is_err());
        assert!(validate_team_name(".").is_err());
    }

    #[test]
    fn validate_team_name_rejects_control_and_whitespace_chars() {
        assert!(validate_team_name("a\0b").is_err());
        assert!(validate_team_name("a b").is_err());
        assert!(validate_team_name(" leading").is_err());
        assert!(validate_team_name("trailing ").is_err());
    }

    /// `TeamCreate::call` must reject a traversal-shaped name itself, not
    /// just document that `validate_input` would catch it — `call()` can be
    /// invoked directly (as every other test in this module does).
    #[tokio::test]
    async fn team_create_call_rejects_a_path_traversal_name() {
        let tool = TeamCreateTool::new(
            Arc::new(NoopModel),
            Arc::new(EngineConfig::defaults_for("test")),
            Arc::new(InMemoryToolRegistry::new()),
            Arc::new(InMemoryToolRegistry::new()),
        );
        let dir = tempfile::tempdir().unwrap();
        let input = serde_json::json!({
            "name": "../escape",
            "agents": [{"label": "a", "prompt": "hi"}],
        });
        let err = tool
            .call(
                input,
                ToolContext::for_test(dir.path().to_path_buf()),
                ProgressSender::noop("t"),
            )
            .await
            .expect_err("a traversal-shaped team name must be rejected");
        match err {
            ToolError::Validation(msg) => assert!(
                msg.contains("name"),
                "expected the error to explain the `name` problem, got: {msg}"
            ),
            other => panic!("expected ToolError::Validation, got {other:?}"),
        }
    }

    struct NoopModel;

    #[async_trait]
    impl base::interface::model::Model for NoopModel {
        fn api_type(&self) -> base::provider::ApiType {
            base::provider::ApiType::Anthropic
        }
        async fn stream(
            &self,
            _prompt_blocks: Vec<base::interface::prompt::PromptBlock>,
            _tools: Vec<base::interface::model::ToolDef>,
            _messages: Vec<base::interface::model::ModelMessage>,
            _params: base::interface::model::StreamParams,
            _cancel: tokio_util::sync::CancellationToken,
        ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError>
        {
            panic!("NoopModel::stream should not be called by this test");
        }
    }
}
