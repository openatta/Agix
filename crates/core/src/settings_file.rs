//! Read-modify-write of the settings files the engine changes itself —
//! `project.trust`, `config.setProvider`, `mcp.addServer` / `updateServer`
//! and the "always allow" answer to a permission prompt.
//!
//! [`SettingsDir::lock`] holds one tier's directory against every other
//! writer: other threads of this process, and, on unix, other processes
//! through `flock` on the directory. While it is held, [`SettingsDir::read`] and
//! [`SettingsDir::write`] act on the files in it. A write goes to a temporary
//! file in the same directory, is flushed, and is renamed over the old one,
//! so a reader sees the old file or the new one and never part of one.
//! `settings.local.json` holds credentials and is always written `0600`; any
//! other file keeps the mode it had.
//!
//! A settings file that is a symbolic link is refused, for reading and for
//! writing alike. In a project tier the link is repository content: writing
//! through it would let a repository pick which of the user's files is
//! rewritten, and replacing it would quietly undo a user's own dotfile link.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

use serde_json::{Map, Value};

use crate::settings::SETTINGS_LOCAL_FILE;

#[derive(Debug, thiserror::Error)]
pub enum SettingsFileError {
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("existing {} is not valid JSON, refusing to overwrite it: {source}", path.display())]
    NotJson {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("existing {} is not a JSON object at the top level, refusing to overwrite it", path.display())]
    NotObject { path: PathBuf },
    #[error("{} is a symbolic link; settings are not written through one, edit the file it points to instead", path.display())]
    Symlink { path: PathBuf },
}

impl SettingsFileError {
    fn io(path: &Path) -> impl FnOnce(std::io::Error) -> Self + '_ {
        move |source| Self::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}

/// One tier's settings directory, held for a read-modify-write.
pub struct SettingsDir {
    dir: PathBuf,
    _process: MutexGuard<'static, ()>,
    /// The directory itself, open: `flock` holds it, and syncing it makes
    /// a rename durable.
    #[cfg(unix)]
    handle: std::fs::File,
}

impl SettingsDir {
    /// Create `dir` if it is missing and wait until no one else holds it.
    pub fn lock(dir: &Path) -> Result<Self, SettingsFileError> {
        std::fs::create_dir_all(dir).map_err(SettingsFileError::io(dir))?;
        let dir = dir.canonicalize().map_err(SettingsFileError::io(dir))?;
        let process = process_lock(&dir);
        #[cfg(unix)]
        let handle = {
            let handle = std::fs::File::open(&dir).map_err(SettingsFileError::io(&dir))?;
            flock_exclusive(&handle).map_err(SettingsFileError::io(&dir))?;
            handle
        };
        Ok(Self {
            dir,
            _process: process,
            #[cfg(unix)]
            handle,
        })
    }

    pub fn path(&self, file: &str) -> PathBuf {
        self.dir.join(file)
    }

    /// The file's top-level object; empty when it does not exist or has
    /// nothing in it.
    pub fn read(&self, file: &str) -> Result<Map<String, Value>, SettingsFileError> {
        let path = self.path(file);
        refuse_symlink(&path)?;
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Map::new()),
            Err(source) => return Err(SettingsFileError::Io { path, source }),
        };
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return Ok(Map::new());
        }
        let parsed: Value =
            serde_json::from_slice(&bytes).map_err(|source| SettingsFileError::NotJson {
                path: path.clone(),
                source,
            })?;
        match parsed {
            Value::Object(fields) => Ok(fields),
            _ => Err(SettingsFileError::NotObject { path }),
        }
    }

    pub fn write(&self, file: &str, value: &Map<String, Value>) -> Result<(), SettingsFileError> {
        let path = self.path(file);
        refuse_symlink(&path)?;
        let body =
            serde_json::to_string_pretty(value).map_err(|source| SettingsFileError::NotJson {
                path: path.clone(),
                source,
            })?;
        let tmp = self.dir.join(format!(".{file}.{}.tmp", std::process::id()));
        let written = write_new(&tmp, body.as_bytes(), mode_for(file, &path))
            .and_then(|()| std::fs::rename(&tmp, &path));
        if let Err(source) = written {
            let _ = std::fs::remove_file(&tmp);
            return Err(SettingsFileError::Io { path, source });
        }
        #[cfg(unix)]
        let _ = self.handle.sync_all();
        if file == SETTINGS_LOCAL_FILE || file == "mcp.local.json" {
            crate::paths::ensure_local_data_gitignore(&self.dir);
        }
        Ok(())
    }
}

