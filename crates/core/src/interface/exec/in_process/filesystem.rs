//! A filesystem that is a map.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::interface::exec::{DirEntry, ExecError, FileSystem, Metadata};

/// A tree of files held in memory.
///
/// Cloning shares the tree, so a provider set handed to a tool and the handle
/// a test keeps are the same filesystem.
#[derive(Clone, Default)]
pub struct InMemoryFileSystem {
    files: Arc<Mutex<BTreeMap<PathBuf, Vec<u8>>>>,
    dirs: Arc<Mutex<BTreeMap<PathBuf, ()>>>,
}

impl InMemoryFileSystem {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed a file, creating its parents.
    pub fn with_file(self, path: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> Self {
        let p = normalize(path.as_ref());
        self.mark_parents(&p);
        self.files
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(p, contents.as_ref().to_vec());
        self
    }

    /// What is in there now, for a test to assert against.
    pub fn snapshot(&self) -> BTreeMap<PathBuf, Vec<u8>> {
        self.files
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn mark_parents(&self, path: &Path) {
        let mut dirs = self
            .dirs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut cur = path.parent();
        while let Some(d) = cur {
            if d.as_os_str().is_empty() {
                break;
            }
            dirs.insert(d.to_path_buf(), ());
            cur = d.parent();
        }
    }

    fn is_dir(&self, path: &Path) -> bool {
        self.dirs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(path)
    }
}

/// Lexical normalization only. There are no symlinks here, which is the one
/// place this provider is honestly simpler rather than merely smaller: a
/// remote's symlink graph is a real thing that this one does not have.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn missing(path: &Path) -> ExecError {
    ExecError::failed(format!("{}: no such file", path.display()))
}

#[async_trait::async_trait]
impl FileSystem for InMemoryFileSystem {
    async fn read(&self, path: &Path) -> Result<Vec<u8>, ExecError> {
        let p = normalize(path);
        self.files
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&p)
            .cloned()
            .ok_or_else(|| missing(path))
    }

    async fn write(&self, path: &Path, bytes: &[u8]) -> Result<(), ExecError> {
        let p = normalize(path);
        self.mark_parents(&p);
        self.files
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(p, bytes.to_vec());
        Ok(())
    }

    async fn create_dir_all(&self, path: &Path) -> Result<(), ExecError> {
        let p = normalize(path);
        self.dirs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(p.clone(), ());
        self.mark_parents(&p);
        Ok(())
    }

    async fn read_dir(&self, path: &Path) -> Result<Vec<DirEntry>, ExecError> {
        let p = normalize(path);
        if !self.is_dir(&p)
            && !self
                .files
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .keys()
                .any(|k| k.starts_with(&p))
        {
            return Err(missing(path));
        }
        let mut out: Vec<DirEntry> = Vec::new();
        for k in self
            .files
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
        {
            if k.parent() == Some(p.as_path()) {
                out.push(DirEntry {
                    path: k.clone(),
                    is_dir: false,
                });
            }
        }
        for d in self
            .dirs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
        {
            if d.parent() == Some(p.as_path()) {
                out.push(DirEntry {
                    path: d.clone(),
                    is_dir: true,
                });
            }
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    async fn remove_dir_all(&self, path: &Path) -> Result<(), ExecError> {
        let p = normalize(path);
        self.files
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|k, _| !k.starts_with(&p));
        self.dirs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|k, _| !k.starts_with(&p));
        Ok(())
    }

    async fn metadata(&self, path: &Path) -> Result<Metadata, ExecError> {
        let p = normalize(path);
        if let Some(bytes) = self
            .files
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&p)
        {
            return Ok(Metadata {
                is_dir: false,
                is_file: true,
                is_symlink: false,
                len: bytes.len() as u64,
            });
        }
        if self.is_dir(&p) {
            return Ok(Metadata {
                is_dir: true,
                is_file: false,
                is_symlink: false,
                len: 0,
            });
        }
        Err(missing(path))
    }

    async fn rename(&self, from: &Path, to: &Path) -> Result<(), ExecError> {
        let (from_n, to_n) = (normalize(from), normalize(to));
        let moved = |p: &PathBuf| match p.strip_prefix(&from_n) {
            Ok(rest) if rest.as_os_str().is_empty() => Some(to_n.clone()),
            Ok(rest) => Some(to_n.join(rest)),
            Err(_) => None,
        };
        let mut files = self
            .files
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut dirs = self
            .dirs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !files.contains_key(&from_n) && !dirs.contains_key(&from_n) {
            return Err(missing(from));
        }
        let files_moved: Vec<PathBuf> = files
            .keys()
            .filter(|k| moved(k).is_some())
            .cloned()
            .collect();
        for old in files_moved {
            if let (Some(new), Some(bytes)) = (moved(&old), files.remove(&old)) {
                files.insert(new, bytes);
            }
        }
        let dirs_moved: Vec<PathBuf> = dirs
            .keys()
            .filter(|k| moved(k).is_some())
            .cloned()
            .collect();
        for old in dirs_moved {
            dirs.remove(&old);
            if let Some(new) = moved(&old) {
                dirs.insert(new, ());
            }
        }
        Ok(())
    }

    async fn canonicalize(&self, path: &Path) -> Result<PathBuf, ExecError> {
        let p = normalize(path);
        if self
            .files
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(&p)
            || self.is_dir(&p)
        {
            Ok(p)
        } else {
            Err(missing(path))
        }
    }
}
