//! Listeners: binding and accept loops for the Unix socket and TCP, including
//! the refusal of connections from processes this daemon started.

use super::*;

/// A unix listener [`DaemonServer::bind_unix`] bound, and the sandbox fence
/// around its socket, which comes down when the listener does.
pub struct UnixBinding {
    listener: UnixListener,
    _fence: platform::sandbox::Fence,
}

/// The pid of the peer on `stream`, if this process started it.
pub(super) fn started_by_this_daemon(stream: &tokio::net::UnixStream) -> Option<u32> {
    let pid = u32::try_from(stream.peer_cred().ok()?.pid()?).ok()?;
    platform::lineage::started_here(pid).then_some(pid)
}

/// Whether the peer on `stream` runs as a different user than this daemon.
/// The socket is chmod 0600 only after `bind`, so for that window the file
/// mode is whatever the umask left; checking the peer on every accept closes
/// it without touching the process-wide umask, which other threads would
/// race with. A peer whose credentials cannot be read is treated as foreign.
pub(super) fn from_another_user(stream: &tokio::net::UnixStream) -> bool {
    match stream.peer_cred() {
        Ok(cred) => differs_from_daemon_user(cred.uid()),
        Err(_) => true,
    }
}

fn differs_from_daemon_user(peer_uid: u32) -> bool {
    #[cfg(unix)]
    {
        // SAFETY: geteuid has no preconditions and cannot fail.
        peer_uid != unsafe { libc::geteuid() }
    }
    #[cfg(not(unix))]
    {
        let _ = peer_uid;
        false
    }
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
        let _fence = listener
            .local_addr()
            .ok()
            .map(platform::sandbox::fence_port);
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
    pub fn bind_unix(socket_path: &Path) -> anyhow::Result<UnixBinding> {
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
        let fence = platform::sandbox::fence_socket(socket_path);
        info!(path=%socket_path.display());
        Ok(UnixBinding {
            listener,
            _fence: fence,
        })
    }

    /// Accept loop over an already-bound unix listener.
    ///
    /// A connection from a process this daemon started — a command, a hook,
    /// an MCP server, or what one of them left running — is refused: file
    /// permissions cannot tell it from the host, and as a client it could
    /// trust its own project or answer its own permission prompts. The
    /// lineage check misses a process that left its process group; under
    /// [`Self::require_unix_socket_auth`] such a process also needs the token.
    pub async fn serve_unix_listener(self: Arc<Self>, bound: UnixBinding) -> anyhow::Result<()> {
        let UnixBinding { listener, _fence } = bound;
        loop {
            tokio::select! {
                _ = self.shutdown_token.cancelled() => break,
                result = listener.accept() => {
                    match result {
                        Ok((stream, _addr)) => {
                            let this = self.clone();
                            tokio::spawn(async move {
                                if from_another_user(&stream) {
                                    warn!("refused a unix socket connection from another user");
                                    return;
                                }
                                if let Some(pid) = started_by_this_daemon(&stream) {
                                    refuse_own_descendant(stream, pid).await;
                                    return;
                                }
                                let needs_auth = this
                                    .unix_socket_auth
                                    .load(std::sync::atomic::Ordering::SeqCst);
                                let (r, w) = stream.into_split();
                                let writer: ByteWriter = Arc::new(AsyncMutex::new(Box::new(w)));
                                let sink: Sink = Arc::new(LineSink(writer));
                                if let Err(e) = this.handle_connection(Box::new(r), sink, needs_auth).await {
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn only_the_daemons_own_uid_is_not_foreign() {
        let mine = unsafe { libc::geteuid() };
        assert!(!differs_from_daemon_user(mine));
        assert!(differs_from_daemon_user(mine.wrapping_add(1)));
    }

    #[tokio::test]
    async fn a_peer_in_this_process_is_not_another_user() {
        let (a, b) = tokio::net::UnixStream::pair().unwrap();
        assert!(!from_another_user(&a));
        assert!(!from_another_user(&b));
    }
}
