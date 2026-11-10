//! `config.*`。

use super::*;
use crate::rpc::methods::config::{
    ConfigGetParams, ConfigGetProviderParams, ConfigSetProviderParams,
};
use crate::rpc::methods::ConfigTarget;
use crate::session_pool::api::{
    ConfigTier, ProviderConfigView, ProviderPatch, ProviderSaved, SettingsReloaded,
};

rpc_methods! {
    "config.get" => get(ConfigGetParams) -> ConfigTier;
    "config.getProvider" => get_provider(ConfigGetProviderParams) -> ProviderConfigView;
    "config.setProvider" => set_provider(ConfigSetProviderParams) -> ProviderSaved;
    "config.reload" => reload(ConfigTarget) -> SettingsReloaded;
}

async fn get(s: &DaemonServer, _: &Call, params: ConfigGetParams) -> Result<ConfigTier, PoolError> {
    s.pool
        .config_tier(
            &params.scene,
            params.project_root.as_deref(),
            params.tier,
            params.include_secrets,
        )
        .await
}

async fn get_provider(
    s: &DaemonServer,
    _: &Call,
    params: ConfigGetProviderParams,
) -> Result<ProviderConfigView, PoolError> {
    let (project_root, scene) = s.target_of(params.target);
    Ok(s.pool
        .get_providers(project_root.as_deref(), &scene, params.include_secrets)
        .await)
}

async fn set_provider(
    s: &DaemonServer,
    _: &Call,
    params: ConfigSetProviderParams,
) -> Result<ProviderSaved, PoolError> {
    let (project_root, scene) = s.target_of(params.target);
    s.pool
        .set_provider(
            project_root.as_deref(),
            &scene,
            ProviderPatch {
                provider_id: params.provider_id,
                delete: params.delete,
                config: params.config,
                default_provider: params.default_provider,
                task_models: params.task_models,
            },
        )
        .await
}

/// Re-reads all three settings.json tiers from disk as they are, for a
/// person (or another process) who edited a file by hand; the same
/// `apply_reloaded_settings` as `config.setProvider`.
async fn reload(
    s: &DaemonServer,
    _: &Call,
    target: ConfigTarget,
) -> Result<SettingsReloaded, PoolError> {
    let (project_root, scene) = s.target_of(target);
    Ok(s.pool
        .reload_settings(project_root.as_deref(), &scene)
        .await)
}
