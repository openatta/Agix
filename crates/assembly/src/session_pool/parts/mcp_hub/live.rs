//! 会话池的 MCP 部分（编进 `mcp` 模块时）：协作者 `McpHub` 持有按 `(project, scene)`
//! 缓存的连接、登录状态和准入策略；`connect`、`auth` 里是池上与 MCP 有关的方法。
//! 没编进 `mcp` 时换成签名相同的 `off.rs`，池的其余部分和 server 都不需要 cfg。
//!
//! 锁序：缓存的表锁只在找、加、删槽时持有；连接和列工具在槽自己的更新锁下进行，
//! 不持池上别的锁。

mod auth;
mod cache;
mod connect;
mod status;
#[cfg(test)]
mod tests;

use crate::session_pool::api::DaemonEvent;
use base::settings::LoadedSettings;
use cache::McpCache;
use mcp::manager::McpManager;
use status::*;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tracing::warn;

/// Whether this build has the `mcp` module.
pub const COMPILED: bool = true;

/// What a host decides about MCP: where sign-ins are kept, how this host
/// presents itself to authorization servers, the enterprise IdP, and the
/// policy servers are admitted by.
#[derive(Default)]
pub struct McpOptions {
    /// Where MCP servers' OAuth sign-ins are kept — a keychain, say. `None`:
    /// 0600 files under the user data directory's `mcp/oauth/`.
    pub credential_store: Option<Arc<dyn mcp::oauth::McpCredentialStore>>,
    /// The client this host presents to MCP authorization servers it has no
    /// registration with: the name shown on their consent page, and its
    /// client metadata document URL if it publishes one. `None`: "Agix",
    /// dynamic registration only.
    pub oauth_client: Option<mcp::oauth::McpOAuthClient>,
    /// The enterprise IdP whose users' identities `"grant": "enterprise"`
    /// MCP servers are authorized from (the host then supplies an identity
    /// with `mcp.auth.setIdentity`). `None`: the MCP policy's
    /// `enterprise_idp`, if it names one.
    pub enterprise_idp: Option<mcp::config::EnterpriseIdp>,
    /// Which configured MCP servers may connect. `None` reads the policy file
    /// (`DaemonConfig::mcp_policy_path`, else the platform's managed path);
    /// a host that passes one is the only source, and no file is read.
    pub policy: Option<mcp::policy::McpPolicy>,
}

/// [`McpOptions`] checked, with the policy read.
pub(crate) struct McpSetup {
    credential_store: Option<Arc<dyn mcp::oauth::McpCredentialStore>>,
    oauth_client: Option<mcp::oauth::McpOAuthClient>,
    enterprise_idp: Option<mcp::config::EnterpriseIdp>,
    policy: mcp::policy::McpPolicyGate,
}

impl McpOptions {
    /// The policy is the host's when it gave one, else the file at
    /// `policy_path` (which must exist), else the platform's managed path.
    /// An enterprise IdP that does not validate fails the assembly.
    pub(crate) fn resolve(self, policy_path: Option<&Path>) -> anyhow::Result<McpSetup> {
        use mcp::policy::{managed_policy_path, McpPolicyGate};
        let policy = match (self.policy, policy_path) {
            (Some(policy), _) => McpPolicyGate::from_host(policy),
            (None, Some(path)) => McpPolicyGate::load(path, true),
            (None, None) => McpPolicyGate::load(&managed_policy_path(), false),
        };
        if let Some(error) = policy.error() {
            warn!(
                error,
                "the MCP policy is unreadable; every MCP server is denied"
            );
        }
        if let Some(Err(e)) = self
            .enterprise_idp
            .as_ref()
            .map(mcp::config::EnterpriseIdp::validate)
        {
            anyhow::bail!("Assembly::mcp.enterprise_idp: {e}");
        }
        for name in [self.enterprise_idp.as_ref(), policy.enterprise_idp()]
            .into_iter()
            .flatten()
            .filter_map(|idp| idp.client_secret_env.as_deref())
        {
            base::interface::exec::env::hold_back_credential(name);
        }
        let enterprise_idp = self
            .enterprise_idp
            .or_else(|| policy.enterprise_idp().cloned());
        Ok(McpSetup {
            credential_store: self.credential_store,
            oauth_client: self.oauth_client,
            enterprise_idp,
            policy,
        })
    }
}

