//! `agixd` — daemon binary.
//!
//! Builds a multi-session agent engine pool at startup and exposes it via
//! JSON-RPC 2.0 over Unix socket (or TCP with token auth).
//!
//! # Startup profiling
//!
//! A [`PerfCollector`] marks when the configuration is read and when the pool
//! is assembled; once the daemon is ready a [`StartupTimingPayload`] goes to the
//! pool-level telemetry (if telemetry is enabled).

use std::path::PathBuf;
use std::sync::Arc;

use daemon::{config::*, write_lock_file, DaemonServer};
use telemetry::events::StartupTimingPayload;
use telemetry::perf::PerfCollector;
use tokio_util::sync::CancellationToken;
use tracing::info;

const HELP: &str = "\
Agix daemon: multi-session agent engine over JSON-RPC

Usage: agixd [OPTIONS]

Options:
      --socket <PATH>              Unix socket path [default: $HOME/.agix/scenes/<scene>/daemon.sock]
      --session-cap <N>            Maximum concurrent sessions [default: 32]
      --session-idle-timeout <SECS>
                                   Session idle timeout in seconds [default: 3600]
      --model <NAME>               Model name for sessions [default: claude-sonnet-4-6]
      --max-tokens <N>             Max tokens per turn [default: 32000]
      --listen <ADDR>              Bind a TCP listener at ADDR (e.g. 127.0.0.1:7878);
                                   requires --token or AGIX_DAEMON_TOKEN
      --listen-ws <ADDR>           Bind a WebSocket listener at a loopback ADDR
                                   (e.g. 127.0.0.1:7879); requires --token or AGIX_DAEMON_TOKEN
      --token <SECRET>             Shared secret for TCP and WebSocket auth [env: AGIX_DAEMON_TOKEN]
      --unix-socket-auth           The unix socket, too, requires daemon.auth: with the
                                   --token / AGIX_DAEMON_TOKEN secret when one is given, else
                                   with a fresh one written to
                                   ~/.agix/daemon/auth/<instance>.token
      --scene <ID>                 Scene this daemon serves; also selects the state root
                                   ~/.agix/scenes/<scene>/ [default: code]
      --scenes <ID,...>            Additional scenes to activate alongside --scene
      --instance <NAME>            Name of this daemon's ~/.agix/daemon/instances.d/ entry
                                   [default: the active scenes, sorted and comma-joined]
      --permission-prompt-timeout <SECS>
                                   Seconds a permission prompt may go unanswered before it is
                                   denied (0 = wait forever) [default: 300]
      --worker-threads <N>         Async worker threads [env: AGIX_WORKER_THREADS]
                                   [default: the number of CPU cores]
      --mcp-policy <PATH>          MCP policy file to enforce instead of the managed one
                                   [default: /Library/Application Support/Agix/mcp-policy.json
                                   on macOS, %ProgramData%\\Agix\\mcp-policy.json on Windows,
                                   /etc/agix/mcp-policy.json elsewhere]
      --managed-settings <PATH>    Managed settings file to enforce instead of the system one
                                   [default: /Library/Application Support/Agix/managed-settings.json
                                   on macOS, %ProgramData%\\Agix\\managed-settings.json on
                                   Windows, /etc/agix/managed-settings.json elsewhere]
  -h, --help                       Print help
  -V, --version                    Print version
";

#[derive(Debug)]
struct Cli {
    /// Unix socket path (default: $HOME/.agix/scenes/<scene>/daemon.sock)
    socket: Option<PathBuf>,

    /// Maximum concurrent sessions
    session_cap: usize,

    /// Session idle timeout in seconds (default: 3600 = 1 hour)
    session_idle_timeout: u64,

    /// Model name for sessions
    model: String,

    /// Max tokens per turn
    max_tokens: u32,

    /// Bind a TCP listener at this addr (e.g. 127.0.0.1:7878).
    /// Requires --token or AGIX_DAEMON_TOKEN env.
    listen: Option<String>,

    /// Bind a WebSocket listener at this addr (e.g. 127.0.0.1:7879), so a
    /// browser front end can reach the daemon directly. Loopback only, and
    /// requires --token or AGIX_DAEMON_TOKEN env.
    listen_ws: Option<String>,

    /// Shared secret for TCP and WebSocket auth. Falls back to
    /// `AGIX_DAEMON_TOKEN`.
    token: Option<String>,

