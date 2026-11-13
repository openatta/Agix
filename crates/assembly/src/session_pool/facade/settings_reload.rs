//! 设置：provider 与项目信任的读写、`reload_settings`，以及设置里密钥的拆出与脱敏。

use crate::session_pool::*;

impl SessionPool {
    /// Read back the current effective provider/task_models configuration —
    /// the read-side counterpart to `set_provider`. `api_key` is redacted
    /// (`***<last 4 chars>`) unless `include_secrets` is `true` — default
    /// to redacted so a caller that just wants to know *which* providers
    /// are configured doesn't accidentally end up with plaintext keys in
    /// its own logs/UI. See the trust-boundary note on `config.setProvider`
    /// in `crates/assembly/src/server/` — `include_secrets: true` is still gated
    /// by nothing more than "can reach this socket", same as every other
    /// method here.
    pub async fn get_providers(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
        include_secrets: bool,
    ) -> api::ProviderConfigView {
        let settings = self
            .binding_for(Some(project_root.unwrap_or(self.cwd())), scene_id)
            .await
            .settings
            .clone();
        let providers = settings
            .values
            .providers
            .iter()
            .map(|(id, cfg)| {
                let api_key = cfg.api_key.as_ref().map(|k| {
                    if include_secrets {
                        k.clone()
                    } else {
                        redact_secret(k)
                    }
                });
                (
                    id.clone(),
                    api::ProviderView {
                        api_type: cfg.api_type.clone(),
                        base_url: cfg.base_url.clone(),
                        api_key,
                        default_model: cfg.default_model.clone(),
                        models: cfg.models.clone(),
                    },
                )
            })
            .collect();
        api::ProviderConfigView {
            providers,
            default_provider: settings.values.default_provider.clone(),
            task_models: settings.values.task_models.clone().into_iter().collect(),
        }
    }

    /// Detect importable cross-tool configuration sources (Claude Code/
    /// Codex/Cursor) in this pool's project directory — pure filesystem
    /// detection, no LLM turn needed. The manual `/import` path (the bundled
    /// `import` skill) does its own detection with the file tools.
    pub async fn list_import_sources(&self, project_root: Option<&Path>) -> Vec<api::ImportSource> {
        let project_root = project_root.unwrap_or(self.cwd());
        base::frozen::detect_import_sources(project_root)
            .await
            .iter()
            .map(|s| api::ImportSource {
                source: s.kind().as_str().to_string(),
                description: s.describe(),
            })
            .collect()
    }

    /// Execute an import for a previously-detected `source` (one of the
    /// `source` values `list_import_sources` returns) and record the
    /// decision so the automatic startup detection doesn't ask again.
    pub async fn run_import(
        &self,
        project_root: Option<&Path>,
        source: &str,
    ) -> Result<api::ImportRun, PoolError> {
        let invalid = |message: String| PoolError::InvalidRequest(message);
        let project_root = project_root.unwrap_or(self.cwd());
        let kind = base::frozen::ImportSourceKind::try_parse(source).ok_or_else(|| {
            invalid(format!(
                "unknown source `{source}` — expected claude_code, codex, or cursor"
            ))
        })?;
        let sources = base::frozen::detect_import_sources(project_root).await;
        let Some(matched) = sources.iter().find(|s| s.kind() == kind) else {
            return Err(invalid(format!(
                "source `{source}` not currently detected in this project"
            )));
        };
        let summary = base::frozen::execute_import(project_root, matched)
            .await
            .map_err(|e| invalid(e.to_string()))?;
        base::frozen::mark_imported(project_root, &sources, kind)
            .await
            .map_err(|e| invalid(e.to_string()))?;
        Ok(api::ImportRun {
            source: summary.kind.as_str().to_string(),
            actions: summary.actions,
        })
    }

