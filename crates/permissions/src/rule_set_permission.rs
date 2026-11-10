//! `RuleSetPermission` — the engine's own permission: decides over
//! `PermissionGate`/`RuleSet` and keeps the session's grants as rules in the
//! same gate.
//!
//! This is what makes `permission_rules` in `settings.json` and
//! `PermissionMode` (`BypassPermissions`/`DontAsk`/`Plan`/...) take effect on
//! real tool calls, and where a skill's `allowed_tools` and a person's
//! "always allow" land.

use crate::gate::PermissionGate;
use async_trait::async_trait;
use base::interface::permission::{
    Grant, GrantError, GrantScope, PermissionGrants, PermissionOutcome, PermissionPolicy,
    PermissionRequest,
};
use base::permission::{
    PermissionDecision as GateDecision, PermissionRule, RuleBehavior, RuleSource,
};
use base::tool::ToolContext;
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub struct RuleSetPermission {
    gate: Arc<PermissionGate>,
    /// The sandbox settings a tool's own `check_permissions` reads.
    ///
    /// This is the only path that reaches a tool's own judgement in
    /// production, so a setting that governs one — `sandbox.deny_write` and
    /// `sandbox.allow_write` do — is inert unless it is carried here. The
    /// context built below is otherwise a bare one, which is how the write
    /// control list came to be configurable everywhere except where it is
    /// consulted.
    sandbox: base::tool::SandboxSettings,
    /// Where a [`GrantScope::Project`] grant is written. `None` — a session
    /// with no project — keeps it for the session only: the state root's
    /// `settings.local.json` is the global tier, and a rule written there would
    /// allow the call everywhere.
    local_settings_file: Option<PathBuf>,
}

impl RuleSetPermission {
    pub fn new(gate: Arc<PermissionGate>) -> Self {
        Self {
            gate,
            sandbox: Default::default(),
            local_settings_file: None,
        }
    }

    /// Build one straight from `Settings`, wiring in every permission-rule
    /// tier with its own `RuleSource` (see
    /// `crate::rule::rules_from_all_tiers`) and holding every call to the
    /// managed layer (`crate::gate::ManagedPolicy`).
    ///
    /// This is the path that makes `permission_rules` and
    /// `settings.local.json` actually take effect, and every embedder that
    /// builds its own permission should come through here rather than
    /// reassembling gate + ruleset itself — `runtime::agent::Builder` takes
    /// an injected `PermissionSet` and does not read `Settings` for rules, so
    /// an embedder that skips this silently gets no configured rules at all.
    ///
    /// `extra_rules` are appended after both tiers — session-scoped rules a
    /// caller adds on top (the daemon's `SessionOptions::permission_rules`),
    /// evaluated alongside the configured ones rather than replacing them.
    pub fn from_settings(
        settings: &base::settings::Settings,
        extra_rules: impl IntoIterator<Item = PermissionRule>,
    ) -> Self {
        let mut rules = crate::rule::rules_from_all_tiers(settings);
        rules.extend(extra_rules);
        Self::new(Arc::new(
            PermissionGate::new(crate::ruleset::RuleSet::new(rules))
                .with_managed(crate::gate::ManagedPolicy::from_settings(settings)),
        ))
        .with_sandbox(base::tool::SandboxSettings {
            allow_read: settings.sandbox.allow_read.clone(),
            deny_read: settings.sandbox.deny_read.clone(),
            allowed_domains: settings.sandbox.allowed_domains.clone(),
            network_mode: settings.sandbox.network_mode,
            deny_write: settings.sandbox.deny_write.clone(),
            allow_write: settings.sandbox.allow_write.clone(),
            state_root: Some(settings.paths.global_data_dir.clone()),
            require_enforcement: settings.sandbox.require_enforcement,
            unattended_requires_network_policy: settings.sandbox.unattended_requires_network_policy,
        })
        .with_local_settings_file(settings.paths.local_settings_file())
    }

    /// The sandbox settings the tools' own checks are judged against.
    pub fn with_sandbox(mut self, sandbox: base::tool::SandboxSettings) -> Self {
        self.sandbox = sandbox;
        self
    }

