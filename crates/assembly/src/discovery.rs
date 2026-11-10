//! Daemon discovery: `daemon.lock` (single-scene, legacy) and
//! `instances.d/` (multi-scene, current — see `docs/daemon_rpc_protocol.md` §2).
//!
//! The daemon writes `~/.agix/scenes/<scene>/daemon.lock` (mode 0600, `<scene>` is
//! the `Scene` id resolved from `--scene` in `main.rs`, default
//! `code`) on startup with `{pid, socket_path, version, started_at}`. IDE
//! plugins read this file to find a running daemon — no env var hunting, no
//! port scan.
//!
//! The daemon holds the file with an advisory `flock` for as long as it
//! runs, so two daemons starting together cannot both claim it; the kernel
//! drops the lock however the process ends. On graceful shutdown the file is
//! removed; after a crash the next daemon takes the lock and overwrites it.
//!
//! `daemon.lock` predates `--scenes` (plural) — a client reading it only
//! ever learns about the one scene it's colocated with, not the full set a
//! daemon may have activated via `--scenes`. `instances.d/<instance>.json`
//! (one file per daemon instance, written under the *global* root so every
//! instance's file lands in the same directory regardless of which scenes
//! it serves) carries the full `scenes` list instead. It's additive: written
//! alongside `daemon.lock`, not a replacement for it — existing single-scene
//! clients keep working unmodified.

use platform::process_identity::{
    decide_stale_lock, ProcessIdentity, ProcessLiveness, StaleLockDecision,
};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonLock {
    /// Daemon PID (for liveness check by clients).
    pub pid: u32,
    /// The OS-reported start time of `pid`, in seconds — see
    /// [`ProcessIdentity`]'s doc comment for why a bare `pid` alone isn't
    /// enough to tell "still the same process" from "pid got recycled".
    /// `#[serde(default)]` so a lock file without this field still
    /// deserializes; such a file gets only the pid liveness test.
    #[serde(default)]
    pub pid_start_time: i64,
    /// Absolute path of the unix socket the daemon is listening on.
    pub socket_path: PathBuf,
    /// Daemon binary version (Cargo).
    pub version: String,
    /// Unix timestamp (seconds) the lock was written.
    pub started_at: i64,
    /// Protocol version the daemon speaks; clients refuse mismatched majors.
    pub protocol_version: String,
}

/// `daemon.lock`, held by this process until dropped; dropping it removes
/// the file.
#[derive(Debug)]
pub struct DaemonLockGuard {
    lock: DaemonLock,
    path: PathBuf,
    file: fs::File,
}

impl std::ops::Deref for DaemonLockGuard {
    type Target = DaemonLock;
    fn deref(&self) -> &DaemonLock {
        &self.lock
    }
}

impl Drop for DaemonLockGuard {
    fn drop(&mut self) {
        if names_open_file(&self.path, &self.file) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Claim `lock_path` for this daemon and write it, mode `0600`.
///
/// The claim is the `flock`: one held by another process refuses it. A lock
/// taken, the recorded holder is checked too, since a daemon from before the
/// file was locked never took one (docs/ARCHITECTURE.md §7):
/// ```text
/// flock 被别的进程持有              → 拒绝，AnotherDaemonRunning
/// 拿到 flock，文件里记录的
///   ├─ 无记录 / 读不出 / pid 是自己 → 写入，获得锁
///   ├─ pid 存活 且 启动时间匹配    → 拒绝，AnotherDaemonRunning
///   ├─ pid 存活 但 启动时间不匹配  → pid 已被复用，视为陈旧，接管
///   └─ pid 不存在                  → 陈旧，接管并覆写（记 warn）
/// ```
pub fn write_lock_file(
    lock_path: &Path,
    socket_path: &Path,
) -> Result<DaemonLockGuard, LockFileError> {
    if let Some(parent) = lock_path.parent() {
        create_private_dir(parent).map_err(LockFileError::Io)?;
    }
    let mut file = loop {
        let file = open_private(lock_path).map_err(LockFileError::Io)?;
        match try_lock_exclusive(&file) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                let prev = read_recorded(&file);
                return Err(LockFileError::AnotherDaemonRunning {
                    pid: prev.as_ref().map_or(0, |p| p.pid),
                    socket_path: prev.map(|p| p.socket_path).unwrap_or_default(),
                });
            }
            Err(e) => return Err(LockFileError::Io(e)),
        }
        // A daemon stopping removes the file while it still holds it; the
        // lock on a file no longer at `lock_path` keeps nobody out.
        if names_open_file(lock_path, &file) {
            break file;
        }
    };

