//! `RuleSetPermission` — the real `base::interface::permission::Permission`
//! implementation, wrapping `PermissionGate`/`RuleSet`.
//!
//! This is what makes `permission_rules` in `settings.json` and
//! `PermissionMode` (`BypassPermissions`/`DontAsk`/`Plan`/...) take effect on
//! real tool calls, and gives `Skill.allowed_tools` (see `SkillTool`)
//! somewhere to inject temporary Allow rules into via `add_rules`.

use crate::gate::PermissionGate;
use async_trait::async_trait;
use base::context::SessionState;
use base::interface::permission::{Permission, PermissionOutcome};
use base::permission::{PermissionDecision as GateDecision, PermissionMode};
#[cfg(test)]
use base::tool::InMemoryToolRegistry;
use base::tool::{ToolContext, ToolRegistry};
use std::path::Path;
use std::sync::{Arc, RwLock};
use tokio_util::sync::CancellationToken;

pub struct RuleSetPermission {
    gate: Arc<PermissionGate>,
    /// The registry `check` resolves tool names against.
    ///
    /// Interior-mutable because the registry this handler is *constructed*
    /// with is frequently not the registry the session ends up dispatching
    /// against — see `Permission::bind_tool_registry`, which swaps this to
    /// the real one once `Builder::build()` has finished populating it. A
    /// stale registry here is not a cosmetic problem: every tool missing from
    /// it falls into `check`'s unknown-tool branch.
    tools: RwLock<Arc<dyn ToolRegistry>>,
    /// The session's live state, once `bind_session_state` has supplied it.
    ///
    /// `None` (embedders that never bind) falls back to a throwaway
    /// `SessionState` carrying `permission_mode` below.
    /// When bound, `check` reads the *live* permission mode, so
    /// `EnterPlanMode`/`ExitPlanMode` actually move the gate.
    session: RwLock<Option<Arc<SessionState>>>,
    /// The mode to assume when no live `SessionState` is bound. Also the mode
    /// a freshly-bound session starts in — `Builder::build()` seeds
    /// `SessionState` from the same `Settings` value.
    permission_mode: PermissionMode,
    /// The sandbox settings a tool's own `check_permissions` reads.
    ///
    /// This is the only path that reaches a tool's own judgement in
    /// production, so a setting that governs one — `sandbox.deny_write` and
    /// `sandbox.allow_write` do — is inert unless it is carried here. The
    /// context built below is otherwise a bare one, which is how the write
    /// control list came to be configurable everywhere except where it is
    /// consulted.
    sandbox: base::tool::SandboxSettings,
}

impl RuleSetPermission {
    pub fn new(
        gate: Arc<PermissionGate>,
        tools: Arc<dyn ToolRegistry>,
        permission_mode: PermissionMode,
    ) -> Self {
        Self {
            gate,
            tools: RwLock::new(tools),
            session: RwLock::new(None),
            permission_mode,
            sandbox: Default::default(),
        }
    }

    /// Build one straight from `Settings`, wiring in both permission-rule
    /// tiers with their own `RuleSource` (see
    /// `crate::rule::rules_from_all_tiers`).
    ///
    /// This is the path that makes `permission_rules` and
    /// `settings.local.json` actually take effect, and every embedder that
    /// builds its own `Permission` should come through here rather than
    /// reassembling gate + ruleset + mode itself — `runtime::agent::Builder`
    /// takes an injected `Permission` and does not read `Settings` for rules,
    /// so an embedder that skips this silently gets no configured rules at
    /// all.
    ///
    /// `extra_rules` are appended after both tiers — session-scoped rules a
    /// caller adds on top (the daemon's `SessionOptions::permission_rules`),
    /// evaluated alongside the configured ones rather than replacing them.
    pub fn from_settings(
        settings: &base::interface::settings::Settings,
        mode: PermissionMode,
        tools: Arc<dyn ToolRegistry>,
        extra_rules: impl IntoIterator<Item = base::permission::PermissionRule>,
    ) -> Self {
        let mut rules = crate::rule::rules_from_all_tiers(settings);
        rules.extend(extra_rules);
        Self::new(
            Arc::new(PermissionGate::new(crate::ruleset::RuleSet::new(rules))),
            tools,
            mode,
        )
        .with_sandbox(base::tool::SandboxSettings {
            allow_read: settings.sandbox.allow_read.clone(),
            deny_read: settings.sandbox.deny_read.clone(),
            allowed_domains: settings.sandbox.allowed_domains.clone(),
            network_mode: settings.sandbox.network_mode,
            deny_write: settings.sandbox.deny_write.clone(),
            allow_write: settings.sandbox.allow_write.clone(),
            state_root: None,
            require_enforcement: settings.sandbox.require_enforcement,
            unattended_requires_network_policy: settings.sandbox.unattended_requires_network_policy,
        })
    }