/// `None` keeps what the system gives a new file.
fn mode_for(file: &str, path: &Path) -> Option<u32> {
    if file == SETTINGS_LOCAL_FILE {
        return Some(0o600);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .ok()
            .map(|m| m.permissions().mode() & 0o7777)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

fn write_new(tmp: &Path, body: &[u8], mode: Option<u32>) -> std::io::Result<()> {
    use std::io::Write;
    let _ = std::fs::remove_file(tmp);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        if let Some(mode) = mode {
            options.mode(mode);
        }
    }
    let mut file = options.open(tmp)?;
    #[cfg(unix)]
    if let Some(mode) = mode {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = mode;
    file.write_all(body)?;
    file.sync_all()
}

fn refuse_symlink(path: &Path) -> Result<(), SettingsFileError> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => Err(SettingsFileError::Symlink {
            path: path.to_path_buf(),
        }),
        _ => Ok(()),
    }
}

/// Other threads of this process queue here — which is also all the
/// exclusion a platform without `flock` gets.
fn process_lock(dir: &Path) -> MutexGuard<'static, ()> {
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, &'static Mutex<()>>>> = OnceLock::new();
    let mut locks = LOCKS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let lock: &'static Mutex<()> = match locks.get(dir) {
        Some(lock) => lock,
        None => {
            let lock = Box::leak(Box::default());
            locks.insert(dir.to_path_buf(), lock);
            lock
        }
    };
    drop(locks);
    lock.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(unix)]
