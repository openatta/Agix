//! Tool security helpers — path validation, sandbox detection, safe guards.

use base::interface::exec::FileSystem;
use std::path::{Component, Path, PathBuf};

/// Validate that a file path is within one of the allowed working directories.
/// Returns Ok(()) if the path is safe, or Err with a description.
pub fn validate_path_within_bounds(
    target: &Path,
    cwd: &Path,
    additional_dirs: &[PathBuf],
) -> Result<(), String> {
    // Canonicalize if possible, fall back to absolute form
    let resolved = canonicalize_best_effort(target);

    // Check against primary cwd
    let cwd_resolved = canonicalize_best_effort(cwd);
    if resolved.starts_with(&cwd_resolved) {
        return Ok(());
    }

    // Check against additional dirs
    for dir in additional_dirs {
        let dir_resolved = canonicalize_best_effort(dir);
        if resolved.starts_with(&dir_resolved) {
            return Ok(());
        }
    }

    Err(format!(
        "Path {:?} is outside allowed working directories. \
         Primary: {:?}",
        target, cwd
    ))
}

/// Detect path traversal attempts via `../` sequences.
pub fn contains_path_traversal(path: &Path) -> bool {
    path.components().any(|c| matches!(c, Component::ParentDir))
}

/// Canonicalize a path, falling back to the input path on error.
pub fn canonicalize_best_effort(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| {
        // On non-existent paths, resolve parent and append
        if let Some(parent) = p.parent() {
            let parent_ok = parent
                .canonicalize()
                .unwrap_or_else(|_| parent.to_path_buf());
            parent_ok.join(p.file_name().unwrap_or_default())
        } else {
            p.to_path_buf()
        }
    })
}

/// The file a write to `raw` lands on, for the permission check, the
/// read-before-write check and the write itself alike.
///
/// The normalized spelling when it means the same file, which keeps it the
/// key a `Read` of that spelling recorded. Otherwise — a `..` after a symlink,
/// which the OS resolves from the link's target, not from the link — the
/// path as the filesystem resolves it, so the write cannot land anywhere the
/// check did not look.
pub async fn resolve_write_target(raw: &str, cwd: &Path, fs: &dyn FileSystem) -> PathBuf {
    let joined = cwd.join(raw);
    let lexical = normalize_path_lexically(&joined);
    if !contains_path_traversal(&joined) {
        return lexical;
    }
    let physical = resolve_physically(&joined, fs).await;
    if fs.canonicalize_best_effort(&lexical).await == physical {
        lexical
    } else {
        physical
    }
}

/// `path`'s deepest prefix that exists, resolved by `fs`, with the rest put
/// back on lexically. The rest does not exist, so it holds no symlink a
/// lexical `..` could misjudge.
async fn resolve_physically(path: &Path, fs: &dyn FileSystem) -> PathBuf {
    let parts: Vec<Component> = path.components().collect();
    for split in (1..=parts.len()).rev() {
        let prefix: PathBuf = parts[..split].iter().collect();
        if let Ok(mut out) = fs.canonicalize(&prefix).await {
            for part in &parts[split..] {
                match part {
                    Component::ParentDir => {
                        out.pop();
                    }
                    Component::CurDir => {}
                    other => out.push(other),
                }
            }
            return out;
        }
    }
    normalize_path_lexically(path)
}

/// The write policy for this call, with every root resolved by the same
/// filesystem the target is resolved by.
///
/// Both sides have to be resolved or neither. `/tmp` is a symlink to
/// `/private/tmp` on macOS, and a project reached through any symlink has the
/// same shape: resolve the target only, and it looks like it sits outside a
/// root it is actually inside, so the write is refused for being somewhere it
/// is not.
pub async fn write_policy(ctx: &base::tool::ToolContext) -> permissions::path_safety::WritePolicy {
    let fs = &*ctx.exec.filesystem;
    let root = fs.canonicalize_best_effort(&ctx.cwd).await;
    let mut additional = Vec::with_capacity(ctx.additional_writable_dirs.len());
    for dir in &ctx.additional_writable_dirs {
        additional.push(fs.canonicalize_best_effort(dir).await);
    }
    let mut allowed = Vec::with_capacity(ctx.sandbox.allow_write.len());
    for path in &ctx.sandbox.allow_write {
        allowed.push(fs.canonicalize_best_effort(path).await);
    }
    permissions::path_safety::WritePolicy::new(root)
        .with_additional_roots(additional)
        .with_extra_blacklist(ctx.sandbox.deny_write.clone())
        .with_allowed(allowed)
}

/// Where the file tools may read without asking: the project, every
/// directory they may write, and `sandbox.read_directories`.
pub fn readable_roots(ctx: &base::tool::ToolContext) -> Vec<PathBuf> {
    std::iter::once(ctx.cwd.clone())
        .chain(ctx.additional_writable_dirs.iter().cloned())
        .chain(ctx.session.read_directories.iter().cloned())
        .collect()
}

/// Whether a file tool may read `path` (absolute, or relative to the
/// working directory) without asking — under a [`readable_roots`] entry, or
/// anywhere when the instance sets `sandbox.read_anywhere`.
pub fn read_allowed(path: &Path, ctx: &base::tool::ToolContext) -> bool {
    if ctx.session.read_anywhere {
        return true;
    }
    let path = normalize_path_lexically(&ctx.cwd.join(path));
    readable_roots(ctx)
        .iter()
        .any(|root| is_path_within_root(&path, &normalize_path_lexically(root)))
}

pub fn is_path_within_root(path: &Path, root: &Path) -> bool {
    canonicalize_best_effort(path).starts_with(canonicalize_best_effort(root))
}

pub fn normalize_path_lexically(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => {
                out.push(other);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cwd_path_is_allowed() {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/tmp"));
        let target = cwd.join("test.txt");
        assert!(validate_path_within_bounds(&target, &cwd, &[]).is_ok());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_write_target_follows_the_os_through_a_symlink_before_a_dotdot() {
        let fs = base::interface::exec::local::LocalFileSystem;
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let (proj, outside) = (root.join("proj"), root.join("outside"));
        std::fs::create_dir_all(proj.join("src")).unwrap();
        std::fs::create_dir_all(outside.join("sub")).unwrap();
        std::os::unix::fs::symlink(outside.join("sub"), proj.join("link")).unwrap();

        assert_eq!(
            resolve_write_target("link/../x", &proj, &fs).await,
            outside.join("x")
        );
        assert_eq!(
            resolve_write_target("src/../a.txt", &proj, &fs).await,
            proj.join("a.txt")
        );
        assert_eq!(
            resolve_write_target("new/../b.txt", &proj, &fs).await,
            proj.join("b.txt")
        );
    }

    #[test]
    fn detects_traversal() {
        assert!(contains_path_traversal(Path::new("../etc/passwd")));
        assert!(!contains_path_traversal(Path::new("foo/bar.txt")));
    }
}