    /// The sandbox settings the tools' own checks are judged against.
    pub fn with_sandbox(mut self, sandbox: base::tool::SandboxSettings) -> Self {
        self.sandbox = sandbox;
        self
    }

    /// The registry currently bound — snapshot of the `Arc`, so a concurrent
    /// `bind_tool_registry` can't hold the lock across an `.await`.
    fn tools(&self) -> Arc<dyn ToolRegistry> {
        self.tools.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// `check` against the session's own state, or against `delegate`'s in
    /// the stricter of the two sessions' modes, on the tool the delegate's
    /// call dispatches to — its own `StructuredOutput`, which the session may
    /// not have — under this session's rules.
    async fn decide(
        &self,
        tool_name: &str,
        tool_input: &serde_json::Value,
        cwd: &Path,
        session_id: &str,
        delegate: Option<&Arc<SessionState>>,
    ) -> PermissionOutcome {
        // Unknown tool name — fail closed.
        //
        // Not `Permit`, even though dispatch's own "Tool not found" error
        // would follow: that only holds while this registry *is* the dispatch
        // registry. If the binding is ever stale, permitting here would let
        // every per-session tool (`Skill`, `Agent`, `mcp__*`, ...) run
        // unchecked. A genuinely nonexistent tool surfaces as a prompt whose
        // message says so, and dispatch's own error follows if approved.
        let Some(tool) = delegate
            .and_then(|d| d.tool(tool_name))
            .or_else(|| self.tools().get(tool_name))
        else {
            return PermissionOutcome::Prompt {
                prompt_id: uuid::Uuid::new_v4().to_string(),
                message: format!(
                    "`{tool_name}` is not registered in this session's permission registry. \
                     It is either an unknown tool, or a tool that reached dispatch without \
                     being registered for permission checks — approve only if you expected it."
                ),
                paths: Vec::new(),
            };
        };

        let mut ctx = ToolContext::from_engine_ctx(cwd.to_path_buf(), CancellationToken::new());
        ctx.session_id = session_id.to_string();
        ctx.sandbox = self.sandbox.clone();
        // Prefer the session's live state (so a runtime `set_permission_mode`
        // — plan mode — is honored); fall back to a throwaway carrying the
        // construction-time mode when nothing was bound.
        let own = match self
            .session
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            Some(live) => live,
            None => Arc::new(
                SessionState::new(cwd.to_path_buf()).with_permission_mode(self.permission_mode),
            ),
        };
        let own_mode = own.permission_mode();
        (ctx.session, ctx.permission_mode) = match delegate {
            Some(delegate) => (
                delegate.clone(),
                own_mode.for_delegate(delegate.permission_mode()),
            ),
            None => (own, own_mode),
        };
        ctx.additional_writable_dirs = ctx.session.additional_writable_dirs.clone();
        ctx.dangerously_disable_sandbox = ctx.session.sandbox_disabled;

        match self.gate.check(tool.as_ref(), tool_input, &ctx).await {
            Ok(GateDecision::Allow { .. }) => PermissionOutcome::Permit,
            Ok(GateDecision::Deny { message, .. }) => PermissionOutcome::Deny { reason: message },
            Ok(GateDecision::Ask { message, .. }) => PermissionOutcome::Prompt {
                prompt_id: uuid::Uuid::new_v4().to_string(),
                message,
                paths: Vec::new(),
            },
            // Gate errors (bad tool input, etc.) are conservative — deny
            // rather than silently let a malformed call through.
            Err(e) => PermissionOutcome::Deny {
                reason: format!("permission check error: {e}"),
            },
        }
    }
}