    /// Which scene this daemon serves (must be one the code actually
    /// registers — see `resolve_scene()`). Determines both session behavior
    /// (system prompt/tools) and the user-level state root (`~/.agix/scenes/<scene>/`).
    /// There is no default scope concept separate from this — an unsupported
    /// value fails startup outright rather than silently falling back to
    /// anything.
    scene: String,

    /// Additional scenes to activate alongside `--scene`, comma-separated
    /// (e.g. `--scenes harnix,mini`). `--scene`'s own value doesn't need
    /// repeating here — it's always active. Each must be one
    /// `scene::SceneRegistry::register_builtin` actually registers;
    /// an unknown name fails startup the same way an unsupported `--scene`
    /// does. Session creation still defaults to `--scene`'s scene;
    /// `session.create {"scene": "..."}` is how a caller reaches one of
    /// these instead.
    scenes: Vec<String>,

    /// Named identity for this daemon instance — the filename of its
    /// `~/.agix/daemon/instances.d/<instance>.json` discovery entry
    /// (`docs/daemon_rpc_protocol.md` §2). Must be unique among daemons running on
    /// the same machine; two live instances racing to write the same name
    /// isn't reconciled here (each just overwrites the file — see
    /// `discovery::write_instance_file`), so operators are expected to pick
    /// distinct names when running more than one daemon.
    ///
    /// Default (when omitted): every active scene (`--scene` plus
    /// `--scenes`), deduplicated, sorted, and comma-joined — e.g.
    /// `--scene code --scenes harnix,mini` defaults to
    /// `code,harnix,mini`. See `default_instance_name()`.
    instance: Option<String>,

    /// Seconds a permission prompt may go unanswered before the daemon
    /// denies it on the client's behalf (0 = wait forever).
    ///
    /// The engine itself waits indefinitely for `session.respondToPrompt`, so a
    /// client that never implements it would otherwise hang its
    /// `session.run_turn` call forever. The timeout fails *closed* — it
    /// denies the tool call, never approves it — and the turn then continues
    /// normally with an error `tool_result`.
    permission_prompt_timeout: u64,

    /// Async worker threads; `None` leaves tokio's default of one per CPU
    /// core. Blocking work (file I/O, searches, scripts) runs on its own pool
    /// whatever this is.
    worker_threads: Option<usize>,

    /// The MCP policy file, in place of the platform's managed path. Unlike
    /// that one it must exist: a policy someone named and that is not there
    /// denies every MCP server rather than none.
    mcp_policy: Option<PathBuf>,

    /// The managed settings file, in place of the platform's. It must exist:
    /// one someone named and that is not there refuses every tool call.
    managed_settings: Option<PathBuf>,

    /// The unix socket requires `daemon.auth` as TCP does.
    unix_socket_auth: bool,
}

fn number<T: std::str::FromStr>(parser: &mut lexopt::Parser, flag: &str) -> Result<T, lexopt::Error>
where
    T::Err: std::fmt::Display,
{
    use lexopt::prelude::*;
    let value = parser.value()?.string()?;
    value
        .parse()
        .map_err(|e| format!("invalid value '{value}' for '--{flag}': {e}").into())
}

enum Invocation {
    Run(Box<Cli>),
    Help,
    Version,
}

