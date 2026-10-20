//! `daemon.doctor` — read-only configuration/wiring diagnostics.
//!
//! Re-inspects the same three settings.json tiers `Settings::load()` already
//! merged at startup, but reports per-tier existence/parse status instead of
//! just logging a warning and moving on — useful for a user (or an IDE
//! plugin) asking "why isn't my provider config taking effect?" without
//! grepping daemon logs.
//!
//! Its three verdicts — the tiers, provider routing, hooks configuration —
//! are [`HealthCheck`]s, so the same answers reach an embedder that never
//! runs a daemon, and so a host's own checks can stand beside them in one
//! report rather than in a second endpoint.

use crate::config::DaemonPaths;
use base::interface::health::{CheckResult, HealthCheck, HealthChecks};
use base::interface::settings::Settings;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Status of a single settings.json layer.
#[derive(serde::Serialize)]
struct TierStatus {
    tier: &'static str,
    path: String,
    exists: bool,
    parses: bool,
    error: Option<String>,
}

fn inspect_tier(tier: &'static str, dir: &Path) -> TierStatus {
    let path = dir.join("settings.json");
    let path_str = path.display().to_string();
    if !path.exists() {
        return TierStatus {
            tier,
            path: path_str,
            exists: false,
            parses: true,
            error: None,
        };
    }
    match std::fs::read_to_string(&path) {
        Ok(text) => match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(_) => TierStatus {
                tier,
                path: path_str,
                exists: true,
                parses: true,
                error: None,
            },
            Err(e) => TierStatus {
                tier,
                path: path_str,
                exists: true,
                parses: false,
                error: Some(format!("invalid JSON: {e}")),
            },
        },
        Err(e) => TierStatus {
            tier,
            path: path_str,
            exists: true,
            parses: false,
            error: Some(format!("read error: {e}")),
        },
    }
}

/// The three settings.json tiers: is each one there, and does it parse?
pub struct SettingsTiersCheck {
    global: PathBuf,
    scene: PathBuf,
    project: PathBuf,
}

impl SettingsTiersCheck {
    pub fn new(paths: &dyn DaemonPaths) -> Self {
        Self {
            global: paths.global_root(),
            scene: paths.config_root(),
            project: paths.project_root().join(".atta"),
        }
    }
}

impl HealthCheck for SettingsTiersCheck {
    fn name(&self) -> &str {
        "settings.tiers"
    }

    fn check(&self) -> CheckResult {
        let tiers = vec![
            inspect_tier("global", &self.global),
            inspect_tier("scene", &self.scene),
            inspect_tier("project", &self.project),
        ];
        let broken: Vec<&str> = tiers.iter().filter(|t| !t.parses).map(|t| t.tier).collect();
        let details = serde_json::to_value(&tiers).unwrap_or(serde_json::Value::Null);

        // A tier that is simply absent is the normal case, not a fault: most
        // deployments write one file and leave the other two alone.
        if broken.is_empty() {
            CheckResult::ok("every settings.json present parses").with_details(details)
        } else {
            CheckResult::degraded(format!("unreadable settings.json: {}", broken.join(", ")))
                .with_details(details)
        }
    }
}

/// Provider and task-model routing — the same resolution `main.rs` runs at
/// startup, re-run here so this reflects the live settings after a reload.
pub struct ProviderRoutingCheck(Arc<Settings>);

impl ProviderRoutingCheck {
    pub fn new(settings: Arc<Settings>) -> Self {
        Self(settings)
    }
}

impl HealthCheck for ProviderRoutingCheck {
    fn name(&self) -> &str {
        "settings.providers"
    }

    fn check(&self) -> CheckResult {
        let settings = &self.0;
        let (ok, warnings, error) = if settings.providers.is_empty() {
            (true, Vec::new(), None)
        } else {
            match base::provider::resolve_task_models(
                &settings.providers,
                settings.default_provider.as_deref(),
                &settings.task_models,
            ) {
                Ok((_, warnings)) => (true, warnings, None),
                Err(e) => (false, Vec::new(), Some(e)),
            }
        };

        let details = serde_json::json!({
            "configured": settings.providers.keys().collect::<Vec<_>>(),
            "default_provider": settings.default_provider,
            "task_models": settings.task_models.keys().collect::<Vec<_>>(),
            "ok": ok,
            "warnings": warnings,
            "error": error,
        });

        match error {
            Some(e) => CheckResult::failing(e).with_details(details),
            None if !warnings.is_empty() => {
                CheckResult::degraded(warnings.join("; ")).with_details(details)
            }
            None => CheckResult::ok("provider routing resolves").with_details(details),
        }
    }
}

