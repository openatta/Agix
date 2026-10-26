//! Content the log points at instead of carrying.
//!
//! A conversation accumulates things that are large and that nobody reads as
//! text: a pasted file, a screenshot, a tool result the size of a book. Inline
//! they make the JSONL slow to parse and impossible to skim. Moved out, the
//! log stays a readable record of what happened with a pointer where the bulk
//! was, and the bulk lives wherever the deployment wants it.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// A pointer to content kept outside the log.
///
/// # Why it names a store
///
/// A log outlives the backend that wrote it. Naming the store is what lets a
/// reader tell "this is somewhere I cannot reach" from "this log is corrupt":
/// the first is a session that loads with a gap where the content was, the
/// second is a session that does not load. Nothing may turn the first into
/// the second — see [`crate::entry::LogEntry::Blob`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BlobRef {
    /// Which store holds it. The kernel only ever compares this.
    pub store: String,
    /// That store's id for the content.
    pub id: String,
    /// The `kind` of the entry that was moved out, so a reader that cannot
    /// fetch the content still knows what stood here.
    pub describes: String,
    /// How many bytes were moved out.
    pub bytes: u64,
}

/// Where large content lives.
///
/// # What an implementation must guarantee
///
/// * **Content-addressed.** The same bytes produce the same id, and an id
///   keeps resolving to the bytes it was issued for. Log entries hold these
///   ids for as long as the log exists; a store that reissues one for
///   different content rewrites history.
/// * **Absent is [`None`], never an error.** Blobs get cleaned up, copied
///   without their sidecar, or left behind by an uninstalled backend, and a
///   session in any of those states still has to load.
/// * **[`name`](Self::name) is stable across runs.** It is written into the
///   log, and a store that renames itself orphans everything it wrote.
pub trait BlobStore: Send + Sync {
    /// What references written by this store say they belong to.
    fn name(&self) -> &str;

    fn put(&self, bytes: &[u8]) -> Result<String, std::io::Error>;

    fn get(&self, id: &str) -> Result<Option<Vec<u8>>, std::io::Error>;
}

/// Files under `<base>/pastes/`, keyed by content hash.
///
/// The shipped default: identical content is written once and every reference
/// to it shares an id, which is what makes the same 200 KB file pasted three
/// times cost 200 KB.
#[derive(Debug, Clone)]
pub struct PasteStore {
    dir: PathBuf,
}

impl PasteStore {
    /// Rooted at `base` (e.g. `~/.agix/scenes/<scope>`); the files live in
    /// `<base>/pastes/`.
    pub fn new(base: &Path) -> Self {
        Self {
            dir: base.join("pastes"),
        }
    }

    /// Store `content` and return its id.
    pub fn store(&self, content: &str) -> Result<String, std::io::Error> {
        self.put(content.as_bytes())
    }

    /// Load content by id, or `None` if it is not here.
    pub fn load(&self, paste_id: &str) -> Result<Option<String>, std::io::Error> {
        match self.get(paste_id)? {
            Some(bytes) => Ok(Some(String::from_utf8_lossy(&bytes).into_owned())),
            None => Ok(None),
        }
    }

    /// Remove files whose last modification time is older than 7 days.
    /// Returns how many were removed.
    pub fn cleanup(&self) -> Result<usize, std::io::Error> {
        let now = std::time::SystemTime::now();
        let max_age = std::time::Duration::from_secs(7 * 24 * 3600);
        let mut removed = 0;

        if !self.dir.exists() {
            return Ok(0);
        }

        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            let meta = entry.metadata()?;
            if !meta.is_file() {
                continue;
            }
            if let Ok(modified) = meta.modified() {
                if now
                    .duration_since(modified)
                    .unwrap_or(std::time::Duration::ZERO)
                    > max_age
                {
                    std::fs::remove_file(entry.path())?;
                    removed += 1;
                }
            }
        }
        Ok(removed)
    }
}

impl BlobStore for PasteStore {
    fn name(&self) -> &str {
        "paste"
    }