    /// Read-only config/wiring diagnostics — see `crate::doctor`.
    pub async fn doctor_report(&self) -> crate::doctor::DoctorReport {
        let settings = self.bindings.pool_settings().await;
        let mut checks: Vec<Arc<dyn base::interface::health::HealthCheck>> = Vec::new();
        checks.push(Arc::new(crate::doctor::ManagedSettingsCheck::new(
            self.bindings.managed_settings_path().to_path_buf(),
            &settings,
        )));
        checks.extend(self.mcp.health_check());
        checks.push(Arc::new(crate::doctor::ProjectTrustCheck::new(
            serde_json::to_value(project_trust_report(
                self.cwd(),
                &settings,
                &self.plugins.agent_types(),
            ))
            .unwrap_or_default(),
        )));
        checks.push(Arc::new(
            crate::doctor_posture::UnattendedNetworkCheck::new(
                settings.clone(),
                self.unattended_scene_ids().await,
            ),
        ));
        checks.extend(self.plugins.health_check());
        checks.extend(
            self.health
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .checks()
                .iter()
                .cloned(),
        );
        let mut report = crate::doctor::run_doctor(
            self.bindings.paths().as_ref(),
            self.scenes.default_id(),
            settings,
            self.directory.store().is_some(),
            self.plugins.status(),
            &base::interface::health::HealthChecks::from_vec(checks),
        );
        report.runtime = self.runtime_report().await;
        report
    }

    async fn runtime_report(&self) -> crate::doctor::RuntimeReport {
        use crate::doctor::{FailureReport, LogReport, RunningTurnReport, SessionRuntime};
        let mut sessions = self
            .live
            .map_all(|live| SessionRuntime {
                session_id: live.core.id().to_string(),
                phase: live.turn.state(),
                generation: live.engine.generation.get(),
                turn: live.turn.running().map(|turn| RunningTurnReport {
                    turn_id: turn.turn_id.clone(),
                    running_ms: u64::try_from(turn.started.elapsed().as_millis())
                        .unwrap_or(u64::MAX),
                }),
                log: LogReport {
                    behind: live.core.log().is_behind(),
                },
                last_failure: live.core.last_failure().map(|kind| FailureReport { kind }),
            })
            .await;
        sessions.sort_by(|a, b| a.session_id.cmp(&b.session_id));
        crate::doctor::RuntimeReport {
            sessions,
            stopping_engines: self.live.stopping_engines(),
        }
    }

