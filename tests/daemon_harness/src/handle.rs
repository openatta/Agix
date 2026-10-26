//! A running daemon, and the connection to it.
//!
//! What a scenario needs from a daemon is a socket to talk to. How that
//! daemon came to exist is this module's business and nobody else's, which is
//! what lets one case run against a server built in this process and against
//! a spawned `agixd` without knowing which it got.
//!
//! The in-process build goes through `load_daemon_config` and
//! `daemon::assemble::pool` — the same two calls `main.rs` makes — rather
//! than assembling a pool by hand: the settings merge, the path resolution,
//! the history layout and the startup steps are all things a daemon-level
//! test is supposed to be exercising, and a harness that shortcuts them
//! tests its own shortcut.
//!
//! It takes its paths as arguments where `main.rs` reads the environment.
//! That is not a shortcut but a requirement: environment variables belong to
//! the whole process, and several of these run at once in one test binary.
//! The spawned build uses the environment, because a child process has one of
//! its own — which is also why it is the only mode that can answer anything
//! about startup, discovery, exit, or a binary built with other features.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use base::interface::permission::{Permission, PermissionOutcome};
use daemon::config::{load_daemon_config, DaemonPaths, StaticDaemonPaths};
use daemon::DaemonServer;
use model::client::{AnthropicClient, AuthMode, HttpAnthropicClient};
use rpc_client::DaemonRpcClient;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::provider::ProviderStub;
use crate::world::World;

pub const TOKEN: &str = "harness-token";

/// Where the daemon under test lives.
///
/// The difference is not meant to be visible to a scenario. Where it is —
/// carriers, startup, discovery, exit — that is the point of having two.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    InProcess,
    Spawned,
}

/// Which transport a connection uses. The daemon claims everything above
/// framing is identical on all three; a scenario that runs on each is how
/// that claim gets checked.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Wire {
    Unix,
    Tcp,
    Ws,
}

pub struct DaemonOptions {
    pub mode: Mode,
    pub scene: String,
    /// The daemon's own working directory — the project a session gets when
    /// `session.create` names none.
    pub project: PathBuf,
    pub session_cap: usize,
    pub prompt_timeout: Duration,
    /// Bind TCP and WebSocket listeners too. Off unless a scenario asks,
    /// because two extra listeners per daemon is two extra things to leak.
    pub extra_wires: bool,
    /// The discovery entry this daemon writes, in the modes that write one.
    pub instance: String,
    /// Which `agixd` to spawn. `None` takes the one built beside this
    /// test; a carrier case names another.
    pub binary: Option<PathBuf>,
    /// Extra arguments for a spawned daemon. Ignored in this process, which
    /// has no command line to put them on.
    pub extra_args: Vec<String>,
}

impl DaemonOptions {
    pub fn new(project: PathBuf) -> Self {
        Self {
            mode: Mode::InProcess,
            scene: "coding".to_string(),
            project,
            session_cap: 8,
            prompt_timeout: Duration::from_secs(10),
            extra_wires: false,
            instance: "harness".to_string(),
            binary: None,
            extra_args: Vec::new(),
        }
    }

    /// Spawn this binary instead of the one beside the test. The carriers
    /// are compile-time features, so a case about one is a case about a
    /// different build — there is no way to ask for it at runtime.
    pub fn binary(mut self, path: PathBuf) -> Self {
        self.binary = Some(path);
        self.mode = Mode::Spawned;
        self
    }

    pub fn mode(mut self, mode: Mode) -> Self {
        self.mode = mode;
        self
    }

    pub fn scene(mut self, scene: &str) -> Self {
        self.scene = scene.to_string();
        self
    }

    pub fn with_extra_wires(mut self) -> Self {
        self.extra_wires = true;
        self
    }

    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.extra_args.push(arg.into());
        self
    }
}

pub struct Daemon {
    socket: PathBuf,
    tcp: Option<SocketAddr>,
    ws: Option<SocketAddr>,
    running: Running,
}

enum Running {
    InProcess {
        cancel: CancellationToken,
        tasks: Vec<JoinHandle<()>>,
    },
    Spawned {
        child: tokio::process::Child,
        stderr: PathBuf,
    },
}

impl Daemon {
    pub async fn start(
        world: &World,
        provider: &ProviderStub,
        opts: DaemonOptions,
    ) -> anyhow::Result<Self> {
        match opts.mode {
            Mode::InProcess => Self::start_in_process(world, provider, opts).await,
            Mode::Spawned => Self::start_spawned(world, provider, opts).await,
        }
    }

