//! `plugin.*`。插件子系统没编进来或被策略关掉时，每个方法都答 `PLUGINS_DISABLED`，
//! 这一关在池里。

use super::*;
use crate::rpc::methods::plugin::{
    PluginInstallParams, PluginList, PluginToggleParams, PluginUninstallParams,
};
use crate::session_pool::api::{PluginInstalled, PluginRemoved, PluginToggled};

rpc_methods! {
    "plugin.list" => list(NoParams) -> PluginList;
    "plugin.install" => install(PluginInstallParams) -> PluginInstalled;
    "plugin.uninstall" => uninstall(PluginUninstallParams) -> PluginRemoved;
    "plugin.enable" => enable(PluginToggleParams) -> PluginToggled;
    "plugin.disable" => disable(PluginToggleParams) -> PluginToggled;
    "plugin.reload" => reload(NoParams) -> PluginList;
}

async fn list(s: &DaemonServer, _: &Call, _: NoParams) -> Result<PluginList, PoolError> {
    Ok(PluginList {
        plugins: s.pool.list_plugins().await?,
    })
}

async fn install(
    s: &DaemonServer,
    _: &Call,
    params: PluginInstallParams,
) -> Result<PluginInstalled, PoolError> {
    s.pool
        .install_plugin(
            &params.name,
            &params.version,
            &params.download_url,
            params.checksum.as_deref(),
            params.scope,
        )
        .await
}

async fn uninstall(
    s: &DaemonServer,
    _: &Call,
    params: PluginUninstallParams,
) -> Result<PluginRemoved, PoolError> {
    s.pool
        .uninstall_plugin(&params.name, params.version.as_deref(), params.scope)
        .await
}

async fn enable(
    s: &DaemonServer,
    _: &Call,
    params: PluginToggleParams,
) -> Result<PluginToggled, PoolError> {
    s.pool
        .set_plugin_enabled(&params.name, true, params.scope)
        .await
}

async fn disable(
    s: &DaemonServer,
    _: &Call,
    params: PluginToggleParams,
) -> Result<PluginToggled, PoolError> {
    s.pool
        .set_plugin_enabled(&params.name, false, params.scope)
        .await
}

async fn reload(s: &DaemonServer, _: &Call, _: NoParams) -> Result<PluginList, PoolError> {
    Ok(PluginList {
        plugins: s.pool.reload_plugins().await?,
    })
}