    async fn unattended_scene_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .scenes
            .served()
            .iter()
            .filter(|scene| !scene.interactive)
            .map(|scene| scene.id.clone())
            .collect();
        ids.sort();
        ids.dedup();
        ids
    }

    /// Add a diagnostic of the host's own to what `daemon.doctor` reports.
    pub fn register_health_check(&self, check: Arc<dyn base::interface::health::HealthCheck>) {
        let mut health = self.health.write().unwrap_or_else(|e| e.into_inner());
        let existing = std::mem::take(&mut *health);
        *health = existing.with(check);
    }

    /// The currently-active settings snapshot (for config-inspection RPCs).
    pub async fn settings(&self) -> Arc<LoadedSettings> {
        self.bindings.pool_settings().await
    }

    /// Whether a write to `project_root`'s settings lands in a project's own
    /// tier (rather than the global or scene directory it may resolve to).
    /// When it does and that project is not trusted, the write is refused
    /// with `PROJECT_UNTRUSTED`: `what` would not take effect, and `instead`
    /// is where it would.
    pub(in crate::session_pool) async fn trusted_project_tier(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
        what: &str,
        instead: &str,
    ) -> Result<bool, PoolError> {
        let startup_scene = self.bindings.startup_scene().await;
        let written_to = self.bindings.settings_path_of(project_root);
        if !base::project_trust::is_own_project_dir(
            written_to.parent().unwrap_or(&written_to),
            &self.bindings.paths().global_root(),
            &self.bindings.scene_root(scene_id, &startup_scene),
        ) {
            return Ok(false);
        }
        let target = project_root.unwrap_or(self.cwd());
        let (trust, _) = base::project_trust::ProjectTrustSettings::read_global(
            &self.bindings.paths().global_root(),
        )
        .decide(target);
        if trust.is_untrusted() {
            return Err(PoolError::ProjectUntrusted(format!(
                "{} is not trusted, so {what}; trust it first (project.trust) or {instead}",
                target.display()
            )));
        }
        Ok(true)
    }

    /// Merge a provider config patch (and/or `default_provider` /
    /// `task_models` patches) into the project-tier settings.json — its
    /// credential fields into the uncommitted `settings.local.json` — then
    /// reload the full effective `Settings` (global → scene → project) so
    /// validation and any session created after this call see the change —
    /// see `crate::doctor` module docs for the same tri-tier layout.
    ///
    /// `config_patch`/`task_models_patch` are raw partial JSON objects
    /// merged key-by-key onto the existing project-tier file (see
    /// `base::settings::merge_json_values`) — deliberately not
    /// typed `ProviderConfig` structs, since deserializing a partial patch
    /// into a `#[serde(default)]` struct would silently null out every field
    /// the caller didn't mention, clobbering unrelated existing values.
    /// `delete: true` removes the `providers.<id>` entry outright rather
    /// than merging; any `task_models` entry that referenced it degrades to
    /// `default_provider` automatically the next time routing is resolved
    /// (see `base::provider::resolve_task_models`'s soft-fallback) — no
    /// separate "downgrade" write is needed.
    ///
    /// Refused with `PROJECT_UNTRUSTED`, writing nothing, when the target is
    /// a project's own tier and that project is not trusted: a provider there
    /// would not be used.
    pub async fn set_provider(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
        patch: api::ProviderPatch,
    ) -> Result<api::ProviderSaved, PoolError> {
        use base::settings::{SETTINGS_FILE, SETTINGS_LOCAL_FILE};
        let api::ProviderPatch {
            provider_id,
            delete,
            config: config_patch,
            default_provider,
            task_models: task_models_patch,
        } = patch;
        let provider_id = provider_id.as_str();
        let invalid = |message: String| PoolError::InvalidRequest(message);
        if provider_id.trim().is_empty() {
            return Err(invalid("provider_id must not be empty".into()));
        }

        if let Some(ref cfg) = config_patch {
            if !cfg.is_object() {
                return Err(invalid("config must be a JSON object".into()));
            }
            serde_json::from_value::<base::provider::ProviderConfig>(cfg.clone())
                .map_err(|e| invalid(format!("invalid provider config: {e}")))?;
        }
        if let Some(ref tm) = task_models_patch {
            let obj = tm
                .as_object()
                .ok_or_else(|| invalid("task_models must be a JSON object".into()))?;
            for (task, ov) in obj {
                serde_json::from_value::<base::provider::TaskModelOverride>(ov.clone())
                    .map_err(|e| invalid(format!("invalid task_models.{task}: {e}")))?;
            }
        }
        self.trusted_project_tier(
            project_root,
            scene_id,
            "a provider configured in its settings would not be used",
            "configure the provider in the global tier",
        )
        .await?;

        let settings_path = self.bindings.settings_path_of(project_root);
        let local_path = settings_path.with_file_name(SETTINGS_LOCAL_FILE);
        let provider_id = provider_id.to_string();
        let local_changed = edit_settings_dir(&settings_path, move |tier| {
            let mut project_json = tier.read(SETTINGS_FILE).map_err(|e| e.to_string())?;
            let mut local_json = tier.read(SETTINGS_LOCAL_FILE).map_err(|e| e.to_string())?;
            let local_before = local_json.clone();
            let obj = &mut project_json;
            let local = &mut local_json;

            if delete {
                for tier in [&mut *obj, &mut *local] {
                    if let Some(providers) =
                        tier.get_mut("providers").and_then(|v| v.as_object_mut())
                    {
                        providers.remove(&provider_id);
                    }
                }
            } else if let Some(mut cfg) = config_patch {
                let secrets = take_provider_secrets(&mut cfg);
                let existing = provider_entry(obj, &provider_id);
                base::settings::merge_json_values(existing, cfg);
                let mut moved = take_provider_secrets(existing);
                moved.extend(secrets);
                if !moved.is_empty() {
                    let local_entry = provider_entry(local, &provider_id);
                    for (key, value) in moved {
                        local_entry[key] = value;
                    }
                }
            }

            if let Some(dp) = default_provider {
                obj.insert(
                    "default_provider".to_string(),
                    serde_json::Value::String(dp),
                );
            }

            if let Some(tm) = task_models_patch {
                let task_models = obj
                    .entry("task_models")
                    .or_insert_with(|| serde_json::json!({}));
                if !task_models.is_object() {
                    *task_models = serde_json::json!({});
                }
                base::settings::merge_json_values(task_models, tm);
            }

            // The key first: a failure after it leaves a key nothing uses yet,
            // where the other order would leave a provider without its key.
            let local_changed = local_json != local_before;
            if local_changed {
                tier.write(SETTINGS_LOCAL_FILE, &local_json)
                    .map_err(|e| e.to_string())?;
            }
            tier.write(SETTINGS_FILE, &project_json)
                .map_err(|e| e.to_string())?;
            Ok(local_changed)
        })
        .await
        .map_err(invalid)?;
        let secrets_written_to = local_changed.then(|| local_path.display().to_string());

        let (reloaded, report) = self
            .apply_reloaded_settings(project_root, scene_id, None)
            .await;
        Ok(api::ProviderSaved {
            written_to: settings_path.display().to_string(),
            secrets_written_to,
            reloaded: settings_reloaded(&reloaded.values, report),
        })
    }

    /// `project.status` — whether `project_root` (this daemon's own project
    /// when `None`) is trusted, and what of the project's own configuration
    /// takes effect only when it is.
    pub async fn project_status(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
    ) -> api::ProjectStatus {
        let root = project_root.unwrap_or(self.cwd());
        let settings = self.settings_for_project(Some(root), scene_id).await;
        project_trust_report(root, &settings, &self.plugins.agent_types())
    }

    /// `project.trust` — add `project_root` to, or take it off, the
    /// `project_trust.trusted` list in the global tier's
    /// `settings.local.json`, and rebuild what that project's sessions were
    /// built from. Taking it off removes only that exact entry: a glob or
    /// `project_trust.default` may still trust the project, and the status
    /// returned says so.
    ///
    /// The entry is the directory the path resolves to (`..` and symlinks
    /// followed), with glob metacharacters escaped — it names that directory
    /// and no other. A relative path is refused: it would be read against
    /// the daemon's working directory, not the caller's.
    pub async fn set_project_trust(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
        trusted: bool,
    ) -> Result<api::ProjectTrustSet, PoolError> {
        use crate::watch_hub::Tier;
        use base::settings::SETTINGS_LOCAL_FILE;
        let root = project_root.unwrap_or(self.cwd()).to_path_buf();
        if !root.is_absolute() {
            return Err(PoolError::InvalidRequest(format!(
                "project_root must be an absolute path: {}",
                root.display()
            )));
        }
        let resolved = match root.canonicalize() {
            Ok(resolved) => resolved,
            Err(_) if !trusted => root.clone(),
            Err(e) => {
                return Err(PoolError::InvalidRequest(format!(
                    "project_root {}: {e}",
                    root.display()
                )))
            }
        };
        let entry = base::project_trust::exact_pattern(&resolved);
        let path = self
            .bindings
            .paths()
            .global_root()
            .join(SETTINGS_LOCAL_FILE);
        edit_settings_dir(&path, move |tier| {
            let mut local = tier.read(SETTINGS_LOCAL_FILE).map_err(|e| e.to_string())?;
            let section = local
                .entry("project_trust")
                .or_insert_with(|| serde_json::json!({}));
            if !section.is_object() {
                *section = serde_json::json!({});
            }
            let list = section
                .as_object_mut()
                .expect("made an object above")
                .entry("trusted")
                .or_insert_with(|| serde_json::json!([]));
            if !list.is_array() {
                *list = serde_json::json!([]);
            }
            let list = list.as_array_mut().expect("made an array above");
            let names_root = |v: &serde_json::Value| {
                v.as_str().is_some_and(|p| {
                    p == entry
                        || (!p.contains(['*', '?', '['])
                            && base::project_trust::pattern_matches(p, &resolved))
                })
            };
            if trusted {
                if !list.iter().any(names_root) {
                    list.push(serde_json::Value::String(entry.clone()));
                }
            } else {
                list.retain(|v| !names_root(v));
            }
            tier.write(SETTINGS_LOCAL_FILE, &local)
                .map_err(|e| e.to_string())
        })
        .await
        .map_err(PoolError::Internal)?;

        let tier = Tier::Project(root.clone());
        self.forget_mcp_built_from(&tier, None).await;
        let startup_scene = self.bindings.startup_scene().await;
        if root == self.cwd() {
            self.reload_settings_from(None, &startup_scene, Some(&tier))
                .await;
        }
        if root != self.cwd() || scene_id != startup_scene {
            self.reload_settings_from(Some(&root), scene_id, Some(&tier))
                .await;
        }

        let status = self.project_status(Some(&root), scene_id).await;
        self.emit_event(api::DaemonEvent::ProjectTrustChanged {
            project_root: root.clone(),
            trust: status.trust.as_str().to_string(),
        });
        Ok(api::ProjectTrustSet {
            status,
            written_to: path.display().to_string(),
        })
    }

    /// Re-read whatever is currently on disk across all three settings.json
    /// tiers and apply it — the `config.reload` RPC's backing implementation,
    /// used when a human (or another process) hand-edited a settings.json
    /// file directly instead of going through `config.setProvider`. Shares
    /// `apply_reloaded_settings()` with `set_provider()`, so both entry
    /// points converge on identical semantics (routing re-resolved,
    /// `task_router` rebuilt, bindings marked stale so already-running
    /// sessions recreate themselves lazily — see `run_turn`).
    pub async fn reload_settings(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
    ) -> api::SettingsReloaded {
        self.reload_settings_from(project_root, scene_id, None)
            .await
    }

    pub(in crate::session_pool) async fn reload_settings_from(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
        changed: Option<&crate::watch_hub::Tier>,
    ) -> api::SettingsReloaded {
        let (reloaded, report) = self
            .apply_reloaded_settings(project_root, scene_id, changed)
            .await;
        settings_reloaded(&reloaded.values, report)
    }

    /// Re-read the three-tier merged settings.json from disk, re-resolve
    /// `providers`/`task_models`, rebuild the pair's router from the result,
    /// swap this daemon's own settings when it is its own pair, and invalidate
    /// the bindings built from `changed` (every binding when `None`: an
    /// explicit reload does not know which tier moved) so their running
    /// sessions recreate themselves before their next turn (see `run_turn`).
    /// Shared by `set_provider()` (called after writing a patch) and
    /// `reload_settings()` (called directly, no patch — just "whatever's on
    /// disk right now").
    ///
    /// Never fails outright — misconfiguration degrades to "keep the
    /// previous router, report why in the returned status";
    /// `resolve_task_models` is advisory.
    pub(in crate::session_pool) async fn apply_reloaded_settings(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
        changed: Option<&crate::watch_hub::Tier>,
    ) -> (Arc<LoadedSettings>, api::RoutingStatus) {
        let project_root = project_root.unwrap_or(self.cwd());
        // The daemon's own pair keeps a separate, hot-swappable copy —
        // everything with no session to ask about reads it. Every other pair
        // lives only in its binding, which the invalidation below rebuilds
        // from disk.
        if self
            .bindings
            .is_own_pair(Some(project_root), scene_id)
            .await
        {
            self.bindings.reload_own(self.scenes.default_id()).await;
        }

        // The cached bindings hold the settings that were just replaced, and
        // the skill/agent catalogs and router built from them. Dropping them
        // is what makes a reload reach the next session.
        self.invalidate_bindings(changed).await;

        let binding = self.binding_for(Some(project_root), scene_id).await;
        let routing = &binding.routing;
        let (mcp_connected, mcp_failed, mcp_connecting) = self
            .reconcile_mcp_servers(Some(project_root), scene_id, &binding.settings)
            .await;
        let status = api::RoutingStatus {
            ok: routing.error.is_none(),
            warnings: routing.warnings.clone(),
            error: routing.error.clone(),
            router_rebuilt: routing.router_rebuilt,
            router_error: routing.router_error.clone(),
            mcp_connected,
            mcp_failed,
            mcp_connecting,
        };

        (binding.settings.clone(), status)
    }
}