    if let Some(prev) = read_recorded(&file) {
        if prev.pid != std::process::id() {
            let recorded = ProcessIdentity {
                pid: prev.pid,
                started_at: prev.pid_start_time,
            };
            if let StaleLockDecision::KeepExisting =
                decide_stale_lock(&recorded, "daemon.lock", lock_path)
            {
                return Err(LockFileError::AnotherDaemonRunning {
                    pid: prev.pid,
                    socket_path: prev.socket_path,
                });
            }
        }
    }

    let identity = ProcessIdentity::current();
    let lock = DaemonLock {
        pid: identity.pid,
        pid_start_time: identity.started_at,
        socket_path: socket_path.to_path_buf(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        started_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
        protocol_version: "1".into(),
    };
    let body = serde_json::to_string_pretty(&lock).map_err(LockFileError::Json)?;
    {
        use std::io::{Seek, Write};
        file.set_len(0).map_err(LockFileError::Io)?;
        file.seek(io::SeekFrom::Start(0))
            .map_err(LockFileError::Io)?;
        file.write_all(body.as_bytes()).map_err(LockFileError::Io)?;
    }
    Ok(DaemonLockGuard {
        lock,
        path: lock_path.to_path_buf(),
        file,
    })
}

/// Create `dir` and any missing parents, each one this user's only.
/// Directories already there are left as they are.
pub fn create_private_dir(dir: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}

fn open_private(path: &Path) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

fn read_recorded(file: &fs::File) -> Option<DaemonLock> {
    use std::io::{Read, Seek};
    let mut file = file;
    let mut bytes = Vec::new();
    file.seek(io::SeekFrom::Start(0)).ok()?;
    file.read_to_end(&mut bytes).ok()?;
    if bytes.is_empty() {
        return None;
    }
    match serde_json::from_slice::<DaemonLock>(&bytes) {
        Ok(prev) => Some(prev),
        Err(e) => {
            tracing::warn!(error = %e, "daemon.lock content could not be parsed as JSON; treating as stale and overwriting");
            None
        }
    }
}

#[cfg(unix)]
fn names_open_file(path: &Path, file: &fs::File) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (fs::metadata(path), file.metadata()) {
        (Ok(at_path), Ok(open)) => at_path.dev() == open.dev() && at_path.ino() == open.ino(),
        _ => false,
    }
}

#[cfg(not(unix))]
fn names_open_file(path: &Path, _file: &fs::File) -> bool {
    path.exists()
}

#[cfg(unix)]
fn try_lock_exclusive(file: &fs::File) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: `flock` only reads the descriptor, which `file` keeps open.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(unix))]
fn try_lock_exclusive(_file: &fs::File) -> io::Result<()> {
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum LockFileError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("malformed daemon.lock: {0}")]
    Json(#[from] serde_json::Error),
    #[error("another daemon is already running (pid={pid}, socket={})", socket_path.display())]
    AnotherDaemonRunning { pid: u32, socket_path: PathBuf },
}

/// Protocol version this daemon speaks, as reported in `instances.d/`
/// entries and by `daemon.auth`, `daemon.status` and `daemon.ping`
/// (`docs/daemon_rpc_protocol.md` §5). Numeric here (unlike
/// `DaemonLock::protocol_version`, which predates v2 and stayed a string for
/// backward compatibility with existing readers).
pub const INSTANCE_PROTOCOL_VERSION: u32 = 3;

