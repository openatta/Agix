//! The outside world of an MCP sign-in: a streamable-HTTP MCP server that
//! answers only with a token its authorization server issued, that
//! authorization server, and on demand an enterprise IdP. They are the
//! `mcp-oauth-mock` fixture `daemon/tests` drives with a scripted model; here
//! a live case points a real one at them.
//!
//! [`DaemonEvents`] is the host's side of a sign-in: a connection subscribed
//! to `daemon.event`, where `mcp_auth_required`, `mcp_auth_completed` and
//! `mcp_connected` arrive.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::Daemon;

pub use mcp_oauth_mock::{
    click, consent, follow, AuthServer, McpServer, JWT_BEARER, TOKEN_EXCHANGE,
};

/// The MCP server is `mcp` (tools `echo` and `write`, the latter needing the
/// `mcp:write` scope), its authorization server `auth`.
pub struct OAuthWorld {
    pub mcp: McpServer,
    pub auth: AuthServer,
    pub idp: Option<AuthServer>,
}

impl OAuthWorld {
    pub async fn start() -> Self {
        let (mcp, auth) = McpServer::start().await;
        Self {
            mcp,
            auth,
            idp: None,
        }
    }

    /// Also an enterprise IdP that accepts `identity` as a subject token and
    /// issues identity assertions for `server_client` (the client registered
    /// at the MCP server's authorization server), which it registers there.
    pub async fn with_enterprise_idp(
        mut self,
        idp_client: &str,
        identity: &str,
        server_client: &str,
    ) -> Self {
        self.auth.preregister(server_client, None);
        let idp = AuthServer::start(&self.mcp.url).await;
        idp.preregister(idp_client, None);
        {
            let mut s = idp.state.lock().unwrap();
            s.identities.push(identity.to_string());
            s.jag_client_id = server_client.to_string();
        }
        self.idp = Some(idp);
        self
    }

    pub fn url(&self) -> &str {
        &self.mcp.url
    }

    /// The `mcp_servers.<name>` entry for the server, with no `oauth` block:
    /// its 401 is what starts the sign-in.
    pub fn config(&self) -> Value {
        json!({ "type": "streamable_http", "url": self.mcp.url })
    }

    pub fn config_with(&self, oauth: Value) -> Value {
        json!({ "type": "streamable_http", "url": self.mcp.url, "oauth": oauth })
    }

    /// The MCP policy's `enterprise_idp` for the IdP of
    /// [`with_enterprise_idp`](Self::with_enterprise_idp), as a public client.
    pub fn enterprise_idp_policy(&self, idp_client: &str) -> Value {
        let idp = self.idp.as_ref().expect("with_enterprise_idp first");
        json!({ "enterprise_idp": {
            "issuer": idp.url,
            "token_endpoint": format!("{}/token", idp.url),
            "client_id": idp_client,
        } })
    }

    /// What every tool result of the server carries, and nothing else can.
    pub fn marker(&self) -> String {
        self.mcp.state.lock().unwrap().marker.clone()
    }

    /// Every token the authorization server and the IdP have issued.
    pub fn tokens(&self) -> Vec<String> {
        let mut out = self.auth.issued();
        if let Some(idp) = &self.idp {
            out.extend(idp.issued());
        }
        out
    }

    /// The requests the authorization server was sent, as `METHOD /path`.
    pub fn auth_requests(&self) -> Vec<String> {
        self.auth.state.lock().unwrap().requests.clone()
    }

    /// The token that is in `text`, if any is.
    pub fn token_in(&self, text: &str) -> Option<String> {
        self.tokens()
            .into_iter()
            .find(|t| text.contains(t.as_str()))
    }

    /// Each file under `dir` holding one of the tokens, outside the daemon's
    /// credential store (`mcp/oauth/`), which is where they belong.
    pub fn files_with_a_token(&self, dir: &Path) -> Vec<PathBuf> {
        let tokens = self.tokens();
        let mut found = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for entry in std::fs::read_dir(&d).into_iter().flatten().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    if !path.ends_with("mcp/oauth") {
                        stack.push(path);
                    }
                    continue;
                }
                let text =
                    String::from_utf8_lossy(&std::fs::read(&path).unwrap_or_default()).into_owned();
                if tokens.iter().any(|t| text.contains(t.as_str())) {
                    found.push(path);
                }
            }
        }
        found
    }
}

/// A connection subscribed to `daemon.event`, keeping every event in order.
///
/// A daemon connects its configured servers after it starts answering, so
/// what they announce at startup may or may not arrive here; a case that
/// waits for an announcement [`clear`](Self::clear)s first, once that is
/// settled.
pub struct DaemonEvents {
    seen: Arc<Mutex<Vec<Value>>>,
    taken: Mutex<std::collections::HashSet<usize>>,
    reader: tokio::task::JoinHandle<()>,
}

impl DaemonEvents {
    pub async fn start(daemon: &Daemon) -> anyhow::Result<Self> {
        let mut client = daemon.connect().await?;
        let subscribed = client.call("daemon.subscribeEvents", json!({})).await?;
        anyhow::ensure!(subscribed.error.is_none(), "{subscribed:?}");
        let seen = Arc::<Mutex<Vec<Value>>>::default();
        let into = seen.clone();
        let reader = tokio::spawn(async move {
            while let Ok(Some(frame)) = client.next_frame().await {
                if frame["method"] == "daemon.event" {
                    lock(&into).push(frame["params"].clone());
                }
            }
        });
        Ok(Self {
            seen,
            taken: Mutex::default(),
            reader,
        })
    }

    pub fn all(&self) -> Vec<Value> {
        lock(&self.seen).clone()
    }

    /// Set aside every event so far: [`pending`](Self::pending) and
    /// [`next`](Self::next) see only what arrives after this.
    pub fn clear(&self) {
        let seen = lock(&self.seen).len();
        lock(&self.taken).extend(0..seen);
    }

    /// The events of `kind` about `server` not yet returned or cleared.
    pub fn pending(&self, kind: &str, server: &str) -> Vec<Value> {
        let taken = lock(&self.taken);
        lock(&self.seen)
            .iter()
            .enumerate()
            .filter(|(i, e)| !taken.contains(i) && e["kind"] == kind && e["server"] == server)
            .map(|(_, e)| e.clone())
            .collect()
    }

    /// The earliest event of `kind` about `server` this has not returned
    /// before, waiting up to `timeout` for one.
    pub async fn next(&self, kind: &str, server: &str, timeout: Duration) -> anyhow::Result<Value> {
        let deadline = Instant::now() + timeout;
        loop {
            {
                let seen = lock(&self.seen);
                let mut taken = lock(&self.taken);
                let found = seen.iter().enumerate().find(|(i, e)| {
                    !taken.contains(i) && e["kind"] == kind && e["server"] == server
                });
                if let Some((i, event)) = found {
                    taken.insert(i);
                    return Ok(event.clone());
                }
            }
            anyhow::ensure!(
                Instant::now() < deadline,
                "no {kind} for {server} within {timeout:?}; events: {:?}",
                self.all()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

impl Drop for DaemonEvents {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}
