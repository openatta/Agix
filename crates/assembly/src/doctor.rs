//! `daemon.doctor` — read-only configuration/wiring diagnostics.
//!
//! Re-inspects the same three settings.json tiers `LoadedSettings::load()` already
//! merged at startup, but reports per-tier existence/parse status instead of
//! just logging a warning and moving on — useful for a user (or an IDE
//! plugin) asking "why isn't my provider config taking effect?" without
//! grepping daemon logs.
//!
//! Its verdicts — the tiers, provider routing, hooks configuration, how full
//! the memory store is — are [`HealthCheck`]s, so the same answers reach an embedder that never
//! runs a daemon, and so a host's own checks can stand beside them in one
//! report rather than in a second endpoint.

use crate::config::DaemonPaths;
use crate::doctor_posture;
use base::interface::health::{CheckResult, HealthCheck, HealthChecks};
use base::settings::LoadedSettings;
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
    /// Top-level keys no setting reads; they are ignored.
    unknown_keys: Vec<String>,
    /// Values in this tier's files that do not fit their setting, left out
    /// of the merge — see `LoadedSettings::ignored`.
    ignored_values: Vec<base::settings::IgnoredValue>,
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
            unknown_keys: Vec::new(),
            ignored_values: Vec::new(),
        };
    }
    match std::fs::read_to_string(&path) {
        Ok(text) => match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(value) => TierStatus {
                tier,
                path: path_str,
                exists: true,
                parses: true,
                error: None,
                unknown_keys: base::settings::unknown_settings_keys(&value),
                ignored_values: Vec::new(),
            },
            Err(e) => TierStatus {
                tier,
                path: path_str,
                exists: true,
                parses: false,
                error: Some(format!("invalid JSON: {e}")),
                unknown_keys: Vec::new(),
                ignored_values: Vec::new(),
            },
        },
        Err(e) => TierStatus {
            tier,
            path: path_str,
            exists: true,
            parses: false,
            error: Some(format!("read error: {e}")),
            unknown_keys: Vec::new(),
            ignored_values: Vec::new(),
        },
    }
}

/// The three settings.json tiers: is each one there, does it parse, and
/// does every value in it fit its setting?
pub struct SettingsTiersCheck {
    global: PathBuf,
    scene: PathBuf,
    project: PathBuf,
    ignored: Vec<base::settings::IgnoredValue>,
}

impl SettingsTiersCheck {
    /// `settings` are the ones loaded from these tiers; what their load left
    /// out is reported against the tier it came from.
    pub fn new(paths: &dyn DaemonPaths, settings: &LoadedSettings) -> Self {
        Self {
            global: paths.global_root(),
            scene: paths.config_root(),
            project: paths.project_root().join(".agix"),
            ignored: settings.ignored.clone(),
        }
    }
}

impl HealthCheck for SettingsTiersCheck {
    fn name(&self) -> &str {
        "settings.tiers"
    }

    fn check(&self) -> CheckResult {
        let mut tiers = vec![
            inspect_tier("global", &self.global),
            inspect_tier("scene", &self.scene),
            inspect_tier("project", &self.project),
        ];
        for tier in &mut tiers {
            tier.ignored_values = self
                .ignored
                .iter()
                .filter(|v| v.tier == tier.tier)
                .cloned()
                .collect();
        }
        let broken: Vec<&str> = tiers.iter().filter(|t| !t.parses).map(|t| t.tier).collect();
        let misfits: Vec<String> = self
            .ignored
            .iter()
            .map(|v| format!("{} ({})", v.tier, v.key))
            .collect();
        let with_unknown: Vec<&str> = tiers
            .iter()
            .filter(|t| !t.unknown_keys.is_empty())
            .map(|t| t.tier)
            .collect();
        let details = serde_json::to_value(&tiers).unwrap_or(serde_json::Value::Null);

        // A tier that is simply absent is the normal case, not a fault: most
        // deployments write one file and leave the other two alone.
        if !broken.is_empty() {
            CheckResult::degraded(format!("unreadable settings.json: {}", broken.join(", ")))
                .with_details(details)
        } else if !misfits.is_empty() {
            CheckResult::degraded(format!(
                "settings values that do not fit their setting, ignored: {}",
                misfits.join(", ")
            ))
            .with_details(details)
        } else if with_unknown.is_empty() {
            CheckResult::ok("every settings.json present parses").with_details(details)
        } else {
            CheckResult::ok(format!(
                "every settings.json present parses; keys no setting reads, ignored, in: {}",
                with_unknown.join(", ")
            ))
            .with_details(details)
        }
    }
}

