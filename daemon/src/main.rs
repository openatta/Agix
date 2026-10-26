//! `agixd` — daemon binary.
//!
//! Builds a multi-session agent engine pool at startup and exposes it via
//! JSON-RPC 2.0 over Unix socket (or TCP with token auth).
//!
//! # Startup profiling
//!
//! A [`PerfCollector`] tracks elapsed time at each startup phase. After all
//! phases complete, a [`StartupTimingPayload`] telemetry event is emitted
//! (if telemetry is enabled).

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use daemon::{config::*, write_lock_file, DaemonServer};
use telemetry::events::StartupTimingPayload;
use telemetry::perf::PerfCollector;
use tokio_util::sync::CancellationToken;
use tracing::info;

#[derive(Parser, Debug)]
#[command(
    version,
    about = "Agix daemon: multi-session agent engine over JSON-RPC"
)]
struct Cli {
    /// Unix socket path (default: $HOME/.agix/<scene>/daemon.sock)
    #[arg(long)]
    socket: Option<PathBuf>,

    /// Maximum concurrent sessions
    #[arg(long, default_value = "32")]
    session_cap: usize,

    /// Session idle timeout in seconds (default: 3600 = 1 hour)
    #[arg(long, default_value = "3600")]
    session_idle_timeout: u64,

    /// Model name for sessions
    #[arg(long, default_value = "claude-sonnet-4-6")]
    model: String,

    /// Max tokens per turn
    #[arg(long, default_value = "2000")]
    max_tokens: u32,

    /// Bind a TCP listener at this addr (e.g. 127.0.0.1:7878).
    /// Requires --token or AGIX_DAEMON_TOKEN env.
    #[arg(long)]
    listen: Option<String>,

    /// Bind a WebSocket listener at this addr (e.g. 127.0.0.1:7879), so a
    /// browser front end can reach the daemon directly. Loopback only, and
    /// requires --token or AGIX_DAEMON_TOKEN env.
    #[arg(long)]
    listen_ws: Option<String>,

    /// Shared secret for TCP and WebSocket auth. Falls back to
    /// `AGIX_DAEMON_TOKEN`.
    #[arg(long)]
    token: Option<String>,

    /// Which scene this daemon serves (must be one the code actually
    /// registers — see `resolve_scene()`). Determines both session behavior
    /// (system prompt/tools) and the user-level state root (`~/.agix/<scene>/`).
    /// There is no default scope concept separate from this — an unsupported
    /// value fails startup outright rather than silently falling back to
    /// anything.
    #[arg(long, default_value = "coding")]
    scene: String,

    /// Additional scenes to activate alongside `--scene`, comma-separated
    /// (e.g. `--scenes chat,research`). `--scene`'s own value doesn't need
    /// repeating here — it's always active. Each must be one
    /// `scene::scene::SceneRegistry::register_builtin` actually registers;
    /// an unknown name fails startup the same way an unsupported `--scene`
    /// does. Session creation still defaults to `--scene`'s scene;
    /// `session.create {"scene": "..."}` is how a caller reaches one of
    /// these instead.
    #[arg(long, value_delimiter = ',')]
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
    /// `--scene coding --scenes chat,research` defaults to
    /// `chat,coding,research`. See `default_instance_name()`.
    #[arg(long)]
    instance: Option<String>,

    /// Seconds a permission prompt may go unanswered before the daemon
    /// denies it on the client's behalf (0 = wait forever).
    ///
    /// Only reachable now that the default permission mode is `ask`: the
    /// engine itself waits indefinitely for `session.respondToPrompt`, so a
    /// client that never implements it would otherwise hang its
    /// `session.run_turn` call forever. The timeout fails *closed* — it
    /// denies the tool call, never approves it — and the turn then continues
    /// normally with an error `tool_result`.
    #[arg(long, default_value = "300")]
    permission_prompt_timeout: u64,
}