impl Cli {
    fn parse() -> Self {
        let parsed = Self::parse_from(std::env::args_os().skip(1)).and_then(|invocation| {
            let Invocation::Run(mut cli) = invocation else {
                return Ok(invocation);
            };
            if cli.worker_threads.is_none() {
                if let Some(value) = std::env::var_os("AGIX_WORKER_THREADS") {
                    cli.worker_threads = Some(worker_threads(&value.to_string_lossy())?);
                }
            }
            Ok(Invocation::Run(cli))
        });
        match parsed {
            Ok(Invocation::Run(cli)) => *cli,
            Ok(Invocation::Help) => {
                print!("{HELP}");
                std::process::exit(0)
            }
            Ok(Invocation::Version) => {
                println!("agixd {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0)
            }
            Err(e) => {
                eprintln!("error: {e}\n\nFor more information, try '--help'.");
                std::process::exit(2)
            }
        }
    }

    fn parse_from(
        args: impl IntoIterator<Item = std::ffi::OsString>,
    ) -> Result<Invocation, lexopt::Error> {
        use lexopt::prelude::*;
        let mut cli = Cli {
            socket: None,
            session_cap: 32,
            session_idle_timeout: 3600,
            model: "claude-sonnet-4-6".into(),
            max_tokens: base::settings::DEFAULT_MAX_TOKENS,
            listen: None,
            listen_ws: None,
            token: None,
            scene: "code".into(),
            scenes: Vec::new(),
            instance: None,
            permission_prompt_timeout: 300,
            worker_threads: None,
            mcp_policy: None,
            managed_settings: None,
            unix_socket_auth: false,
        };
        let mut parser = lexopt::Parser::from_args(args);
        while let Some(arg) = parser.next()? {
            match arg {
                Long("socket") => cli.socket = Some(parser.value()?.into()),
                Long("session-cap") => cli.session_cap = number(&mut parser, "session-cap")?,
                Long("session-idle-timeout") => {
                    cli.session_idle_timeout = number(&mut parser, "session-idle-timeout")?
                }
                Long("model") => cli.model = parser.value()?.string()?,
                Long("max-tokens") => cli.max_tokens = number(&mut parser, "max-tokens")?,
                Long("listen") => cli.listen = Some(parser.value()?.string()?),
                Long("listen-ws") => cli.listen_ws = Some(parser.value()?.string()?),
                Long("token") => cli.token = Some(parser.value()?.string()?),
                Long("scene") => cli.scene = parser.value()?.string()?,
                Long("scenes") => cli.scenes.extend(
                    parser
                        .value()?
                        .string()?
                        .split(',')
                        .filter(|s| !s.is_empty())
                        .map(str::to_string),
                ),
                Long("instance") => cli.instance = Some(parser.value()?.string()?),
                Long("permission-prompt-timeout") => {
                    cli.permission_prompt_timeout =
                        number(&mut parser, "permission-prompt-timeout")?
                }
                Long("worker-threads") => {
                    cli.worker_threads = Some(worker_threads(&parser.value()?.string()?)?)
                }
                Long("mcp-policy") => cli.mcp_policy = Some(parser.value()?.into()),
                Long("managed-settings") => cli.managed_settings = Some(parser.value()?.into()),
                Long("unix-socket-auth") => cli.unix_socket_auth = true,
                Short('h') | Long("help") => return Ok(Invocation::Help),
                Short('V') | Long("version") => return Ok(Invocation::Version),
                _ => return Err(arg.unexpected()),
            }
        }
        Ok(Invocation::Run(Box::new(cli)))
    }
}

fn worker_threads(value: &str) -> Result<usize, lexopt::Error> {
    match value.parse::<usize>() {
        Ok(n) if n > 0 => Ok(n),
        _ => Err(format!(
            "invalid worker thread count '{value}': expected an integer of at least 1"
        )
        .into()),
    }
}

fn runtime(worker_threads: Option<usize>) -> std::io::Result<tokio::runtime::Runtime> {
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    if let Some(n) = worker_threads {
        builder.worker_threads(n);
    }
    builder.enable_all().build()
}

/// The scene `--scene` names: a built-in, or a declarative scene under
/// `user_scenes` (`<state root>/scenes/<id>/scene.json`).
fn resolve_scene(
    name: &str,
    user_scenes: &std::path::Path,
) -> anyhow::Result<Arc<base::interface::scene::SceneSpec>> {
    let mut registry = scene::SceneRegistry::new();
    registry.register_builtin();
    for e in registry.register_declarative_dir(user_scenes) {
        tracing::warn!(error = %e, "a declarative scene was not loaded");
    }
    registry.resolve(name).ok_or_else(|| {
        let mut known = registry.ids();
        known.sort();
        anyhow::anyhow!(
            "unsupported --scene `{name}` — supported scenes: {}",
            known.join(", ")
        )
    })
}

/// Every scene this daemon activates at startup — `--scene` plus
/// `--scenes`, deduplicated and sorted. Sorting makes this (and anything
/// derived from it, like [`default_instance_name`]) independent of the
/// order `--scenes` happened to list them in.
fn active_scenes(scene: &str, scenes: &[String]) -> Vec<String> {
    let mut all: Vec<String> = std::iter::once(scene.to_string())
        .chain(scenes.iter().cloned())
        .collect();
    all.sort();
    all.dedup();
    all
}

/// Default `--instance` name: the active scene set, sorted and joined with
/// `,` — `docs/daemon_rpc_protocol.md` §2 specifies "排序后连接" for the analogous
/// default socket name without pinning a delimiter; comma matches
/// `--scenes`' own list syntax.
fn default_instance_name(scene: &str, scenes: &[String]) -> String {
    active_scenes(scene, scenes).join(",")
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    runtime(cli.worker_threads)?.block_on(serve(cli))
}

async fn serve(cli: Cli) -> anyhow::Result<()> {
    let perf = PerfCollector::new();

    init_logging();
    let scene = resolve_scene(
        &cli.scene,
        &DefaultDaemonPaths::from_env(&cli.scene)
            .global_root()
            .join("scenes"),
    )?;
    let scope = scene.id.clone();

    let mut daemon_config = daemon_config_from_cli(&cli, &scope)?;
    let daemon_root = daemon_config.paths.global_root().join("daemon");
    platform::crash::install(
        daemon_root.join("crashes"),
        "agixd",
        env!("CARGO_PKG_VERSION"),
    );

    configure_network_listeners(&cli, &mut daemon_config)?;

    let socket = daemon_config.socket_path.clone();
    let lock_path = daemon_config.lock_path.clone();
    if let Some(parent) = socket.parent() {
        daemon::discovery::create_private_dir(parent)?;
    }

    // Held until this function returns, which removes the file.
    let lock = match write_lock_file(&lock_path, &socket) {
        Ok(lock) => lock,
        Err(e) => anyhow::bail!("could not claim daemon lock: {e}"),
    };
    info!(pid = lock.pid, "wrote discovery lock");

    let instance_scenes = active_scenes(&cli.scene, &cli.scenes);
    let instance_name = cli
        .instance
        .clone()
        .unwrap_or_else(|| default_instance_name(&cli.scene, &cli.scenes));
    let instances_dir = daemon_root.join("instances.d");

    // Only after the lock is ours: a second daemon refused above must not
    // touch the first one's commands.
    let groups_file = daemon_root
        .join("groups")
        .join(format!("{instance_name}.json"));
    reap_previous_groups(&groups_file).await?;

    let (unix_socket_token, token_file) =
        unix_socket_auth_token(&cli, &daemon_config, &daemon_root, &instance_name)?;
    let instance_file = instance_entry(
        &instance_name,
        instance_scenes,
        &daemon_config,
        unix_socket_token.is_some(),
        token_file.as_ref(),
    );
    let _config_load_ms = perf.checkpoint("config_loaded");
    info!(elapsed_ms = _config_load_ms, "startup: config loaded");

    let pool = build_pool(&cli, &daemon_config, scene).await?;
    let pool_built_ms = perf.checkpoint("pool_built");
    info!(
        elapsed_ms = pool_built_ms,
        "startup: session pool assembled"
    );

    spawn_import_detection(pool.clone());

    pool.start_janitor();

    record_startup_timing(&pool, &perf, _config_load_ms, pool_built_ms);

    let cancel = CancellationToken::new();
    let server = Arc::new(DaemonServer::new(pool.clone(), cancel.clone()));

    spawn_shutdown_handler(server.clone());

    set_transport_tokens(&server, &daemon_config, unix_socket_token).await;

    bind_network_listeners(&server, &daemon_config).await?;

    let unix = DaemonServer::bind_unix(&socket)?;

    announce_instance(&instances_dir, &instance_name, &instance_file);

    server.clone().serve_unix_listener(unix).await?;
    shut_down_sessions(&pool).await;
    drop(lock);
    let _ = std::fs::remove_file(&groups_file);
    drop(token_file);
    daemon::discovery::remove_instance_file(&instances_dir, &instance_name);
    info!("daemon stopped");
    Ok(())
}

fn configure_network_listeners(
    cli: &Cli,
    daemon_config: &mut daemon::config::DaemonConfig,
) -> anyhow::Result<()> {
    if let Some(ref addr_str) = cli.listen {
        let addr: std::net::SocketAddr = addr_str
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid --listen `{addr_str}`: {e}"))?;
        let token = cli
            .token
            .clone()
            .or_else(|| std::env::var("AGIX_DAEMON_TOKEN").ok())
            .ok_or_else(|| anyhow::anyhow!("--listen requires --token or env AGIX_DAEMON_TOKEN"))?;
        if !addr.ip().is_loopback() {
            tracing::warn!(
                %addr,
                "--listen binds an address reachable from the network; the connection is not \
                 encrypted, so the token and everything after it travel in the clear"
            );
        }
        daemon_config.tcp_addr = Some(addr);
        daemon_config.tcp_token = Some(token);
    }

    #[cfg(not(feature = "ws"))]
    if cli.listen_ws.is_some() {
        anyhow::bail!("--listen-ws: this build has no WebSocket listener (feature `ws`)");
    }
    if let Some(ref addr_str) = cli.listen_ws {
        let addr: std::net::SocketAddr = addr_str
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid --listen-ws `{addr_str}`: {e}"))?;
        // Refused rather than warned about: this transport is what a browser
        // connects to, and a browser reaching it from another machine would
        // mean the token is the only thing between the daemon and the network.
        if !addr.ip().is_loopback() {
            anyhow::bail!(
                "--listen-ws must bind a loopback address; `{addr}` is reachable from the network"
            );
        }
        let token = cli
            .token
            .clone()
            .or_else(|| std::env::var("AGIX_DAEMON_TOKEN").ok())
            .or_else(|| daemon_config.tcp_token.clone())
            .ok_or_else(|| {
                anyhow::anyhow!("--listen-ws requires --token or env AGIX_DAEMON_TOKEN")
            })?;
        daemon_config.ws_addr = Some(addr);
        daemon_config.tcp_token = Some(token);
    }
    Ok(())
}

fn unix_socket_auth_token(
    cli: &Cli,
    daemon_config: &daemon::config::DaemonConfig,
    daemon_root: &std::path::Path,
    instance_name: &str,
) -> anyhow::Result<(
    Option<String>,
    Option<daemon::discovery::UnixSocketTokenFile>,
)> {
    let unix_socket_token = if cli.unix_socket_auth {
        let given = cli
            .token
            .clone()
            .or_else(|| std::env::var("AGIX_DAEMON_TOKEN").ok())
            .or_else(|| daemon_config.tcp_token.clone());
        Some(match given {
            Some(token) => (token, None),
            None => {
                let path = daemon::discovery::unix_socket_token_path(daemon_root, instance_name);
                let (token, file) =
                    daemon::discovery::UnixSocketTokenFile::create(&path).map_err(|e| {
                        anyhow::anyhow!(
                            "--unix-socket-auth: could not write {}: {e}",
                            path.display()
                        )
                    })?;
                (token, Some(file))
            }
        })
    } else {
        None
    };
    Ok(match unix_socket_token {
        Some((token, file)) => (Some(token), file),
        None => (None, None),
    })
}

/// `daemon` is headless: no client is connected yet, so there is nobody to
/// ask "import from X?" synchronously. Detection emits an `import_detected`
/// event instead, which a subscribed client acts on through `import.list` /
/// `import.run`; the `.imported.json` marker keeps it from nagging on every restart.
fn spawn_import_detection(pool: Arc<daemon::session_pool::SessionPool>) {
    let import_cwd = pool.cwd().to_path_buf();
    tokio::spawn(async move {
        if base::frozen::import_already_decided(&import_cwd).await {
            return;
        }
        let sources = base::frozen::detect_import_sources(&import_cwd).await;
        if sources.is_empty() {
            return;
        }
        pool.emit_event(import_detected(&sources));
    });
}

/// Ctrl-C, and the SIGTERM a supervisor stops a service with, shut down
/// gracefully; a second one exits at once.
fn spawn_shutdown_handler(server: Arc<DaemonServer>) {
    tokio::spawn(async move {
        shutdown_signal().await;
        tokio::spawn(async {
            shutdown_signal().await;
            std::process::exit(130);
        });
        server.shutdown().await;
    });
}

/// Bound before the daemon is announced, so a port already in use fails
/// startup instead of leaving a daemon running without the transport the user
/// asked for, and so a client that finds the `instances.d` entry can connect at once.
async fn bind_network_listeners(
    server: &Arc<DaemonServer>,
    daemon_config: &daemon::config::DaemonConfig,
) -> anyhow::Result<()> {
    // `instances.d` entry can connect at once.
    if let Some(addr) = daemon_config.tcp_addr {
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|e| anyhow::anyhow!("could not bind --listen `{addr}`: {e}"))?;
        info!(%addr, "TCP listener bound (token-auth required)");
        let s = server.clone();
        tokio::spawn(async move { s.serve_tcp_listener(listener).await });
    }

