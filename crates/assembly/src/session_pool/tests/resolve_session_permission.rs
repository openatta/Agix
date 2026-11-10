//! `resolve_session_permission_tests` 原样搬来的用例。

use super::*;

struct AllowAllTestPermission;
#[async_trait::async_trait]
impl base::interface::permission::Permission for AllowAllTestPermission {
    async fn check(
        &self,
        _: &str,
        _: &serde_json::Value,
        _: &std::path::Path,
        _: &str,
    ) -> base::interface::permission::PermissionOutcome {
        base::interface::permission::PermissionOutcome::Permit
    }
}

fn defaults() -> Settings {
    Settings::defaults_for("claude-sonnet-4-6")
}

/// Everything the resolver needs beyond the settings/options under test,
/// with the host having supplied its own `Permission`.
fn resolve(
    settings: &Settings,
    options: Option<&SessionOptions>,
) -> (Arc<dyn Permission>, Arc<dyn Permission>) {
    let host: Arc<dyn Permission> = Arc::new(AllowAllTestPermission);
    let tools = Arc::new(base::tool::InMemoryToolRegistry::new());
    let mode = effective_permission_mode(settings, options);
    let resolved = resolve_session_permission(Some(&host), &tools, settings, mode, options);
    (host, resolved)
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
async fn a_bypass_session_gets_the_hosts_permission_or_else_a_gate_that_only_validates() {
    use base::interface::permission::PermissionOutcome;
    let mut settings = defaults();
    settings.permission_mode = base::interface::settings::PermissionMode::BypassPermissions;
    settings.permission_rules = vec![base::interface::settings::PermissionRule {
        tool: "Bash".into(),
        action: base::interface::settings::PermissionAction::Deny,
        tier: None,
    }];

    let (host, resolved) = resolve(&settings, None);
    assert!(
        Arc::ptr_eq(&host, &resolved),
        "a host that supplied its own permission gets that very instance"
    );

    let tools = Arc::new(base::tool::InMemoryToolRegistry::new());
    tools::register_builtin_tools(&tools);
    let mode = effective_permission_mode(&settings, None);
    let gate = resolve_session_permission(None, &tools, &settings, mode, None);
    let cwd = std::env::temp_dir();
    assert!(matches!(
        gate.check("Bash", &serde_json::json!({"command": "ls"}), &cwd, "s")
            .await,
        PermissionOutcome::Permit
    ));
    assert!(
        matches!(
            gate.check("Bash", &serde_json::json!({"command": "  "}), &cwd, "s")
                .await,
            PermissionOutcome::Deny { .. }
        ),
        "input validation still runs under bypass"
    );
}

#[test]
fn a_session_clock_is_the_hosts_start_else_the_recordings_else_the_machines() {
    let date = |env: Option<Arc<dyn base::interface::environment::Environment>>| {
        env.map(|e| e.now().date().to_string())
    };
    let asked = SessionOptions {
        clock: Some(crate::rpc::ClockOptions {
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
        clock: Some(crate::rpc::ClockOptions {
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
        permission_mode: Some(base::interface::settings::PermissionMode::BypassPermissions),
        ..Default::default()
    };
    let (bypass, resolved) = resolve(&defaults(), Some(&opts));
    assert!(
        !Arc::ptr_eq(&bypass, &resolved),
        "a client must not be able to talk the daemon into allow-all"
    );
    assert_eq!(
        effective_permission_mode(&defaults(), Some(&opts)),
        base::interface::settings::PermissionMode::Default,
        "the request should clamp down to the configured mode"
    );
}

/// ...unless the operator explicitly says clients are as trusted as the
/// daemon itself.
#[test]
fn allow_client_permission_override_restores_client_wins() {
    let mut settings = defaults();
    settings.allow_client_permission_override = true;
    let opts = SessionOptions {
        permission_mode: Some(base::interface::settings::PermissionMode::BypassPermissions),
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
    settings.permission_mode = base::interface::settings::PermissionMode::BypassPermissions;
    let opts = SessionOptions {
        permission_mode: Some(base::interface::settings::PermissionMode::Plan),
        ..Default::default()
    };
    assert_eq!(
        effective_permission_mode(&settings, Some(&opts)),
        base::interface::settings::PermissionMode::Plan
    );
}

#[test]
fn auto_and_yolo_in_session_options_clamp_and_resolve_as_bypass() {
    use base::interface::settings::PermissionMode;
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
        accepts_edits.permission_mode = PermissionMode::AcceptEdits;
        assert_eq!(
            effective_permission_mode(&accepts_edits, Some(&opts)),
            PermissionMode::AcceptEdits
        );

        let mut bypass = defaults();
        bypass.permission_mode = PermissionMode::BypassPermissions;
        let (host, resolved) = resolve(&bypass, Some(&opts));
        assert!(Arc::ptr_eq(&host, &resolved), "{spelling}");

        let mut overridable = defaults();
        overridable.allow_client_permission_override = true;
        let (host, resolved) = resolve(&overridable, Some(&opts));
        assert!(Arc::ptr_eq(&host, &resolved), "{spelling}");
    }
}

#[test]
fn settings_ask_default_is_overridable_per_session_without_becoming_allow_all() {
    let opts = SessionOptions {
        permission_mode: Some(base::interface::settings::PermissionMode::DontAsk),
        ..Default::default()
    };
    let (bypass, resolved) = resolve(&defaults(), Some(&opts));
    assert!(!Arc::ptr_eq(&bypass, &resolved));
}

#[test]
fn effective_mode_prefers_session_options_then_settings_then_ask() {
    use base::interface::settings::PermissionMode;
    let mut settings = defaults();
    assert_eq!(
        effective_permission_mode(&settings, None),
        PermissionMode::Default,
        "an unconfigured daemon asks"
    );

    settings.permission_mode = PermissionMode::BypassPermissions;
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