    async fn start_in_process(
        world: &World,
        provider: &ProviderStub,
        opts: DaemonOptions,
    ) -> anyhow::Result<Self> {
        let paths =
            StaticDaemonPaths::with_project(world.config_root(&opts.scene), opts.project.clone())
                .with_global(world.global_root());
        std::fs::create_dir_all(paths.config_root())?;

        let socket = world.socket();
        let config = load_daemon_config(
            "claude-sonnet-4-6",
            2000,
            Some(&socket),
            &opts.scene,
            &paths,
        );

        let client: Arc<dyn AnthropicClient> = Arc::new(HttpAnthropicClient::with_base(
            AuthMode::ApiKey("harness".to_string()),
            provider.base_url(),
        )?);

        let mut registry = scene::scene::SceneRegistry::new();
        registry.register_builtin();
        let agent_scene = registry
            .resolve(&opts.scene)
            .ok_or_else(|| anyhow::anyhow!("unknown scene `{}`", opts.scene))?;

        let mut config = config;
        config.session_cap = opts.session_cap;

        // The same entry point `main.rs` uses, for the same reason a scenario
        // exists: a harness that assembled its own pool would be testing its
        // own assembly. Only the three things a spawned daemon would take
        // from its own process differ here — the project it serves, the model
        // it talks to, and how long an unanswered prompt may block.
        let pool = daemon::assemble::pool(
            &config,
            agent_scene,
            daemon::Assembly {
                cwd: Some(opts.project.clone()),
                permission_prompt_timeout: opts.prompt_timeout,
                model_client: Some(client),
                permission: Some(Arc::new(AllowAll) as Arc<dyn Permission>),
                ..Default::default()
            },
        )
        .await?;

        let cancel = CancellationToken::new();
        let server = Arc::new(DaemonServer::new(pool, cancel.clone()));
        server.set_tcp_token(TOKEN.to_string()).await;

        let mut tasks = Vec::new();
        {
            let server = server.clone();
            let socket = socket.clone();
            tasks.push(tokio::spawn(async move {
                let _ = server.serve_unix(&socket).await;
            }));
        }

        let (tcp, ws) = if opts.extra_wires {
            let tcp_listener = TcpListener::bind("127.0.0.1:0").await?;
            let tcp = tcp_listener.local_addr()?;
            {
                let server = server.clone();
                tasks.push(tokio::spawn(async move {
                    let _ = server.serve_tcp_listener(tcp_listener).await;
                }));
            }
            let ws_listener = TcpListener::bind("127.0.0.1:0").await?;
            let ws = ws_listener.local_addr()?;
            {
                let server = server.clone();
                tasks.push(tokio::spawn(async move {
                    let _ = daemon::ws::serve_ws_listener(server, ws_listener).await;
                }));
            }
            (Some(tcp), Some(ws))
        } else {
            (None, None)
        };

        let mut daemon = Self {
            socket,
            tcp,
            ws,
            running: Running::InProcess { cancel, tasks },
        };
        daemon.wait_until_answering().await?;
        Ok(daemon)
    }

    async fn start_spawned(
        world: &World,
        provider: &ProviderStub,
        opts: DaemonOptions,
    ) -> anyhow::Result<Self> {
        let (mut cmd, socket, stderr_path, tcp, ws) = spawn_command(world, provider, &opts).await?;
        let child = cmd.spawn()?;
        let mut daemon = Self {
            socket,
            tcp,
            ws,
            running: Running::Spawned {
                child,
                stderr: stderr_path,
            },
        };
        daemon.wait_until_answering().await?;
        Ok(daemon)
    }