    if let Some(addr) = daemon_config.ws_addr {
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|e| anyhow::anyhow!("could not bind --listen-ws `{addr}`: {e}"))?;
        info!(%addr, "WebSocket listener bound (token-auth required)");
        let s = server.clone();
        #[cfg(feature = "ws")]
        tokio::spawn(async move { daemon::ws::serve_ws_listener(s, listener).await });
        #[cfg(not(feature = "ws"))]
        drop((s, listener));
    }
    Ok(())
}

fn init_logging() {
    tracing_subscriber::fmt()
        .with_env_filter(daemon::logging::log_filter(
            std::env::var("RUST_LOG").ok().as_deref(),
        ))
        .init();
}

/// `daemon::assemble` does all the wiring; this binary adds only what a
/// process has: its working directory, command line and listeners.
async fn build_pool(
    cli: &Cli,
    daemon_config: &DaemonConfig,
    scene: Arc<base::interface::scene::SceneSpec>,
) -> anyhow::Result<Arc<daemon::session_pool::SessionPool>> {
    daemon::assemble::pool(
        daemon_config,
        scene,
        daemon::Assembly {
            cwd: Some(std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))),
            extra_scenes: cli.scenes.clone(),
            permission_prompt_timeout: std::time::Duration::from_secs(
                cli.permission_prompt_timeout,
            ),
            ..Default::default()
        },
    )
    .await
}

