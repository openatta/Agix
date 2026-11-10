//! 设置：provider 与项目信任的读写、`reload_settings`，以及设置里密钥的拆出与脱敏。

use super::*;

impl SessionPool {
    /// Read back the current effective provider/task_models configuration —
    /// the read-side counterpart to `set_provider`. `api_key` is redacted
    /// (`***<last 4 chars>`) unless `include_secrets` is `true` — default
    /// to redacted so a caller that just wants to know *which* providers
    /// are configured doesn't accidentally end up with plaintext keys in
    /// its own logs/UI. See the trust-boundary note on `config.setProvider`
    /// in `crates/assembly/src/server.rs` — `include_secrets: true` is still gated
    /// by nothing more than "can reach this socket", same as every other
    /// method here.
    pub async fn get_providers(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
        include_secrets: bool,
    ) -> serde_json::Value {
        let settings = self
            .binding_for(Some(project_root.unwrap_or(self.cwd.as_path())), scene_id)
            .await
            .settings
            .clone();
        let providers: serde_json::Map<String, serde_json::Value> = settings
            .providers
            .iter()
            .map(|(id, cfg)| {
                let api_key = match &cfg.api_key {
                    None => serde_json::Value::Null,
                    Some(k) if include_secrets => serde_json::Value::String(k.clone()),
                    Some(k) => serde_json::Value::String(redact_secret(k)),
                };
                (
                    id.clone(),
                    serde_json::json!({
                        "api_type": cfg.api_type,
                        "base_url": cfg.base_url,
                        "api_key": api_key,
                        "default_model": cfg.default_model,
                        "models": cfg.models,
                    }),
                )
            })
            .collect();
        serde_json::json!({
            "providers": providers,
            "default_provider": settings.default_provider,
            "task_models": settings.task_models,
        })
    }

    /// Detect importable cross-tool configuration sources (Claude Code/
    /// Codex/Cursor) in this pool's project directory — pure filesystem
    /// detection, no LLM turn needed. The manual `/import` path (the bundled
    /// `import` skill) does its own detection with the file tools.
    pub async fn list_import_sources(&self, project_root: Option<&Path>) -> Vec<serde_json::Value> {
        let project_root = project_root.unwrap_or(self.cwd.as_path());
        base::frozen::detect_import_sources(project_root)
            .await
            .iter()
            .map(|s| serde_json::json!({"source": s.kind().as_str(), "description": s.describe()}))
            .collect()
    }