/// The closed set of scenes this daemon knows how to construct. Keep in sync
/// with `crates/scene/src/scene/*.rs` and the sandbox's per-scene settings.json
/// protection list (`crates/tools/src/bash/sandbox.rs::KNOWN_SCENES`).
/// Resolves against `scene::scene::SceneRegistry` (P2: the registry existed
/// with zero production callers before this — every scene was a hardcoded
/// match arm here instead) rather than a hardcoded match, so `--scenes`
/// (plural, P4) can register more than the four built-ins without touching
/// this function again.
fn resolve_scene(name: &str) -> anyhow::Result<Arc<dyn base::interface::scene::AgentScene>> {
    let mut registry = scene::scene::SceneRegistry::new();
    registry.register_builtin();
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // ── Startup profiler ────────────────────────────────────────────────────
    let perf = PerfCollector::new();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    // ── Resolve scene (fails fast on an unsupported value) ──────────────
    let scene = resolve_scene(&cli.scene)?;
    let scope = scene.id().to_string();

    // ── Resolve paths + load settings ────────────────────────────────────
    // `load_daemon_config` delegates all settings.json parsing/merging to
    // `Settings::load()` — the single canonical loader (global → scene →
    // project). See `daemon::config` module docs for why this replaced a
    // separate hand-rolled `Settings{}` literal that used to live in this
    // function (that duplication is exactly how `permission_rules` ended up
    // parsed nowhere despite being a real `Settings` field).
    let paths = DefaultDaemonPaths::from_env(&scope);
    let mut daemon_config = load_daemon_config(
        &cli.model,
        cli.max_tokens,
        cli.socket.as_deref(),
        &scope,
        &paths,
    );
    daemon_config.session_cap = cli.session_cap;
    daemon_config.session_idle_timeout_secs = cli.session_idle_timeout;
    daemon_config.settings.session_dir = Some(daemon_config.settings.paths.local_data_dir.clone());

    // TCP listener config
    if let Some(ref addr_str) = cli.listen {
        let addr: std::net::SocketAddr = addr_str
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid --listen `{addr_str}`: {e}"))?;
        let token = cli
            .token
            .clone()
            .or_else(|| std::env::var("AGIX_DAEMON_TOKEN").ok())
            .ok_or_else(|| anyhow::anyhow!("--listen requires --token or env AGIX_DAEMON_TOKEN"))?;
        daemon_config.tcp_addr = Some(addr);
        daemon_config.tcp_token = Some(token);
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

    let socket = daemon_config.socket_path.clone();
    let lock_path = daemon_config.lock_path.clone();
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Write discovery lock
    match write_lock_file(&lock_path, &socket) {
        Ok(lock) => info!(pid = lock.pid, "wrote discovery lock"),
        Err(e) => anyhow::bail!("could not claim daemon lock: {e}"),
    }

    // Write the multi-scene instances.d discovery entry alongside the
    // legacy single-scene `daemon.lock` above — additive, see
    // `daemon::discovery`'s module docs. Lives under the *global* root
    // (shared by every scene), not `config_root` (scene-specific), since
    // one instance file must be reachable regardless of which of this
    // daemon's scenes a client cares about.
    let instance_scenes = active_scenes(&cli.scene, &cli.scenes);
    let instance_name = cli
        .instance
        .clone()
        .unwrap_or_else(|| default_instance_name(&cli.scene, &cli.scenes));
    let instances_dir = daemon_config
        .paths
        .global_root()
        .join("daemon")
        .join("instances.d");
    let identity = base::process_identity::ProcessIdentity::current();
    let instance_file = daemon::discovery::InstanceFile {
        instance: instance_name.clone(),
        pid: identity.pid,
        pid_start_time: identity.started_at,
        socket: socket.clone(),
        tcp: daemon_config.tcp_addr.map(|a| a.to_string()),
        ws: daemon_config.ws_addr.map(|a| a.to_string()),
        scenes: instance_scenes,
        protocol_version: daemon::discovery::INSTANCE_PROTOCOL_VERSION,
        started_at: rfc3339_now(),
    };
    // ── Startup checkpoint: config_loaded ──────────────────────────────
    let _config_load_ms = perf.checkpoint("config_loaded");
    info!(elapsed_ms = _config_load_ms, "startup: config loaded");

    // ── Build the engine ───────────────────────────────────────────────
    // Every step of this — provider routing, credentials, the memory store,
    // the history layout and its migration, the pool's own arguments, and the
    // three startup steps a session is quietly worse without — lives in
    // `daemon::assemble`. This binary contributes the things only a process
    // has: its working directory, its command line, its listeners.
    let pool = daemon::assemble::pool(
        &daemon_config,
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
    .await?;

    // ── Startup checkpoints ────────────────────────────────────────────
    // Phases the assembly performs in one call; the markers stay because the
    // startup-timing telemetry below is keyed by them.
    for phase in [
        "memory_loaded",
        "skills_scanned",
        "mcp_connected",
        "tools_registered",
    ] {
        let elapsed = perf.checkpoint(phase);
        info!(elapsed_ms = elapsed, phase, "startup phase complete");
    }

    // ── Cross-tool config import (process-level, once) ─────────────────
    // `daemon` is headless — no client is connected yet at this point in
    // startup, so there's nobody to synchronously ask "import from X?" the
    // way `ImportCallback` expects. Rather than wire a callback with no
    // sensible synchronous answer, detect directly and — if anything's
    // found — emit an `import_detected` `daemon.event` notification instead
    // of blocking for a decision; a subscribed client can act on it via the
    // `import.list`/`import.run` RPCs whenever it's ready, no LLM turn
    // required. Respects the same `.imported.json` marker the automatic
    // path always has, so this doesn't nag on every restart. The manual
    // `/import` slash command (the bundled `import` skill) remains available
    // regardless and always re-detects.
    {
        let pool = pool.clone();
        let import_cwd = pool.cwd().to_path_buf();
        tokio::spawn(async move {
            if base::frozen::import_already_decided(&import_cwd).await {
                return;
            }
            let sources = base::frozen::detect_import_sources(&import_cwd).await;
            if sources.is_empty() {
                return;
            }
            pool.emit_event(serde_json::json!({
                "kind": "import_detected",
                "sources": sources.iter().map(|s| serde_json::json!({
                    "source": s.kind().as_str(),
                    "description": s.describe(),
                })).collect::<Vec<_>>(),
            }));
        });
    }

    // 启动后台回收
    pool.start_janitor();

    // ── Startup checkpoint: pool_ready ─────────────────────────────────
    let _pool_ready_ms = perf.checkpoint("pool_ready");
    info!(elapsed_ms = _pool_ready_ms, "startup: session pool ready");

    // ── Emit StartupTimingPayload ──────────────────────────────────────
    let total_startup_ms = perf.elapsed_ms() as u64;
    let checkpoints: std::collections::HashMap<String, u64> = perf
        .checkpoint_snapshot()
        .into_iter()
        .map(|(k, v)| (k, v as u64))
        .collect();

    let timing = StartupTimingPayload {
        total_startup_ms,
        config_load_ms: checkpoints.get("config_loaded").copied().unwrap_or(0),
        tool_registration_ms: checkpoints.get("tools_registered").copied().unwrap_or(0),
        mcp_connect_ms: checkpoints.get("mcp_connected").copied().unwrap_or(0),
        plugin_load_ms: 0,
        skill_load_ms: checkpoints.get("skills_scanned").copied().unwrap_or(0),
        history_resume_ms: 0,
        first_api_call_ms: 0,
    };

    info!(total_ms = total_startup_ms, ?timing, "startup complete");

    // If telemetry is available, emit the startup timing event.
    // (The daemon currently does not wire up the full telemetry pipeline
    //  at startup, but this demonstrates the pattern.)

    // ── Start server ───────────────────────────────────────────────────
    let cancel = CancellationToken::new();

    // Ctrl-C graceful shutdown
    {
        let cancel = cancel.clone();
        let pool = pool.clone();
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            info!("ctrl-c received; shutting down");
            pool.shutdown_all().await;
            cancel.cancel();
        });
    }

    let server = Arc::new(DaemonServer::new(pool, cancel.clone()));

    // One token covers both network transports; either flag alone sets it.
    if let Some(token) = daemon_config.tcp_token.clone() {
        server.set_tcp_token(token).await;
    }

    // Every listener is bound here, before the daemon is announced, so a port
    // already in use fails startup instead of leaving a daemon running without
    // the transport the user asked for — and so a client that finds the
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
        tokio::spawn(async move { daemon::ws::serve_ws_listener(s, listener).await });
    }

    let unix = DaemonServer::bind_unix(&socket)?;

    match daemon::discovery::write_instance_file(&instances_dir, &instance_name, &instance_file) {
        Ok(()) => info!(instance = %instance_name, "wrote instances.d discovery entry"),
        Err(e) => tracing::warn!(
            instance = %instance_name,
            error = %e,
            "failed to write instances.d discovery entry; multi-scene discovery clients won't see this daemon"
        ),
    }

    server.serve_unix_listener(unix).await?;

    let _ = std::fs::remove_file(&lock_path);
    daemon::discovery::remove_instance_file(&instances_dir, &instance_name);
    info!("daemon stopped");
    Ok(())
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
        for id in ["coding", "chat", "demo", "research"] {
            assert_eq!(
                resolve_scene(id)
                    .expect("builtin scene should resolve")
                    .id(),
                id
            );
        }
    }

    #[test]
    fn resolve_scene_rejects_an_unknown_name_and_lists_the_known_ones() {
        let err = match resolve_scene("not-a-real-scene") {
            Ok(_) => panic!("expected an error for an unknown scene name"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("not-a-real-scene"), "{err}");
        for id in ["coding", "chat", "demo", "research"] {
            assert!(err.contains(id), "expected `{id}` in error, got: {err}");
        }
    }

    #[test]
    fn default_instance_name_sorts_and_dedupes_scenes() {
        assert_eq!(
            default_instance_name("coding", &["chat".to_string(), "research".to_string()]),
            "chat,coding,research"
        );
        assert_eq!(
            default_instance_name("coding", &["coding".to_string()]),
            "coding",
            "--scene's own value repeated in --scenes must not appear twice"
        );
    }

    #[test]
    fn default_instance_name_is_order_independent() {
        assert_eq!(
            default_instance_name("research", &["chat".to_string(), "coding".to_string()]),
            default_instance_name("coding", &["chat".to_string(), "research".to_string()]),
        );
    }
}