fn daemon_config_from_cli(cli: &Cli, scope: &str) -> anyhow::Result<DaemonConfig> {
    let paths = DefaultDaemonPaths::from_env(scope);
    let mut daemon_config = load_daemon_config(
        &cli.model,
        cli.max_tokens,
        cli.socket.as_deref(),
        scope,
        &paths,
    );
    daemon_config.session_cap = cli.session_cap;
    daemon_config.session_idle_timeout_secs = cli.session_idle_timeout;
    #[cfg(not(feature = "mcp"))]
    if cli.mcp_policy.is_some() {
        anyhow::bail!("--mcp-policy: this build has no MCP module (feature `mcp`)");
    }
    daemon_config.mcp_policy_path = cli.mcp_policy.clone();
    daemon_config.managed_settings_path = cli.managed_settings.clone();
    Ok(daemon_config)
}

/// Only after the daemon lock is ours: a second daemon refused by it must not
/// touch the first one's commands.
async fn reap_previous_groups(groups_file: &std::path::Path) -> anyhow::Result<()> {
    let path = groups_file.to_path_buf();
    let reaped = tokio::task::spawn_blocking(move || {
        platform::orphans::reap_previous(&path, std::time::Duration::from_millis(500))
    })
    .await?;
    if !reaped.is_empty() {
        tracing::warn!(
            groups = ?reaped,
            "stopped process groups a previous run of this instance left running"
        );
    }
    platform::orphans::persist_to(groups_file.to_path_buf());
    Ok(())
}

