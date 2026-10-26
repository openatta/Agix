//! `Permission` trait — tool execution authorization.

use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Permission check outcome.
#[derive(Debug, Clone)]
pub enum PermissionOutcome {
    /// Allowed.
    Permit,
    /// Denied with a reason.
    Deny { reason: String },
    /// Needs upper-layer decision. Engine emits `AgentEvent::PermissionPrompt`
    /// and waits for `InputMessage::PermissionResponse`.
    Prompt {
        prompt_id: String,
        message: String,
        paths: Vec<PathBuf>,
    },
}

/// Tool execution permission interface.
///
/// Implementations decide whether a tool call is allowed.
/// Agix uses `RuleSetPermission` (Allow/Deny/Ask + RuleSet engine).
/// Jiandu uses `CwdPermission` (cwd boundary + callback hook).
#[async_trait]
pub trait Permission: Send + Sync {
    /// Check whether a tool call is permitted.
    async fn check(
        &self,
        tool_name: &str,
        tool_input: &serde_json::Value,
        cwd: &Path,
        session_id: &str,
    ) -> PermissionOutcome;

    /// Bind this handler to the tool registry the session actually dispatches
    /// against.
    ///
    /// A rule-engine implementation has to look a tool up by name to consult
    /// its `check_permissions`/`is_read_only`/`permission_match_content` —
    /// which means the registry it holds must be *the same one* dispatch uses.
    /// It usually is not at construction time: a daemon builds the handler
    /// from its pool-level template of self-contained built-ins, and
    /// `runtime::agent::Builder::build()` afterwards creates a fresh
    /// per-session registry and registers every engine-state tool
    /// (`Skill`/`Agent`/`Team*`/`WebSearch`/`EnterWorktree`/`Cron*`/`Task*`/
    /// `Import`/every `mcp__*` adapter) into *that*. Anything registered in
    /// that second step was invisible to the handler, and
    /// `RuleSetPermission`'s "unknown tool" branch let it straight through —
    /// so the entire MCP surface, `Skill`, and `Agent` ran with no permission
    /// check at all. `Builder::build()` now calls this right after it finishes
    /// populating the session registry.
    ///
    /// Default no-op: handlers that don't consult a registry (always-permit,
    /// bubble-to-parent, cwd-boundary) have nothing to rebind.
    fn bind_tool_registry(&self, _tools: Arc<dyn crate::tool::ToolRegistry>) {}

    /// Bind this handler to the session's live [`SessionState`], so mode
    /// changes made *during* the session are visible to permission decisions.
    ///
    /// [`SessionState::permission_mode`] is runtime-mutable — `EnterPlanMode`
    /// and `ExitPlanMode` flip it — but a handler that constructs a throwaway
    /// `SessionState` per check can only ever see the mode frozen at session
    /// construction, which made entering plan mode a no-op as far as the gate
    /// was concerned.
    ///
    /// Default no-op, same reasoning as [`Self::bind_tool_registry`].
    ///
    /// [`SessionState`]: crate::context::SessionState
    /// [`SessionState::permission_mode`]: crate::context::SessionState::permission_mode
    fn bind_session_state(&self, _session: Arc<crate::context::SessionState>) {}

    /// Temporarily allow `tool_name` without going through the normal
    /// Allow/Deny/Ask flow — used by a skill's `allowed_tools` frontmatter
    /// field while that skill is active. Default no-op: implementations
    /// with no backing rule engine (an always-permit or bubble-to-parent
    /// implementation, say) have nothing meaningful to add a temporary
    /// override on top of — `check` already decides everything for them.
    ///
    /// `rule_content` narrows the grant to a content pattern (a command
    /// prefix for `Bash`, a path glob for `Read`/`Write`, ...), matched the
    /// same way a `settings.json` rule is. `None` means "every call to this
    /// tool", which is a blanket grant — callers relaying a grant that
    /// originated in *project-controlled* data (a skill's frontmatter, say)
    /// must not pass `None`, because anyone who can write a file in the
    /// repository could then switch off confirmation for `Bash` wholesale.
    fn add_temporary_allow(&self, _tool_name: &str, _rule_content: Option<&str>) {}

    /// Clear every temporary allow `add_temporary_allow` added. Same
    /// no-op default as above.
    fn clear_temporary_allows(&self) {}

    /// Persist an Allow decision for `tool_name` beyond this single check —
    /// unlike `add_temporary_allow`, this is NOT cleared by
    /// `clear_temporary_allows` and is meant to last for the rest of the
    /// session (and, separately, the caller is responsible for any on-disk
    /// persistence — this method only affects in-memory/live behavior).
    /// Default no-op for implementations without a backing rule engine.
    fn add_persistent_allow(&self, _tool_name: &str, _rule_content: Option<&str>) {}
}