/// What this daemon can do beyond the protocol version, named so a client can
/// check for one feature rather than infer it from a version number.
pub const CAPABILITIES: &[&str] = &[
    "on_busy_steer",
    "subscription_dropped_notice",
    "session_lifecycle_events",
    "events_lagged_notice",
    "schedule",
    "session_tasks",
    "task_output",
    "task_progress",
    "tool_progress",
    "task_output_offsets",
    "task_messages",
    "context_edits",
    "extension_entries",
    "goals",
    "question_prompts",
    "session_export",
    "strict_options",
];

/// [`CAPABILITIES`], plus those of the modules this build carries.
pub fn capabilities() -> Vec<&'static str> {
    let mut out = CAPABILITIES.to_vec();
    if cfg!(feature = "agents") {
        out.push("host_agents");
    }
    if runtime::PTC_ENABLED {
        out.push("ptc");
    }
    out
}

/// Version, protocol version and capabilities: what a client is told
/// wherever it first meets the daemon, so it can refuse an incompatible one
/// at the handshake rather than discover it mid-session.
pub fn identity() -> serde_json::Value {
    serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "protocol_version": INSTANCE_PROTOCOL_VERSION,
        "capabilities": capabilities(),
        "modules": crate::modules::compiled(),
    })
}

/// One `~/.agix/daemon/instances.d/<instance>.json` entry — `docs/daemon_rpc_protocol.md`
/// §2. Every daemon instance writes its own file under a shared directory
/// (no shared write point, so concurrent daemon startups can't clobber each
/// other's entries via a racing read-modify-write of one shared index).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceFile {
    pub instance: String,
    pub pid: u32,
    pub pid_start_time: i64,
    pub socket: PathBuf,
    /// The TCP listener's address, when one is bound (`--listen`).
    ///
    /// Published for the same reason as `socket`: a client that finds this
    /// instance should not also have to be told, out of band, how to reach
    /// it. Absent when the daemon serves only the Unix socket.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tcp: Option<String>,
    /// The WebSocket listener's address, when one is bound (`--listen-ws`).
    /// A web front end reads this to know which port to connect to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ws: Option<String>,
    /// The unix socket, too, wants `daemon.auth` first (`--unix-socket-auth`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unix_socket_auth: bool,
    /// Where the token for that handshake is, when the daemon made one up
    /// rather than being given one. The path, never the token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_file: Option<PathBuf>,
    pub scenes: Vec<String>,
    pub protocol_version: u32,
    pub started_at: String,
}

/// Where a daemon instance writes the token it made up for its unix socket.
pub fn unix_socket_token_path(daemon_root: &Path, instance: &str) -> PathBuf {
    daemon_root.join("auth").join(format!("{instance}.token"))
}

/// A fresh random token, written to `path` (`0600`, in a `0700` directory)
/// and returned.
pub fn write_unix_socket_token(path: &Path) -> io::Result<String> {
    if let Some(parent) = path.parent() {
        create_private_dir(parent)?;
    }
    let token: String = [base::id::Id::new(), base::id::Id::new()]
        .iter()
        .flat_map(|id| id.as_bytes().to_vec())
        .map(|b| format!("{b:02x}"))
        .collect();
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    let _ = fs::remove_file(&tmp);
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    {
        use std::io::Write;
        let mut file = options.open(&tmp)?;
        file.write_all(token.as_bytes())?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    Ok(token)
}

/// Atomically write `<dir>/<instance>.json` (tmp + rename, `0600` on Unix).
pub fn write_instance_file(dir: &Path, instance: &str, file: &InstanceFile) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let path = dir.join(format!("{instance}.json"));
    let body = serde_json::to_string_pretty(file)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let tmp = dir.join(format!(".{instance}.json.tmp-{}", std::process::id()));
    fs::write(&tmp, body)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&tmp)?.permissions();
        perms.set_mode(0o600);
        fs::set_permissions(&tmp, perms)?;
    }
    fs::rename(&tmp, &path)?;
    Ok(())
}