/// The multi-scene `instances.d` discovery entry, written alongside the legacy
/// single-scene `daemon.lock` (additive; see `daemon::discovery`). It lives
/// under the global root, shared by every scene, because one instance file
/// must be reachable whichever of this daemon's scenes a client cares about.
fn instance_entry(
    instance_name: &str,
    scenes: Vec<String>,
    daemon_config: &DaemonConfig,
    unix_socket_auth: bool,
    token_file: Option<&daemon::discovery::UnixSocketTokenFile>,
) -> daemon::discovery::InstanceFile {
    let identity = platform::process_identity::ProcessIdentity::current();
    daemon::discovery::InstanceFile {
        instance: instance_name.to_string(),
        pid: identity.pid,
        pid_start_time: identity.started_at,
        socket: daemon_config.socket_path.clone(),
        tcp: daemon_config.tcp_addr.map(|a| a.to_string()),
        ws: daemon_config.ws_addr.map(|a| a.to_string()),
        unix_socket_auth,
        token_file: token_file.map(|f| f.path().to_path_buf()),
        scenes,
        protocol_version: daemon::discovery::INSTANCE_PROTOCOL_VERSION,
        started_at: rfc3339_now(),
    }
}

fn announce_instance(
    instances_dir: &std::path::Path,
    instance_name: &str,
    instance_file: &daemon::discovery::InstanceFile,
) {
    match daemon::discovery::write_instance_file(instances_dir, instance_name, instance_file) {
        Ok(()) => info!(instance = %instance_name, "wrote instances.d discovery entry"),
        Err(e) => tracing::warn!(
            instance = %instance_name,
            error = %e,
            "failed to write instances.d discovery entry; multi-scene discovery clients won't see this daemon"
        ),
    }
}