/// The hooks section, parsed the same tolerant way the engine parses it — so
/// this reports what will actually load rather than a whole-map verdict the
/// engine no longer shares.
pub struct HooksCheck(Arc<Settings>);

impl HooksCheck {
    pub fn new(settings: Arc<Settings>) -> Self {
        Self(settings)
    }
}

impl HealthCheck for HooksCheck {
    fn name(&self) -> &str {
        "settings.hooks"
    }

    fn check(&self) -> CheckResult {
        let configured = self.0.hooks_config.is_some();
        let (ok, error) = match &self.0.hooks_config {
            None => (true, None),
            Some(v) => {
                let (_, report) = hooks::parse_hooks_settings(v);
                if report.is_clean() {
                    (true, None)
                } else {
                    let mut parts = Vec::new();
                    if !report.unknown_events.is_empty() {
                        parts.push(format!("unknown events: {:?}", report.unknown_events));
                    }
                    for (event, err) in &report.invalid_configs {
                        parts.push(format!("{event}: {err}"));
                    }
                    (false, Some(parts.join("; ")))
                }
            }
        };

        let details = serde_json::json!({
            "configured": configured,
            "ok": ok,
            "error": error,
        });

        match error {
            Some(e) => CheckResult::degraded(e).with_details(details),
            None => CheckResult::ok("hooks configuration parses").with_details(details),
        }
    }
}

