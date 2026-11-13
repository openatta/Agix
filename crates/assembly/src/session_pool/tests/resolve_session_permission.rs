//! `resolve_session_permission_tests` 原样搬来的用例。

use super::*;
use crate::session_pool::parts::factory::resolve_session_permission;
use base::settings::Divergence;

struct AllowAllTestPermission;
#[async_trait::async_trait]
impl base::interface::permission::PermissionPolicy for AllowAllTestPermission {
    async fn decide(
        &self,
        _req: &base::interface::permission::PermissionRequest<'_>,
    ) -> base::interface::permission::PermissionOutcome {
        base::interface::permission::PermissionOutcome::Permit {
            basis: base::interface::permission::Basis::Bypass,
            profile: Default::default(),
            obligations: Default::default(),
        }
    }
}

fn defaults() -> base::settings::LoadedSettings {
    base::settings::LoadedSettings::new(
        Settings::defaults_for("claude-sonnet-4-6"),
        base::settings::PathSettings::default(),
    )
}

/// Everything the resolver needs beyond the settings/options under test,
/// with the host having supplied its own permission.
fn resolve(
    settings: &base::settings::LoadedSettings,
    options: Option<&SessionOptions>,
) -> (
    Arc<dyn base::interface::permission::PermissionPolicy>,
    Arc<dyn base::interface::permission::PermissionPolicy>,
) {
    let host = PermissionSet::without_grants(Arc::new(AllowAllTestPermission));
    let mode = effective_permission_mode(settings, options);
    let resolved = resolve_session_permission(Some(&host), settings, mode, options);
    (host.policy, resolved.policy)
}

/// With nothing configured, a session does not get the allow-all
/// instance.
#[test]
fn nothing_configured_builds_a_real_asking_permission() {
    let (bypass, resolved) = resolve(&defaults(), None);
    assert!(
        !Arc::ptr_eq(&bypass, &resolved),
        "the daemon default is no longer allow-all — it must build a real asking permission"
    );

    let opts = SessionOptions::default();
    let (bypass, resolved) = resolve(&defaults(), Some(&opts));
    assert!(
        !Arc::ptr_eq(&bypass, &resolved),
        "an options object that simply omits permission_mode must not re-enable allow-all"
    );
}

#[tokio::test]
async fn a_bypass_session_gets_the_hosts_permission_or_else_a_policy_that_consults_no_rule() {
    use base::interface::permission::PermissionOutcome;
    let mut settings = defaults();
    settings.values.permission_mode = base::settings::PermissionMode::BypassPermissions;
    settings.rules = vec![base::settings::LoadedRule::new(
        base::settings::PermissionRule {
            tool: "Bash".into(),
            action: base::settings::PermissionAction::Deny,
        },
        base::interface::config_source::Tier::Project,
        false,
    )];

    let (host, resolved) = resolve(&settings, None);
    assert!(
        Arc::ptr_eq(&host, &resolved),
        "a host that supplied its own permission gets that very instance"
    );

    let tools = Arc::new(base::tool::InMemoryToolRegistry::new());
    tools::register_builtin_tools(&tools);
    let bash = tools.get("Bash");
    let mode = effective_permission_mode(&settings, None);
    let gate = resolve_session_permission(None, &settings, mode, None);
    let cwd = std::env::temp_dir();
    let session = Arc::new(
        base::context::SessionState::for_tests(cwd.clone()).with_permission_mode(mode.into()),
    );
    let bash_says = |input: serde_json::Value| {
        let (gate, bash, cwd, session) = (gate.clone(), bash.clone(), cwd.clone(), session.clone());
        async move {
            gate.policy
                .decide(&base::interface::permission::PermissionRequest::new(
                    "Bash",
                    bash.as_ref(),
                    &input,
                    &cwd,
                    "s",
                    &session,
                ))
                .await
        }
    };
    assert!(matches!(
        bash_says(serde_json::json!({"command": "ls"})).await,
        PermissionOutcome::Permit { .. }
    ));
    // Input validation is the runtime's, once, before the policy is asked;
    // under bypass it still refuses what the tool rejects.
    let blank = serde_json::json!({"command": "  "});
    let ctx = base::tool::ToolContext::for_test_with(
        cwd.clone(),
        base::interface::exec::ExecProviders::in_process(),
    );
    assert!(bash
        .as_ref()
        .unwrap()
        .validate_input(&blank, &ctx)
        .await
        .is_err());
}