fn record_startup_timing(
    pool: &daemon::session_pool::SessionPool,
    perf: &PerfCollector,
    config_load_ms: f64,
    pool_built_ms: f64,
) {
    let pool_ready_ms = perf.checkpoint("pool_ready");
    info!(elapsed_ms = pool_ready_ms, "startup: session pool ready");
    let total_startup_ms = perf.elapsed_ms() as u64;
    let timing = StartupTimingPayload {
        total_startup_ms,
        config_load_ms: config_load_ms as u64,
        pool_build_ms: (pool_built_ms - config_load_ms).max(0.0) as u64,
    };
    info!(total_ms = total_startup_ms, ?timing, "startup complete");
    pool.record_startup(timing);
}

/// One token covers both network transports; either flag alone sets it.
async fn set_transport_tokens(
    server: &DaemonServer,
    daemon_config: &DaemonConfig,
    unix_socket_token: Option<String>,
) {
    if let Some(token) = daemon_config.tcp_token.clone() {
        server.set_tcp_token(token).await;
    }
    if let Some(token) = unix_socket_token {
        server.set_tcp_token(token).await;
        server.require_unix_socket_auth();
    }
}

/// Whoever stopped the listener is stopping the sessions; this waits for it.
async fn shut_down_sessions(pool: &daemon::session_pool::SessionPool) {
    if tokio::time::timeout(SHUTDOWN_GRACE, pool.shutdown_all())
        .await
        .is_err()
    {
        tracing::warn!("sessions did not shut down within {SHUTDOWN_GRACE:?}; exiting anyway");
    }
    #[cfg(feature = "otel")]
    let _ = tokio::time::timeout(
        SHUTDOWN_GRACE,
        tokio::task::spawn_blocking(assembly::telemetry_otel::shutdown_process_exporter),
    )
    .await;
}

fn import_detected(
    sources: &[base::frozen::ImportSource],
) -> daemon::session_pool::api::DaemonEvent {
    daemon::session_pool::api::DaemonEvent::ImportDetected {
        sources: sources
            .iter()
            .map(|s| daemon::session_pool::api::ImportSource {
                source: s.kind().as_str().to_string(),
                description: s.describe(),
            })
            .collect(),
    }
}

const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        if let Ok(mut term) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => info!("ctrl-c received; shutting down"),
                _ = term.recv() => info!("SIGTERM received; shutting down"),
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
    info!("ctrl-c received; shutting down");
}

