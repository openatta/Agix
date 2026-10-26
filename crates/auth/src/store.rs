//! Token persistence. v0 uses plaintext JSON under `~/.agix/scenes/<scope>/tokens/<provider>.json`
//! with `0600` perms on Unix. Future: switch to `keyring` crate so the OS
//! keychain holds the secret.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use thiserror::Error;
use time::OffsetDateTime;

#[derive(Debug, Error)]
pub enum TokenStoreError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredToken {
    pub access_token: String,
    /// Unix seconds; None = no expiry advertised by provider.
    pub expires_at: Option<i64>,
    pub refresh_token: Option<String>,
    pub scopes: Vec<String>,
    pub token_type: String,
    /// When we last wrote this entry (for debugging stale tokens).
    pub stored_at: i64,
}

impl StoredToken {
    /// True if expired.
    pub fn is_expired(&self, now: OffsetDateTime) -> bool {
        match self.expires_at {
            Some(exp) => now.unix_timestamp() >= exp,
            None => false,
        }
    }

    /// Will the token be expired within `secs` from now? Used to refresh
    /// pre-emptively rather than mid-request.
    pub fn expires_within(&self, now: OffsetDateTime, secs: i64) -> bool {
        match self.expires_at {
            Some(exp) => now.unix_timestamp() + secs >= exp,
            None => false,
        }
    }
}

pub struct TokenStore {
    root: PathBuf,
}

impl TokenStore {
    /// Open a store rooted at `root` — conventionally `<scene root>/tokens`,
    /// but the caller says where.
    ///
    /// These are credentials, so there is no constructor that finds the
    /// directory on its own: whose home the tokens belong to is not a
    /// question this type gets to answer.
    pub fn at_root(root: PathBuf) -> Self {
        Self { root }
    }

    fn path_for(&self, provider: &str) -> PathBuf {
        let safe: String = provider
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        self.root.join(format!("{safe}.json"))
    }

    /// Persist to disk under the named slot.
    pub fn save(&self, provider: &str, token: &StoredToken) -> Result<(), TokenStoreError> {
        std::fs::create_dir_all(&self.root)?;
        let p = self.path_for(provider);
        let body = serde_json::to_string_pretty(token)?;
        std::fs::write(&p, body)?;
        // 0600 on Unix
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&p)?.permissions();
            perms.set_mode(0o600);
            let _ = std::fs::set_permissions(&p, perms);
        }
        Ok(())
    }

    /// Read from disk; `Ok(None)` if slot is missing.
    pub fn load(&self, provider: &str) -> Result<Option<StoredToken>, TokenStoreError> {
        let p = self.path_for(provider);
        if !p.exists() {
            return Ok(None);
        }
        let body = std::fs::read(&p)?;
        let t: StoredToken = serde_json::from_slice(&body)?;
        Ok(Some(t))
    }

    /// Remove the slot from disk.
    pub fn delete(&self, provider: &str) -> Result<(), TokenStoreError> {
        let p = self.path_for(provider);
        if p.exists() {
            std::fs::remove_file(p)?;
        }
        Ok(())
    }

    /// Names of all providers with stored tokens.
    pub fn list_providers(&self) -> Result<Vec<String>, TokenStoreError> {
        if !self.root.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for e in std::fs::read_dir(&self.root)? {
            let e = e?;
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) == Some("json") {
                if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                    out.push(stem.to_string());
                }
            }
        }
        out.sort();
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fake_token(at: &str) -> StoredToken {
        StoredToken {
            access_token: at.into(),
            expires_at: None,
            refresh_token: Some("rt".into()),
            scopes: vec!["read".into()],
            token_type: "Bearer".into(),
            stored_at: 0,
        }
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = TempDir::new().unwrap();
        let store = TokenStore::at_root(dir.path().to_path_buf());
        store.save("acme", &fake_token("a1")).unwrap();
        let loaded = store.load("acme").unwrap().unwrap();
        assert_eq!(loaded.access_token, "a1");
        assert_eq!(loaded.refresh_token.as_deref(), Some("rt"));
    }

    #[test]
    fn load_missing_returns_none() {
        let dir = TempDir::new().unwrap();
        let store = TokenStore::at_root(dir.path().to_path_buf());
        assert!(store.load("ghost").unwrap().is_none());
    }

    #[test]
    fn delete_removes_file() {
        let dir = TempDir::new().unwrap();
        let store = TokenStore::at_root(dir.path().to_path_buf());
        store.save("acme", &fake_token("a1")).unwrap();
        store.delete("acme").unwrap();
        assert!(store.load("acme").unwrap().is_none());
    }

    #[test]
    fn list_providers_sorted() {
        let dir = TempDir::new().unwrap();
        let store = TokenStore::at_root(dir.path().to_path_buf());
        store.save("zeta", &fake_token("z")).unwrap();
        store.save("alpha", &fake_token("a")).unwrap();
        let names = store.list_providers().unwrap();
        assert_eq!(names, vec!["alpha", "zeta"]);
    }

    #[test]
    fn provider_name_with_unsafe_chars_is_sanitised() {
        let dir = TempDir::new().unwrap();
        let store = TokenStore::at_root(dir.path().to_path_buf());
        store.save("a/b c", &fake_token("x")).unwrap();
        // round-trip works via the same sanitisation
        let loaded = store.load("a/b c").unwrap().unwrap();
        assert_eq!(loaded.access_token, "x");
    }

    #[test]
    fn is_expired_handles_no_expiry_and_past_expiry() {
        let mut t = fake_token("x");
        assert!(!t.is_expired(OffsetDateTime::now_utc()));
        t.expires_at = Some(0);
        assert!(t.is_expired(OffsetDateTime::now_utc()));
        t.expires_at = Some(i64::MAX);
        assert!(!t.is_expired(OffsetDateTime::now_utc()));
    }
}