    /// Start a daemon that is expected not to stay up, and report how it
    /// ended. The refusals worth testing — a network listener with no token,
    /// a `scripts` section in a build with no script engine — are all
    /// startup failures, and a harness that only knows how to wait for a
    /// healthy daemon cannot observe any of them.
    pub async fn spawn_and_wait(
        world: &World,
        provider: &ProviderStub,
        opts: DaemonOptions,
        timeout: Duration,
    ) -> anyhow::Result<(std::process::ExitStatus, String)> {
        let (mut cmd, _socket, stderr_path, _tcp, _ws) =
            spawn_command(world, provider, &opts).await?;
        let mut child = cmd.spawn()?;
        let status = match tokio::time::timeout(timeout, child.wait()).await {
            Ok(status) => status?,
            Err(_) => {
                let _ = child.kill().await;
                anyhow::bail!(
                    "daemon was still running after {timeout:?}; it was expected to refuse to start"
                )
            }
        };
        Ok((
            status,
            std::fs::read_to_string(&stderr_path).unwrap_or_default(),
        ))
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// What a spawned daemon wrote to stderr — empty for an in-process one,
    /// which has no separate stream to write to.
    pub fn stderr(&self) -> String {
        match &self.running {
            Running::Spawned { stderr, .. } => std::fs::read_to_string(stderr).unwrap_or_default(),
            Running::InProcess { .. } => String::new(),
        }
    }

    pub async fn connect(&self) -> anyhow::Result<DaemonRpcClient> {
        self.connect_via(Wire::Unix).await
    }

    pub async fn connect_via(&self, wire: Wire) -> anyhow::Result<DaemonRpcClient> {
        match wire {
            Wire::Unix => DaemonRpcClient::connect(&self.socket).await,
            Wire::Tcp => {
                let addr = self
                    .tcp
                    .ok_or_else(|| anyhow::anyhow!("daemon has no TCP listener"))?;
                DaemonRpcClient::connect_tcp(addr, TOKEN).await
            }
            Wire::Ws => {
                let addr = self
                    .ws
                    .ok_or_else(|| anyhow::anyhow!("daemon has no WebSocket listener"))?;
                DaemonRpcClient::connect_ws(addr, TOKEN, None).await
            }
        }
    }

    /// Wait for the daemon to exit on its own — after `daemon.shutdown`, say.
    /// `None` means it was still running when the wait ran out.
    pub async fn wait_for_exit(&mut self, timeout: Duration) -> Option<std::process::ExitStatus> {
        match &mut self.running {
            Running::Spawned { child, .. } => {
                tokio::time::timeout(timeout, child.wait()).await.ok()?.ok()
            }
            // Nothing to exit: the server is a task in this process, and
            // `daemon.shutdown` cancels it rather than ending anything.
            Running::InProcess { .. } => None,
        }
    }

    pub async fn stop(mut self) {
        match &mut self.running {
            Running::InProcess { cancel, tasks } => {
                cancel.cancel();
                for task in tasks.drain(..) {
                    task.abort();
                }
            }
            Running::Spawned { child, .. } => {
                // Ask first: the daemon removes its lock and discovery entry
                // on the way out, and a killed one leaves both behind, which
                // the next case in the same world would then read.
                if let Ok(mut client) = DaemonRpcClient::connect(&self.socket).await {
                    let _ = client.daemon_shutdown().await;
                }
                if tokio::time::timeout(Duration::from_secs(5), child.wait())
                    .await
                    .is_err()
                {
                    let _ = child.kill().await;
                }
            }
        }
    }

    /// Connected and answering, not merely listening. A spawned daemon has a
    /// socket well before it has a session pool, and a case that raced it
    /// would fail somewhere unrelated to what it was testing.
    async fn wait_until_answering(&mut self) -> anyhow::Result<()> {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            if let Ok(mut client) = DaemonRpcClient::connect(&self.socket).await {
                if let Ok(resp) = client.call("daemon.ping", serde_json::Value::Null).await {
                    if resp.error.is_none() {
                        return Ok(());
                    }
                }
            }
            // A daemon that has already exited will never answer, and the
            // reason it exited is in its stderr rather than in a timeout
            // twenty seconds from now.
            if let Running::Spawned { child, .. } = &mut self.running {
                if let Ok(Some(status)) = child.try_wait() {
                    anyhow::bail!("daemon exited during startup ({status})\n{}", self.stderr());
                }
            }
            if std::time::Instant::now() > deadline {
                anyhow::bail!(
                    "daemon never answered on {}\nstderr:\n{}",
                    self.socket.display(),
                    self.stderr()
                );
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}

type SpawnPlan = (
    tokio::process::Command,
    PathBuf,
    PathBuf,
    Option<SocketAddr>,
    Option<SocketAddr>,
);

/// The command line and environment a spawned daemon gets. Everything the
/// in-process build receives as an argument is passed here instead, which is
/// the whole difference between the two modes.
async fn spawn_command(
    world: &World,
    provider: &ProviderStub,
    opts: &DaemonOptions,
) -> anyhow::Result<SpawnPlan> {
    std::fs::create_dir_all(world.config_root(&opts.scene))?;
    let socket = world.socket();
    let stderr_path = world.root().join(format!("{}.stderr", opts.instance));
    let stderr = std::fs::File::create(&stderr_path)?;

    let (tcp, ws) = if opts.extra_wires {
        (Some(free_addr().await?), Some(free_addr().await?))
    } else {
        (None, None)
    };

    let binary = match &opts.binary {
        Some(path) => path.clone(),
        None => daemon_binary()?,
    };
    let mut cmd = tokio::process::Command::new(binary);
    cmd.arg("--socket")
        .arg(&socket)
        .arg("--scene")
        .arg(&opts.scene)
        .arg("--instance")
        .arg(&opts.instance)
        .arg("--session-cap")
        .arg(opts.session_cap.to_string())
        .arg("--permission-prompt-timeout")
        .arg(opts.prompt_timeout.as_secs().to_string());
    if let (Some(tcp), Some(ws)) = (tcp, ws) {
        cmd.arg("--listen")
            .arg(tcp.to_string())
            .arg("--listen-ws")
            .arg(ws.to_string())
            .arg("--token")
            .arg(TOKEN);
    }
    cmd.args(&opts.extra_args)
        .current_dir(&opts.project)
        .env("HOME", world.home())
        .env("AGIX_CONFIG_HOME", world.global_root())
        .env("ANTHROPIC_BASE_URL", provider.base_url().as_str())
        .env("ANTHROPIC_AUTH_TOKEN", "harness")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("AGIX_DAEMON_TOKEN")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr))
        // A panicking case must not leave a daemon behind holding the socket
        // the next one will try to bind.
        .kill_on_drop(true);

    Ok((cmd, socket, stderr_path, tcp, ws))
}

/// The `agixd` to spawn.
///
/// `AGIX_TEST_DAEMON_BIN` first, because the carrier matrix is exactly the
/// case where the binary under test is *not* the one this test binary was
/// built alongside. Otherwise the sibling of this test executable, which is
/// where cargo puts it.
pub fn daemon_binary() -> anyhow::Result<PathBuf> {
    if let Ok(path) = std::env::var("AGIX_TEST_DAEMON_BIN") {
        let path = PathBuf::from(path);
        anyhow::ensure!(
            path.exists(),
            "AGIX_TEST_DAEMON_BIN points at {}, which does not exist",
            path.display()
        );
        return Ok(path);
    }

    let exe = std::env::current_exe()?;
    let mut dir = exe
        .parent()
        .ok_or_else(|| anyhow::anyhow!("test executable has no directory"))?;
    if dir.file_name().is_some_and(|n| n == "deps") {
        dir = dir
            .parent()
            .ok_or_else(|| anyhow::anyhow!("`deps` has no parent"))?;
    }
    let candidate = dir.join("agixd");
    anyhow::ensure!(
        candidate.exists(),
        "no daemon binary at {} — build one with `cargo build -p daemon`, \
         or point AGIX_TEST_DAEMON_BIN at the one to test",
        candidate.display()
    );
    Ok(candidate)
}

/// A daemon built with features this test binary was not built with.
///
/// It cannot be found by looking beside the test executable: both builds
/// produce a binary called `agixd`, so the second one has to be built
/// into a target directory of its own and named here. CI does that; a
/// developer running these by hand is told how.
pub fn alternate_daemon_binary(var: &str, how_to_build: &str) -> anyhow::Result<PathBuf> {
    let path = std::env::var(var).map_err(|_| {
        anyhow::anyhow!(
            "{var} is not set, so there is no daemon to run this against.\n\
             Build one and point at it:\n  {how_to_build}"
        )
    })?;
    let path = PathBuf::from(path);
    anyhow::ensure!(
        path.exists(),
        "{var} points at {}, which does not exist",
        path.display()
    );
    Ok(path)
}

/// A port nothing is listening on. Racy by construction — the listener is
/// closed before the daemon binds it — but a spawned daemon has to be told an
/// address before it starts, and `:0` gives an address only it would know.
async fn free_addr() -> anyhow::Result<SocketAddr> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    drop(listener);
    Ok(addr)
}

/// The pool's permission of last resort, matching what `main.rs` hands it.
/// A session only ever receives it in `bypassPermissions`; everything else
/// builds a real `RuleSetPermission` from its own settings.
struct AllowAll;

#[async_trait::async_trait]
impl Permission for AllowAll {
    async fn check(
        &self,
        _tool: &str,
        _input: &serde_json::Value,
        _cwd: &std::path::Path,
        _session: &str,
    ) -> PermissionOutcome {
        PermissionOutcome::Permit
    }
}
