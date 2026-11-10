//! Listeners: binding and accept loops for the Unix socket and TCP, including
//! the refusal of connections from processes this daemon started.

use super::*;

/// The pid of the peer on `stream`, if this process started it.
pub(super) fn started_by_this_daemon(stream: &tokio::net::UnixStream) -> Option<u32> {
    let pid = u32::try_from(stream.peer_cred().ok()?.pid()?).ok()?;
    base::interface::exec::local::lineage::started_here(pid).then_some(pid)
}

/// One error frame, so the refused process learns why, then the connection
/// is dropped unread.
pub(super) async fn refuse_own_descendant(mut stream: tokio::net::UnixStream, pid: u32) {
    warn!(
        pid,
        "refused a connection on the unix socket from a process this daemon started"
    );
    let refusal = RpcResponse::err(
        serde_json::Value::Null,
        codes::UNAUTHORIZED,
        "this daemon does not serve processes it started itself",
    );
    if let Ok(mut line) = serde_json::to_vec(&refusal) {
        line.push(b'\n');
        let _ = stream.write_all(&line).await;
    }
}

impl DaemonServer {
    pub async fn serve_tcp(self: Arc<Self>, addr: SocketAddr) -> anyhow::Result<()> {
        if self.tcp_token.read().await.is_none() {
            anyhow::bail!("TCP requires token");
        }
        let listener = TcpListener::bind(addr).await?;
        info!(addr=%addr, "TCP listener bound");
        self.serve_tcp_listener(listener).await
    }

    /// Accept loop over an already-bound TCP listener — split out from
    /// [`Self::serve_tcp`] so tests can bind an ephemeral port (`127.0.0.1:0`),
    /// read the OS-assigned port via `listener.local_addr()`, and drive this
    /// loop directly instead of guessing a fixed port.
    ///
    /// Re-checks (doesn't just trust `serve_tcp`'s check) that a token is
    /// configured — this is a `pub` method, so a caller could reach it
    /// without going through `serve_tcp` at all (e.g. binding the listener
    /// itself for socket-activation setups) and forgetting `set_tcp_token`
    /// first. Without this, that mistake wouldn't fail loudly — every
    /// connection would just silently get rejected by `authenticate_tcp`
    /// forever, which is safe but confusing to debug.
    pub async fn serve_tcp_listener(self: Arc<Self>, listener: TcpListener) -> anyhow::Result<()> {
        if self.tcp_token.read().await.is_none() {
            anyhow::bail!("TCP requires token");
        }
        if let Ok(addr) = listener.local_addr() {
            base::interface::exec::local::sandbox::fence_port(addr);
        }
        loop {
            tokio::select! {
                _ = self.shutdown_token.cancelled() => break,
                result = listener.accept() => {
                    match result {
                        Ok((stream, peer)) => {
                            debug!(peer=%peer);
                            let this = self.clone();
                            tokio::spawn(async move {
                                let (r, w) = stream.into_split();
                                let writer: ByteWriter = Arc::new(AsyncMutex::new(Box::new(w)));
                                let sink: Sink = Arc::new(LineSink(writer));
                                if let Err(e) = this.handle_connection(Box::new(r), sink, true).await {
                                    warn!(error=%e);
                                }
                            });
                        }
                        Err(e) => warn!(error=%e),
                    }
                }
            }
        }
        Ok(())
    }

    pub async fn serve_unix(self: Arc<Self>, socket_path: &Path) -> anyhow::Result<()> {
        let listener = Self::bind_unix(socket_path)?;
        self.serve_unix_listener(listener).await
    }

    /// Bind the unix socket, replacing a stale one, readable and writable by
    /// this user only; a directory it creates for it is this user's only
    /// too. Split from [`Self::serve_unix_listener`] so a caller can bind
    /// every listener before announcing the daemon — a client that finds it
    /// must be able to connect. No command this process sandboxes can connect
    /// to it.
    pub fn bind_unix(socket_path: &Path) -> anyhow::Result<UnixListener> {
        if let Some(parent) = socket_path.parent() {
            crate::discovery::create_private_dir(parent)?;
        }
        if socket_path.exists() {
            std::fs::remove_file(socket_path)?;
        }
        let listener = UnixListener::bind(socket_path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600))?;
        }
        base::interface::exec::local::sandbox::fence_socket(socket_path);
        info!(path=%socket_path.display());
        Ok(listener)
    }

    /// Accept loop over an already-bound unix listener.
    ///
    /// A connection from a process this daemon started — a command, a hook,
    /// an MCP server, or what one of them left running — is refused: file
    /// permissions cannot tell it from the host, and as a client it could
    /// trust its own project or answer its own permission prompts.
    pub async fn serve_unix_listener(
        self: Arc<Self>,
        listener: UnixListener,
    ) -> anyhow::Result<()> {
        loop {
            tokio::select! {
                _ = self.shutdown_token.cancelled() => break,
                result = listener.accept() => {
                    match result {
                        Ok((stream, _addr)) => {
                            let this = self.clone();
                            tokio::spawn(async move {
                                if let Some(pid) = started_by_this_daemon(&stream) {
                                    refuse_own_descendant(stream, pid).await;
                                    return;
                                }
                                let (r, w) = stream.into_split();
                                let writer: ByteWriter = Arc::new(AsyncMutex::new(Box::new(w)));
                                let sink: Sink = Arc::new(LineSink(writer));
                                if let Err(e) = this.handle_connection(Box::new(r), sink, false).await {
                                    warn!(error=%e);
                                }
                            });
                        }
                        Err(e) => warn!(error=%e),
                    }
                }
            }
        }
        Ok(())
    }
}