fn settings_reloaded(settings: &Settings, routing: api::RoutingStatus) -> api::SettingsReloaded {
    api::SettingsReloaded {
        providers: settings.providers.keys().cloned().collect(),
        default_provider: settings.default_provider.clone(),
        task_models: settings.task_models.keys().cloned().collect(),
        routing,
    }
}

/// The provider fields that are credentials, which `config.setProvider` keeps
/// out of the committed `settings.json`.
pub(in crate::session_pool) const PROVIDER_SECRET_KEYS: &[&str] = &["api_key"];

/// Remove the credential fields from one provider's JSON, returning them.
pub(in crate::session_pool) fn take_provider_secrets(
    provider: &mut serde_json::Value,
) -> Vec<(String, serde_json::Value)> {
    let Some(fields) = provider.as_object_mut() else {
        return Vec::new();
    };
    PROVIDER_SECRET_KEYS
        .iter()
        .filter_map(|key| fields.remove(*key).map(|value| (key.to_string(), value)))
        .collect()
}

/// `providers.<id>` of a settings object, created as `{}` when it is missing
/// or not an object.
pub(in crate::session_pool) fn provider_entry<'a>(
    settings: &'a mut serde_json::Map<String, serde_json::Value>,
    provider_id: &str,
) -> &'a mut serde_json::Value {
    let providers = settings
        .entry("providers")
        .or_insert_with(|| serde_json::json!({}));
    if !providers.is_object() {
        *providers = serde_json::json!({});
    }
    let entry = providers
        .as_object_mut()
        .expect("just normalized to an object")
        .entry(provider_id.to_string())
        .or_insert_with(|| serde_json::json!({}));
    if !entry.is_object() {
        *entry = serde_json::json!({});
    }
    entry
}