/// Provider and task-model routing — the same resolution `main.rs` runs at
/// startup, re-run here so this reflects the live settings after a reload.
pub struct ProviderRoutingCheck(Arc<LoadedSettings>);

impl ProviderRoutingCheck {
    pub fn new(settings: Arc<LoadedSettings>) -> Self {
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
/// engine does not share.
pub struct HooksCheck(Arc<LoadedSettings>);

impl HooksCheck {
    pub fn new(settings: Arc<LoadedSettings>) -> Self {
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

/// How full the durable memory store is. At the cap, turn-end extraction
/// stops adding memories, and only removing some makes room again.
pub struct MemoryCheck(Arc<LoadedSettings>);

impl MemoryCheck {
    pub fn new(settings: Arc<LoadedSettings>) -> Self {
        Self(settings)
    }
}

impl HealthCheck for MemoryCheck {
    fn name(&self) -> &str {
        "memory"
    }

    fn check(&self) -> CheckResult {
        let cap = base::memory::MAX_MEMORIES;
        if !self.0.memory_enabled {
            return CheckResult::ok("memory is off")
                .with_details(serde_json::json!({ "enabled": false, "cap": cap }));
        }
        let store = base::memory::MemoryStore::for_paths(&base::paths::ConfigPaths::from_settings(
            &self.0.paths,
        ));
        let count = store.count();
        let details = serde_json::json!({ "enabled": true, "count": count, "cap": cap });
        if count >= cap {
            CheckResult::degraded(format!(
                "{count}/{cap} memories: the store is full and no new memories are \
                 extracted until some are removed"
            ))
            .with_details(details)
        } else {
            CheckResult::ok(format!("{count}/{cap} memories")).with_details(details)
        }
    }
}

/// The deployment's managed settings: where they were looked for, whether
/// they are there, and — `failing` — whether they cannot be honoured, in
/// which case every tool call is refused.
pub struct ManagedSettingsCheck {
    path: PathBuf,
    managed: Option<base::managed_settings::ManagedSettings>,
    unreadable: Option<String>,
}

impl ManagedSettingsCheck {
    pub fn new(path: PathBuf, settings: &LoadedSettings) -> Self {
        Self {
            path,
            managed: settings.managed.clone(),
            unreadable: permissions::policy::ManagedLayer::from(&settings.managed)
                .unreadable()
                .map(str::to_string),
        }
    }
}

impl HealthCheck for ManagedSettingsCheck {
    fn name(&self) -> &str {
        "settings.managed"
    }

    fn check(&self) -> CheckResult {
        let Some(managed) = &self.managed else {
            return CheckResult::ok(format!("no managed settings at {}", self.path.display()))
                .with_details(serde_json::json!({
                    "present": false,
                    "path": self.path.display().to_string(),
                }));
        };
        let details = serde_json::json!({
            "present": true,
            "path": self.path.display().to_string(),
            "origin": managed.origin,
            "error": self.unreadable,
            "permission_rules": managed.permission_rules,
            "disable_bypass_permissions_mode": managed.disable_bypass_permissions_mode,
        });
        match &self.unreadable {
            Some(why) => CheckResult::failing(format!(
                "managed settings cannot be honoured, every tool call is refused: {why}"
            )),
            None => CheckResult::ok(format!(
                "managed settings from {}: {} permission rule(s){}",
                managed.origin,
                managed.permission_rules.len(),
                if managed.disable_bypass_permissions_mode {
                    ", bypassPermissions disabled"
                } else {
                    ""
                }
            )),
        }
        .with_details(details)
    }
}

/// Whether a project is trusted, and what of its own configuration that
/// decides — `project.status`'s answer for it, as a verdict.
///
/// `degraded` when something is set aside: the project is not trusted and
/// has configuration that only a trusted one gets, or it is trusted only by
/// `project_trust.default` — which a later 0.3.x release is planned to turn to `untrusted` — and has such
/// configuration that would then stop taking effect.
pub struct ProjectTrustCheck(serde_json::Value);

impl ProjectTrustCheck {
    pub fn new(status: serde_json::Value) -> Self {
        Self(status)
    }
}

impl HealthCheck for ProjectTrustCheck {
    fn name(&self) -> &str {
        "project.trust"
    }

    fn check(&self) -> CheckResult {
        let status = &self.0;
        let gated: Vec<&str> = status["requires_trust"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str())
            .collect();
        let by_list = !status["matched"].is_null();
        let result = match (status["trust"].as_str(), gated.is_empty()) {
            (Some("untrusted"), true) => {
                CheckResult::ok("project not trusted; nothing in it needs trust")
            }
            (Some("untrusted"), false) => CheckResult::degraded(format!(
                "project not trusted; withheld until it is: {}",
                gated.join(", ")
            )),
            (Some("trusted"), _) if by_list => {
                CheckResult::ok("project trusted by project_trust.trusted")
            }
            (Some("trusted"), true) => CheckResult::ok(
                "project trusted by project_trust.default; nothing in it needs trust",
            ),
            (Some("trusted"), false) => CheckResult::degraded(format!(
                "project trusted only by project_trust.default, which is planned to turn to untrusted in a later 0.3.x release; \
                 these would then stop taking effect: {}",
                gated.join(", ")
            )),
            _ => CheckResult::ok("no project tier"),
        };
        result.with_details(status.clone())
    }
}

/// Build the full diagnostic report as a JSON value (the RPC result body).
///
/// `extra` is whatever the process wired in beside the engine's own checks —
/// the plugin fault records, and anything the embedder registered.
pub fn run_doctor(
    paths: &dyn DaemonPaths,
    scene_id: &str,
    settings: Arc<LoadedSettings>,
    history_store_wired: bool,
    plugin_status: crate::plugins::PluginStatus,
    extra: &HealthChecks,
) -> DoctorReport {
    let mut checks = HealthChecks::new()
        .with(Arc::new(SettingsTiersCheck::new(paths, &settings)))
        .with(Arc::new(ProviderRoutingCheck::new(settings.clone())))
        .with(Arc::new(HooksCheck::new(settings.clone())))
        .with(Arc::new(MemoryCheck::new(settings.clone())))
        .with(Arc::new(doctor_posture::PermissionModeCheck::new(
            settings.clone(),
        )))
        .with(Arc::new(doctor_posture::HooksFailOpenCheck::new(
            settings.clone(),
        )))
        .with(Arc::new(doctor_posture::NetworkAllowlistCheck::new(
            settings.clone(),
        )))
        .with(Arc::new(doctor_posture::PanicStrategyCheck));
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

    DoctorReport {
        scene: scene_id.to_string(),
        settings_tiers: detail("settings.tiers"),
        providers: detail("settings.providers"),
        hooks: detail("settings.hooks"),
        health: health.to_json(),
        session_persistence: SessionPersistence {
            history_store_wired,
        },
        plugins: PluginsReport {
            status: plugin_status.as_str().to_string(),
        },
        permission_rules_count: settings.rules.len(),
        model: ModelReport {
            model_name: settings.model.model_name.clone(),
            api_type: format!("{:?}", settings.model.api_type),
        },
    }
}

/// What `daemon.doctor` answers. `settings_tiers`, `providers`, `hooks` and
/// `health` are the checks' own `details`, whose shape each check decides.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct DoctorReport {
    pub scene: String,
    pub settings_tiers: serde_json::Value,
    pub providers: serde_json::Value,
    pub hooks: serde_json::Value,
    pub health: serde_json::Value,
    pub session_persistence: SessionPersistence,
    pub plugins: PluginsReport,
    pub permission_rules_count: usize,
    pub model: ModelReport,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct SessionPersistence {
    pub history_store_wired: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct PluginsReport {
    pub status: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct ModelReport {
    pub model_name: String,
    pub api_type: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::StaticDaemonPaths;
    use base::settings::Settings;

    fn run_doctor(
        paths: &dyn DaemonPaths,
        scene_id: &str,
        settings: Arc<LoadedSettings>,
        history_store_wired: bool,
        plugin_status: crate::plugins::PluginStatus,
        extra: &HealthChecks,
    ) -> serde_json::Value {
        let report = super::run_doctor(
            paths,
            scene_id,
            settings,
            history_store_wired,
            plugin_status,
            extra,
        );
        serde_json::to_value(report).unwrap()
    }

    fn base_settings() -> Arc<LoadedSettings> {
        let mut settings = Settings::defaults_for("test-model");
        settings.memory_enabled = false;
        Arc::new(LoadedSettings::new(
            settings,
            base::settings::PathSettings::default(),
        ))
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
            "code",
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
            "code",
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
    fn reports_the_keys_no_setting_reads() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("settings.json"),
            r#"{"permision_mode": "plan", "language": "en"}"#,
        )
        .unwrap();
        let paths = StaticDaemonPaths::new(dir.path().to_path_buf());
        let report = run_doctor(
            &paths,
            "code",
            base_settings(),
            false,
            crate::plugins::PluginStatus::Enabled,
            &no_extra_checks(),
        );
        let tier = report["settings_tiers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["exists"] == true)
            .unwrap()
            .clone();
        assert_eq!(tier["unknown_keys"], serde_json::json!(["permision_mode"]));
        let check = &report["health"]["checks"][0];
        assert_eq!(check["status"], "ok", "{check}");
        assert!(
            check["summary"].as_str().unwrap().contains("ignored"),
            "{check}"
        );
    }

    #[test]
    fn a_value_that_does_not_fit_its_setting_degrades_the_tiers_check() {
        let root = tempfile::tempdir().unwrap();
        let (global, project) = (root.path().join("global"), root.path().join("work"));
        std::fs::create_dir_all(&global).unwrap();
        std::fs::create_dir_all(project.join(".agix")).unwrap();
        std::fs::write(
            project.join(".agix/settings.json"),
            r#"{"language": 5, "memory_enabled": false}"#,
        )
        .unwrap();
        let settings = LoadedSettings::load(
            global.clone(),
            global.join("scenes/code"),
            project.join(".agix"),
            "code",
            "m",
        );
        let paths = StaticDaemonPaths::with_project(global.join("scenes/code"), project)
            .with_global(global);

        let report = run_doctor(
            &paths,
            "code",
            Arc::new(settings),
            false,
            crate::plugins::PluginStatus::Enabled,
            &no_extra_checks(),
        );

        let check = &report["health"]["checks"][0];
        assert_eq!(check["name"], "settings.tiers");
        assert_eq!(check["status"], "degraded", "{check}");
        assert!(
            check["summary"].as_str().unwrap().contains("language"),
            "{check}"
        );
        let project_tier = report["settings_tiers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["tier"] == "project")
            .unwrap()
            .clone();
        assert_eq!(
            project_tier["ignored_values"][0]["key"], "language",
            "{project_tier}"
        );
    }

    #[test]
    fn reports_provider_routing_errors() {
        let dir = tempfile::tempdir().unwrap();
        let paths = StaticDaemonPaths::new(dir.path().to_path_buf());
        let mut settings = base::settings::LoadedSettings::new(
            Settings::defaults_for("test-model"),
            base::settings::PathSettings::default(),
        );
        settings.values.default_provider = Some("does-not-exist".into());
        settings.values.providers.insert(
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
            "code",
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
            "code",
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
            crate::plugins::PluginStatus::DisabledByPolicy,
            crate::plugins::PluginStatus::CompiledOut,
        ] {
            let report = run_doctor(
                &paths,
                "code",
                settings.clone(),
                false,
                status,
                &no_extra_checks(),
            );
            assert_eq!(report["plugins"]["status"], status.as_str());
        }
    }

    #[test]
    fn a_full_memory_store_is_reported_degraded() {
        let dir = tempfile::tempdir().unwrap();
        let mut settings = base::settings::LoadedSettings::new(
            Settings::defaults_for("test-model"),
            base::settings::PathSettings::default(),
        );
        settings.paths.global_data_dir = dir.path().join("global");
        settings.paths.user_data_dir = dir.path().join("global/scenes/code");
        settings.paths.local_data_dir = dir.path().join("project");
        settings.paths.workspace =
            base::settings::Workspace::project_of(&settings.paths.local_data_dir);
        let settings = Arc::new(settings);
        let store = base::memory::MemoryStore::for_paths(&base::paths::ConfigPaths::from_settings(
            &settings.paths,
        ));
        let fill = |from: usize, to: usize| {
            store
                .persist_batch(
                    (from..to)
                        .map(|i| base::memory::DurableMemory {
                            name: format!("m{i}"),
                            description: String::new(),
                            memory_type: Default::default(),
                            content: String::new(),
                            source_session_id: String::new(),
                            confidence: 0.9,
                            last_seen: String::new(),
                            recall_count: 0,
                        })
                        .collect(),
                )
                .unwrap();
        };
        let cap = base::memory::MAX_MEMORIES;
        let memory_check = |settings: &Arc<LoadedSettings>| {
            let report = run_doctor(
                &StaticDaemonPaths::new(dir.path().to_path_buf()),
                "code",
                settings.clone(),
                false,
                crate::plugins::PluginStatus::Enabled,
                &no_extra_checks(),
            );
            report["health"]["checks"]
                .as_array()
                .unwrap()
                .iter()
                .find(|c| c["name"] == "memory")
                .cloned()
                .unwrap()
        };

        fill(0, cap - 1);
        let below = memory_check(&settings);
        assert_eq!(below["status"], "ok", "{below}");
        assert_eq!(below["details"]["count"], cap - 1);

        fill(cap - 1, cap);
        let full = memory_check(&settings);
        assert_eq!(full["status"], "degraded", "{full}");
        assert_eq!(full["details"]["count"], cap);
        assert_eq!(full["details"]["cap"], cap);
    }

    /// The diagnostics an embedder
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
            "code",
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
                "memory",
                "settings.permission_mode",
                "settings.hooks_fail_open",
                "sandbox.network_allowlist",
                "build.panic",
                "acme.licence"
            ]
        );
        assert_eq!(
            report["health"]["checks"][8]["details"]["last_ok"],
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
            "code",
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
        let mut settings = base::settings::LoadedSettings::new(
            base::settings::Settings::defaults_for("claude-sonnet-4-6"),
            base::settings::PathSettings::default(),
        );
        settings.paths.global_data_dir = dir.join("global");
        settings.paths.user_data_dir = dir.join("global/scenes/code");
        settings.paths.local_data_dir = dir.join("project");
        settings.paths.workspace =
            base::settings::Workspace::project_of(&settings.paths.local_data_dir);
        serde_json::to_value(run_doctor(
            &StaticDaemonPaths::new(dir.to_path_buf()),
            "code",
            Arc::new(settings),
            true,
            crate::plugins::PluginStatus::Enabled,
            &HealthChecks::new(),
        ))
        .unwrap()
    }

    fn protocol_doc() -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
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