fn rfc3339_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    time::OffsetDateTime::from_unix_timestamp(secs as i64)
        .ok()
        .and_then(|t| {
            t.format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_scene_finds_every_builtin_via_the_registry() {
        for id in ["code", "harnix", "mini"] {
            assert_eq!(
                resolve_scene(id, std::path::Path::new("/nonexistent"))
                    .expect("builtin scene should resolve")
                    .id
                    .as_str(),
                id
            );
        }
    }

    #[test]
    fn resolve_scene_rejects_an_unknown_name_and_lists_the_known_ones() {
        let err = match resolve_scene("not-a-real-scene", std::path::Path::new("/nonexistent")) {
            Ok(_) => panic!("expected an error for an unknown scene name"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("not-a-real-scene"), "{err}");
        for id in ["code", "harnix", "mini"] {
            assert!(err.contains(id), "expected `{id}` in error, got: {err}");
        }
    }

    #[test]
    fn resolve_scene_finds_a_declarative_scene_under_the_state_root() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("triage");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("scene.json"),
            r#"{"id": "triage", "name": "Triage"}"#,
        )
        .unwrap();
        std::fs::write(dir.join("prompt.md"), "Sort incoming issues.").unwrap();
        assert_eq!(
            resolve_scene("triage", root.path()).unwrap().id.as_str(),
            "triage"
        );
    }

    #[test]
    fn default_instance_name_sorts_and_dedupes_scenes() {
        assert_eq!(
            default_instance_name("mini", &["harnix".to_string(), "code".to_string()]),
            "code,harnix,mini"
        );
        assert_eq!(
            default_instance_name("code", &["code".to_string()]),
            "code",
            "--scene's own value repeated in --scenes must not appear twice"
        );
    }

    fn parse(args: &[&str]) -> Cli {
        match Cli::parse_from(args.iter().map(std::ffi::OsString::from)) {
            Ok(Invocation::Run(cli)) => *cli,
            Ok(_) => panic!("expected a run invocation for {args:?}"),
            Err(e) => panic!("{args:?}: {e}"),
        }
    }

    #[test]
    fn no_arguments_give_the_documented_defaults() {
        let cli = parse(&[]);
        assert_eq!(cli.session_cap, 32);
        assert_eq!(cli.session_idle_timeout, 3600);
        assert_eq!(cli.model, "claude-sonnet-4-6");
        assert_eq!(cli.max_tokens, 32_000);
        assert_eq!(cli.scene, "code");
        assert_eq!(cli.permission_prompt_timeout, 300);
        assert!(cli.socket.is_none() && cli.instance.is_none() && cli.scenes.is_empty());
        assert_eq!(cli.worker_threads, None);
        assert_eq!(cli.mcp_policy, None);
        assert_eq!(cli.managed_settings, None);
        assert!(!cli.unix_socket_auth);
        assert!(parse(&["--unix-socket-auth"]).unix_socket_auth);
    }

    #[test]
    fn worker_threads_take_a_positive_count_and_size_the_runtime() {
        assert_eq!(parse(&["--worker-threads", "1"]).worker_threads, Some(1));
        assert_eq!(parse(&["--worker-threads=6"]).worker_threads, Some(6));
        let parse_err =
            |args: &[&str]| Cli::parse_from(args.iter().map(std::ffi::OsString::from)).is_err();
        assert!(parse_err(&["--worker-threads", "0"]));
        assert!(parse_err(&["--worker-threads", "-2"]));
        assert!(parse_err(&["--worker-threads", "all"]));

        let rt = runtime(Some(1)).unwrap();
        assert_eq!(rt.metrics().num_workers(), 1);
        let rt = runtime(Some(3)).unwrap();
        assert_eq!(rt.metrics().num_workers(), 3);
    }

    #[test]
    fn options_take_separate_or_inline_values() {
        let cli = parse(&[
            "--socket",
            "/tmp/a.sock",
            "--session-cap=4",
            "--scene",
            "mini",
            "--scenes",
            "harnix,triage",
            "--scenes=code",
            "--listen",
            "127.0.0.1:7878",
            "--token=s",
            "--mcp-policy",
            "/etc/corp/mcp.json",
            "--managed-settings=/etc/corp/managed.json",
        ]);
        assert_eq!(
            cli.socket.as_deref(),
            Some(std::path::Path::new("/tmp/a.sock"))
        );
        assert_eq!(cli.session_cap, 4);
        assert_eq!(cli.scene, "mini");
        assert_eq!(cli.scenes, ["harnix", "triage", "code"]);
        assert_eq!(cli.listen.as_deref(), Some("127.0.0.1:7878"));
        assert_eq!(cli.token.as_deref(), Some("s"));
        assert_eq!(
            cli.mcp_policy.as_deref(),
            Some(std::path::Path::new("/etc/corp/mcp.json"))
        );
        assert_eq!(
            cli.managed_settings.as_deref(),
            Some(std::path::Path::new("/etc/corp/managed.json"))
        );
    }

    #[test]
    fn unknown_flags_and_bad_numbers_are_errors() {
        let parse_err =
            |args: &[&str]| Cli::parse_from(args.iter().map(std::ffi::OsString::from)).is_err();
        assert!(parse_err(&["--bogus"]));
        assert!(parse_err(&["--session-cap", "many"]));
        assert!(parse_err(&["--model"]));
        assert!(parse_err(&["stray"]));
    }

    #[test]
    fn help_and_version_short_circuit() {
        let run = |args: &[&str]| Cli::parse_from(args.iter().map(std::ffi::OsString::from));
        assert!(matches!(run(&["--model", "x", "-h"]), Ok(Invocation::Help)));
        assert!(matches!(run(&["--version"]), Ok(Invocation::Version)));
    }

    #[test]
    fn default_instance_name_is_order_independent() {
        assert_eq!(
            default_instance_name("harnix", &["mini".to_string(), "code".to_string()]),
            default_instance_name("code", &["mini".to_string(), "harnix".to_string()]),
        );
    }
}