    /// Execute an import for a previously-detected `source` (one of the
    /// `source` values `list_import_sources` returns) and record the
    /// decision so the automatic startup detection doesn't ask again.
    pub async fn run_import(
        &self,
        project_root: Option<&Path>,
        source: &str,
    ) -> Result<serde_json::Value, String> {
        let project_root = project_root.unwrap_or(self.cwd.as_path());
        let kind = base::frozen::ImportSourceKind::try_parse(source).ok_or_else(|| {
            format!("unknown source `{source}` — expected claude_code, codex, or cursor")
        })?;
        let sources = base::frozen::detect_import_sources(project_root).await;
        let Some(matched) = sources.iter().find(|s| s.kind() == kind) else {
            return Err(format!(
                "source `{source}` not currently detected in this project"
            ));
        };
        let summary = base::frozen::execute_import(project_root, matched)
            .await
            .map_err(|e| e.to_string())?;
        base::frozen::mark_imported(project_root, &sources, kind)
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_json::json!({
            "source": summary.kind.as_str(),
            "actions": summary.actions,
        }))
    }

    /// Read-only config/wiring diagnostics — see `crate::doctor`.
    pub async fn doctor_report(&self) -> serde_json::Value {
        let settings = self.settings.read().await.clone();
        let mut checks: Vec<Arc<dyn base::interface::health::HealthCheck>> = Vec::new();
        #[cfg(feature = "mcp")]
        checks.push(self.mcp_policy.clone());
        checks.push(Arc::new(crate::doctor::ProjectTrustCheck::new(
            project_trust_report(&self.cwd, &settings, &self.plugins.agent_types()),
        )));
        checks.extend(self.plugins.health_check());
        checks.extend(
            self.health
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .checks()
                .iter()
                .cloned(),
        );
        crate::doctor::run_doctor(
            self.paths.as_ref(),
            self.scene.id(),
            settings,
            self.history_store.is_some(),
            self.plugins.status(),
            &base::interface::health::HealthChecks::from_vec(checks),
        )
    }

    /// Add a diagnostic of the host's own to what `daemon.doctor` reports.
    pub fn register_health_check(&self, check: Arc<dyn base::interface::health::HealthCheck>) {
        let mut health = self.health.write().unwrap_or_else(|e| e.into_inner());
        let existing = std::mem::take(&mut *health);
        *health = existing.with(check);
    }

    /// The currently-active settings snapshot (for config-inspection RPCs).
    pub async fn settings(&self) -> Arc<Settings> {
        self.settings.read().await.clone()
    }

    /// Whether a write to `project_root`'s settings lands in a project's own
    /// tier (rather than the global or scene directory it may resolve to).
    /// When it does and that project is not trusted, the write is refused
    /// with `PROJECT_UNTRUSTED`: `what` would not take effect, and `instead`
    /// is where it would.
    pub(super) async fn trusted_project_tier(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
        what: &str,
        instead: &str,
    ) -> Result<bool, RpcError> {
        let startup_scene = self.settings.read().await.paths.scope.clone();
        let written_to = self.settings_path_of(project_root);
        if !base::project_trust::is_own_project_dir(
            written_to.parent().unwrap_or(&written_to),
            &self.paths.global_root(),
            &self.scene_root(scene_id, &startup_scene),
        ) {
            return Ok(false);
        }
        let target = project_root.unwrap_or(self.cwd.as_path());
        let (trust, _) =
            base::project_trust::ProjectTrustSettings::read_global(&self.paths.global_root())
                .decide(target);
        if trust.is_untrusted() {
            return Err((
                codes::PROJECT_UNTRUSTED,
                format!(
                    "{} is not trusted, so {what}; trust it first (project.trust) or {instead}",
                    target.display()
                ),
            ));
        }
        Ok(true)
    }

    /// The project-tier `settings.json` of `project_root`, or of this
    /// daemon's own project when none is named.
    pub(super) fn settings_path_of(&self, project_root: Option<&Path>) -> PathBuf {
        project_root
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.paths.project_root())
            .join(".agix")
            .join(base::interface::settings::SETTINGS_FILE)
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
    /// `base::interface::settings::merge_json_values`) — deliberately not
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
    #[allow(clippy::too_many_arguments)]
    pub async fn set_provider(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
        provider_id: &str,
        delete: bool,
        config_patch: Option<serde_json::Value>,
        default_provider: Option<String>,
        task_models_patch: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, RpcError> {
        use base::interface::settings::{SETTINGS_FILE, SETTINGS_LOCAL_FILE};
        let invalid = |message: String| (codes::INVALID_PARAMS, message);
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

        let settings_path = self.settings_path_of(project_root);
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
                base::interface::settings::merge_json_values(existing, cfg);
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
                base::interface::settings::merge_json_values(task_models, tm);
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
        let mut result = serde_json::json!({
            "written_to": settings_path.display().to_string(),
            "providers": reloaded.providers.keys().collect::<Vec<_>>(),
            "default_provider": reloaded.default_provider,
            "task_models": reloaded.task_models.keys().collect::<Vec<_>>(),
            "routing": report.to_json(),
        });
        if let Some(path) = secrets_written_to {
            result["secrets_written_to"] = serde_json::Value::String(path);
        }

        Ok(result)
    }

    /// `project.status` — whether `project_root` (this daemon's own project
    /// when `None`) is trusted, and what of the project's own configuration
    /// takes effect only when it is.
    pub async fn project_status(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
    ) -> serde_json::Value {
        let root = project_root.unwrap_or(self.cwd.as_path());
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
    ) -> Result<serde_json::Value, RpcError> {
        use crate::watch_hub::Tier;
        use base::interface::settings::SETTINGS_LOCAL_FILE;
        let root = project_root.unwrap_or(self.cwd.as_path()).to_path_buf();
        if !root.is_absolute() {
            return Err((
                codes::INVALID_PARAMS,
                format!("project_root must be an absolute path: {}", root.display()),
            ));
        }
        let resolved = match root.canonicalize() {
            Ok(resolved) => resolved,
            Err(_) if !trusted => root.clone(),
            Err(e) => {
                return Err((
                    codes::INVALID_PARAMS,
                    format!("project_root {}: {e}", root.display()),
                ))
            }
        };
        let entry = base::project_trust::exact_pattern(&resolved);
        let path = self.paths.global_root().join(SETTINGS_LOCAL_FILE);
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
        .map_err(|e| (codes::INTERNAL_ERROR, e))?;

        let tier = Tier::Project(root.clone());
        #[cfg(feature = "mcp")]
        self.forget_mcp_built_from(&tier, None).await;
        let startup_scene = self.settings.read().await.paths.scope.clone();
        if root == self.cwd {
            self.reload_settings_from(None, &startup_scene, Some(&tier))
                .await;
        }
        if root != self.cwd || scene_id != startup_scene {
            self.reload_settings_from(Some(&root), scene_id, Some(&tier))
                .await;
        }

        let mut status = self.project_status(Some(&root), scene_id).await;
        self.emit_event(serde_json::json!({
            "kind": "project_trust_changed",
            "project_root": root,
            "trust": status["trust"],
        }));
        status["written_to"] = serde_json::Value::String(path.display().to_string());
        Ok(status)
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
    ) -> serde_json::Value {
        self.reload_settings_from(project_root, scene_id, None)
            .await
    }

    pub(super) async fn reload_settings_from(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
        changed: Option<&crate::watch_hub::Tier>,
    ) -> serde_json::Value {
        let (reloaded, report) = self
            .apply_reloaded_settings(project_root, scene_id, changed)
            .await;
        serde_json::json!({
            "providers": reloaded.providers.keys().collect::<Vec<_>>(),
            "default_provider": reloaded.default_provider,
            "task_models": reloaded.task_models.keys().collect::<Vec<_>>(),
            "routing": report.to_json(),
        })
    }

    /// Re-read the three-tier merged settings.json from disk, re-resolve
    /// `providers`/`task_models`, rebuild `self.task_router` from the result,
    /// swap `self.settings`, and invalidate the bindings built from `changed`
    /// (every binding when `None`: an explicit reload does not know which tier
    /// moved) so their running sessions recreate themselves before their next
    /// turn (see `run_turn`). Shared by `set_provider()` (called after writing a
    /// patch) and `reload_settings()` (called directly, no patch — just
    /// "whatever's on disk right now").
    ///
    /// Never fails outright — misconfiguration degrades to "keep the
    /// previous `task_router`, report why in the returned `ReloadReport`";
    /// `resolve_task_models` is advisory.
    pub(super) async fn apply_reloaded_settings(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
        changed: Option<&crate::watch_hub::Tier>,
    ) -> (Arc<Settings>, ReloadReport) {
        let startup_scene = self.settings.read().await.paths.scope.clone();
        let project_root = project_root.unwrap_or(self.cwd.as_path());
        let is_default = project_root == self.cwd.as_path() && scene_id == startup_scene;

        // The daemon's own pair keeps its separate, hot-swappable copy in
        // `self.settings` — everything with no session to ask about reads it.
        // Every other pair lives only in its binding, which the invalidation
        // below rebuilds from disk.
        if is_default {
            let default_model = self.settings.read().await.model.model_name.clone();
            let reloaded = Arc::new(Settings::load(
                self.paths.global_root(),
                self.paths.config_root(),
                self.paths.project_root().join(".agix"),
                self.scene.id(),
                &default_model,
            ));
            *self.settings.write().await = reloaded;
        }

        // The cached bindings hold the settings that were just replaced, and
        // the skill/agent catalogs and router built from them. Dropping them
        // is what makes a reload reach the next session.
        self.invalidate_bindings(changed).await;

        let binding = self.binding_for(Some(project_root), scene_id).await;
        #[cfg_attr(not(feature = "mcp"), allow(unused_mut))]
        let mut report = binding.routing.clone();
        #[cfg(feature = "mcp")]
        {
            let (mcp_connected, mcp_failed, mcp_connecting) = self
                .reconcile_mcp_servers(Some(project_root), scene_id, &binding.settings)
                .await;
            report.mcp_connected = mcp_connected;
            report.mcp_failed = mcp_failed;
            report.mcp_connecting = mcp_connecting;
        }

        (binding.settings.clone(), report)
    }
}

