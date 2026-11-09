//! Which process writes a session's log.
//!
//! A log has one writer, and two daemons sharing a state directory must not
//! both append to one session. Each session has a lock file in its sidecar
//! directory, `<sessions_root>/<id>/writer.lock`, held with an advisory
//! `flock` by the process writing it. The kernel drops the lock when that
//! process exits, however it exits, so a lock is never stale.
//!
//! Within a process every holder of a session shares one open lock file: a
//! `flock` belongs to an open file, and a second open of the same file by the
//! same process would be refused by the first.

use crate::error::HistoryError;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, MutexGuard};

const LOCK_FILE: &str = "writer.lock";

struct Held {
    _file: std::fs::File,
    holders: usize,
}

static HELD: LazyLock<Mutex<HashMap<PathBuf, Held>>> = LazyLock::new(Default::default);

fn held() -> MutexGuard<'static, HashMap<PathBuf, Held>> {
    HELD.lock().unwrap_or_else(|e| e.into_inner())
}

/// This process's hold on one session's log; released when the last one for
/// that session is dropped.
#[derive(Debug)]
pub(crate) struct WriterLock {
    path: PathBuf,
}

impl Drop for WriterLock {
    fn drop(&mut self) {
        let mut held = held();
        if let Some(entry) = held.get_mut(&self.path) {
            entry.holders -= 1;
            if entry.holders == 0 {
                held.remove(&self.path);
            }
        }
    }
}

/// Hold the lock in `sidecar_dir`, or [`HistoryError::Locked`] when another
/// process holds it.
pub(crate) fn hold(sidecar_dir: &Path, session: &str) -> Result<WriterLock, HistoryError> {
    std::fs::create_dir_all(sidecar_dir)?;
    let path = std::fs::canonicalize(sidecar_dir)?.join(LOCK_FILE);
    let mut held = held();
    if let Some(entry) = held.get_mut(&path) {
        entry.holders += 1;
        return Ok(WriterLock { path });
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)?;
    try_lock_exclusive(&file).map_err(|e| match e.kind() {
        std::io::ErrorKind::WouldBlock => HistoryError::Locked(session.to_string()),
        _ => HistoryError::Io(e),
    })?;
    held.insert(
        path.clone(),
        Held {
            _file: file,
            holders: 1,
        },
    );
    Ok(WriterLock { path })
}

#[cfg(unix)]
fn try_lock_exclusive(file: &std::fs::File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: `flock` only reads the descriptor, which `file` keeps open.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(unix))]
fn try_lock_exclusive(_file: &std::fs::File) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holders_in_one_process_share_the_lock_and_release_it_together() {
        let dir = tempfile::TempDir::new().unwrap();
        let sidecar = dir.path().join("s");
        let first = hold(&sidecar, "s").unwrap();
        let second = hold(&sidecar, "s").unwrap();
        drop(first);
        let path = std::fs::canonicalize(&sidecar).unwrap().join(LOCK_FILE);
        assert!(held().contains_key(&path));
        drop(second);
        assert!(!held().contains_key(&path));

        let other = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        try_lock_exclusive(&other).expect("released once the last holder is gone");
    }

    #[cfg(unix)]
    #[test]
    fn a_lock_held_by_another_open_file_is_refused() {
        let dir = tempfile::TempDir::new().unwrap();
        let sidecar = dir.path().join("s");
        std::fs::create_dir_all(&sidecar).unwrap();
        let elsewhere = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(sidecar.join(LOCK_FILE))
            .unwrap();
        try_lock_exclusive(&elsewhere).unwrap();
        assert!(matches!(
            hold(&sidecar, "s"),
            Err(HistoryError::Locked(id)) if id == "s"
        ));
    }
}