#[async_trait]
impl Permission for RuleSetPermission {
    async fn check(
        &self,
        tool_name: &str,
        tool_input: &serde_json::Value,
        cwd: &Path,
        session_id: &str,
    ) -> PermissionOutcome {
        self.decide(tool_name, tool_input, cwd, session_id, None)
            .await
    }

    async fn check_for_delegate(
        &self,
        tool_name: &str,
        tool_input: &serde_json::Value,
        cwd: &Path,
        session_id: &str,
        delegate: &Arc<SessionState>,
    ) -> PermissionOutcome {
        self.decide(tool_name, tool_input, cwd, session_id, Some(delegate))
            .await
    }

    fn bind_tool_registry(&self, tools: Arc<dyn ToolRegistry>) {
        *self.tools.write().unwrap_or_else(|e| e.into_inner()) = tools;
    }

    fn bind_session_state(&self, session: Arc<SessionState>) {
        *self.session.write().unwrap_or_else(|e| e.into_inner()) = Some(session);
    }

    fn add_temporary_allow(&self, tool_name: &str, rule_content: Option<&str>) {
        self.gate.add_rules(vec![base::permission::PermissionRule {
            source: base::permission::RuleSource::Command,
            behavior: base::permission::RuleBehavior::Allow,
            tool_name: tool_name.to_string(),
            rule_content: rule_content.map(|s| s.to_string()),
        }]);
    }

    fn clear_temporary_allows(&self) {
        self.gate
            .remove_rules_by_source(base::permission::RuleSource::Command);
    }