/// Build the full diagnostic report as a JSON value (the RPC result body).
///
/// `extra` is whatever the process wired in beside the engine's own checks —
/// the plugin fault records, and anything the embedder registered.
pub fn run_doctor(
    paths: &dyn DaemonPaths,
    scene_id: &str,
    settings: Arc<Settings>,
    history_store_wired: bool,
    plugin_status: crate::plugins::PluginStatus,
    extra: &HealthChecks,
) -> serde_json::Value {
    let mut checks = HealthChecks::new()
        .with(Arc::new(SettingsTiersCheck::new(paths)))
        .with(Arc::new(ProviderRoutingCheck::new(settings.clone())))
        .with(Arc::new(HooksCheck::new(settings.clone())));
    for check in extra.checks() {
        checks = checks.with(check.clone());
    }
    let health = checks.report();

    let detail = |name: &str| {
        health
            .find(name)
            .map(|c| c.result.details.clone())
            .unwrap_or(serde_json::Value::Null)
    };

    // The three named keys carry the same data as their entries under
    // `health`. They are what every existing client reads, so they stay.
    serde_json::json!({
        "scene": scene_id,
        "settings_tiers": detail("settings.tiers"),
        "providers": detail("settings.providers"),
        "hooks": detail("settings.hooks"),
        "health": health.to_json(),
        "session_persistence": {
            "history_store_wired": history_store_wired,
        },
        // Whether this binary can load plugins at all is a deployment fact a
        // locked-down install has to be able to verify from the running
        // process, not from release notes.
        "plugins": {
            "status": plugin_status.as_str(),
        },
        "permission_rules_count": settings.permission_rules.len(),
        "model": {
            "model_name": settings.model.model_name,
            "api_type": format!("{:?}", settings.model.api_type),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::StaticDaemonPaths;

    fn base_settings() -> Arc<Settings> {
        Arc::new(Settings::defaults_for("test-model"))
    }

    fn no_extra_checks() -> HealthChecks {
        HealthChecks::new()
    }

    #[test]
    fn reports_missing_tiers_as_not_existing_but_ok() {
        let dir = tempfile::tempdir().unwrap();
        let paths = StaticDaemonPaths::new(dir.path().to_path_buf());
        let settings = base_settings();
        let report = run_doctor(
            &paths,
            "coding",
            settings,
            false,
            crate::plugins::PluginStatus::Enabled,
            &no_extra_checks(),
        );
        let tiers = report["settings_tiers"].as_array().unwrap();
        assert_eq!(tiers.len(), 3);
        for t in tiers {
            assert_eq!(t["exists"], false);
            assert_eq!(t["parses"], true);
        }
    }

    #[test]
    fn reports_malformed_tier_as_not_parsing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("settings.json"), "{not json").unwrap();
        let paths = StaticDaemonPaths::new(dir.path().to_path_buf());
        let settings = base_settings();
        let report = run_doctor(
            &paths,
            "coding",
            settings,
            false,
            crate::plugins::PluginStatus::Enabled,
            &no_extra_checks(),
        );
        let tiers = report["settings_tiers"].as_array().unwrap();
        // config_root == project_root == dir here, so both entries see the
        // malformed file — only assert the ones that actually point at `dir`.
        assert!(tiers
            .iter()
            .any(|t| t["exists"] == true && t["parses"] == false));
    }

    #[test]
    fn reports_provider_routing_errors() {
        let dir = tempfile::tempdir().unwrap();
        let paths = StaticDaemonPaths::new(dir.path().to_path_buf());
        let mut settings = Settings::defaults_for("test-model");
        settings.default_provider = Some("does-not-exist".into());
        settings.providers.insert(
            "real".into(),
            base::provider::ProviderConfig {
                api_type: Some("anthropic".into()),
                base_url: None,
                api_key: None,
                api_key_env: None,
                default_model: Some("m".into()),
                models: vec![],
                ..Default::default()
            },
        );
        let settings = Arc::new(settings);
        let report = run_doctor(
            &paths,
            "coding",
            settings,
            false,
            crate::plugins::PluginStatus::Enabled,
            &no_extra_checks(),
        );
        assert_eq!(report["providers"]["ok"], false);
        assert!(report["providers"]["error"].as_str().is_some());
    }

    #[test]
    fn reports_history_store_wiring_status() {
        let dir = tempfile::tempdir().unwrap();
        let paths = StaticDaemonPaths::new(dir.path().to_path_buf());
        let settings = base_settings();
        let report = run_doctor(
            &paths,
            "coding",
            settings,
            true,
            crate::plugins::PluginStatus::Enabled,
            &no_extra_checks(),
        );
        assert_eq!(report["session_persistence"]["history_store_wired"], true);
    }

    /// A locked-down deployment has to be able to confirm from the running
    /// process that plugins are not merely switched off but absent.
    #[test]
    fn doctor_reports_the_plugin_subsystem_status() {
        let dir = tempfile::tempdir().unwrap();
        let paths = StaticDaemonPaths::new(dir.path().to_path_buf());
        let settings = base_settings();

        for status in [
            crate::plugins::PluginStatus::Enabled,
            crate::plugins::PluginStatus::PackagesOnly,
            crate::plugins::PluginStatus::DisabledByPolicy,
            crate::plugins::PluginStatus::CompiledOut,
        ] {
            let report = run_doctor(
                &paths,
                "coding",
                settings.clone(),
                false,
                status,
                &no_extra_checks(),
            );
            assert_eq!(report["plugins"]["status"], status.as_str());
        }
    }

    /// P3-15's acceptance from the daemon's side: the diagnostics an embedder
    /// registers appear beside the engine's own, and a verdict of theirs
    /// decides the overall status.
    #[test]
    fn a_host_registered_check_joins_the_report_and_can_set_its_status() {
        struct LicenceServer;
        impl HealthCheck for LicenceServer {
            fn name(&self) -> &str {
                "acme.licence"
            }
            fn check(&self) -> CheckResult {
                CheckResult::failing("licence server unreachable")
                    .with_details(serde_json::json!({ "last_ok": "2026-08-30T11:00:00Z" }))
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let paths = StaticDaemonPaths::new(dir.path().to_path_buf());
        let report = run_doctor(
            &paths,
            "coding",
            base_settings(),
            false,
            crate::plugins::PluginStatus::Enabled,
            &HealthChecks::new().with(Arc::new(LicenceServer)),
        );

        assert_eq!(report["health"]["status"], "failing");
        let names: Vec<&str> = report["health"]["checks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            vec![
                "settings.tiers",
                "settings.providers",
                "settings.hooks",
                "acme.licence"
            ]
        );
        assert_eq!(
            report["health"]["checks"][3]["details"]["last_ok"],
            "2026-08-30T11:00:00Z"
        );
    }

    /// The keys every existing client reads are the same data as the health
    /// entries, not a second copy that can drift from them.
    #[test]
    fn the_legacy_keys_are_the_checks_own_details() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("settings.json"), "{not json").unwrap();
        let paths = StaticDaemonPaths::new(dir.path().to_path_buf());
        let report = run_doctor(
            &paths,
            "coding",
            base_settings(),
            false,
            crate::plugins::PluginStatus::Enabled,
            &no_extra_checks(),
        );

        assert_eq!(
            report["settings_tiers"],
            report["health"]["checks"][0]["details"]
        );
        assert_eq!(
            report["providers"],
            report["health"]["checks"][1]["details"]
        );
        assert_eq!(report["hooks"], report["health"]["checks"][2]["details"]);
        assert_eq!(report["health"]["status"], "degraded");
    }
}

/// The `daemon.doctor` example in `docs/daemon_rpc_protocol.md`, checked
/// against a report this process actually produces.
///
/// A protocol example is what a client gets written against, and nothing
/// compiles it — it can describe a response no version of the daemon ever
/// returned and stay that way indefinitely. So it is read back out of the file
/// and compared to a real report field by field.
#[cfg(test)]
mod documented_shape_tests {
    use super::*;
    use crate::config::StaticDaemonPaths;

