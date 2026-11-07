//! Tool security helpers — path validation, sandbox detection, safe guards.

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

/// Check if the current platform supports sandbox execution.
/// macOS: sandbox-exec available; Linux: bwrap available.
pub fn platform_sandbox_available() -> bool {
    if cfg!(target_os = "macos") {
        std::process::Command::new("which")
            .arg("sandbox-exec")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    } else if cfg!(target_os = "linux") {
        std::process::Command::new("which")
            .arg("bwrap")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    } else {
        false
    }
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

// ── Write policy types ──

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

    #[test]
    fn detects_traversal() {
        assert!(contains_path_traversal(Path::new("../etc/passwd")));
        assert!(!contains_path_traversal(Path::new("foo/bar.txt")));
    }
}