#[test]
fn a_session_clock_is_the_hosts_start_else_the_recordings_else_the_machines() {
    let date = |env: Option<Arc<dyn base::interface::environment::Environment>>| {
        env.map(|e| e.now().date().to_string())
    };
    let asked = SessionOptions {
        clock: Some(crate::session_pool::api::ClockOptions {
            start: "2026-10-28T09:00:00Z".into(),
        }),
        ..Default::default()
    };
    assert_eq!(
        date(session_clock(Some(&asked), None, "s")),
        Some("2026-10-28".into())
    );

    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("rec")).unwrap();
    std::fs::write(
        root.path().join("rec/calls.jsonl"),
        "{\"type\":\"recording\",\"version\":1,\"name\":\"rec\",\"session_id\":\"s0\",\
             \"created_at\":1761955200000,\"engine_version\":\"x\"}\n",
    )
    .unwrap();
    let recorder = |mode, name: Option<&str>| RecorderConfig {
        mode,
        name: name.map(str::to_string),
        root: root.path().to_path_buf(),
        on_divergence: Divergence::Warn,
    };
    let replay = recorder(RecorderMode::Replay, Some("rec"));
    assert_eq!(
        date(session_clock(None, Some(&replay), "s")),
        Some("2025-11-01".into())
    );
    assert_eq!(
        date(session_clock(Some(&asked), Some(&replay), "s")),
        Some("2026-10-28".into()),
        "what the host asked for wins"
    );
    assert!(session_clock(
        None,
        Some(&recorder(RecorderMode::Record, Some("rec"))),
        "s"
    )
    .is_none());
    assert!(
        session_clock(None, Some(&recorder(RecorderMode::Replay, None)), "no-such").is_none(),
        "no recording, no shift"
    );
    let garbled = SessionOptions {
        clock: Some(crate::session_pool::api::ClockOptions {
            start: "yesterday".into(),
        }),
        ..Default::default()
    };
    assert!(session_clock(Some(&garbled), None, "s").is_none());
}

/// A client asking for a *more permissive* mode than the daemon is
/// configured with gets clamped, not obeyed.
#[test]
fn session_options_cannot_loosen_an_asking_settings_default() {
    let opts = SessionOptions {
        permission_mode: Some(base::settings::PermissionMode::BypassPermissions),
        ..Default::default()
    };
    let (bypass, resolved) = resolve(&defaults(), Some(&opts));
    assert!(
        !Arc::ptr_eq(&bypass, &resolved),
        "a client must not be able to talk the daemon into allow-all"
    );
    assert_eq!(
        effective_permission_mode(&defaults(), Some(&opts)),
        base::settings::PermissionMode::Default,
        "the request should clamp down to the configured mode"
    );
}

/// ...unless the operator explicitly says clients are as trusted as the
/// daemon itself.
#[test]
fn allow_client_permission_override_restores_client_wins() {
    let mut settings = defaults();
    settings.values.allow_client_permission_override = true;
    let opts = SessionOptions {
        permission_mode: Some(base::settings::PermissionMode::BypassPermissions),
        ..Default::default()
    };
    let (bypass, resolved) = resolve(&settings, Some(&opts));
    assert!(Arc::ptr_eq(&bypass, &resolved));
}

/// Tightening is always allowed, in both directions of the comparison:
/// a `bypassPermissions` daemon still honors a client that wants to be
/// asked.
#[test]
fn a_client_may_always_tighten() {
    let mut settings = defaults();
    settings.values.permission_mode = base::settings::PermissionMode::BypassPermissions;
    let opts = SessionOptions {
        permission_mode: Some(base::settings::PermissionMode::Plan),
        ..Default::default()
    };
    assert_eq!(
        effective_permission_mode(&settings, Some(&opts)),
        base::settings::PermissionMode::Plan
    );
}

#[test]
fn auto_and_yolo_in_session_options_clamp_and_resolve_as_bypass() {
    use base::settings::PermissionMode;
    for spelling in ["auto", "yolo"] {
        let opts: SessionOptions =
            serde_json::from_value(serde_json::json!({ "permission_mode": spelling })).unwrap();
        assert_eq!(
            opts.permission_mode,
            Some(PermissionMode::BypassPermissions)
        );

        assert_eq!(
            effective_permission_mode(&defaults(), Some(&opts)),
            PermissionMode::Default,
            "{spelling} must clamp like bypassPermissions"
        );
        let mut accepts_edits = defaults();
        accepts_edits.values.permission_mode = PermissionMode::AcceptEdits;
        assert_eq!(
            effective_permission_mode(&accepts_edits, Some(&opts)),
            PermissionMode::AcceptEdits
        );

        let mut bypass = defaults();
        bypass.values.permission_mode = PermissionMode::BypassPermissions;
        let (host, resolved) = resolve(&bypass, Some(&opts));
        assert!(Arc::ptr_eq(&host, &resolved), "{spelling}");

        let mut overridable = defaults();
        overridable.values.allow_client_permission_override = true;
        let (host, resolved) = resolve(&overridable, Some(&opts));
        assert!(Arc::ptr_eq(&host, &resolved), "{spelling}");
    }
}

#[test]
fn settings_ask_default_is_overridable_per_session_without_becoming_allow_all() {
    let opts = SessionOptions {
        permission_mode: Some(base::settings::PermissionMode::DontAsk),
        ..Default::default()
    };
    let (bypass, resolved) = resolve(&defaults(), Some(&opts));
    assert!(!Arc::ptr_eq(&bypass, &resolved));
}

#[test]
fn effective_mode_prefers_session_options_then_settings_then_ask() {
    use base::settings::PermissionMode;
    let mut settings = defaults();
    assert_eq!(
        effective_permission_mode(&settings, None),
        PermissionMode::Default,
        "an unconfigured daemon asks"
    );

    settings.values.permission_mode = PermissionMode::BypassPermissions;
    assert_eq!(
        effective_permission_mode(&settings, None),
        PermissionMode::BypassPermissions
    );

    let opts = SessionOptions {
        permission_mode: Some(PermissionMode::Plan),
        ..Default::default()
    };
    assert_eq!(
        effective_permission_mode(&settings, Some(&opts)),
        PermissionMode::Plan,
        "a per-session mode wins over the settings default"
    );
}
