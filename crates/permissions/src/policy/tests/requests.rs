use super::*;
use crate::ruleset::RuleSet;
use async_trait::async_trait as async_trait2;
use base::context::SessionState;
use base::error::ToolError;
use base::interface::permission::Delegate;
use base::permission::PermissionMode;
use base::tool::Tool;
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
    async fn permission_facts(
        &self,
        _: &serde_json::Value,
        _: &base::tool::ToolContext,
    ) -> base::interface::permission::PermissionFacts {
        use base::interface::permission::{PermissionFacts, Reach, Subject};
        PermissionFacts::new(Subject::None, Reach::Unknown).with_question("?")
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

/// Like `FakeTool`, but with a settable subject —
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
    async fn permission_facts(
        &self,
        _: &serde_json::Value,
        _: &base::tool::ToolContext,
    ) -> base::interface::permission::PermissionFacts {
        use base::interface::permission::{PermissionFacts, Reach, Subject};
        PermissionFacts::new(
            { self.content.map(|s| s.to_string()) }.map_or(Subject::None, Subject::Opaque),
            Reach::Unknown,
        )
        .with_question("?")
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
    Arc::new(SessionState::for_tests("/tmp".into()).with_permission_mode(mode))
}

fn empty() -> Policy {
    Policy::empty()
}

async fn decide(
    perm: &Policy,
    tool: &Arc<dyn Tool>,
    session: &Arc<SessionState>,
) -> PermissionOutcome {
    let input = json!({});
    let (ctx, facts) = call_in(Some(tool), &input, session).await;
    perm.decide(&PermissionRequest::new(
        tool.name(),
        Some(tool),
        &input,
        &facts,
        &ctx,
        session,
    ))
    .await
}

#[tokio::test]
async fn a_call_to_a_tool_nobody_looked_up_is_asked_about_not_waved_through() {
    let input = json!({});
    let session = session(PermissionMode::BypassPermissions);
    let (ctx, facts) = call_in(None, &input, &session).await;
    let outcome = empty()
        .decide(&PermissionRequest::new(
            "Nonexistent",
            None,
            &input,
            &facts,
            &ctx,
            &session,
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
        PermissionOutcome::Permit { .. }
    ));
}

#[tokio::test]
async fn a_delegate_is_judged_in_its_own_mode_held_to_plan_and_taken_along_by_bypass() {
    let perm = empty();
    let write = fake("Write", false);
    let session = session(PermissionMode::Default);
    let delegate = Arc::new(SessionState::for_tests(std::path::PathBuf::from("/tmp")));
    let input = json!({});
    let judged = || {
        let (perm, write, input, session, delegate) = (&perm, &write, &input, &session, &delegate);
        async move {
            let (ctx, facts) = call_in(Some(write), input, delegate).await;
            let req = PermissionRequest::new("Write", Some(write), input, &facts, &ctx, session)
                .for_delegate(Delegate::of(delegate));
            perm.decide(&req).await
        }
    };

    delegate.set_permission_mode(PermissionMode::Plan);
    assert!(matches!(judged().await, PermissionOutcome::Deny { .. }));
    delegate.set_permission_mode(PermissionMode::BypassPermissions);
    assert!(matches!(judged().await, PermissionOutcome::Permit { .. }));
    session.set_permission_mode(PermissionMode::BypassPermissions);
    delegate.set_permission_mode(PermissionMode::DontAsk);
    assert!(matches!(judged().await, PermissionOutcome::Permit { .. }));
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
    async fn permission_facts(
        &self,
        _: &serde_json::Value,
        _: &base::tool::ToolContext,
    ) -> base::interface::permission::PermissionFacts {
        use base::interface::permission::{PermissionFacts, Reach, Subject};
        PermissionFacts::new(Subject::None, Reach::SessionOnly)
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
    let judged = |perm: Policy| {
        let (own, delegate, hand_in, input) = (
            own.clone(),
            delegate.clone(),
            hand_in.clone(),
            input.clone(),
        );
        async move {
            let (ctx, facts) = call_in(Some(&hand_in), &input, &delegate).await;
            perm.decide(
                &PermissionRequest::new(
                    "StructuredOutput",
                    Some(&hand_in),
                    &input,
                    &facts,
                    &ctx,
                    &own,
                )
                .for_delegate(Delegate::of(&delegate)),
            )
            .await
        }
    };

    assert!(matches!(
        judged(empty()).await,
        PermissionOutcome::Permit { .. }
    ));
    let denying = Policy::new(RuleSet::new(vec![PermissionRule {
        source: RuleSource::UserSettings,
        behavior: RuleBehavior::Deny,
        tool_name: "StructuredOutput".into(),
        rule_content: None,
    }]));
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
        PermissionOutcome::Permit { .. }
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
        PermissionOutcome::Permit { .. }
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
    let perm = Policy::new(RuleSet::new(vec![PermissionRule {
        source: RuleSource::UserSettings,
        behavior: RuleBehavior::Allow,
        tool_name: "Bash".into(),
        rule_content: None,
    }]));
    let outcome = decide(
        &perm,
        &fake("Bash", false),
        &session(PermissionMode::Default),
    )
    .await;
    assert!(matches!(outcome, PermissionOutcome::Permit { .. }));
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
        matches!(outcome, PermissionOutcome::Permit { .. }),
        "a session grant must hold past the turn's cleanup, got {outcome:?}"
    );
}

/// A person's answer covers the call they were shown: its text, not what
/// the text would match as a pattern.
#[tokio::test]
async fn a_session_grant_permits_only_the_same_text() {
    let perm = empty();
    let session = session(PermissionMode::Default);
    for (tool, allowed, other) in [
        ("Bash", "touch *.rs", "touch other.rs"),
        ("Bash", "git log:*", "git log --all"),
        ("WebFetch", "https://h.example/a?b", "https://h.example/axb"),
    ] {
        perm.grant(Grant::new(tool, Some(allowed.into()), GrantScope::Session))
            .await
            .unwrap();
        let same = decide(&perm, &content_tool(tool, allowed), &session).await;
        assert!(matches!(same, PermissionOutcome::Permit { .. }), "{same:?}");
        let different = decide(&perm, &content_tool(tool, other), &session).await;
        assert!(
            matches!(different, PermissionOutcome::Prompt { .. }),
            "{allowed} let {other} through: {different:?}"
        );
    }
    perm.clear(GrantScope::Session);
    let cleared = decide(&perm, &content_tool("Bash", "touch *.rs"), &session).await;
    assert!(
        matches!(cleared, PermissionOutcome::Prompt { .. }),
        "{cleared:?}"
    );
}

#[tokio::test]
async fn a_session_allow_cannot_carve_an_exception_out_of_a_settings_deny() {
    let perm = Policy::new(RuleSet::new(vec![PermissionRule {
        source: RuleSource::UserSettings,
        behavior: RuleBehavior::Deny,
        tool_name: "Bash".into(),
        rule_content: None,
    }]));
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

fn project_settings(root: &Path) -> base::settings::LoadedSettings {
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    base::settings::LoadedSettings::load(
        root.join("global"),
        root.join("scene"),
        project,
        "code",
        "m",
    )
}

/// The rule is written to the project's `settings.local.json`, in the
/// shape the next `LoadedSettings::load` reads back as a local-tier allow.
#[tokio::test]
async fn a_project_grant_is_written_where_the_next_session_reads_it() {
    let root = tempfile::tempdir().unwrap();
    let settings = project_settings(root.path());
    let local = settings.paths.local_settings_file().unwrap();
    let perm = Policy::from_settings(&settings, []);

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
        PermissionOutcome::Permit { .. }
    ));
    let reloaded = project_settings(root.path());
    assert!(
        crate::rule::rules_from(&reloaded)
            .iter()
            .all(|r| r.source != RuleSource::LocalSettings),
        "an exact allow is not a pattern rule"
    );
    let rules: Vec<_> = crate::rule::exact_allows_from(&reloaded)
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
    let settings = base::settings::LoadedSettings::load_without_project(
        root.path().join("global"),
        root.path().join("scene"),
        "code",
        "m",
    );
    assert_eq!(settings.paths.local_settings_file(), None);
    let perm = Policy::from_settings(&settings, []);

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
        PermissionOutcome::Permit { .. }
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
        PermissionOutcome::Permit { .. }
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
    let settings = base::settings::LoadedSettings::load(
        global,
        root.path().join("scene"),
        project,
        "code",
        "m",
    );

    let perm = Policy::from_settings(&settings, []);
    assert_eq!(
        perm.rules().iter().map(|r| r.source).collect::<Vec<_>>(),
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
) -> base::settings::LoadedSettings {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("settings.json"), user.to_string()).unwrap();
    base::settings::LoadedSettings::load(
        root.path().to_path_buf(),
        root.path().join("scene"),
        root.path().join("project"),
        "code",
        "m",
    )
    .with_managed(Some(base::managed_settings::ManagedSettings::from_value(
        "it", managed,
    )))
}

async fn verdict(
    settings: &base::settings::LoadedSettings,
    mode: PermissionMode,
    command: &'static str,
) -> PermissionOutcome {
    decide(
        &Policy::from_settings(settings, []),
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
    assert!(
        matches!(outcome, PermissionOutcome::Permit { .. }),
        "{outcome:?}"
    );

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
            matches!(outcome, PermissionOutcome::Deny { ref reason } if reason.message.contains("managed")),
            "{spelling}: {outcome:?}"
        );
        let outcome = verdict(&settings, mode, "chmod 600 key").await;
        assert!(
            matches!(outcome, PermissionOutcome::Prompt { .. }),
            "{spelling}: {outcome:?}"
        );
        let outcome = verdict(&settings, mode, "ls").await;
        assert!(
            matches!(outcome, PermissionOutcome::Permit { .. }),
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
        settings.values.permission_mode,
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
                matches!(outcome, PermissionOutcome::Deny { ref reason } if reason.message.contains("managed settings")),
                "{managed} {mode:?}: {outcome:?}"
            );
        }
    }
}