pub(in crate::session_pool) struct McpHub {
    /// Centrally-connected MCP servers, one entry per `(project, scene)` — a
    /// scene tier can declare its own `mcp_servers`, so two scenes on one
    /// project are not interchangeable. The pool's default pair connects
    /// eagerly at startup; every other pair on its first session. Each
    /// session gets its own manager built from the entry's client handles.
    cache: McpCache,
    /// Sign-in state per server, the flows in progress, and the credential
    /// store (files under the pool's own user data root unless the host gave
    /// one).
    auth: Arc<mcp::oauth::McpAuth>,
    /// What every MCP server this pool connects is admitted by.
    policy: Arc<mcp::policy::McpPolicyGate>,
}

impl McpHub {
    /// `setup` is what the host decided; `None` keeps sign-ins under
    /// `user_data_dir` and admits every server.
    pub(in crate::session_pool) fn new(setup: Option<McpSetup>, user_data_dir: &Path) -> Self {
        let setup = setup.unwrap_or_else(|| McpSetup {
            credential_store: None,
            oauth_client: None,
            enterprise_idp: None,
            policy: mcp::policy::McpPolicyGate::open(),
        });
        let auth = match setup.credential_store {
            Some(store) => mcp::oauth::McpAuth::new(store),
            None => mcp::oauth::McpAuth::with_file_store(user_data_dir.join("mcp").join("oauth")),
        };
        let auth = match setup.oauth_client {
            Some(client) => auth.with_client(client),
            None => auth,
        };
        let policy = Arc::new(setup.policy);
        auth.set_policy(policy.clone());
        auth.set_enterprise_idp(setup.enterprise_idp);
        Self {
            cache: McpCache::default(),
            auth: Arc::new(auth),
            policy,
        }
    }

    /// The policy's own diagnostics, for `daemon.doctor`.
    pub(in crate::session_pool) fn health_check(
        &self,
    ) -> Option<Arc<dyn base::interface::health::HealthCheck>> {
        Some(self.policy.clone())
    }
}

/// What `SessionPool::admit_mcp_servers` lets through, and what it does not:
/// `refused` names every entry not connected (invalid or denied), `denied`
/// those that parse (the policy refuses them, or a project may not declare
/// them as written), `invalid` those that do not; the last two are kept for
/// `mcp.status`.
#[derive(Default)]
pub(in crate::session_pool) struct AdmittedMcp {
    pub(in crate::session_pool) servers: HashMap<String, mcp::config::McpServerConfig>,
    pub(in crate::session_pool) denied: Vec<mcp::manager::DeniedServer>,
    pub(in crate::session_pool) invalid: Vec<mcp::manager::FailedServer>,
    pub(in crate::session_pool) refused: Vec<String>,
}

/// The `mcp_servers` keys the project tier's own files (`settings.json`,
/// `settings.local.json`) declare. A key a project sets at all, even one
/// field of a global server's, makes that server the project's.
pub(in crate::session_pool) fn project_tier_mcp_servers(
    settings: &LoadedSettings,
) -> std::collections::HashSet<String> {
    settings.project_mcp_servers().keys().cloned().collect()
}

/// How long a session's creation waits for its project's MCP servers to
/// connect. One still connecting then goes on in the background and joins
/// the project's sessions at their next turn.
pub(in crate::session_pool) const MCP_SESSION_CONNECT_WAIT: std::time::Duration =
    std::time::Duration::from_secs(10);

pub(in crate::session_pool) fn mcp_connect_failed_event(
    manager: &McpManager,
    name: &str,
) -> DaemonEvent {
    let error = manager
        .failed()
        .iter()
        .find(|f| f.name == name)
        .map(|f| f.error.clone())
        .unwrap_or_else(|| "connect failed — see daemon log for the specific reason".into());
    DaemonEvent::McpConnectFailed {
        server: name.to_string(),
        error,
    }
}

pub(in crate::session_pool) fn mcp_connected_event(
    status: &mcp::manager::ServerStatus,
) -> DaemonEvent {
    DaemonEvent::McpConnected {
        server: status.name.clone(),
        transport: status.transport.to_string(),
        tool_count: status.tool_count,
    }
}

/// The MCP server fields that carry credentials — request headers, the
/// process environment — which `mcp.addServer` / `mcp.updateServer` keep out
/// of the committed `settings.json`.
pub(in crate::session_pool) const MCP_SERVER_SECRET_KEYS: &[&str] = &["headers", "env"];

pub(in crate::session_pool) fn take_mcp_server_secrets(
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
pub(in crate::session_pool) fn mcp_servers_of(
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
