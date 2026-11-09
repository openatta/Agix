//! The process groups this process has started and would leave running if it
//! died without stopping them.
//!
//! Commands lead groups of their own (see `process`), and so do MCP stdio
//! servers (put on the list with [`list`]), so they do not die with the
//! daemon. A clean shutdown stops them through their handles and ledgers;
//! an abort or a `SIGKILL` runs neither. This keeps the one process-wide list
//! those two cases need: [`kill_all`] for a panic hook that is about to abort,
//! and a copy on disk that the next run — after a supervisor restarts it —
//! hands to [`reap_previous`].

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::process_identity::{process_start_time, ProcessIdentity, ProcessLiveness};

struct Orphans {
    groups: BTreeSet<i32>,
    file: Option<(PathBuf, ProcessIdentity)>,
}

static ORPHANS: Mutex<Orphans> = Mutex::new(Orphans {
    groups: BTreeSet::new(),
    file: None,
});

#[derive(Serialize, Deserialize)]
struct Record {
    owner_pid: u32,
    owner_started_at: i64,
    groups: Vec<i32>,
}

pub(super) fn started(group: i32) {
    update(|groups| {
        groups.retain(|g| super::process::group_alive(*g));
        groups.insert(group)
    });
}

pub(super) fn stopped(group: i32) {
    update(|groups| groups.remove(&group));
}

/// A process group started outside `process` — an MCP server's — on the
/// list until this is dropped.
#[derive(Debug)]
pub struct Listed(i32);

pub fn list(group: i32) -> Listed {
    started(group);
    Listed(group)
}

impl Drop for Listed {
    fn drop(&mut self) {
        stopped(self.0);
    }
}

/// The groups on the list now.
pub fn listed() -> Vec<i32> {
    ORPHANS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .groups
        .iter()
        .copied()
        .collect()
}

fn update(change: impl FnOnce(&mut BTreeSet<i32>) -> bool) {
    let mut orphans = ORPHANS.lock().unwrap_or_else(|e| e.into_inner());
    let changed = change(&mut orphans.groups);
    if let (true, Some((path, owner))) = (changed, &orphans.file) {
        write(path, owner, &orphans.groups);
    }
}

fn write(path: &Path, owner: &ProcessIdentity, groups: &BTreeSet<i32>) {
    let record = Record {
        owner_pid: owner.pid,
        owner_started_at: owner.started_at,
        groups: groups.iter().copied().collect(),
    };
    let Ok(bytes) = serde_json::to_vec(&record) else {
        return;
    };
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, bytes).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// Keeps the list at `path` from now on, so a run that dies without a chance
/// to stop its commands leaves the next one a list of what to stop.
pub fn persist_to(path: PathBuf) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let owner = ProcessIdentity::current();
    let mut orphans = ORPHANS.lock().unwrap_or_else(|e| e.into_inner());
    write(&path, &owner, &orphans.groups);
    orphans.file = Some((path, owner));
}

/// `SIGKILL` to every group on the list, for a process that is about to abort.
///
/// Never blocks: a panic raised while the list is locked finds it locked by
/// its own thread, and the on-disk copy is left for the next run instead.
pub fn kill_all() -> Vec<i32> {
    let Ok(orphans) = ORPHANS.try_lock() else {
        return Vec::new();
    };
    let groups: Vec<i32> = orphans.groups.iter().copied().collect();
    for g in &groups {
        #[cfg(unix)]
        super::process::signal_group(*g, libc::SIGKILL);
    }
    groups
}

/// Stops the groups a previous run recorded at `path` and did not live to
/// stop, then removes the record. Returns the groups it stopped.
///
/// A record whose owner is still running — or might be — is left alone: it is
/// another daemon's list, not leftovers. Blocks for up to `grace` between
/// `SIGTERM` and `SIGKILL`.
pub fn reap_previous(path: &Path, grace: Duration) -> Vec<i32> {
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };
    let Ok(record) = serde_json::from_slice::<Record>(&bytes) else {
        let _ = std::fs::remove_file(path);
        return Vec::new();
    };
    let owner = ProcessIdentity {
        pid: record.owner_pid,
        started_at: record.owner_started_at,
    };
    if owner.pid == std::process::id() {
        return Vec::new();
    }
    match ProcessIdentity::check(&owner) {
        ProcessLiveness::Alive | ProcessLiveness::Unknown => return Vec::new(),
        ProcessLiveness::Dead | ProcessLiveness::Reused => {}
    }
    let written = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0)
        });
    let targets: Vec<i32> = record
        .groups
        .into_iter()
        .filter(|g| super::process::group_alive(*g) && !leader_is_newer(*g, written))
        .collect();
    #[cfg(unix)]
    {
        for g in &targets {
            super::process::signal_group(*g, libc::SIGTERM);
        }
        let deadline = std::time::Instant::now() + grace;
        while std::time::Instant::now() < deadline
            && targets.iter().any(|g| super::process::group_alive(*g))
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        for g in &targets {
            if super::process::group_alive(*g) {
                super::process::signal_group(*g, libc::SIGKILL);
            }
        }
    }
    #[cfg(not(unix))]
    let _ = grace;
    let _ = std::fs::remove_file(path);
    targets
}

/// A group id is not reused while the group has members, but once it empties
/// the id can come back as someone else's. A leader that started after the
/// record was last written cannot be one of ours. A leader that has exited is
/// no evidence either way, and its group is still the one the record names.
fn leader_is_newer(group: i32, written: i64) -> bool {
    process_start_time(group as u32).is_some_and(|started| started > written + 2)
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;

    fn spawn_group(cmd: &str) -> std::process::Child {
        use std::os::unix::process::CommandExt;
        std::process::Command::new("sh")
            .args(["-c", cmd])
            .process_group(0)
            .spawn()
            .unwrap()
    }

    #[test]
    fn a_dead_owners_groups_are_stopped_and_the_record_removed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("groups.json");
        let mut child = spawn_group("sleep 30");
        let group = child.id() as i32;
        let record = Record {
            owner_pid: i32::MAX as u32,
            owner_started_at: 1,
            groups: vec![group],
        };
        std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();

        assert_eq!(
            reap_previous(&path, Duration::from_millis(500)),
            vec![group]
        );
        let _ = child.wait();
        assert!(!super::super::process::group_alive(group));
        assert!(!path.exists());
    }

    #[test]
    fn a_live_owners_record_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("groups.json");
        let mut child = spawn_group("sleep 30");
        let group = child.id() as i32;
        let owner = std::os::unix::process::parent_id();
        let record = Record {
            owner_pid: owner,
            owner_started_at: process_start_time(owner).unwrap_or(0),
            groups: vec![group],
        };
        std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();

        assert!(reap_previous(&path, Duration::from_millis(100)).is_empty());
        assert!(path.exists());
        let _ = child.kill();
        let _ = child.wait();
    }
}