    /// The doc's example is written for a fresh install with no settings.json
    /// anywhere; `run_doctor` has to be called under exactly that to compare.
    fn report_the_example_describes(dir: &Path) -> serde_json::Value {
        run_doctor(
            &StaticDaemonPaths::new(dir.to_path_buf()),
            "coding",
            Arc::new(Settings::defaults_for("claude-sonnet-4-6")),
            true,
            crate::plugins::PluginStatus::Enabled,
            &HealthChecks::new(),
        )
    }

    fn protocol_doc() -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("daemon crate has a parent")
            .join("docs")
            .join("daemon_rpc_protocol.md");
        std::fs::read_to_string(&path).expect("protocol doc is readable")
    }

    /// The first fenced block under `#### \`daemon.doctor\``, with its `//`
    /// annotations removed. Quote-aware, because a `//` inside a string would
    /// otherwise take the rest of the line with it.
    fn documented_doctor_example(doc: &str) -> serde_json::Value {
        let section = doc
            .split_once("#### `daemon.doctor`")
            .expect("the doc still documents daemon.doctor")
            .1;
        let fenced = section
            .split_once("```jsonc\n")
            .expect("the section still carries a jsonc example")
            .1
            .split_once("\n```")
            .expect("the example is fenced")
            .0;

        let mut json = String::new();
        for line in fenced.lines() {
            let mut in_string = false;
            let mut escaped = false;
            let mut cut = line.len();
            let bytes: Vec<char> = line.chars().collect();
            for (i, c) in bytes.iter().enumerate() {
                if escaped {
                    escaped = false;
                    continue;
                }
                match c {
                    '\\' if in_string => escaped = true,
                    '"' => in_string = !in_string,
                    '/' if !in_string && bytes.get(i + 1) == Some(&'/') => {
                        cut = line
                            .char_indices()
                            .nth(i)
                            .map(|(byte, _)| byte)
                            .unwrap_or(line.len());
                        break;
                    }
                    _ => {}
                }
            }
            json.push_str(&line[..cut]);
            json.push('\n');
        }
        serde_json::from_str(&json).expect("the example parses as JSON once comments are stripped")
    }

    /// A documented string containing `…` stands for something that varies by
    /// machine; only its presence and type are claimed.
    fn is_placeholder(v: &serde_json::Value) -> bool {
        v.as_str().is_some_and(|s| s.contains('…'))
    }

    /// Every field the example writes must be in `actual` at the same path,
    /// with the same value. The example may say less than the response — it
    /// may not say anything different.
    fn assert_documented(path: &str, documented: &serde_json::Value, actual: &serde_json::Value) {
        match documented {
            serde_json::Value::Object(fields) => {
                let actual = actual
                    .as_object()
                    .unwrap_or_else(|| panic!("{path}: documented as an object, response is not"));
                for (key, value) in fields {
                    let found = actual.get(key).unwrap_or_else(|| {
                        panic!("{path}.{key}: documented, absent from response")
                    });
                    assert_documented(&format!("{path}.{key}"), value, found);
                }
            }
            serde_json::Value::Array(items) => {
                let actual = actual
                    .as_array()
                    .unwrap_or_else(|| panic!("{path}: documented as an array, response is not"));
                assert!(
                    actual.len() >= items.len(),
                    "{path}: {} entries documented, {} in the response",
                    items.len(),
                    actual.len()
                );
                for (i, item) in items.iter().enumerate() {
                    assert_documented(&format!("{path}[{i}]"), item, &actual[i]);
                }
            }
            scalar if is_placeholder(scalar) => {
                assert!(
                    actual.is_string(),
                    "{path}: documented as a string placeholder, response has {actual}"
                );
            }
            scalar => assert_eq!(scalar, actual, "{path}"),
        }
    }

    #[test]
    fn the_documented_example_is_a_real_response() {
        let dir = tempfile::tempdir().unwrap();
        let actual = report_the_example_describes(dir.path());
        let documented = documented_doctor_example(&protocol_doc());
        assert_documented("doctor", &documented, &actual);
    }

    /// A field in the response that the example never mentions is the drift
    /// this test exists to catch, so the top level is checked both ways.
    #[test]
    fn the_example_accounts_for_every_top_level_key() {
        let dir = tempfile::tempdir().unwrap();
        let actual = report_the_example_describes(dir.path());
        let documented = documented_doctor_example(&protocol_doc());

        let mut undocumented: Vec<&String> = actual
            .as_object()
            .unwrap()
            .keys()
            .filter(|k| documented.get(*k).is_none())
            .collect();
        undocumented.sort();
        assert!(
            undocumented.is_empty(),
            "daemon.doctor returns keys the protocol doc does not mention: {undocumented:?}"
        );
    }
}
