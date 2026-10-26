//! `<project>/.agix/teams/.lock` — a coarse, pid-reuse-safe lock over the
//! whole `teams/` directory for one project.
//!
//! One project is expected to be worked on by one daemon instance at a
//! time; a second instance touching the same `teams/` directory (index
//! rebuild, cross-team scan) concurrently is the rare case this exists to
//! reject rather than to arbitrate finely. Same staleness rules as
//! `daemon::discovery`'s single-instance lock (docs/ARCHITECTURE.md §7) (same staleness algorithm,
//! same [`base::process_identity::ProcessIdentity`] reuse-safe check) rather
//! than introducing a second locking mechanism.
//!
//! Not yet called from the team-creation/orchestration path: that only
//! becomes contended once `TeamStore` is promoted from per-session to
//! project-level (deferred — see the design doc's §6.6). Today, every
//! `TeamCreate` call gets its own freshly-generated `team_id` subdirectory,
//! so there is no concurrent write to the *same* file for this lock to
//! protect yet.

use base::process_identity::{decide_stale_lock, ProcessIdentity, StaleLockDecision};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamsDirLock {
    pub pid: u32,
    #[serde(default)]
    pub pid_start_time: i64,
    /// Free-form label for the holding daemon instance (e.g. `"desktop"`,
    /// `"cli"`) — surfaced to a rejected caller so a human can tell which
    /// process to look at, not used in the staleness decision itself.
    pub instance: String,
    pub acquired_at: String,
}

/// Who holds the lock, for [`TeamsDirLockError::Locked`] — shaped to drop
/// straight into the `TEAM_DIR_LOCKED` `error.data` in
/// `docs/daemon_rpc_protocol.md` §9.1.
#[derive(Debug, Clone)]
pub struct LockHolder {
    pub pid: u32,
    pub instance: String,
    pub acquired_at: String,
}

/// Write `<teams_dir>/.lock`, applying the same accept/reject/reclaim
/// staleness algorithm `daemon::discovery::write_lock_file` uses:
/// ```text
/// 文件不存在                    → 写入，获得锁
/// 文件存在
///   ├─ pid 是自己                → 重入
///   ├─ pid 存活 且 启动时间匹配  → 拒绝，Locked
///   ├─ pid 存活 但 启动时间不匹配 → pid 已被复用，视为陈旧，接管
///   └─ pid 不存在                → 陈旧，接管并覆写（记 warn）
/// ```
pub fn write_lock_file(
    teams_dir: &Path,
    instance: &str,
) -> Result<TeamsDirLock, TeamsDirLockError> {
    fs::create_dir_all(teams_dir).map_err(TeamsDirLockError::Io)?;
    let lock_path = teams_dir.join(".lock");
    let my_pid = std::process::id();

    let identity = ProcessIdentity::current();
    let lock = TeamsDirLock {
        pid: identity.pid,
        pid_start_time: identity.started_at,
        instance: instance.to_string(),
        acquired_at: iso_now(),
    };
    let body = serde_json::to_string_pretty(&lock).map_err(TeamsDirLockError::Json)?;

    // Fast path: nobody holds `lock_path` at all yet. `create_new` folds
    // the "does it exist" check and the write into one atomic filesystem
    // call, closing the window two processes starting at the same instant
    // would otherwise race through — both seeing `!lock_path.exists()`,
    // both writing a tmp file, both `rename`-ing over each other and
    // walking away believing they hold the lock alone.
    match open_lock_exclusive(&lock_path) {
        Ok(mut file) => {
            use std::io::Write;
            file.write_all(body.as_bytes())
                .map_err(TeamsDirLockError::Io)?;
            set_owner_only_permissions(&file).map_err(TeamsDirLockError::Io)?;
            return Ok(lock);
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(TeamsDirLockError::Io(e)),
    }

    // Someone already holds `lock_path` — read it to tell a live holder,
    // our own reentry, or a stale record apart.
    match fs::read(&lock_path) {
        Ok(bytes) => match serde_json::from_slice::<TeamsDirLock>(&bytes) {
            Ok(prev) if prev.pid != my_pid => {
                let recorded = ProcessIdentity {
                    pid: prev.pid,
                    started_at: prev.pid_start_time,
                };
                match decide_stale_lock(&recorded, "teams/.lock", &lock_path) {
                    StaleLockDecision::KeepExisting => {
                        return Err(TeamsDirLockError::Locked(LockHolder {
                            pid: prev.pid,
                            instance: prev.instance,
                            acquired_at: prev.acquired_at,
                        }));
                    }
                    StaleLockDecision::Reclaim => {}
                }
            }
            Ok(_) => { /* reentry — same pid, fall through and overwrite */ }
            Err(e) => {
                tracing::warn!(
                    teams_dir = %teams_dir.display(),
                    error = %e,
                    "teams/.lock content could not be parsed as JSON; treating as stale and overwriting"
                );
            }
        },
        Err(e) => {
            tracing::warn!(
                teams_dir = %teams_dir.display(),
                error = %e,
                "teams/.lock could not be read; treating as stale and overwriting"
            );
        }
    }

    write_lock_file_replacing(&lock_path, &body)?;
    Ok(lock)
}

fn open_lock_exclusive(lock_path: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(lock_path)
}

#[cfg(unix)]
fn set_owner_only_permissions(file: &fs::File) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = file.metadata()?.permissions();
    perms.set_mode(0o600);
    file.set_permissions(perms)
}