    fn add_persistent_allow(&self, tool_name: &str, rule_content: Option<&str>) {
        // `RuleSource::Session` (50), not `Command` (45) — `Command`-sourced
        // rules get bulk-deleted by `clear_temporary_allows` (called from
        // `turn.rs` per-turn skill cleanup via `RuleSet::remove_by_source`)
        // whenever a skill goes inactive, and a "permit always" decision
        // from a user's interactive prompt answer must survive that
        // unrelated cleanup for the rest of the session.
        self.gate.add_rules(vec![base::permission::PermissionRule {
            source: base::permission::RuleSource::Session,
            behavior: base::permission::RuleBehavior::Allow,
            tool_name: tool_name.to_string(),
            rule_content: rule_content.map(|s| s.to_string()),
        }]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ruleset::RuleSet;
    use async_trait::async_trait as async_trait2;
    use base::error::ToolError;
    use base::tool::{PermissionDecision as ToolPermissionDecision, Tool};
    use serde_json::json;

    struct FakeTool {
        name: &'static str,
        read_only: bool,
    }

    #[async_trait2]
    impl Tool for FakeTool {
        fn name(&self) -> &str {
            self.name
        }
        fn input_schema(&self) -> serde_json::Value {
            json!({"type": "object"})
        }
        async fn prompt(&self, _: &base::tool::PromptContext) -> String {
            "fake".into()
        }
        fn is_read_only(&self, _: &serde_json::Value) -> bool {
            self.read_only
        }
        async fn check_permissions(
            &self,
            _: &serde_json::Value,
            _: &ToolContext,
        ) -> ToolPermissionDecision {
            ToolPermissionDecision::ask("?")
        }
        async fn call(
            &self,
            _: serde_json::Value,
            _: ToolContext,
            _: base::tool::ProgressSender,
        ) -> Result<base::tool::ToolResult, ToolError> {
            Ok(base::tool::ToolResult::text("ok"))
        }
    }

    fn registry_with(tool: FakeTool) -> Arc<InMemoryToolRegistry> {
        let reg = Arc::new(InMemoryToolRegistry::new());
        reg.register(Arc::new(tool));
        reg
    }

    /// Like `FakeTool`, but with a settable `permission_match_content` —
    /// needed to exercise `add_persistent_allow`'s content-aware rule
    /// (`FakeTool` always derives `None`, which only a blanket/no-content
    /// rule would match).
    struct ContentAwareFakeTool {
        name: &'static str,
        content: Option<&'static str>,
    }

    #[async_trait2]
    impl Tool for ContentAwareFakeTool {
        fn name(&self) -> &str {
            self.name
        }
        fn input_schema(&self) -> serde_json::Value {
            json!({"type": "object"})
        }
        async fn prompt(&self, _: &base::tool::PromptContext) -> String {
            "fake".into()
        }
        fn is_read_only(&self, _: &serde_json::Value) -> bool {
            false
        }
        async fn check_permissions(
            &self,
            _: &serde_json::Value,
            _: &ToolContext,
        ) -> ToolPermissionDecision {
            ToolPermissionDecision::ask("?")
        }
        fn permission_match_content(&self, _: &serde_json::Value) -> Option<String> {
            self.content.map(|s| s.to_string())
        }
        async fn call(
            &self,
            _: serde_json::Value,
            _: ToolContext,
            _: base::tool::ProgressSender,
        ) -> Result<base::tool::ToolResult, ToolError> {
            Ok(base::tool::ToolResult::text("ok"))
        }
    }

    fn registry_with_content_tool(tool: ContentAwareFakeTool) -> Arc<InMemoryToolRegistry> {
        let reg = Arc::new(InMemoryToolRegistry::new());
        reg.register(Arc::new(tool));
        reg
    }

    /// An unresolvable tool name must **not** be waved through.
    #[tokio::test]
    async fn unknown_tool_name_fails_closed() {
        let perm = RuleSetPermission::new(
            Arc::new(PermissionGate::empty()),
            Arc::new(InMemoryToolRegistry::new()),
            PermissionMode::Default,
        );
        let outcome = perm
            .check("Nonexistent", &json!({}), Path::new("/tmp"), "s1")
            .await;
        match outcome {
            PermissionOutcome::Prompt { ref message, .. } => {
                assert!(message.contains("Nonexistent"), "{message}");
            }
            other => panic!("expected a Prompt for an unregistered tool, got {other:?}"),
        }
    }

    /// Rebinding makes a tool registered *after* construction visible,
    /// which is exactly the `Builder::build()` ordering — the handler is
    /// built from a template, the session registry is populated afterwards.
    #[tokio::test]
    async fn bind_tool_registry_makes_late_registered_tools_checkable() {
        let perm = RuleSetPermission::new(
            Arc::new(PermissionGate::empty()),
            Arc::new(InMemoryToolRegistry::new()),
            PermissionMode::DontAsk,
        );
        // Before binding: not in the registry at all → fails closed as a prompt.
        assert!(matches!(
            perm.check("Skill", &json!({}), Path::new("/tmp"), "s1")
                .await,
            PermissionOutcome::Prompt { .. }
        ));

        let session_registry = registry_with(FakeTool {
            name: "Skill",
            read_only: false,
        });
        perm.bind_tool_registry(session_registry);

        // After binding: a real tool, so `DontAsk` with no matching rule denies.
        assert!(
            matches!(
                perm.check("Skill", &json!({}), Path::new("/tmp"), "s1")
                    .await,
                PermissionOutcome::Deny { .. }
            ),
            "after rebinding, the tool must go through real mode dispatch"
        );
    }

    /// A bound `SessionState` is read live, so switching the mode at
    /// runtime (what `EnterPlanMode`/`ExitPlanMode` do) moves the gate.
    #[tokio::test]
    async fn bound_session_state_supplies_the_live_permission_mode() {
        let tools = registry_with(FakeTool {
            name: "Write",
            read_only: false,
        });
        let perm = RuleSetPermission::new(
            Arc::new(PermissionGate::empty()),
            tools,
            PermissionMode::Default,
        );
        let session = Arc::new(
            SessionState::new(std::path::PathBuf::from("/tmp"))
                .with_permission_mode(PermissionMode::Default),
        );
        perm.bind_session_state(session.clone());

        assert!(matches!(
            perm.check("Write", &json!({}), Path::new("/tmp"), "s1")
                .await,
            PermissionOutcome::Prompt { .. }
        ));

        session.set_permission_mode(PermissionMode::Plan);
        assert!(
            matches!(
                perm.check("Write", &json!({}), Path::new("/tmp"), "s1")
                    .await,
                PermissionOutcome::Deny { .. }
            ),
            "entering plan mode must deny a non-read-only tool"
        );

        session.set_permission_mode(PermissionMode::BypassPermissions);
        assert!(matches!(
            perm.check("Write", &json!({}), Path::new("/tmp"), "s1")
                .await,
            PermissionOutcome::Permit
        ));
    }

    #[tokio::test]
    async fn a_delegate_is_judged_in_its_own_mode_held_to_plan_and_taken_along_by_bypass() {
        let perm = RuleSetPermission::new(
            Arc::new(PermissionGate::empty()),
            registry_with(FakeTool {
                name: "Write",
                read_only: false,
            }),
            PermissionMode::Default,
        );
        let session = Arc::new(
            SessionState::new(std::path::PathBuf::from("/tmp"))
                .with_permission_mode(PermissionMode::Default),
        );
        perm.bind_session_state(session.clone());
        let delegate = Arc::new(SessionState::new(std::path::PathBuf::from("/tmp")));
        let input = json!({});
        let judged =
            || perm.check_for_delegate("Write", &input, Path::new("/tmp"), "s1", &delegate);

        delegate.set_permission_mode(PermissionMode::Plan);
        assert!(matches!(judged().await, PermissionOutcome::Deny { .. }));
        delegate.set_permission_mode(PermissionMode::BypassPermissions);
        assert!(matches!(judged().await, PermissionOutcome::Permit));
        session.set_permission_mode(PermissionMode::BypassPermissions);
        delegate.set_permission_mode(PermissionMode::DontAsk);
        assert!(matches!(judged().await, PermissionOutcome::Permit));
        session.set_permission_mode(PermissionMode::Plan);
        delegate.set_permission_mode(PermissionMode::BypassPermissions);
        assert!(matches!(judged().await, PermissionOutcome::Deny { .. }));
        assert_eq!(session.permission_mode(), PermissionMode::Plan);
    }

    struct HandIn;

    #[async_trait2]
    impl Tool for HandIn {
        fn name(&self) -> &str {
            "StructuredOutput"
        }
        fn input_schema(&self) -> serde_json::Value {
            json!({"type": "object"})
        }
        async fn prompt(&self, _: &base::tool::PromptContext) -> String {
            "hand in".into()
        }
        fn is_read_only(&self, _: &serde_json::Value) -> bool {
            true
        }
        async fn check_permissions(
            &self,
            _: &serde_json::Value,
            _: &ToolContext,
        ) -> ToolPermissionDecision {
            ToolPermissionDecision::allow()
        }
        async fn call(
            &self,
            _: serde_json::Value,
            _: ToolContext,
            _: base::tool::ProgressSender,
        ) -> Result<base::tool::ToolResult, ToolError> {
            Ok(base::tool::ToolResult::text("ok"))
        }
    }

    #[tokio::test]
    async fn a_delegates_own_tool_is_judged_under_the_sessions_rules() {
        let delegate = Arc::new(SessionState::new(std::path::PathBuf::from("/tmp")));
        let delegate_tools: Arc<dyn ToolRegistry> = Arc::new(InMemoryToolRegistry::new());
        delegate_tools.register(Arc::new(HandIn));
        delegate.bind_tools(&delegate_tools);
        let judged = |perm: RuleSetPermission| {
            let delegate = delegate.clone();
            async move {
                perm.check_for_delegate(
                    "StructuredOutput",
                    &json!({}),
                    Path::new("/tmp"),
                    "s1",
                    &delegate,
                )
                .await
            }
        };

        let parent_without_it = || {
            RuleSetPermission::new(
                Arc::new(PermissionGate::empty()),
                Arc::new(InMemoryToolRegistry::new()),
                PermissionMode::Default,
            )
        };
        assert!(matches!(
            parent_without_it()
                .check("StructuredOutput", &json!({}), Path::new("/tmp"), "s1")
                .await,
            PermissionOutcome::Prompt { .. }
        ));
        assert!(matches!(
            judged(parent_without_it()).await,
            PermissionOutcome::Permit
        ));

        let denying = RuleSetPermission::new(
            Arc::new(PermissionGate::new(RuleSet::new(vec![
                base::permission::PermissionRule {
                    source: base::permission::RuleSource::UserSettings,
                    behavior: base::permission::RuleBehavior::Deny,
                    tool_name: "StructuredOutput".into(),
                    rule_content: None,
                },
            ]))),
            Arc::new(InMemoryToolRegistry::new()),
            PermissionMode::Default,
        );
        assert!(matches!(
            judged(denying).await,
            PermissionOutcome::Deny { .. }
        ));
    }

    /// A temporary allow carrying `rule_content` must only match calls whose
    /// content matches — a blanket grant would let a project-supplied skill's
    /// `allowed-tools` switch off confirmation wholesale.
    #[tokio::test]
    async fn temporary_allow_with_content_does_not_grant_everything() {
        let tools = registry_with_content_tool(ContentAwareFakeTool {
            name: "Bash",
            content: Some("cargo test"),
        });
        let perm = RuleSetPermission::new(
            Arc::new(PermissionGate::empty()),
            tools,
            PermissionMode::Default,
        );

        // Grant only `git status`; the tool reports `cargo test` as its
        // content, so the grant must not apply.
        perm.add_temporary_allow("Bash", Some("git status"));
        assert!(
            matches!(
                perm.check("Bash", &json!({}), Path::new("/tmp"), "s1")
                    .await,
                PermissionOutcome::Prompt { .. }
            ),
            "a content-scoped grant must not cover a different command"
        );

        perm.add_temporary_allow("Bash", Some("cargo test"));
        assert!(matches!(
            perm.check("Bash", &json!({}), Path::new("/tmp"), "s1")
                .await,
            PermissionOutcome::Permit
        ));
    }

    #[tokio::test]
    async fn bypass_mode_permits_without_rules() {
        let tools = registry_with(FakeTool {
            name: "Bash",
            read_only: false,
        });
        let perm = RuleSetPermission::new(
            Arc::new(PermissionGate::empty()),
            tools,
            PermissionMode::BypassPermissions,
        );
        let outcome = perm
            .check("Bash", &json!({}), Path::new("/tmp"), "s1")
            .await;
        assert!(matches!(outcome, PermissionOutcome::Permit));
    }

    #[tokio::test]
    async fn dontask_mode_denies_without_rules() {
        let tools = registry_with(FakeTool {
            name: "Bash",
            read_only: false,
        });
        let perm = RuleSetPermission::new(
            Arc::new(PermissionGate::empty()),
            tools,
            PermissionMode::DontAsk,
        );
        let outcome = perm
            .check("Bash", &json!({}), Path::new("/tmp"), "s1")
            .await;
        assert!(matches!(outcome, PermissionOutcome::Deny { .. }));
    }

    #[tokio::test]
    async fn default_mode_with_no_rule_prompts() {
        let tools = registry_with(FakeTool {
            name: "Bash",
            read_only: false,
        });
        let perm = RuleSetPermission::new(
            Arc::new(PermissionGate::empty()),
            tools,
            PermissionMode::Default,
        );
        let outcome = perm
            .check("Bash", &json!({}), Path::new("/tmp"), "s1")
            .await;
        assert!(matches!(outcome, PermissionOutcome::Prompt { .. }));
    }

    #[tokio::test]
    async fn allow_rule_permits_even_in_default_mode() {
        let tools = registry_with(FakeTool {
            name: "Bash",
            read_only: false,
        });
        let rules = RuleSet::new(vec![base::permission::PermissionRule {
            source: base::permission::RuleSource::UserSettings,
            behavior: base::permission::RuleBehavior::Allow,
            tool_name: "Bash".into(),
            rule_content: None,
        }]);
        let perm = RuleSetPermission::new(
            Arc::new(PermissionGate::new(rules)),
            tools,
            PermissionMode::Default,
        );
        let outcome = perm
            .check("Bash", &json!({}), Path::new("/tmp"), "s1")
            .await;
        assert!(matches!(outcome, PermissionOutcome::Permit));
    }

    #[tokio::test]
    async fn add_persistent_allow_permits_matching_content_without_prompting() {
        // The tool derives "git status" as its match content for any input
        // (a stand-in for e.g. Bash's command-line extraction) — matching
        // what a real "always allow" prompt answer would persist for the
        // specific call the user just approved.
        let tools = registry_with_content_tool(ContentAwareFakeTool {
            name: "Bash",
            content: Some("git status"),
        });
        let perm = RuleSetPermission::new(
            Arc::new(PermissionGate::empty()),
            tools,
            PermissionMode::Default,
        );

        // Before: no rule, Default mode with a real tool asks.
        let outcome = perm
            .check("Bash", &json!({}), Path::new("/tmp"), "s1")
            .await;
        assert!(matches!(outcome, PermissionOutcome::Prompt { .. }));

        perm.add_persistent_allow("Bash", Some("git status"));

        let outcome = perm
            .check("Bash", &json!({}), Path::new("/tmp"), "s1")
            .await;
        assert!(
            matches!(outcome, PermissionOutcome::Permit),
            "expected immediate Permit after add_persistent_allow, got {outcome:?}"
        );
    }

    #[tokio::test]
    async fn add_persistent_allow_survives_clear_temporary_allows() {
        // A Session-sourced persistent allow must NOT be wiped by `clear_temporary_allows`
        // (which only removes `Command`-sourced rules — see
        // `RuleSetPermission::clear_temporary_allows` and the skill
        // deactivation cleanup in `turn.rs` that calls it).
        let tools = registry_with(FakeTool {
            name: "Bash",
            read_only: false,
        });
        let perm = RuleSetPermission::new(
            Arc::new(PermissionGate::empty()),
            tools,
            PermissionMode::Default,
        );
        perm.add_persistent_allow("Bash", None);
        perm.clear_temporary_allows();

        let outcome = perm
            .check("Bash", &json!({}), Path::new("/tmp"), "s1")
            .await;
        assert!(
            matches!(outcome, PermissionOutcome::Permit),
            "persistent allow must survive clear_temporary_allows, got {outcome:?}"
        );
    }

    #[tokio::test]
    async fn a_session_allow_cannot_carve_an_exception_out_of_a_settings_deny() {
        let perm = RuleSetPermission::new(
            Arc::new(PermissionGate::new(RuleSet::new(vec![
                base::permission::PermissionRule {
                    source: base::permission::RuleSource::UserSettings,
                    behavior: base::permission::RuleBehavior::Deny,
                    tool_name: "Bash".into(),
                    rule_content: None,
                },
            ]))),
            registry_with_content_tool(ContentAwareFakeTool {
                name: "Bash",
                content: Some("git status"),
            }),
            PermissionMode::Default,
        );
        perm.add_persistent_allow("Bash", Some("git status"));

        let outcome = perm
            .check("Bash", &json!({}), Path::new("/tmp"), "s1")
            .await;
        assert!(
            matches!(outcome, PermissionOutcome::Deny { .. }),
            "{outcome:?}"
        );
    }

    #[tokio::test]
    async fn a_rule_in_the_project_tier_does_not_erase_the_users_global_denies() {
        let root = tempfile::tempdir().unwrap();
        let global = root.path().join("global");
        let project = root.path().join("project");
        for (dir, body) in [
            (
                &global,
                r#"{"permission_rules": [{"tool": "Bash(rm:*)", "action": "deny"}]}"#,
            ),
            (
                &project,
                r#"{"permission_rules": [{"tool": "Bash(ls)", "action": "allow"}]}"#,
            ),
        ] {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(dir.join("settings.json"), body).unwrap();
        }
        let settings = base::interface::settings::Settings::load(
            global,
            root.path().join("scene"),
            project,
            "code",
            "m",
        );

        let perm = RuleSetPermission::from_settings(
            &settings,
            PermissionMode::Default,
            registry_with_content_tool(ContentAwareFakeTool {
                name: "Bash",
                content: Some("rm -rf build"),
            }),
            [],
        );
        assert_eq!(
            perm.gate
                .rules()
                .iter()
                .map(|r| r.source)
                .collect::<Vec<_>>(),
            vec![
                base::permission::RuleSource::UserSettings,
                base::permission::RuleSource::ProjectSettings
            ]
        );
        let outcome = perm.check("Bash", &json!({}), root.path(), "s1").await;
        assert!(
            matches!(outcome, PermissionOutcome::Deny { .. }),
            "{outcome:?}"
        );
    }
}