fn flock_exclusive(file: &std::fs::File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    loop {
        // SAFETY: `flock` only reads the descriptor, which `file` keeps open.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
            return Ok(());
        }
        let e = std::io::Error::last_os_error();
        if e.kind() != std::io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::SETTINGS_FILE;
    use serde_json::json;

    fn obj(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(fields) => fields,
            _ => unreachable!(),
        }
    }

    #[test]
    fn a_missing_or_empty_file_reads_as_an_empty_object() {
        let dir = tempfile::tempdir().unwrap();
        let tier = SettingsDir::lock(&dir.path().join("new")).unwrap();
        assert_eq!(Value::Object(tier.read(SETTINGS_FILE).unwrap()), json!({}));
        std::fs::write(tier.path(SETTINGS_FILE), "  \n").unwrap();
        assert_eq!(Value::Object(tier.read(SETTINGS_FILE).unwrap()), json!({}));
    }

    #[test]
    fn what_is_not_a_settings_object_is_refused_not_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let tier = SettingsDir::lock(dir.path()).unwrap();
        std::fs::write(tier.path(SETTINGS_FILE), "{oops").unwrap();
        assert!(matches!(
            tier.read(SETTINGS_FILE),
            Err(SettingsFileError::NotJson { .. })
        ));
        std::fs::write(tier.path(SETTINGS_FILE), "[]").unwrap();
        assert!(matches!(
            tier.read(SETTINGS_FILE),
            Err(SettingsFileError::NotObject { .. })
        ));
    }

    /// The first credentials written into a directory bring a `.gitignore`
    /// that keeps them, and the engine's other local files, out of the
    /// repository the directory sits in. One already there is the user's.
    #[test]
    fn writing_credentials_keeps_them_out_of_git() {
        let dir = tempfile::tempdir().unwrap();
        let tier = SettingsDir::lock(dir.path()).unwrap();
        tier.write(SETTINGS_LOCAL_FILE, &obj(json!({"providers": {}})))
            .unwrap();
        let ignored = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        for line in ["settings.local.json", "mcp.local.json", "telemetry/"] {
            assert!(ignored.lines().any(|l| l == line), "{ignored}");
        }

        let mine = tempfile::tempdir().unwrap();
        std::fs::write(mine.path().join(".gitignore"), "mine\n").unwrap();
        let tier = SettingsDir::lock(mine.path()).unwrap();
        tier.write(SETTINGS_LOCAL_FILE, &obj(json!({}))).unwrap();
        assert_eq!(
            std::fs::read_to_string(mine.path().join(".gitignore")).unwrap(),
            "mine\n"
        );
    }

    #[test]
    fn a_write_replaces_the_file_whole_and_leaves_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        let tier = SettingsDir::lock(dir.path()).unwrap();
        tier.write(SETTINGS_FILE, &obj(json!({"language": "en"})))
            .unwrap();
        tier.write(SETTINGS_FILE, &obj(json!({"language": "zh"})))
            .unwrap();
        assert_eq!(
            Value::Object(tier.read(SETTINGS_FILE).unwrap()),
            json!({"language": "zh"})
        );
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, [SETTINGS_FILE]);
    }

    #[cfg(unix)]
    #[test]
    fn the_local_file_is_always_private_and_the_shared_one_keeps_its_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let tier = SettingsDir::lock(dir.path()).unwrap();
        let mode = |file: &str| {
            std::fs::metadata(tier.path(file))
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        };

        std::fs::write(tier.path(SETTINGS_LOCAL_FILE), "{}").unwrap();
        std::fs::set_permissions(
            tier.path(SETTINGS_LOCAL_FILE),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        tier.write(SETTINGS_LOCAL_FILE, &obj(json!({"a": 1})))
            .unwrap();
        assert_eq!(mode(SETTINGS_LOCAL_FILE), 0o600);

        std::fs::write(tier.path(SETTINGS_FILE), "{}").unwrap();
        std::fs::set_permissions(
            tier.path(SETTINGS_FILE),
            std::fs::Permissions::from_mode(0o640),
        )
        .unwrap();
        tier.write(SETTINGS_FILE, &obj(json!({"a": 1}))).unwrap();
        assert_eq!(mode(SETTINGS_FILE), 0o640);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_settings_file_is_neither_read_nor_written() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = dir.path().join("elsewhere.json");
        std::fs::write(&elsewhere, r#"{"keep": true}"#).unwrap();
        let tier_dir = dir.path().join(".agix");
        std::fs::create_dir_all(&tier_dir).unwrap();
        std::os::unix::fs::symlink(&elsewhere, tier_dir.join(SETTINGS_LOCAL_FILE)).unwrap();

        let tier = SettingsDir::lock(&tier_dir).unwrap();
        assert!(matches!(
            tier.read(SETTINGS_LOCAL_FILE),
            Err(SettingsFileError::Symlink { .. })
        ));
        assert!(matches!(
            tier.write(SETTINGS_LOCAL_FILE, &obj(json!({"api_key": "k"}))),
            Err(SettingsFileError::Symlink { .. })
        ));
        assert_eq!(
            std::fs::read_to_string(&elsewhere).unwrap(),
            r#"{"keep": true}"#
        );
        assert!(
            std::fs::symlink_metadata(tier_dir.join(SETTINGS_LOCAL_FILE))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn concurrent_edits_of_one_directory_do_not_lose_updates() {
        let dir = tempfile::tempdir().unwrap();
        let n = 16;
        std::thread::scope(|scope| {
            for i in 0..n {
                let dir = dir.path();
                scope.spawn(move || {
                    let tier = SettingsDir::lock(dir).unwrap();
                    let mut value = tier.read(SETTINGS_LOCAL_FILE).unwrap();
                    value.insert(format!("k{i}"), json!(i));
                    tier.write(SETTINGS_LOCAL_FILE, &value).unwrap();
                });
            }
        });
        let tier = SettingsDir::lock(dir.path()).unwrap();
        assert_eq!(tier.read(SETTINGS_LOCAL_FILE).unwrap().len(), n);
    }
}