    pub fn with_local_settings_file(mut self, file: Option<PathBuf>) -> Self {
        self.local_settings_file = file;
        self
    }

    fn allow(&self, source: RuleSource, tool_name: &str, rule_content: Option<String>) {
        self.gate.add_rules(vec![PermissionRule {
            source,
            behavior: RuleBehavior::Allow,
            tool_name: tool_name.to_string(),
            rule_content,
        }]);
    }

    async fn write_project_rule(
        &self,
        path: &std::path::Path,
        grant: &Grant,
    ) -> Result<(), GrantError> {
        let not_persisted = |reason: String| GrantError::NotPersisted {
            path: path.to_path_buf(),
            reason,
        };
        let Some(rule) =
            crate::settings_patch::build_rule_string(&grant.tool_name, grant.content.as_deref())
        else {
            return Err(not_persisted(
                "the call has no content a rule could be scoped to".into(),
            ));
        };
        let target = path.to_path_buf();
        match tokio::task::spawn_blocking(move || {
            crate::settings_patch::append_permission_rule(
                &target,
                crate::settings_patch::AppendTarget::Allow,
                &rule,
            )
        })
        .await
        {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => Err(not_persisted(e.to_string())),
            Err(e) => Err(not_persisted(e.to_string())),
        }
    }
}

