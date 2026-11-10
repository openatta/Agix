//! Tool security helpers — path validation, sandbox detection, safe guards.

use base::interface::exec::FileSystem;
use permissions::path_rule::{is_within_root_resolved, normalize_path_lexically};
use std::path::{Component, Path, PathBuf};

/// Validate that a file path is within one of the allowed working directories.
/// Returns Ok(()) if the path is safe, or Err with a description.
pub fn validate_path_within_bounds(
    target: &Path,
    cwd: &Path,
    additional_dirs: &[PathBuf],
) -> Result<(), String> {
    let inside = std::iter::once(cwd)
        .chain(additional_dirs.iter().map(PathBuf::as_path))
        .any(|root| is_within_root_resolved(target, root));
    if inside {
        return Ok(());
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

/// What a file-writing tool decides about writing `file_path` — `None` when
/// the input did not parse: inside the write policy it allows, outside every
/// root it asks, onto a guarded name or system path it denies.
pub async fn check_write_permission(
    tool: &str,
    file_path: Option<&str>,
    ctx: &base::tool::ToolContext,
) -> base::tool::PermissionDecision {
    use base::tool::PermissionDecision;
    use permissions::path_safety::{check_write_for, PathSafetyError};

    let Some(file_path) = file_path else {
        return PermissionDecision::Ask {
            message: format!("{tool} requires confirmation"),
            decision_reason: None,
        };
    };
    let fs = &*ctx.exec.filesystem;
    let target = resolve_write_target(file_path, &ctx.cwd, fs).await;
    let resolved = fs.canonicalize_best_effort(&target).await;
    let policy = write_policy(ctx).await;
    match check_write_for(Path::new(file_path), &resolved, &policy) {
        Ok(()) => PermissionDecision::Allow {
            decision_reason: Some("project_write".into()),
        },
        Err(PathSafetyError::OutsideAllowedRoots { .. }) => PermissionDecision::Ask {
            message: format!("{tool} outside the project requires confirmation"),
            decision_reason: None,
        },
        Err(err) => PermissionDecision::Deny {
            reason: Some(format!("{err:?}")),
            decision_reason: Some("path_safety".into()),
        },
    }
}

/// Whether `sandbox.allowed_domains` names the host `url` reaches. What the
/// url carries reaches that host, so `acceptEdits` sends a request without
/// asking only to a host the settings vouch for.
pub fn allowed_domains_name(url: &str, ctx: &base::tool::ToolContext) -> bool {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .is_some_and(|host| {
            base::interface::exec::domains_name_host(&ctx.sandbox.allowed_domains, &host)
        })
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
///
/// Both the normalized spelling and where the OS takes the path as written
/// have to be inside: the tools open the path as written, and a `..` after a
/// symlink climbs out of the link's target, not out of the link.
pub fn read_allowed(path: &Path, ctx: &base::tool::ToolContext) -> bool {
    ctx.session.read_anywhere || within_readable_roots(path, ctx)
}

/// Both where `path` reads as written and where the filesystem actually
/// lands it are inside a readable root.
pub fn within_readable_roots(path: &Path, ctx: &base::tool::ToolContext) -> bool {
    let written = ctx.cwd.join(path);
    let roots: Vec<PathBuf> = readable_roots(ctx)
        .iter()
        .map(|root| normalize_path_lexically(root))
        .collect();
    [
        normalize_path_lexically(&written),
        permissions::path_rule::resolve_physically(&written),
    ]
    .iter()
    .all(|p| roots.iter().any(|root| is_within_root_resolved(p, root)))
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
        let fs = platform::LocalFileSystem;
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

    #[cfg(unix)]
    #[test]
    fn a_read_through_a_symlink_before_a_dotdot_is_judged_where_it_lands() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let (proj, outside) = (root.join("proj"), root.join("outside"));
        std::fs::create_dir_all(proj.join("src")).unwrap();
        std::fs::create_dir_all(outside.join("sub")).unwrap();
        std::fs::write(outside.join("secret"), "s").unwrap();
        std::fs::write(proj.join("secret"), "p").unwrap();
        std::os::unix::fs::symlink(outside.join("sub"), proj.join("link")).unwrap();
        let ctx = platform::tool_context_for_test(proj.clone());

        assert_eq!(
            std::fs::read_to_string(proj.join("link/../secret")).unwrap(),
            "s"
        );
        assert!(!read_allowed(Path::new("link/../secret"), &ctx));
        assert!(!read_allowed(&proj.join("link/../secret"), &ctx));
        assert!(read_allowed(Path::new("src/../secret"), &ctx));
        assert!(read_allowed(Path::new("secret"), &ctx));
    }

    #[test]
    fn detects_traversal() {
        assert!(contains_path_traversal(Path::new("../etc/passwd")));
        assert!(!contains_path_traversal(Path::new("foo/bar.txt")));
    }
}
