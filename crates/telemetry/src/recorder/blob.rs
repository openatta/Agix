//! Content-addressed blob storage for one recording.
//!
//! A recording references its bulky, highly repetitive parts — system prompt
//! blocks, the tool table, conversation messages — by content hash instead of
//! inlining them into every call record. Identical content stored twice
//! occupies one file, which is what keeps a recording linear in the number of
//! calls rather than quadratic in messages: the 50th call's 49 historical
//! messages are all references to blobs that already exist.
//!
//! Deduplication is scoped to a single recording directory. Sharing blobs
//! across recordings would save more space but would forfeit the property that
//! makes cleanup trivial for the host — deleting one recording is `rm -rf` on
//! its directory, with nothing else referencing what went away.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// A blob's identity: the first 16 hex characters of the SHA-256 of its
/// canonical JSON encoding.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BlobId(pub String);

impl BlobId {
    fn of(json: &str) -> Self {
        let digest = Sha256::digest(json.as_bytes());
        Self(hex::encode(&digest[..8]))
    }

    /// The id `value` would get, without storing it. Replay uses this to line a
    /// live request up against a recorded one without writing anything.
    pub fn of_value<T: Serialize>(value: &T) -> Self {
        match serde_json::to_string(value) {
            Ok(json) => Self::of(&json),
            Err(_) => Self(String::new()),
        }
    }
}

impl std::fmt::Display for BlobId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The `blobs/` directory of one recording.
///
/// Values are serialized with `serde_json`, whose output for a given Rust type
/// is field-order deterministic — the same value always hashes to the same id.
/// Content is stored verbatim: a recording's worth is that it holds what was
/// actually sent, so nothing is normalized or redacted on the way in.
#[derive(Debug, Clone)]
pub struct BlobStore {
    dir: PathBuf,
    /// The blobs this store has written or checked, which a recording puts
    /// again on every call (each call's history is mostly the last call's).
    known: Arc<Mutex<HashSet<BlobId>>>,
}

impl BlobStore {
    pub fn new(recording_dir: &Path) -> Self {
        Self {
            dir: recording_dir.join("blobs"),
            known: Arc::default(),
        }
    }

    pub fn put<T: Serialize>(&self, value: &T) -> Result<BlobId, std::io::Error> {
        let json = serde_json::to_string(value)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        self.put_raw(&json)
    }

    /// Store `json`, returning the id it is addressed by.
    ///
    /// An existing file with the same id normally holds the same bytes, so it
    /// is left alone. But a blob can be edited by hand — that is a supported
    /// way to explore a recording — and an edited file no longer matches the
    /// name it sits under. Overwriting it silently would make the next
    /// recording discard the edit without saying so, so the mismatch is
    /// reported and the correct content restored.
    pub fn put_raw(&self, json: &str) -> Result<BlobId, std::io::Error> {
        let id = BlobId::of(json);
        if self.is_known(&id) {
            return Ok(id);
        }
        let path = self.dir.join(&id.0);
        let on_disk = match std::fs::metadata(&path) {
            Ok(meta) if meta.len() == json.len() as u64 => std::fs::read_to_string(&path),
            Ok(_) => Ok(String::new()),
            Err(e) => Err(e),
        };
        match on_disk {
            Ok(existing) if existing == json => {
                self.remember(id.clone());
                return Ok(id);
            }
            Ok(_) => {
                tracing::warn!(
                    blob = %id,
                    dir = %self.dir.display(),
                    "recorder: blob content does not match its id — an edit is being overwritten \
                     by a fresh recording; copy it elsewhere first"
                );
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        super::private_dir(&self.dir)?;
        std::io::Write::write_all(&mut super::private_file(&path)?, json.as_bytes())?;
        self.remember(id.clone());
        Ok(id)
    }

    fn is_known(&self, id: &BlobId) -> bool {
        self.known
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(id)
    }

    fn remember(&self, id: BlobId) {
        self.known
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id);
    }

    pub fn get<T: DeserializeOwned>(&self, id: &BlobId) -> Result<Option<T>, std::io::Error> {
        let Some(json) = self.get_raw(id)? else {
            return Ok(None);
        };
        serde_json::from_str(&json)
            .map(Some)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    pub fn get_raw(&self, id: &BlobId) -> Result<Option<String>, std::io::Error> {
        match std::fs::read_to_string(self.dir.join(&id.0)) {
            Ok(s) => Ok(Some(s)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (BlobStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (BlobStore::new(dir.path()), dir)
    }

    #[test]
    fn same_content_yields_one_file() {
        let (store, dir) = store();
        let a = store.put(&"hello").unwrap();
        let b = store.put(&"hello").unwrap();
        assert_eq!(a, b);
        let count = std::fs::read_dir(dir.path().join("blobs")).unwrap().count();
        assert_eq!(count, 1);
    }

    #[test]
    fn different_content_yields_different_ids() {
        let (store, _dir) = store();
        assert_ne!(store.put(&"a").unwrap(), store.put(&"b").unwrap());
    }

    #[test]
    fn round_trips_a_typed_value() {
        let (store, _dir) = store();
        let value = vec![1u32, 2, 3];
        let id = store.put(&value).unwrap();
        assert_eq!(store.get::<Vec<u32>>(&id).unwrap(), Some(value));
    }

    #[test]
    fn missing_blob_reads_as_none() {
        let (store, _dir) = store();
        let absent = BlobId("0123456789abcdef".into());
        assert_eq!(store.get_raw(&absent).unwrap(), None);
    }

    #[test]
    fn id_is_sixteen_hex_chars() {
        let (store, _dir) = store();
        let id = store.put(&"anything").unwrap();
        assert_eq!(id.0.len(), 16);
        assert!(id.0.chars().all(|c| c.is_ascii_hexdigit()));
    }

    /// Reading a blob does not verify its hash, which is what makes editing one
    /// by hand a usable way to explore a recording. The cost is that a later
    /// recording finds a file whose content no longer matches its name — it
    /// must restore the real content rather than trust the name and skip.
    #[test]
    fn a_hand_edited_blob_is_restored_rather_than_trusted() {
        let (store, dir) = store();
        let id = store.put(&"original").unwrap();
        let path = dir.path().join("blobs").join(&id.0);
        std::fs::write(&path, "tampered").unwrap();
        assert_eq!(store.get_raw(&id).unwrap().as_deref(), Some("tampered"));

        let later = BlobStore::new(dir.path());
        let again = later.put(&"original").unwrap();
        assert_eq!(again, id);
        assert_eq!(
            store.get::<String>(&id).unwrap().as_deref(),
            Some("original"),
            "re-recording must not leave a blob whose content contradicts its id"
        );
    }

    #[test]
    fn a_blob_put_again_is_not_read_back() {
        let (store, dir) = store();
        let id = store.put(&"original").unwrap();
        let path = dir.path().join("blobs").join(&id.0);
        std::fs::remove_file(&path).unwrap();
        assert_eq!(store.put(&"original").unwrap(), id);
        assert!(!path.exists(), "the second put went to disk");
    }
}