#[async_trait]
impl PermissionPolicy for RuleSetPermission {
    /// Judged against the session's own state, or against the delegate's in
    /// the stricter of the two sessions' modes, under this session's rules.
    async fn decide(&self, req: &PermissionRequest<'_>) -> PermissionOutcome {
        let tool_name = req.tool_name;
        // Not `Permit`, even though dispatch's own "Tool not found" error
        // would follow: a caller that asks before looking the tool up must
        // not be able to run it unchecked by naming it wrongly.
        let Some(tool) = req.tool else {
            return PermissionOutcome::Prompt {
                prompt_id: uuid::Uuid::new_v4().to_string(),
                message: format!(
                    "`{tool_name}` is not a tool this session has. It is either an unknown \
                     tool, or one that reached the permission check without being looked up \
                     — approve only if you expected it."
                ),
                paths: Vec::new(),
                managed: false,
            };
        };

        let own_mode = req.session.permission_mode();
        let (judged, permission_mode) = match req.delegate {
            Some(delegate) => (
                delegate.clone(),
                own_mode.for_delegate(delegate.permission_mode()),
            ),
            None => (req.session.clone(), own_mode),
        };
        // The machine the call will run on is the judged session's; one that
        // never had its providers bound runs on this one.
        let exec = judged.exec().unwrap_or_else(platform::local_providers);
        let mut ctx = ToolContext::from_engine_ctx_with(
            req.cwd.to_path_buf(),
            CancellationToken::new(),
            exec,
        );
        ctx.session_id = req.session_id.to_string();
        ctx.sandbox = self.sandbox.clone();
        ctx.session = judged;
        ctx.permission_mode = permission_mode;
        ctx.additional_writable_dirs = ctx.session.additional_writable_dirs.clone();
        ctx.dangerously_disable_sandbox = ctx.session.sandbox_disabled;

        match self.gate.check(tool.as_ref(), req.input, &ctx).await {
            Ok(GateDecision::Allow { .. }) => PermissionOutcome::Permit,
            Ok(GateDecision::Deny { message, .. }) => PermissionOutcome::Deny { reason: message },
            Ok(GateDecision::Ask {
                message,
                decision_reason,
            }) => PermissionOutcome::Prompt {
                prompt_id: uuid::Uuid::new_v4().to_string(),
                message,
                paths: Vec::new(),
                managed: self.gate.asked_by_managed(decision_reason.as_ref()),
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
impl PermissionGrants for RuleSetPermission {
    async fn grant(&self, grant: Grant) -> Result<(), GrantError> {
        if grant.scope == GrantScope::Turn {
            self.allow(RuleSource::Command, &grant.tool_name, grant.content);
            return Ok(());
        }
        // `Session`, not `Command`: `clear(Turn)` bulk-deletes
        // `Command`-sourced rules whenever a skill's turn ends, and an answer
        // a person gave must survive that.
        self.allow(
            RuleSource::Session,
            &grant.tool_name,
            grant
                .content
                .as_deref()
                .map(|content| crate::ruleset::literal_rule_content(&grant.tool_name, content)),
        );
        match (grant.scope, &self.local_settings_file) {
            (GrantScope::Project, Some(path)) => self.write_project_rule(path, &grant).await,
            _ => Ok(()),
        }
    }

    fn clear(&self, scope: GrantScope) {
        self.gate.remove_rules_by_source(match scope {
            GrantScope::Turn => RuleSource::Command,
            _ => RuleSource::Session,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ruleset::RuleSet;
    use async_trait::async_trait as async_trait2;
    use base::context::SessionState;
    use base::error::ToolError;
    use base::permission::PermissionMode;
    use base::tool::{PermissionDecision as ToolPermissionDecision, Tool};
    use serde_json::json;
    use std::path::Path;

    struct FakeTool {
        name: &'static str,
        read_only: bool,
    }

    #[async_trait2]
    impl Tool for FakeTool {
        fn name(&self) -> &str {
            self.name
        }
        fn spec(&self) -> base::tool::ToolSpec {
            base::tool::ToolSpec::new().read_only(self.read_only)
        }

        fn description(&self) -> &str {
            "fake tool (test tool)"
        }

        fn input_schema(&self, _: base::tool::PathStyle) -> serde_json::Value {
            json!({"type": "object"})
        }
        async fn guide(&self, _: &base::tool::PromptContext) -> Option<String> {
            Some("fake".into())
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

    fn fake(name: &'static str, read_only: bool) -> Arc<dyn Tool> {
        Arc::new(FakeTool { name, read_only })
    }

    /// Like `FakeTool`, but with a settable `permission_match_content` —
    /// a grant scoped to content only matches a tool that derives some.
    struct ContentAwareFakeTool {
        name: &'static str,
        content: Option<&'static str>,
    }

    #[async_trait2]
    impl Tool for ContentAwareFakeTool {
        fn name(&self) -> &str {
            self.name
        }
        fn spec(&self) -> base::tool::ToolSpec {
            base::tool::ToolSpec::new().read_only(false)
        }

        fn description(&self) -> &str {
            "content aware fake tool (test tool)"
        }

        fn input_schema(&self, _: base::tool::PathStyle) -> serde_json::Value {
            json!({"type": "object"})
        }
        async fn guide(&self, _: &base::tool::PromptContext) -> Option<String> {
            Some("fake".into())
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

    fn content_tool(name: &'static str, content: &'static str) -> Arc<dyn Tool> {
        Arc::new(ContentAwareFakeTool {
            name,
            content: Some(content),
        })
    }

    fn session(mode: PermissionMode) -> Arc<SessionState> {
        Arc::new(SessionState::new("/tmp".into()).with_permission_mode(mode))
    }

    fn empty() -> RuleSetPermission {
        RuleSetPermission::new(Arc::new(PermissionGate::empty()))
    }

    async fn decide(
        perm: &RuleSetPermission,
        tool: &Arc<dyn Tool>,
        session: &Arc<SessionState>,
    ) -> PermissionOutcome {
        let input = json!({});
        perm.decide(&PermissionRequest::new(
            tool.name(),
            Some(tool),
            &input,
            Path::new("/tmp"),
            "s1",
            session,
        ))
        .await
    }

    #[tokio::test]
    async fn a_call_to_a_tool_nobody_looked_up_is_asked_about_not_waved_through() {
        let input = json!({});
        let outcome = empty()
            .decide(&PermissionRequest::new(
                "Nonexistent",
                None,
                &input,
                Path::new("/tmp"),
                "s1",
                &session(PermissionMode::BypassPermissions),
            ))
            .await;
        match outcome {
            PermissionOutcome::Prompt { ref message, .. } => {
                assert!(message.contains("Nonexistent"), "{message}");
            }
            other => panic!("expected a Prompt for an unregistered tool, got {other:?}"),
        }
    }

    /// The request's session is read as it is now, so switching the mode at
    /// runtime (what `EnterPlanMode`/`ExitPlanMode` do) moves the gate.
    #[tokio::test]
    async fn the_requests_session_supplies_the_live_permission_mode() {
        let perm = empty();
        let write = fake("Write", false);
        let session = session(PermissionMode::Default);

        assert!(matches!(
            decide(&perm, &write, &session).await,
            PermissionOutcome::Prompt { .. }
        ));
        session.set_permission_mode(PermissionMode::Plan);
        assert!(
            matches!(
                decide(&perm, &write, &session).await,
                PermissionOutcome::Deny { .. }
            ),
            "entering plan mode must deny a non-read-only tool"
        );
        session.set_permission_mode(PermissionMode::BypassPermissions);
        assert!(matches!(
            decide(&perm, &write, &session).await,
            PermissionOutcome::Permit
        ));
    }

    #[tokio::test]
    async fn a_delegate_is_judged_in_its_own_mode_held_to_plan_and_taken_along_by_bypass() {
        let perm = empty();
        let write = fake("Write", false);
        let session = session(PermissionMode::Default);
        let delegate = Arc::new(SessionState::new(std::path::PathBuf::from("/tmp")));
        let input = json!({});
        let judged = || {
            let req = PermissionRequest::new(
                "Write",
                Some(&write),
                &input,
                Path::new("/tmp"),
                "s1",
                &session,
            )
            .for_delegate(&delegate);
            let perm = &perm;
            async move { perm.decide(&req).await }
        };

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
        fn spec(&self) -> base::tool::ToolSpec {
            base::tool::ToolSpec::new().read_only(true)
        }

        fn description(&self) -> &str {
            "hand in (test tool)"
        }

        fn input_schema(&self, _: base::tool::PathStyle) -> serde_json::Value {
            json!({"type": "object"})
        }
        async fn guide(&self, _: &base::tool::PromptContext) -> Option<String> {
            Some("hand in".into())
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

    /// A delegate's call carries the tool the delegate dispatches to — its own
    /// `StructuredOutput`, which the session does not have — and is judged
    /// under the session's rules.
    #[tokio::test]
    async fn a_delegates_own_tool_is_judged_under_the_sessions_rules() {
        let own = session(PermissionMode::Default);
        let delegate = session(PermissionMode::Default);
        let hand_in: Arc<dyn Tool> = Arc::new(HandIn);
        let input = json!({});
        let judged = |perm: RuleSetPermission| {
            let (own, delegate, hand_in, input) = (
                own.clone(),
                delegate.clone(),
                hand_in.clone(),
                input.clone(),
            );
            async move {
                perm.decide(
                    &PermissionRequest::new(
                        "StructuredOutput",
                        Some(&hand_in),
                        &input,
                        Path::new("/tmp"),
                        "s1",
                        &own,
                    )
                    .for_delegate(&delegate),
                )
                .await
            }
        };

        assert!(matches!(judged(empty()).await, PermissionOutcome::Permit));
        let denying = RuleSetPermission::new(Arc::new(PermissionGate::new(RuleSet::new(vec![
            PermissionRule {
                source: RuleSource::UserSettings,
                behavior: RuleBehavior::Deny,
                tool_name: "StructuredOutput".into(),
                rule_content: None,
            },
        ]))));
        assert!(matches!(
            judged(denying).await,
            PermissionOutcome::Deny { .. }
        ));
    }

    /// A turn grant carrying content must only match calls whose content
    /// matches — a blanket grant would let a project-supplied skill's
    /// `allowed-tools` switch off confirmation wholesale.
    #[tokio::test]
    async fn a_turn_grant_with_content_does_not_grant_everything() {
        let perm = empty();
        let bash = content_tool("Bash", "cargo test");
        let session = session(PermissionMode::Default);

        perm.grant(Grant::new(
            "Bash",
            Some("git status".into()),
            GrantScope::Turn,
        ))
        .await
        .unwrap();
        assert!(
            matches!(
                decide(&perm, &bash, &session).await,
                PermissionOutcome::Prompt { .. }
            ),
            "a content-scoped grant must not cover a different command"
        );

        perm.grant(Grant::new(
            "Bash",
            Some("cargo test".into()),
            GrantScope::Turn,
        ))
        .await
        .unwrap();
        assert!(matches!(
            decide(&perm, &bash, &session).await,
            PermissionOutcome::Permit
        ));

        perm.clear(GrantScope::Turn);
        assert!(matches!(
            decide(&perm, &bash, &session).await,
            PermissionOutcome::Prompt { .. }
        ));
    }

    #[tokio::test]
    async fn with_no_rule_bypass_permits_dontask_denies_and_default_prompts() {
        let perm = empty();
        let bash = fake("Bash", false);
        assert!(matches!(
            decide(&perm, &bash, &session(PermissionMode::BypassPermissions)).await,
            PermissionOutcome::Permit
        ));
        assert!(matches!(
            decide(&perm, &bash, &session(PermissionMode::DontAsk)).await,
            PermissionOutcome::Deny { .. }
        ));
        assert!(matches!(
            decide(&perm, &bash, &session(PermissionMode::Default)).await,
            PermissionOutcome::Prompt { .. }
        ));
    }

    #[tokio::test]
    async fn allow_rule_permits_even_in_default_mode() {
        let perm = RuleSetPermission::new(Arc::new(PermissionGate::new(RuleSet::new(vec![
            PermissionRule {
                source: RuleSource::UserSettings,
                behavior: RuleBehavior::Allow,
                tool_name: "Bash".into(),
                rule_content: None,
            },
        ]))));
        let outcome = decide(
            &perm,
            &fake("Bash", false),
            &session(PermissionMode::Default),
        )
        .await;
        assert!(matches!(outcome, PermissionOutcome::Permit));
    }

    #[tokio::test]
    async fn a_session_grant_permits_matching_content_without_prompting_and_survives_the_turn() {
        let perm = empty();
        let bash = content_tool("Bash", "git status");
        let session = session(PermissionMode::Default);
        assert!(matches!(
            decide(&perm, &bash, &session).await,
            PermissionOutcome::Prompt { .. }
        ));

        perm.grant(Grant::new(
            "Bash",
            Some("git status".into()),
            GrantScope::Session,
        ))
        .await
        .unwrap();
        perm.clear(GrantScope::Turn);

        let outcome = decide(&perm, &bash, &session).await;
        assert!(
            matches!(outcome, PermissionOutcome::Permit),
            "a session grant must hold past the turn's cleanup, got {outcome:?}"
        );
    }

    #[tokio::test]
    async fn a_session_allow_cannot_carve_an_exception_out_of_a_settings_deny() {
        let perm = RuleSetPermission::new(Arc::new(PermissionGate::new(RuleSet::new(vec![
            PermissionRule {
                source: RuleSource::UserSettings,
                behavior: RuleBehavior::Deny,
                tool_name: "Bash".into(),
                rule_content: None,
            },
        ]))));
        perm.grant(Grant::new(
            "Bash",
            Some("git status".into()),
            GrantScope::Session,
        ))
        .await
        .unwrap();

        let outcome = decide(
            &perm,
            &content_tool("Bash", "git status"),
            &session(PermissionMode::Default),
        )
        .await;
        assert!(
            matches!(outcome, PermissionOutcome::Deny { .. }),
            "{outcome:?}"
        );
    }

    fn project_settings(root: &Path) -> base::settings::Settings {
        let project = root.join("project");
        std::fs::create_dir_all(&project).unwrap();
        base::settings::Settings::load(
            root.join("global"),
            root.join("scene"),
            project,
            "code",
            "m",
        )
    }

    /// The rule is written to the project's `settings.local.json`, in the
    /// shape the next `Settings::load` reads back as a local-tier allow.
    #[tokio::test]
    async fn a_project_grant_is_written_where_the_next_session_reads_it() {
        let root = tempfile::tempdir().unwrap();
        let settings = project_settings(root.path());
        let local = settings.paths.local_settings_file().unwrap();
        let perm = RuleSetPermission::from_settings(&settings, []);

        perm.grant(Grant::new(
            "Probe",
            Some("git status".into()),
            GrantScope::Project,
        ))
        .await
        .unwrap();

        assert!(matches!(
            decide(
                &perm,
                &content_tool("Probe", "git status"),
                &session(PermissionMode::Default)
            )
            .await,
            PermissionOutcome::Permit
        ));
        let reloaded = project_settings(root.path());
        let rules: Vec<_> = crate::rule::rules_from_all_tiers(&reloaded)
            .into_iter()
            .filter(|r| r.source == RuleSource::LocalSettings)
            .collect();
        assert_eq!(rules.len(), 1, "{rules:?} in {}", local.display());
        assert_eq!(rules[0].tool_name, "Probe");
        assert_eq!(rules[0].rule_content.as_deref(), Some("git status"));
        assert_eq!(rules[0].behavior, RuleBehavior::Allow);
        assert!(
            !local.with_file_name(base::settings::SETTINGS_FILE).exists(),
            "must not touch the shared/committed settings.json tier"
        );
    }

    /// Without a project there is no `settings.local.json` of its own; the
    /// state root's is the global tier. The grant is kept for the session.
    #[tokio::test]
    async fn a_project_grant_without_a_project_is_kept_for_the_session() {
        let root = tempfile::tempdir().unwrap();
        let settings = base::settings::Settings::load_without_project(
            root.path().join("global"),
            root.path().join("scene"),
            "code",
            "m",
        );
        assert_eq!(settings.paths.local_settings_file(), None);
        let perm = RuleSetPermission::from_settings(&settings, []);

        perm.grant(Grant::new(
            "Probe",
            Some("git status".into()),
            GrantScope::Project,
        ))
        .await
        .unwrap();

        assert!(matches!(
            decide(
                &perm,
                &content_tool("Probe", "git status"),
                &session(PermissionMode::Default)
            )
            .await,
            PermissionOutcome::Permit
        ));
        assert!(
            !root
                .path()
                .join("global")
                .join(base::settings::SETTINGS_LOCAL_FILE)
                .exists(),
            "a rule in the global tier would allow the call everywhere"
        );
    }

    /// The write takes the settings directory's lock, which another writer may
    /// hold. Done on the async thread, it stalls every task on that thread for
    /// as long as the other writer takes; here the other writer holds it until
    /// this thread shows it is still free.
    #[tokio::test(flavor = "current_thread")]
    async fn a_project_grant_writes_to_disk_off_the_async_thread() {
        let root = tempfile::tempdir().unwrap();
        let file = root
            .path()
            .join(".agix")
            .join(base::settings::SETTINGS_LOCAL_FILE);
        let perm = Arc::new(empty().with_local_settings_file(Some(file.clone())));

        let dir = root.path().join(".agix");
        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let holder = std::thread::spawn(move || {
            let _held = base::settings_file::SettingsDir::lock(&dir).unwrap();
            locked_tx.send(()).unwrap();
            release_rx
                .recv_timeout(std::time::Duration::from_secs(10))
                .is_ok()
        });
        locked_rx.recv().unwrap();

        let granting = tokio::spawn({
            let perm = perm.clone();
            async move {
                perm.grant(Grant::new(
                    "Bash",
                    Some("git status".into()),
                    GrantScope::Project,
                ))
                .await
            }
        });
        // Parks this thread's scheduler, so the grant above runs first.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let _ = release_tx.send(());

        assert!(
            holder.join().unwrap(),
            "the grant blocked the async thread on the settings lock"
        );
        granting.await.unwrap().unwrap();
        let written = std::fs::read_to_string(&file).unwrap();
        assert!(written.contains("Bash(git status)"), "{written}");
    }

    #[tokio::test]
    async fn a_project_grant_with_nothing_to_scope_it_says_it_was_not_written() {
        let root = tempfile::tempdir().unwrap();
        let file = root
            .path()
            .join(".agix")
            .join(base::settings::SETTINGS_LOCAL_FILE);
        let perm = empty().with_local_settings_file(Some(file.clone()));

        let kept = perm
            .grant(Grant::new("Probe", None, GrantScope::Project))
            .await;

        assert!(
            matches!(kept, Err(GrantError::NotPersisted { ref path, .. }) if *path == file),
            "{kept:?}"
        );
        assert!(!file.exists());
        assert!(matches!(
            decide(
                &perm,
                &fake("Probe", false),
                &session(PermissionMode::Default)
            )
            .await,
            PermissionOutcome::Permit
        ));
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
        let settings =
            base::settings::Settings::load(global, root.path().join("scene"), project, "code", "m");

        let perm = RuleSetPermission::from_settings(&settings, []);
        assert_eq!(
            perm.gate
                .rules()
                .iter()
                .map(|r| r.source)
                .collect::<Vec<_>>(),
            vec![RuleSource::UserSettings, RuleSource::ProjectSettings]
        );
        let outcome = decide(
            &perm,
            &content_tool("Bash", "rm -rf build"),
            &session(PermissionMode::Default),
        )
        .await;
        assert!(
            matches!(outcome, PermissionOutcome::Deny { .. }),
            "{outcome:?}"
        );
    }

    fn with_managed(
        user: serde_json::Value,
        managed: serde_json::Value,
    ) -> base::settings::Settings {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("settings.json"), user.to_string()).unwrap();
        let mut settings = base::settings::Settings::load(
            root.path().to_path_buf(),
            root.path().join("scene"),
            root.path().join("project"),
            "code",
            "m",
        );
        settings.apply_managed(Some(base::managed_settings::ManagedSettings::from_value(
            "it", managed,
        )));
        settings
    }

    async fn verdict(
        settings: &base::settings::Settings,
        mode: PermissionMode,
        command: &'static str,
    ) -> PermissionOutcome {
        decide(
            &RuleSetPermission::from_settings(settings, []),
            &content_tool("Bash", command),
            &session(mode),
        )
        .await
    }

    #[tokio::test]
    async fn a_managed_deny_overrides_a_users_allow_and_a_managed_allow_no_users_deny() {
        let settings = with_managed(
            json!({"permission_rules": [{"tool": "Bash", "action": "allow"}]}),
            json!({"permission_rules": [{"tool": "Bash(rm:*)", "action": "deny"}]}),
        );
        let outcome = verdict(&settings, PermissionMode::Default, "rm -rf build").await;
        assert!(
            matches!(outcome, PermissionOutcome::Deny { .. }),
            "{outcome:?}"
        );
        let outcome = verdict(&settings, PermissionMode::Default, "ls").await;
        assert!(matches!(outcome, PermissionOutcome::Permit), "{outcome:?}");

        let settings = with_managed(
            json!({"permission_rules": [{"tool": "Bash(rm:*)", "action": "deny"}]}),
            json!({"permission_rules": [{"tool": "Bash", "action": "allow"}]}),
        );
        let outcome = verdict(&settings, PermissionMode::Default, "rm -rf build").await;
        assert!(
            matches!(outcome, PermissionOutcome::Deny { .. }),
            "{outcome:?}"
        );
    }

    #[tokio::test]
    async fn bypass_and_its_aliases_still_answer_to_the_managed_deny_and_ask() {
        let settings = with_managed(
            json!({"permission_rules": [{"tool": "Bash(ls)", "action": "deny"}]}),
            json!({"permission_rules": [
                {"tool": "Bash(rm:*)", "action": "deny"},
                {"tool": "Bash(chmod:*)", "action": "ask"},
            ]}),
        );
        for spelling in ["bypassPermissions", "auto", "yolo"] {
            let mode: PermissionMode = serde_json::from_value(json!(spelling)).unwrap();
            let outcome = verdict(&settings, mode, "rm -rf build").await;
            assert!(
                matches!(outcome, PermissionOutcome::Deny { ref reason } if reason.contains("managed")),
                "{spelling}: {outcome:?}"
            );
            let outcome = verdict(&settings, mode, "chmod 600 key").await;
            assert!(
                matches!(outcome, PermissionOutcome::Prompt { .. }),
                "{spelling}: {outcome:?}"
            );
            let outcome = verdict(&settings, mode, "ls").await;
            assert!(
                matches!(outcome, PermissionOutcome::Permit),
                "{spelling}: a user's deny is not consulted under bypass: {outcome:?}"
            );
        }
    }

    #[tokio::test]
    async fn disabling_bypass_judges_a_bypass_session_as_default() {
        let settings = with_managed(
            json!({"permission_mode": "bypassPermissions"}),
            json!({"disable_bypass_permissions_mode": true}),
        );
        assert_eq!(
            settings.permission_mode,
            base::settings::PermissionMode::Default
        );
        let outcome = verdict(&settings, PermissionMode::BypassPermissions, "touch x").await;
        assert!(
            matches!(outcome, PermissionOutcome::Prompt { .. }),
            "{outcome:?}"
        );
    }

    #[tokio::test]
    async fn a_managed_layer_that_cannot_be_honoured_refuses_every_call() {
        for managed in [
            json!({"permission_mode": "plan"}),
            json!({"permission_rules": [{"tool": "Bash(rm:*", "action": "deny"}]}),
        ] {
            let settings = with_managed(json!({}), managed.clone());
            for mode in [PermissionMode::BypassPermissions, PermissionMode::Default] {
                let outcome = verdict(&settings, mode, "ls").await;
                assert!(
                    matches!(outcome, PermissionOutcome::Deny { ref reason } if reason.contains("managed settings")),
                    "{managed} {mode:?}: {outcome:?}"
                );
            }
        }
    }

    struct FilesystemProbe(std::sync::Mutex<Vec<Arc<dyn base::interface::exec::FileSystem>>>);

    #[async_trait2]
    impl Tool for FilesystemProbe {
        fn name(&self) -> &str {
            "FilesystemProbe"
        }
        fn spec(&self) -> base::tool::ToolSpec {
            base::tool::ToolSpec::new()
        }

        fn description(&self) -> &str {
            "filesystem probe (test tool)"
        }

        fn input_schema(&self, _: base::tool::PathStyle) -> serde_json::Value {
            json!({"type": "object"})
        }
        async fn check_permissions(
            &self,
            _: &serde_json::Value,
            ctx: &ToolContext,
        ) -> ToolPermissionDecision {
            self.0.lock().unwrap().push(ctx.exec.filesystem.clone());
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

    /// A call is judged on the providers of the session it runs in: the
    /// session's for its own calls, the delegate's for a delegate's.
    #[tokio::test]
    async fn a_call_is_judged_on_the_providers_of_the_session_it_runs_in() {
        let probe = Arc::new(FilesystemProbe(Default::default()));
        let tool: Arc<dyn Tool> = probe.clone();
        let perm = empty();
        let own = Arc::new(SessionState::new("/tmp".into()));
        let own_exec = base::interface::exec::ExecProviders::in_process();
        own.bind_exec(own_exec.clone());
        let delegate = Arc::new(SessionState::new("/tmp".into()));
        let delegate_exec = base::interface::exec::ExecProviders::in_process();
        delegate.bind_exec(delegate_exec.clone());
        let input = json!({});
        let req = PermissionRequest::new(
            "FilesystemProbe",
            Some(&tool),
            &input,
            Path::new("/tmp"),
            "s1",
            &own,
        );

        perm.decide(&req).await;
        perm.decide(&req.for_delegate(&delegate)).await;

        let seen = probe.0.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert!(Arc::ptr_eq(&seen[0], &own_exec.filesystem));
        assert!(Arc::ptr_eq(&seen[1], &delegate_exec.filesystem));
    }
}