#[cfg(not(unix))]
fn set_owner_only_permissions(_file: &fs::File) -> io::Result<()> {
    Ok(())
}

/// Atomically replaces an already-existing (stale or reentrant) lock file:
/// write to a tmp path, then `rename` over `lock_path`. Reclaiming a lock
/// this way still has a narrow race if two processes both decide the same
/// existing lock is stale at once — the `create_new` fast path above is
/// what actually closes the "lock doesn't exist yet" race; this is "last
/// writer wins" for the reclaim case, which is no worse than before.
fn write_lock_file_replacing(lock_path: &Path, body: &str) -> Result<(), TeamsDirLockError> {
    let tmp = lock_path.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(&tmp, body).map_err(TeamsDirLockError::Io)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&tmp)
            .map_err(TeamsDirLockError::Io)?
            .permissions();
        perms.set_mode(0o600);
        fs::set_permissions(&tmp, perms).map_err(TeamsDirLockError::Io)?;
    }
    fs::rename(&tmp, lock_path).map_err(TeamsDirLockError::Io)?;
    Ok(())
}

/// Release the lock — a no-op if it's already gone (nothing to reverse).
pub fn release_lock_file(teams_dir: &Path) -> io::Result<()> {
    match fs::remove_file(teams_dir.join(".lock")) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn iso_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
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

#[derive(Debug, thiserror::Error)]
pub enum TeamsDirLockError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("malformed teams/.lock: {0}")]
    Json(#[from] serde_json::Error),
    #[error("teams/ directory is locked by pid={} (instance={})", .0.pid, .0.instance)]
    Locked(LockHolder),
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

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
    fn write_creates_the_teams_dir_and_lock_file() {
        let dir = TempDir::new().unwrap();
        let teams_dir = dir.path().join(".agix/teams");
        let written = write_lock_file(&teams_dir, "desktop").unwrap();
        assert_eq!(written.pid, std::process::id());
        assert!(teams_dir.join(".lock").exists());
    }

    #[test]
    fn write_reenters_when_the_recorded_pid_is_its_own() {
        let dir = TempDir::new().unwrap();
        let teams_dir = dir.path().join(".agix/teams");
        write_lock_file(&teams_dir, "first").unwrap();
        let written = write_lock_file(&teams_dir, "second")
            .expect("re-writing under the same pid must be a reentry, not a rejection");
        assert_eq!(written.instance, "second");
    }

    #[test]
    fn write_rejects_when_another_live_process_holds_the_lock() {
        let child = Child::spawn();
        // Same as the daemon's lock case: a confirmed-alive holder is the
        // premise, and a host that cannot read start times has none.
        let Some(recorded_start) = base::process_identity::process_start_time(child.pid()) else {
            eprintln!("skipping: this host cannot read process start times");
            return;
        };
        let dir = TempDir::new().unwrap();
        let teams_dir = dir.path().join(".agix/teams");
        fs::create_dir_all(&teams_dir).unwrap();
        let alive = TeamsDirLock {
            pid: child.pid(),
            pid_start_time: recorded_start,
            instance: "other-daemon".to_string(),
            acquired_at: iso_now(),
        };
        fs::write(teams_dir.join(".lock"), serde_json::to_vec(&alive).unwrap()).unwrap();

        let r = write_lock_file(&teams_dir, "me");
        match r {
            Err(TeamsDirLockError::Locked(holder)) => {
                assert_eq!(holder.pid, child.pid());
                assert_eq!(holder.instance, "other-daemon");
            }
            other => panic!("expected Locked, got {other:?}"),
        }
    }

    #[test]
    fn write_reclaims_the_lock_when_the_pid_was_reused_by_a_different_process() {
        let child = Child::spawn();
        let dir = TempDir::new().unwrap();
        let teams_dir = dir.path().join(".agix/teams");
        fs::create_dir_all(&teams_dir).unwrap();
        let stale = TeamsDirLock {
            pid: child.pid(),
            pid_start_time: 12345, // deliberately wrong — simulates reuse
            instance: "old-holder".to_string(),
            acquired_at: iso_now(),
        };
        fs::write(teams_dir.join(".lock"), serde_json::to_vec(&stale).unwrap()).unwrap();

        let written = write_lock_file(&teams_dir, "new-holder").unwrap();
        assert_eq!(written.instance, "new-holder");
    }

    #[test]
    fn write_reclaims_a_lock_from_a_dead_pid() {
        let dir = TempDir::new().unwrap();
        let teams_dir = dir.path().join(".agix/teams");
        fs::create_dir_all(&teams_dir).unwrap();
        let stale = TeamsDirLock {
            pid: 999_999_999,
            pid_start_time: 0,
            instance: "long-gone".to_string(),
            acquired_at: iso_now(),
        };
        fs::write(teams_dir.join(".lock"), serde_json::to_vec(&stale).unwrap()).unwrap();

        let written = write_lock_file(&teams_dir, "new-holder").unwrap();
        assert_ne!(written.pid, 999_999_999);
    }

    #[test]
    fn release_removes_the_lock_and_is_a_noop_if_already_gone() {
        let dir = TempDir::new().unwrap();
        let teams_dir = dir.path().join(".agix/teams");
        write_lock_file(&teams_dir, "me").unwrap();
        assert!(teams_dir.join(".lock").exists());
        release_lock_file(&teams_dir).unwrap();
        assert!(!teams_dir.join(".lock").exists());
        release_lock_file(&teams_dir).expect("releasing an already-gone lock must be a no-op");
    }
}
