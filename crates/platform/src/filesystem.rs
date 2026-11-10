//! `LocalFileSystem` — this machine's disk.

use std::path::{Path, PathBuf};

use base::interface::exec::{DirEntry, ExecError, FileSystem, Metadata};

pub struct LocalFileSystem;

/// Classify an io error the way the contract's three sentences require: a
/// permission refusal is the operating system's policy talking, everything
/// else is the target failing. Nothing here is ever `Unavailable` — the local
/// disk is the one provider that cannot be out of reach.
fn failed(path: &Path, e: std::io::Error) -> ExecError {
    if e.kind() == std::io::ErrorKind::PermissionDenied {
        ExecError::denied(format!("{}: {e}", path.display()))
    } else {
        ExecError::failed(format!("{}: {e}", path.display()))
    }
}

#[async_trait::async_trait]
impl FileSystem for LocalFileSystem {
    async fn read(&self, path: &Path) -> Result<Vec<u8>, ExecError> {
        tokio::fs::read(path).await.map_err(|e| failed(path, e))
    }

    async fn write(&self, path: &Path, bytes: &[u8]) -> Result<(), ExecError> {
        tokio::fs::write(path, bytes)
            .await
            .map_err(|e| failed(path, e))
    }

    async fn create_dir_all(&self, path: &Path) -> Result<(), ExecError> {
        tokio::fs::create_dir_all(path)
            .await
            .map_err(|e| failed(path, e))
    }

    async fn read_dir(&self, path: &Path) -> Result<Vec<DirEntry>, ExecError> {
        let mut rd = tokio::fs::read_dir(path)
            .await
            .map_err(|e| failed(path, e))?;
        let mut out = Vec::new();
        while let Some(e) = rd.next_entry().await.map_err(|e| failed(path, e))? {
            let is_dir = e
                .file_type()
                .await
                .map(|t| t.is_dir())
                .map_err(|e| failed(path, e))?;
            out.push(DirEntry {
                path: e.path(),
                is_dir,
            });
        }
        Ok(out)
    }

    async fn remove_dir_all(&self, path: &Path) -> Result<(), ExecError> {
        tokio::fs::remove_dir_all(path)
            .await
            .map_err(|e| failed(path, e))
    }

    async fn metadata(&self, path: &Path) -> Result<Metadata, ExecError> {
        // `symlink_metadata` rather than `metadata`, because the contract has
        // an `is_symlink` field and `metadata` follows links — it can never
        // report one.
        let m = tokio::fs::symlink_metadata(path)
            .await
            .map_err(|e| failed(path, e))?;
        Ok(Metadata {
            is_dir: m.is_dir(),
            is_file: m.is_file(),
            is_symlink: m.file_type().is_symlink(),
            len: m.len(),
        })
    }

    async fn canonicalize(&self, path: &Path) -> Result<PathBuf, ExecError> {
        tokio::fs::canonicalize(path)
            .await
            .map_err(|e| failed(path, e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_round_trip_through_the_contract_is_a_round_trip_through_the_disk() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFileSystem;
        let f = dir.path().join("a/b/note.txt");
        fs.create_dir_all(f.parent().unwrap()).await.unwrap();
        fs.write_str(&f, "hello").await.unwrap();
        assert_eq!(fs.read_to_string(&f).await.unwrap(), "hello");
        assert!(fs.metadata(&f).await.unwrap().is_file);
        assert_eq!(fs.read_dir(f.parent().unwrap()).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_missing_file_is_the_target_failing_not_the_provider() {
        let dir = tempfile::tempdir().unwrap();
        let e = LocalFileSystem
            .read(&dir.path().join("nope"))
            .await
            .unwrap_err();
        assert!(
            matches!(e, ExecError::Failed(_)),
            "a host that saw `Unavailable` here would go looking for a broken \
             execution environment: {e:?}"
        );
    }

    /// The reason `metadata` does not use `tokio::fs::metadata`.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlink_reports_as_one() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        let link = dir.path().join("link");
        LocalFileSystem.write_str(&target, "x").await.unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(LocalFileSystem.metadata(&link).await.unwrap().is_symlink);
        assert_eq!(
            LocalFileSystem.canonicalize(&link).await.unwrap(),
            tokio::fs::canonicalize(&target).await.unwrap(),
            "and canonicalize resolves it, which is why path safety has to run \
             on the result rather than the argument"
        );
    }
}