/// Remove `<dir>/<instance>.json` on shutdown — best-effort, mirroring
/// [`write_lock_file`]'s caller (`main.rs`) discarding the `daemon.lock`
/// removal error: there's nothing more useful to do with a cleanup failure
/// on the way out than log it.
pub fn remove_instance_file(dir: &Path, instance: &str) {
    let path = dir.join(format!("{instance}.json"));
    if let Err(e) = fs::remove_file(&path) {
        if e.kind() != io::ErrorKind::NotFound {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "failed to remove instances.d entry on shutdown"
            );
        }
    }
}

/// Reads every `<dir>/*.json` entry, dropping ones whose recorded `(pid,
/// pid_start_time)` prove the writer is gone (dead, or the pid was reused
/// by an unrelated process) — same [`ProcessIdentity::check`] this module's
/// `daemon.lock` staleness check uses, reused rather than re-implemented.
/// An entry that can't be verified either way (`ProcessLiveness::Unknown`)
/// is kept: dropping it on inconclusive evidence would make a possibly-live
/// daemon undiscoverable, which is worse than a reader having to try (and
/// fail) to connect to it once.
pub fn discover_instances(dir: &Path) -> Vec<InstanceFile> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let file: InstanceFile = match serde_json::from_slice(&bytes) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "instances.d entry could not be parsed; skipping"
                );
                continue;
            }
        };
        let recorded = ProcessIdentity {
            pid: file.pid,
            started_at: file.pid_start_time,
        };
        match ProcessIdentity::check(&recorded) {
            ProcessLiveness::Dead | ProcessLiveness::Reused => {
                tracing::info!(
                    instance = %file.instance,
                    path = %path.display(),
                    "stale instances.d entry; removing"
                );
                let _ = fs::remove_file(&path);
                continue;
            }
            ProcessLiveness::Alive | ProcessLiveness::Unknown => {}
        }
        out.push(file);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use platform::process_identity::process_start_time;
    use tempfile::TempDir;

    fn read_lock_file(lock_path: &Path) -> Result<Option<DaemonLock>, LockFileError> {
        if !lock_path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(lock_path).map_err(LockFileError::Io)?;
        let lock: DaemonLock = serde_json::from_slice(&bytes).map_err(LockFileError::Json)?;
        Ok(Some(lock))
    }

    /// A short-lived child process — gives tests a real, distinct, live pid
    /// (with its own real start time) instead of reusing the test's own,
    /// which would trip the `prev.pid == my_pid` reentry branch instead of
    /// the "another process" branch these tests mean to exercise.
    struct Child(std::process::Child);
    impl Child {
        fn spawn() -> Self {
            Self(
                std::process::Command::new("sleep")
                    .arg("5")
                    .spawn()
                    .expect("spawn a short-lived child for the test"),
            )
        }
        fn pid(&self) -> u32 {
            self.0.id()
        }
    }
    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn write_then_read_round_trips() {
        let dir = TempDir::new().unwrap();
        let lock = dir.path().join("daemon.lock");
        let sock = dir.path().join("daemon.sock");
        let written = write_lock_file(&lock, &sock).unwrap();
        let read = read_lock_file(&lock).unwrap().unwrap();
        assert_eq!(read.pid, written.pid);
        assert_eq!(read.socket_path, sock);
        assert_eq!(read.protocol_version, "1");
        assert_eq!(read.pid_start_time, written.pid_start_time);
    }

    #[test]
    fn write_overwrites_stale_lock() {
        let dir = TempDir::new().unwrap();
        let lock_path = dir.path().join("daemon.lock");
        let stale = DaemonLock {
            pid: 999_999_999,
            pid_start_time: 0,
            socket_path: PathBuf::from("/tmp/whatever.sock"),
            version: "0.0.0".into(),
            started_at: 0,
            protocol_version: "1".into(),
        };
        std::fs::write(&lock_path, serde_json::to_vec(&stale).unwrap()).unwrap();
        let sock = dir.path().join("new.sock");
        let written = write_lock_file(&lock_path, &sock).unwrap();
        assert_ne!(written.pid, 999_999_999);
        assert_eq!(written.socket_path, sock);
    }

    #[test]
    fn read_missing_returns_none() {
        let dir = TempDir::new().unwrap();
        let r = read_lock_file(&dir.path().join("does-not-exist")).unwrap();
        assert!(r.is_none());
    }

    #[test]
    fn write_rejects_when_another_live_process_holds_a_matching_start_time() {
        let child = Child::spawn();
        // The case is about refusing a lock whose holder is *confirmed* alive.
        // Where start times cannot be read there is no such confirmation, and
        // `decide_stale_lock` is deliberately conservative instead — a
        // different branch, covered by its own tests.
        let Some(recorded_start) = process_start_time(child.pid()) else {
            eprintln!("skipping: this host cannot read process start times");
            return;
        };
        let dir = TempDir::new().unwrap();
        let lock_path = dir.path().join("daemon.lock");
        let alive = DaemonLock {
            pid: child.pid(),
            pid_start_time: recorded_start,
            socket_path: PathBuf::from("/tmp/x.sock"),
            version: "9.9.9".into(),
            started_at: 0,
            protocol_version: "1".into(),
        };
        std::fs::write(&lock_path, serde_json::to_vec(&alive).unwrap()).unwrap();
        let r = write_lock_file(&lock_path, &dir.path().join("new.sock"));
        assert!(
            matches!(r, Err(LockFileError::AnotherDaemonRunning { .. })),
            "{r:?}"
        );
    }

    /// A lock file recording a pid that's alive again — but under a *different* process than the
    /// one that wrote it (start time doesn't match) — must be reclaimed,
    /// not treated as "another daemon is still running" forever.
    #[test]
    fn write_reclaims_the_lock_when_the_pid_was_reused_by_a_different_process() {
        let child = Child::spawn();
        let dir = TempDir::new().unwrap();
        let lock_path = dir.path().join("daemon.lock");
        let stale = DaemonLock {
            pid: child.pid(),
            pid_start_time: 12345, // deliberately wrong — simulates reuse
            socket_path: PathBuf::from("/tmp/old.sock"),
            version: "0.0.0".into(),
            started_at: 0,
            protocol_version: "1".into(),
        };
        std::fs::write(&lock_path, serde_json::to_vec(&stale).unwrap()).unwrap();
        let sock = dir.path().join("new.sock");
        let written = write_lock_file(&lock_path, &sock).unwrap();
        assert_eq!(written.socket_path, sock);
    }

    /// What a container restart leaves: a lock nobody holds, recording the pid
    /// this process happens to have been given again.
    #[test]
    fn a_lock_nobody_holds_that_records_this_pid_is_taken_over() {
        let dir = TempDir::new().unwrap();
        let lock_path = dir.path().join("daemon.lock");
        let left = DaemonLock {
            pid: std::process::id(),
            pid_start_time: 1,
            socket_path: dir.path().join("first.sock"),
            version: "0.0.0".into(),
            started_at: 0,
            protocol_version: "1".into(),
        };
        std::fs::write(&lock_path, serde_json::to_vec(&left).unwrap()).unwrap();

        let sock2 = dir.path().join("second.sock");
        let written = write_lock_file(&lock_path, &sock2).unwrap();
        assert_eq!(written.pid, std::process::id());
        assert_eq!(
            read_lock_file(&lock_path).unwrap().unwrap().socket_path,
            sock2
        );
    }

    #[test]
    fn a_held_lock_refuses_every_other_claim_and_goes_with_its_holder() {
        let dir = TempDir::new().unwrap();
        let lock_path = dir.path().join("daemon.lock");
        let claims: Vec<_> = std::thread::scope(|scope| {
            let attempts: Vec<_> = (0..8)
                .map(|i| {
                    let (lock_path, sock) = (&lock_path, dir.path().join(format!("{i}.sock")));
                    scope.spawn(move || write_lock_file(lock_path, &sock))
                })
                .collect();
            attempts.into_iter().map(|t| t.join().unwrap()).collect()
        });
        let held: Vec<_> = claims.iter().filter_map(|c| c.as_ref().ok()).collect();
        assert_eq!(held.len(), 1, "{claims:?}");
        let holder = held[0];
        for refused in claims.iter().filter_map(|c| c.as_ref().err()) {
            match refused {
                LockFileError::AnotherDaemonRunning { pid, .. } => {
                    assert!(*pid == 0 || *pid == holder.pid, "{refused:?}");
                }
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(
            read_lock_file(&lock_path).unwrap().unwrap().socket_path,
            holder.socket_path
        );

        drop(claims);
        assert!(!lock_path.exists(), "the holder removes its lock");
        write_lock_file(&lock_path, &dir.path().join("next.sock"))
            .expect("free once the holder is gone");
    }

    // `ProcessIdentity`/`ProcessLiveness`/`parse_elapsed` behavior itself is
    // covered by `platform::process_identity`'s own tests — the tests here only
    // cover this module's use of them (the lock file's accept/reject/reclaim
    // decisions).

    fn instance_file(instance: &str, pid: u32, pid_start_time: i64) -> InstanceFile {
        InstanceFile {
            instance: instance.to_string(),
            pid,
            pid_start_time,
            socket: PathBuf::from(format!("/tmp/{instance}.sock")),
            tcp: None,
            ws: None,
            unix_socket_auth: false,
            token_file: None,
            scenes: vec!["code".to_string()],
            protocol_version: INSTANCE_PROTOCOL_VERSION,
            started_at: "2026-08-12T00:00:00Z".to_string(),
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_unix_socket_token_is_fresh_and_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = unix_socket_token_path(&dir.path().join("daemon"), "desktop");
        let first = write_unix_socket_token(&path).unwrap();
        let second = write_unix_socket_token(&path).unwrap();
        assert_ne!(first, second);
        assert_eq!(first.len(), 64);
        assert_eq!(fs::read_to_string(&path).unwrap(), second);
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
    }

    #[test]
    fn write_instance_file_then_discover_finds_it() {
        let dir = TempDir::new().unwrap();
        let instances_dir = dir.path().join("instances.d");
        let identity = ProcessIdentity::current();
        let file = instance_file("desktop", identity.pid, identity.started_at);
        write_instance_file(&instances_dir, "desktop", &file).unwrap();

        let found = discover_instances(&instances_dir);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].instance, "desktop");
        assert_eq!(found[0].pid, identity.pid);
    }

    #[test]
    fn write_instance_file_reentry_overwrites_the_same_entry() {
        let dir = TempDir::new().unwrap();
        let instances_dir = dir.path().join("instances.d");
        let identity = ProcessIdentity::current();
        let first = instance_file("desktop", identity.pid, identity.started_at);
        write_instance_file(&instances_dir, "desktop", &first).unwrap();

        let mut second = first.clone();
        second.scenes = vec!["harnix".to_string(), "mini".to_string()];
        write_instance_file(&instances_dir, "desktop", &second).unwrap();

        let found = discover_instances(&instances_dir);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].scenes, vec!["harnix", "mini"]);
    }

    #[test]
    fn discover_instances_keeps_an_entry_for_a_live_process_with_matching_start_time() {
        let child = Child::spawn();
        // `unwrap_or(0)` rather than an expect: a host whose `ps` this code
        // cannot read records zero, which reads back as "cannot verify", and
        // an entry nobody can verify is kept for the same reason a live one
        // is. The case holds on both, and covers both.
        let recorded_start = process_start_time(child.pid()).unwrap_or(0);
        let dir = TempDir::new().unwrap();
        let instances_dir = dir.path().join("instances.d");
        let file = instance_file("other-daemon", child.pid(), recorded_start);
        write_instance_file(&instances_dir, "other-daemon", &file).unwrap();

        let found = discover_instances(&instances_dir);
        assert_eq!(
            found.len(),
            1,
            "the entry was dropped. recorded={recorded_start}, \
             now={:?}, verdict={:?}, dir_entries={:?}",
            process_start_time(child.pid()),
            platform::process_identity::ProcessIdentity::check(&ProcessIdentity {
                pid: child.pid(),
                started_at: recorded_start,
            }),
            std::fs::read_dir(&instances_dir).map(|d| d
                .flatten()
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect::<Vec<_>>()),
        );
        assert_eq!(found[0].instance, "other-daemon");
    }

    #[test]
    fn discover_instances_drops_an_entry_for_a_dead_pid() {
        let dir = TempDir::new().unwrap();
        let instances_dir = dir.path().join("instances.d");
        let file = instance_file("long-gone", 999_999_999, 0);
        write_instance_file(&instances_dir, "long-gone", &file).unwrap();

        let found = discover_instances(&instances_dir);
        assert!(found.is_empty());
        assert!(
            !instances_dir.join("long-gone.json").exists(),
            "stale entry should be removed, not just filtered out"
        );
    }

    #[test]
    fn discover_instances_drops_an_entry_whose_pid_was_reused() {
        let child = Child::spawn();
        let dir = TempDir::new().unwrap();
        let instances_dir = dir.path().join("instances.d");
        let file = instance_file("stale-holder", child.pid(), 12345); // deliberately wrong
        write_instance_file(&instances_dir, "stale-holder", &file).unwrap();

        let found = discover_instances(&instances_dir);
        assert!(found.is_empty());
    }

    #[test]
    fn discover_instances_multiple_instances_do_not_interfere() {
        let dir = TempDir::new().unwrap();
        let instances_dir = dir.path().join("instances.d");
        let identity = ProcessIdentity::current();
        let desktop = instance_file("desktop", identity.pid, identity.started_at);
        let ci_runner = instance_file("ci-runner", identity.pid, identity.started_at);
        write_instance_file(&instances_dir, "desktop", &desktop).unwrap();
        write_instance_file(&instances_dir, "ci-runner", &ci_runner).unwrap();

        let mut found: Vec<String> = discover_instances(&instances_dir)
            .into_iter()
            .map(|f| f.instance)
            .collect();
        found.sort();
        assert_eq!(
            found,
            vec!["ci-runner".to_string(), "desktop".to_string()],
            "recorded={}, now={:?}, verdict={:?}, dir_entries={:?}",
            identity.started_at,
            process_start_time(identity.pid),
            platform::process_identity::ProcessIdentity::check(&identity),
            std::fs::read_dir(&instances_dir).map(|d| d
                .flatten()
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect::<Vec<_>>()),
        );
    }

    #[test]
    fn remove_instance_file_deletes_it_and_is_a_noop_if_already_gone() {
        let dir = TempDir::new().unwrap();
        let instances_dir = dir.path().join("instances.d");
        let identity = ProcessIdentity::current();
        let file = instance_file("desktop", identity.pid, identity.started_at);
        write_instance_file(&instances_dir, "desktop", &file).unwrap();
        assert!(instances_dir.join("desktop.json").exists());

        remove_instance_file(&instances_dir, "desktop");
        assert!(!instances_dir.join("desktop.json").exists());

        remove_instance_file(&instances_dir, "desktop"); // must not panic
    }

    /// A reader finds the transports along with the instance. Omitted
    /// entirely when unbound, so an older reader sees the same document it
    /// always did rather than `"tcp": null`.
    #[test]
    fn network_transports_are_published_when_bound_and_absent_when_not() {
        let quiet = instance_file("quiet", 1, 2);
        let json = serde_json::to_string(&quiet).unwrap();
        assert!(!json.contains("tcp"), "{json}");
        assert!(!json.contains("\"ws\""), "{json}");

        let mut serving = instance_file("serving", 1, 2);
        serving.tcp = Some("127.0.0.1:7878".into());
        serving.ws = Some("127.0.0.1:7879".into());
        let round: InstanceFile =
            serde_json::from_str(&serde_json::to_string(&serving).unwrap()).unwrap();
        assert_eq!(round.tcp.as_deref(), Some("127.0.0.1:7878"));
        assert_eq!(round.ws.as_deref(), Some("127.0.0.1:7879"));
    }
}