/// Run `edit` holding the settings directory `settings_file` is in — see
/// `base::settings_file` — on a blocking thread, since the lock can wait on
/// another process.
pub(in crate::session_pool) async fn edit_settings_dir<R: Send + 'static>(
    settings_file: &Path,
    edit: impl FnOnce(&base::settings_file::SettingsDir) -> Result<R, String> + Send + 'static,
) -> Result<R, String> {
    let dir = settings_file
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    tokio::task::spawn_blocking(move || {
        let tier = base::settings_file::SettingsDir::lock(&dir).map_err(|e| e.to_string())?;
        edit(&tier)
    })
    .await
    .map_err(|e| format!("writing settings did not finish: {e}"))?
}

/// Redact every credential-shaped field in a settings blob.
///
/// Walks by key name rather than by type, because the same secrets appear at
/// several depths (`providers.<id>.api_key`, `model.auth_token`) and a
/// settings shape that grows a new provider field should not silently start
/// returning it in the clear. Key names match case-insensitively, and every
/// string under a `headers` or `env` object is redacted whatever its key: an
/// MCP server's `Authorization` header and the variables a stdio server is
/// started with are credentials as often as not, under names nobody can list.
pub(in crate::session_pool) fn redact_settings_secrets(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (k, v) in map.iter_mut() {
                let key = k.to_ascii_lowercase();
                if matches!(key.as_str(), "headers" | "env") && v.is_object() {
                    redact_every_string(v);
                    continue;
                }
                match v {
                    serde_json::Value::String(s) if is_secret_key(&key) => {
                        if !s.is_empty() {
                            *v = serde_json::Value::String(redact_secret(s));
                        }
                    }
                    other => redact_settings_secrets(other),
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                redact_settings_secrets(item);
            }
        }
        _ => {}
    }
}

pub(in crate::session_pool) fn is_secret_key(lowercase_key: &str) -> bool {
    const SECRET_KEYS: &[&str] = &[
        "api_key",
        "auth_token",
        "token",
        "secret",
        "password",
        "authorization",
    ];
    SECRET_KEYS.contains(&lowercase_key)
        || ["_token", "_secret", "_password"]
            .iter()
            .any(|suffix| lowercase_key.ends_with(suffix))
}

pub(in crate::session_pool) fn redact_every_string(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(s) if !s.is_empty() => {
            *value = serde_json::Value::String(redact_secret(s));
        }
        serde_json::Value::Object(map) => map.values_mut().for_each(redact_every_string),
        serde_json::Value::Array(items) => items.iter_mut().for_each(redact_every_string),
        _ => {}
    }
}

/// Redact a secret to `***<last 4 chars>` (char-boundary safe — slices by
/// `char`, not byte, so this never panics on non-ASCII input); too-short
/// secrets (≤4 chars) redact to a bare `***` rather than revealing the
/// whole thing.
pub(in crate::session_pool) fn redact_secret(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= 4 {
        return "***".to_string();
    }
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("***{tail}")
}