/// The provider fields that are credentials, which `config.setProvider` keeps
/// out of the committed `settings.json`.
pub(super) const PROVIDER_SECRET_KEYS: &[&str] = &["api_key"];

/// Remove the credential fields from one provider's JSON, returning them.
pub(super) fn take_provider_secrets(
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

/// The MCP server fields that carry credentials — request headers, the
/// process environment — which `mcp.addServer` / `mcp.updateServer` keep out
/// of the committed `settings.json`.
#[cfg(feature = "mcp")]
pub(super) const MCP_SERVER_SECRET_KEYS: &[&str] = &["headers", "env"];

#[cfg(feature = "mcp")]
pub(super) fn take_mcp_server_secrets(
    server: &mut serde_json::Value,
) -> Vec<(String, serde_json::Value)> {
    let Some(fields) = server.as_object_mut() else {
        return Vec::new();
    };
    MCP_SERVER_SECRET_KEYS
        .iter()
        .filter_map(|key| fields.remove(*key).map(|value| (key.to_string(), value)))
        .collect()
}

/// `mcp_servers` of a settings object, created as `{}` when it is missing or
/// not an object.
#[cfg(feature = "mcp")]
pub(super) fn mcp_servers_of(
    settings: &mut serde_json::Map<String, serde_json::Value>,
) -> &mut serde_json::Map<String, serde_json::Value> {
    let servers = settings
        .entry("mcp_servers")
        .or_insert_with(|| serde_json::json!({}));
    if !servers.is_object() {
        *servers = serde_json::json!({});
    }
    servers
        .as_object_mut()
        .expect("just normalized to an object")
}

/// `providers.<id>` of a settings object, created as `{}` when it is missing
/// or not an object.
pub(super) fn provider_entry<'a>(
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
pub(super) async fn edit_settings_dir<R: Send + 'static>(
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
pub(super) fn redact_settings_secrets(value: &mut serde_json::Value) {
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

pub(super) fn is_secret_key(lowercase_key: &str) -> bool {
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

pub(super) fn redact_every_string(value: &mut serde_json::Value) {
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
pub(super) fn redact_secret(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= 4 {
        return "***".to_string();
    }
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("***{tail}")
}