    fn put(&self, bytes: &[u8]) -> Result<String, std::io::Error> {
        let id = content_id(bytes);
        std::fs::create_dir_all(&self.dir)?;
        let path = self.dir.join(&id);
        if !path.exists() {
            std::fs::write(&path, bytes)?;
        }
        Ok(id)
    }

    fn get(&self, id: &str) -> Result<Option<Vec<u8>>, std::io::Error> {
        match std::fs::read(self.dir.join(id)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// The same thing without a disk.
///
/// A host embedding the engine for a conversation it will throw away pairs
/// this with [`crate::store::InMemoryHistoryStore`] and never writes a file,
/// while still keeping a screenshot out of every message it builds.
#[derive(Debug, Default)]
pub struct InMemoryBlobStore {
    blobs: std::sync::Mutex<HashMap<String, Vec<u8>>>,
}

impl InMemoryBlobStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.blobs.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl BlobStore for InMemoryBlobStore {
    fn name(&self) -> &str {
        "memory"
    }

    fn put(&self, bytes: &[u8]) -> Result<String, std::io::Error> {
        let id = content_id(bytes);
        self.blobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(id.clone())
            .or_insert_with(|| bytes.to_vec());
        Ok(id)
    }

    fn get(&self, id: &str) -> Result<Option<Vec<u8>>, std::io::Error> {
        Ok(self
            .blobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned())
    }
}

/// SHA-256, first 16 hex characters.
fn content_id(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))[..16].to_string()
}

/// The contract, exercised against both shipped stores.
#[cfg(test)]
mod contract_tests {
    use super::*;

    fn stores() -> (PasteStore, tempfile::TempDir, InMemoryBlobStore) {
        let dir = tempfile::TempDir::new().unwrap();
        (PasteStore::new(dir.path()), dir, InMemoryBlobStore::new())
    }

    macro_rules! for_each_blob_store {
        ($name:ident, |$store:ident| $body:block) => {
            #[test]
            fn $name() {
                let (paste, _dir, memory) = stores();
                {
                    let $store: &dyn BlobStore = &paste;
                    $body
                }
                {
                    let $store: &dyn BlobStore = &memory;
                    $body
                }
            }
        };
    }

    for_each_blob_store!(what_goes_in_comes_back_out, |store| {
        let id = store.put(b"a screenshot, pretend").unwrap();
        assert_eq!(
            store.get(&id).unwrap().as_deref(),
            Some(&b"a screenshot, pretend"[..])
        );
    });

    for_each_blob_store!(the_same_bytes_get_the_same_id, |store| {
        let first = store.put(b"identical content").unwrap();
        let second = store.put(b"identical content").unwrap();
        assert_eq!(first, second);
        assert_ne!(first, store.put(b"different content").unwrap());
    });

    for_each_blob_store!(content_that_is_not_here_is_none_not_an_error, |store| {
        assert_eq!(store.get("0123456789abcdef").unwrap(), None);
    });

    for_each_blob_store!(the_name_is_stable, |store| {
        let first = store.name().to_string();
        store.put(b"anything").unwrap();
        assert_eq!(store.name(), first);
        assert!(!first.is_empty());
    });

    #[test]
    fn the_two_stores_do_not_answer_to_the_same_name() {
        let (paste, _dir, memory) = stores();
        assert_ne!(paste.name(), memory.name());
    }

    #[test]
    fn a_paste_id_is_sixteen_hex_characters() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = PasteStore::new(dir.path());
        let id = store.store("hello paste store").unwrap();
        assert_eq!(id.len(), 16);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(
            store.load(&id).unwrap().as_deref(),
            Some("hello paste store")
        );
    }

    #[test]
    fn cleanup_skips_fresh_files_and_an_empty_directory() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = PasteStore::new(dir.path());
        assert_eq!(store.cleanup().unwrap(), 0);
        store.store("fresh content").unwrap();
        assert_eq!(store.cleanup().unwrap(), 0);
    }
}
