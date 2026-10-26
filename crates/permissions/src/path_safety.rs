//! 路径写权限校验：FileWrite / FileEdit / Bash 改文件路径前调用，挡掉
//! 跨域写、`.env*` 等敏感目标。

use std::path::{Component, Path, PathBuf};

use unicode_normalization::UnicodeNormalization;

/// 默认黑名单（路径任意位置含这些段则拒）。可被 `WritePolicy::with_extra_blacklist` 扩展。
const DEFAULT_FILENAME_BLACKLIST: &[&str] = &[
    ".env",
    ".env.local",
    ".env.production",
    "id_rsa",
    "id_ed25519",
    ".ssh",
    ".aws",
    ".gnupg",
    ".netrc",
    ".pypirc",
    ".npmrc",
    ".pgpass",
];

/// Not secrets — files a tool should not casually rewrite.
///
/// Generated or curated, and a model editing one directly is nearly always
/// doing it the wrong way round. Denied like the credential list, and
/// exempted the same way: a deployment whose agent is supposed to maintain a
/// `.gitignore` says so in `sandbox.allow_write`.
const GUARDED_FILENAMES: &[&str] = &[".gitignore", "package-lock.json", "Cargo.lock"];

/// The engine's own configuration, wherever it lives.
///
/// A tool that can rewrite these can rewrite the rules it is judged by, so
/// they are guarded — but by *name and location*, not by directory. `.agix`
/// is also where a sub-agent's git worktree lives, and denying the directory
/// denies every write the sub-agent was created to make.
const SETTINGS_FILENAMES: &[&str] = &["settings.json", "settings.local.json"];

/// Directories those settings files live in.
const SETTINGS_DIRS: &[&str] = &[".agix", ".claude"];

/// 不论 cwd 在哪，下面这些系统目录都拒写。
const ABSOLUTE_DENY_PREFIXES: &[&str] = &[
    "/etc",
    "/System",
    "/Library/Apple",
    "/usr/bin",
    "/usr/sbin",
    "/sbin",
    "/bin",
    "/boot",
    "/proc",
    "/sys",
];

#[derive(Debug, Clone)]
pub struct WritePolicy {
    /// 写允许的根目录（典型为 cwd）
    pub primary_root: PathBuf,
    /// 用户配置的额外可写目录（settings 的 `additional_directories`）
    pub additional_roots: Vec<PathBuf>,
    /// 路径里任意 component 命中就拒（除默认黑名单外的扩展项）
    pub extra_blacklist: Vec<String>,
    /// Paths exempt from the deny rules — the filename blacklist and the
    /// system-directory list, not the root containment check.
    ///
    /// Without this the defaults are a wall rather than a policy: a project
    /// with a checked-in `.env.example` had no way to say so, and the only
    /// available answer was to not run the check at all. Which is what
    /// happened.
    pub allow: Vec<PathBuf>,
}

impl WritePolicy {
    /// Construct a new instance.
    pub fn new(primary_root: PathBuf) -> Self {
        Self {
            primary_root,
            additional_roots: Vec::new(),
            extra_blacklist: Vec::new(),
            allow: Vec::new(),
        }
    }

    /// Builder: paths the deny rules do not apply to.
    pub fn with_allowed(mut self, paths: Vec<PathBuf>) -> Self {
        self.allow = paths;
        self
    }

    /// Builder: set additional roots.
    pub fn with_additional_roots(mut self, roots: Vec<PathBuf>) -> Self {
        self.additional_roots = roots;
        self
    }

    /// Builder: set extra blacklist.
    pub fn with_extra_blacklist(mut self, items: Vec<String>) -> Self {
        self.extra_blacklist = items;
        self
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum PathSafetyError {
    /// 路径含 `..` —— 解析失败（保守拒绝；调用方应自己 canonicalize 后再 check）
    UnresolvedTraversal(PathBuf),
    /// 路径不在任何允许的 root 之下
    OutsideAllowedRoots {
        path: PathBuf,
        primary: PathBuf,
        additional: Vec<PathBuf>,
    },
    /// 路径包含黑名单 component
    BlacklistedFilename { path: PathBuf, matched: String },
    /// 系统级绝对路径拒写
    SystemPathDenied(PathBuf),
    /// 路径含未展开的 `~`（调用方应先做 tilde 展开再 check）
    TildeExpansion(PathBuf),
    /// 路径含 shell 展开模式（`$(...)` / `` ` ``）—— 可能是注入
    ShellExpansion { path: PathBuf, matched: String },
    /// Windows UNC 路径（`\\server\share\...`）—— 远程路径不被允许
    UncPathDetected(PathBuf),
    /// 路径解析后实际路径脱离允许 root（符号链接 / 挂载点绕过）
    SymlinkEscape {
        symlink_path: PathBuf,
        real_path: PathBuf,
        primary: PathBuf,
    },
    /// Unicode NFC normalization mismatch indicates a potential attack
    /// (e.g., path components that visually appear within the allowed root
    /// but decode to a different path under NFC normalization).
    UnicodeNormalizationAttack {
        /// The original path provided
        path: PathBuf,
        /// What the path normalizes to under NFC
        normalized: PathBuf,
    },
}

impl std::fmt::Display for PathSafetyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnresolvedTraversal(p) => write!(
                f,
                "path contains unresolved `..` traversal (canonicalize first): {}",
                p.display()
            ),
            Self::OutsideAllowedRoots {
                path,
                primary,
                additional,
            } => {
                write!(
                    f,
                    "path is outside the allowed write roots: {} (primary={}, additional={:?})",
                    path.display(),
                    primary.display(),
                    additional
                )
            }
            Self::BlacklistedFilename { path, matched } => write!(
                f,
                "path component '{}' is on the write blacklist: {}",
                matched,
                path.display()
            ),
            Self::SystemPathDenied(p) => {
                write!(f, "writing to system path is denied: {}", p.display())
            }
            Self::TildeExpansion(p) => write!(
                f,
                "path contains unexpanded tilde (use absolute paths): {}",
                p.display()
            ),
            Self::ShellExpansion { path, matched } => write!(
                f,
                "path contains shell expansion pattern '{matched}': {}",
                path.display()
            ),
            Self::UncPathDetected(p) => {
                write!(f, "Windows UNC path is not allowed: {}", p.display())
            }
            Self::SymlinkEscape {
                symlink_path,
                real_path,
                primary,
            } => write!(
                f,
                "symlink at {} resolves to {} which is outside allowed root {}",
                symlink_path.display(),
                real_path.display(),
                primary.display()
            ),
            Self::UnicodeNormalizationAttack { path, normalized } => write!(
                f,
                "Unicode normalization attack detected: path '{}' normalizes to \
                 '{}' under NFC, which indicates mixed normalization forms that \
                 could bypass path safety checks",
                path.display(),
                normalized.display()
            ),
        }
    }
}

impl std::error::Error for PathSafetyError {}

/// 检查 `target` 是否能写。
///
/// 调用方有两种用法：
/// - 文件**不存在**（要新建）：传**绝对**路径；本函数会用 parent 目录判断 root 隶属
/// - 文件**已存在**：建议先 `tokio::fs::canonicalize` 再调；本函数会拒绝含 `..` 的非规范化路径
pub fn check_write(target: &Path, policy: &WritePolicy) -> Result<(), PathSafetyError> {
    check_write_for(target, target, policy)
}

/// The same, with the two questions told apart.
///
/// Steps 1–3 and 5 are about the path someone *asked for*: an unexpanded `~`,
/// a UNC share, a shell substitution, a name whose normalization differs from
/// what it displays as. Steps 4 and 6–8 are about the file it *resolves to*.
///
/// Running the first group on a resolved path is wrong, and expensively so on
/// macOS: `realpath` hands back the name as the filesystem stores it, which
/// for anything created through Cocoa is NFD — so `café.txt` came back
/// decomposed, failed the normalization check, and an ordinary file the user
/// asked to edit was refused as an attack.
pub fn check_write_for(
    requested: &Path,
    target: &Path,
    policy: &WritePolicy,
) -> Result<(), PathSafetyError> {
    // 1. Tilde 展开阻断：路径以 `~` 开头说明未展开家目录
    //    必须在绝对路径检查之前，因为 `~/foo` 不是绝对路径（shell 未展开）
    {
        let path_str = requested.to_string_lossy();
        if path_str == "~" || path_str.starts_with("~/") || path_str.starts_with("~\\") {
            return Err(PathSafetyError::TildeExpansion(requested.to_path_buf()));
        }
    }

    // 2. UNC / 远程路径检测：`\\server\share\...`
    //    必须在绝对路径检查之前，因为 `\\host\share` 只在 Windows 上是绝对路径
    {
        let path_str = requested.to_string_lossy();
        if path_str.starts_with("\\\\") {
            return Err(PathSafetyError::UncPathDetected(requested.to_path_buf()));
        }
    }

    // 3. Unicode normalization attack detection (NFC round-trip check)
    {
        let path_str = requested.to_string_lossy();
        let nfc_normalized: String = path_str.chars().nfc().collect();
        if nfc_normalized != path_str.as_ref() {
            return Err(PathSafetyError::UnicodeNormalizationAttack {
                path: requested.to_path_buf(),
                normalized: PathBuf::from(nfc_normalized),
            });
        }
    }

    // 4. 绝对路径
    if !target.is_absolute() {
        return Err(PathSafetyError::UnresolvedTraversal(target.to_path_buf()));
    }

    // 4. 不接受未规范化（含 `..`）的路径
    for c in target.components() {
        if matches!(c, Component::ParentDir) {
            return Err(PathSafetyError::UnresolvedTraversal(target.to_path_buf()));
        }
    }

    // 5. Shell 展开阻断：`$(...)` 与 `` ` `` 可能是注入
    {
        let path_str = requested.to_string_lossy();
        if path_str.contains("$(") {
            return Err(PathSafetyError::ShellExpansion {
                matched: "$(".into(),
                path: requested.to_path_buf(),
            });
        }
        if path_str.contains('`') {
            return Err(PathSafetyError::ShellExpansion {
                matched: "`".into(),
                path: requested.to_path_buf(),
            });
        }
    }

    // Steps 6 and 7 are the ones a deployment can carve exceptions out of.
    // Root containment below is not: an exception says "this sensitive name
    // is fine here", never "this path may be outside the project".
    if !policy.allow.iter().any(|a| starts_within(target, a)) {
        // 6. 系统目录硬黑名单
        for prefix in ABSOLUTE_DENY_PREFIXES {
            if target.starts_with(prefix) {
                return Err(PathSafetyError::SystemPathDenied(target.to_path_buf()));
            }
        }

        // 7a. The engine's own settings, by name and location.
        if let Some(name) = target.file_name().and_then(|n| n.to_str()) {
            let under_settings_dir = target.components().any(
                |c| matches!(c, Component::Normal(d) if SETTINGS_DIRS.iter().any(|s| d == *s)),
            );
            if under_settings_dir && SETTINGS_FILENAMES.contains(&name) {
                return Err(PathSafetyError::BlacklistedFilename {
                    path: target.to_path_buf(),
                    matched: format!("{name} under an engine settings directory"),
                });
            }
        }

        // 7b. 文件名黑名单（任意 component 命中即拒）
        for c in target.components() {
            if let Component::Normal(name) = c {
                let s = name.to_string_lossy();
                let hit = DEFAULT_FILENAME_BLACKLIST
                    .iter()
                    .chain(GUARDED_FILENAMES.iter())
                    .map(|b| b.to_string())
                    .chain(policy.extra_blacklist.iter().cloned())
                    .find(|b| s == *b || s.starts_with(&format!("{b}.")));
                if let Some(matched) = hit {
                    return Err(PathSafetyError::BlacklistedFilename {
                        path: target.to_path_buf(),
                        matched,
                    });
                }
            }
        }
    }

    // 8. root 隶属：path 必须在 primary_root 或某个 additional_root 子树下
    let mut roots = vec![policy.primary_root.clone()];
    roots.extend(policy.additional_roots.iter().cloned());

    if !roots.iter().any(|r| starts_within(target, r)) {
        return Err(PathSafetyError::OutsideAllowedRoots {
            path: target.to_path_buf(),
            primary: policy.primary_root.clone(),
            additional: policy.additional_roots.clone(),
        });
    }

    // 9. Intermediate symlink escape detection: resolve each parent
    //    directory component that lies WITHIN the allowed root and verify
    //    it does not escape via symlink.
    //
    //    We only check components that are children of the root.  Components
    //    above the root (e.g. /tmp → /private/tmp on macOS) are irrelevant
    //    because a symlink there doesn't allow escape — only symlinks that
    //    are *inside* the root tree can redirect a write outside of it.
    {
        // Pre-compute canonical forms of the roots so we can compare
        // canonicalised resolved paths against the same canonical base
        // (handles e.g. /tmp → /private/tmp on macOS).  Empty when the
        // root doesn't exist yet — in that case the check below won't
        // find any components within the root, so it's safe to skip.
        let canonical_roots: Vec<PathBuf> = roots
            .iter()
            .filter_map(|r| std::fs::canonicalize(r).ok())
            .collect();

        let ancestors: Vec<_> = target.ancestors().collect();
        // ancestors[0] = target itself, ancestors[last] = root "/"
        // Check all intermediate directory components, from root toward
        // target, but only those that are WITHIN a root.
        for component in ancestors[1..ancestors.len().saturating_sub(1)].iter().rev() {
            // Skip components that ARE one of the roots
            // (root-level symlinks are not our concern here)
            if roots.iter().any(|r| r == *component) {
                continue;
            }
            // Only check components that are INSIDE the allowed root tree
            if !roots.iter().any(|r| starts_within(component, r)) {
                continue;
            }
            if let Ok(real_path) = std::fs::canonicalize(component) {
                if real_path != *component
                    && !roots.iter().any(|r| starts_within(&real_path, r))
                    && !canonical_roots.iter().any(|r| starts_within(&real_path, r))
                {
                    return Err(PathSafetyError::SymlinkEscape {
                        symlink_path: component.to_path_buf(),
                        real_path,
                        primary: policy.primary_root.clone(),
                    });
                }
            }
        }
    }

    Ok(())
}

/// Like `check_write`, but also validates the path symlinks resolve to.
///
/// `resolved` is what the filesystem the write will land on says `target`
/// really is, or `None` when it could not say — a file that does not exist
/// yet, which leaves the lexical checks as the whole answer.
///
/// Resolution is the caller's to perform, and deliberately so: the symlink
/// graph belongs to the filesystem being written to, so resolving a path here
/// would answer about this machine's disk even when the write is going
/// somewhere else. See `docs/ARCHITECTURE.md` §5.
pub fn check_write_resolve_symlinks(
    target: &Path,
    resolved: Option<&Path>,
    policy: &WritePolicy,
) -> Result<(), PathSafetyError> {
    check_write(target, policy)?;

    match resolved {
        Some(real_path) if real_path != target => {
            check_write(real_path, policy).map_err(|_| PathSafetyError::SymlinkEscape {
                symlink_path: target.to_path_buf(),
                real_path: real_path.to_path_buf(),
                primary: policy.primary_root.clone(),
            })
        }
        _ => Ok(()),
    }
}

/// Lexically normalize a path without touching the filesystem.
///
/// This collapses `.` and `..` components so callers can classify
/// `cwd/../outside` as "outside cwd" instead of rejecting it before a user
/// has a chance to approve an outside-project access. It intentionally does
/// not resolve symlinks; sandbox/path safety still runs at execution time.
pub fn normalize_path_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Public wrapper for root containment checks used by tool-level policy.
pub fn is_path_within_root(target: &Path, root: &Path) -> bool {
    starts_within(target, root)
}

/// `target` 是否在 `root` 子树内（按字符串前缀；调用方要自行 canonicalize 防符号链接绕过）。
fn starts_within(target: &Path, root: &Path) -> bool {
    // 用 components 比对而非字符串 prefix，避免 `/foo/bar2` 误匹配 `/foo/bar` root
    let tcomp: Vec<_> = target.components().collect();
    let rcomp: Vec<_> = root.components().collect();
    if tcomp.len() < rcomp.len() {
        return false;
    }
    tcomp.iter().zip(rcomp.iter()).all(|(a, b)| a == b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(root: &str) -> WritePolicy {
        WritePolicy::new(PathBuf::from(root))
    }

    #[test]
    fn allows_path_inside_root() {
        let p = policy("/tmp/work");
        assert!(check_write(Path::new("/tmp/work/foo.txt"), &p).is_ok());
        assert!(check_write(Path::new("/tmp/work/sub/foo.txt"), &p).is_ok());
    }

    #[test]
    fn rejects_path_outside_root() {
        let p = policy("/tmp/work");
        let err = check_write(Path::new("/tmp/other/foo.txt"), &p).unwrap_err();
        assert!(matches!(err, PathSafetyError::OutsideAllowedRoots { .. }));
    }

    #[test]
    fn rejects_sibling_with_prefix_overlap() {
        // /tmp/work2 不应该被 /tmp/work 接受
        let p = policy("/tmp/work");
        let err = check_write(Path::new("/tmp/work2/foo.txt"), &p).unwrap_err();
        assert!(matches!(err, PathSafetyError::OutsideAllowedRoots { .. }));
    }

    #[test]
    fn additional_roots_extend_allowance() {
        let p = WritePolicy::new(PathBuf::from("/tmp/work"))
            .with_additional_roots(vec![PathBuf::from("/tmp/extra")]);
        assert!(check_write(Path::new("/tmp/extra/x.txt"), &p).is_ok());
        assert!(check_write(Path::new("/tmp/work/x.txt"), &p).is_ok());
        assert!(check_write(Path::new("/tmp/somewhere/x.txt"), &p).is_err());
    }

    #[test]
    fn rejects_relative_path() {
        let p = policy("/tmp/work");
        let err = check_write(Path::new("foo.txt"), &p).unwrap_err();
        assert!(matches!(err, PathSafetyError::UnresolvedTraversal(_)));
    }

    #[test]
    fn rejects_dotdot() {
        let p = policy("/tmp/work");
        let err = check_write(Path::new("/tmp/work/../etc/x"), &p).unwrap_err();
        assert!(matches!(err, PathSafetyError::UnresolvedTraversal(_)));
    }

    #[test]
    fn lexical_normalize_and_root_check_classify_outside_paths() {
        let path = normalize_path_lexically(Path::new("/tmp/work/../other/x"));
        assert_eq!(path, PathBuf::from("/tmp/other/x"));
        assert!(!is_path_within_root(&path, Path::new("/tmp/work")));
        assert!(is_path_within_root(
            &normalize_path_lexically(Path::new("/tmp/work/sub/../x")),
            Path::new("/tmp/work")
        ));
    }

    #[test]
    fn rejects_env_files() {
        let p = policy("/tmp/work");
        for bad in &[
            "/tmp/work/.env",
            "/tmp/work/.env.local",
            "/tmp/work/.env.production",
            "/tmp/work/sub/.env",
        ] {
            let err = check_write(Path::new(bad), &p).unwrap_err();
            assert!(
                matches!(err, PathSafetyError::BlacklistedFilename { .. }),
                "expected blacklist error for {bad}, got {err:?}"
            );
        }
    }

    #[test]
    fn rejects_ssh_keys() {
        let p = policy("/tmp/work");
        let err = check_write(Path::new("/tmp/work/.ssh/id_rsa"), &p).unwrap_err();
        assert!(matches!(err, PathSafetyError::BlacklistedFilename { .. }));
    }

    #[test]
    fn rejects_system_paths() {
        let p = policy("/tmp/work")
            .clone()
            .with_additional_roots(vec![PathBuf::from("/etc")]);
        // 即便用户允许 /etc，也仍然拒 —— 系统路径黑名单优先
        let err = check_write(Path::new("/etc/passwd"), &p).unwrap_err();
        assert!(matches!(err, PathSafetyError::SystemPathDenied(_)));
    }

    #[test]
    fn extra_blacklist_works() {
        let p = WritePolicy::new(PathBuf::from("/tmp/work"))
            .with_extra_blacklist(vec!["secret-config".into()]);
        let err = check_write(Path::new("/tmp/work/secret-config"), &p).unwrap_err();
        assert!(matches!(err, PathSafetyError::BlacklistedFilename { .. }));
        // 同名后缀也拒
        let err = check_write(Path::new("/tmp/work/secret-config.toml"), &p).unwrap_err();
        assert!(matches!(err, PathSafetyError::BlacklistedFilename { .. }));
    }

    #[test]
    fn allows_normal_dotfiles() {
        let p = policy("/tmp/work");
        // 黑名单只匹配特定文件名，不是所有 dotfile
        assert!(check_write(Path::new("/tmp/work/.editorconfig"), &p).is_ok());
        assert!(check_write(Path::new("/tmp/work/.prettierrc"), &p).is_ok());
    }

    /// The guarded list is not about secrets: these are generated or curated
    /// files a model editing directly is usually doing the wrong way round.
    /// They were denied by a list inside `FileWrite`; they are denied here
    /// now, which is the same answer from somewhere a deployment can reach.
    #[test]
    fn guarded_files_are_denied_but_a_deployment_can_say_otherwise() {
        let p = policy("/tmp/work");
        for name in [".gitignore", "Cargo.lock", "package-lock.json"] {
            let path = PathBuf::from("/tmp/work").join(name);
            assert!(
                matches!(
                    check_write(&path, &p),
                    Err(PathSafetyError::BlacklistedFilename { .. })
                ),
                "{name} should be guarded"
            );
        }

        let permitted =
            policy("/tmp/work").with_allowed(vec![PathBuf::from("/tmp/work/.gitignore")]);
        assert!(
            check_write(Path::new("/tmp/work/.gitignore"), &permitted).is_ok(),
            "an exemption is what makes the default a policy rather than a wall"
        );
        assert!(
            matches!(
                check_write(Path::new("/tmp/work/Cargo.lock"), &permitted),
                Err(PathSafetyError::BlacklistedFilename { .. })
            ),
            "and it exempts what it names, not everything"
        );
    }

    /// An exemption lifts the deny rules, never the project boundary.
    #[test]
    fn an_exemption_does_not_let_a_write_leave_the_project() {
        let p = policy("/tmp/work").with_allowed(vec![PathBuf::from("/etc")]);
        assert!(
            matches!(
                check_write(Path::new("/etc/passwd"), &p),
                Err(PathSafetyError::OutsideAllowedRoots { .. })
            ),
            "otherwise one setting would quietly widen where a tool may write"
        );
    }

    #[test]
    fn rejects_tilde_expansion() {
        let p = policy("/tmp/work");
        // 待展开的家目录引用
        let err = check_write(Path::new("~/myfile"), &p).unwrap_err();
        assert!(
            matches!(err, PathSafetyError::TildeExpansion(_)),
            "expected tilde error for ~/myfile, got {err:?}"
        );
        let err = check_write(Path::new("~"), &p).unwrap_err();
        assert!(
            matches!(err, PathSafetyError::TildeExpansion(_)),
            "expected tilde error for bare ~, got {err:?}"
        );
    }

    #[test]
    fn rejects_shell_expansion() {
        let p = policy("/tmp/work");
        // $(...) 注入
        let err = check_write(Path::new("/tmp/$(ls)/x"), &p).unwrap_err();
        assert!(
            matches!(err, PathSafetyError::ShellExpansion { .. }),
            "expected shell expansion error, got {err:?}"
        );
        // backtick 注入
        let err = check_write(Path::new("/tmp/`ls`/x"), &p).unwrap_err();
        assert!(
            matches!(err, PathSafetyError::ShellExpansion { .. }),
            "expected shell expansion error, got {err:?}"
        );
    }

    #[test]
    fn rejects_unc_path() {
        let p = policy("/tmp/work");
        // \\ 前缀 = UNC 路径，无论平台都拒
        let err = check_write(Path::new("\\\\server\\share\\file"), &p).unwrap_err();
        assert!(
            matches!(err, PathSafetyError::UncPathDetected(_)),
            "expected UNC path error, got {err:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_resolution_detects_escape() {
        use std::fs;
        use std::os::unix;

        // A `TempDir` rather than a name built from a counter: the counter
        // restarts at zero in every process, and $TMPDIR is shared, so two
        // test binaries running at once picked the same directory and the
        // second one failed to create a symlink that already existed.
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        let inside = dir.join("workdir");
        let outside = dir.join("outside");

        fs::create_dir_all(&outside).unwrap();
        fs::create_dir_all(&inside).unwrap();

        let real_file = outside.join("secret.txt");
        fs::write(&real_file, b"secret").unwrap();

        let link_path = inside.join("escape_link");
        unix::fs::symlink(&real_file, &link_path).unwrap();

        let p = WritePolicy::new(inside.clone());

        // Lexical check alone passes — the symlink path is inside workdir
        assert!(
            check_write(&link_path, &p).is_ok(),
            "lexical check should pass for symlink inside workdir"
        );

        // Symlink-resolved check should catch the escape
        let resolved = fs::canonicalize(&link_path).unwrap();
        let err = check_write_resolve_symlinks(&link_path, Some(&resolved), &p).unwrap_err();
        assert!(
            matches!(err, PathSafetyError::SymlinkEscape { .. }),
            "expected SymlinkEscape, got {err:?}"
        );

        // A caller whose filesystem cannot resolve the path — because it does
        // not exist there — gets the lexical answer and nothing more.
        assert!(check_write_resolve_symlinks(&link_path, None, &p).is_ok());
    }

    // ── Unicode normalization attack detection ─────────────────────────

    #[test]
    fn rejects_unicode_normalization_attack() {
        let p = policy("/tmp/work");
        // 'é' in NFD: 'e' (U+0065) + combining acute accent (U+0301)
        //   NFC: 'é' as single codepoint U+00E9
        let nfd_path = "/tmp/work/cafe\u{0301}.txt";
        let err = check_write(Path::new(nfd_path), &p).unwrap_err();
        assert!(
            matches!(err, PathSafetyError::UnicodeNormalizationAttack { .. }),
            "expected UnicodeNormalizationAttack for NFD path, got {err:?}"
        );
    }

    #[test]
    fn allows_nfc_path() {
        let p = policy("/tmp/work");
        // 'é' in NFC: single codepoint U+00E9
        let nfc_path = "/tmp/work/caf\u{E9}.txt";
        assert!(
            check_write(Path::new(nfc_path), &p).is_ok(),
            "NFC path should pass all checks"
        );
    }

    // ── Intermediate symlink escape via parent dirs ────────────────────

    #[cfg(unix)]
    #[test]
    fn intermediate_symlink_detects_escape() {
        use std::fs;
        use std::os::unix;
        use std::sync::atomic::{AtomicU64, Ordering};

        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let stamp = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("agix_symlink_midtest_{}", stamp));
        let allowed_root = dir.join("workdir");
        let outside = dir.join("outside");

        // Clean residual from any prior failed run, then set up fresh
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&allowed_root).unwrap();
        fs::create_dir_all(&outside).unwrap();

        // Create intermediate symlink: allowed_root/link -> outside/
        let link_dir = allowed_root.join("link_to_outside");
        unix::fs::symlink(&outside, &link_dir).unwrap();

        // Path that traverses the intermediate symlink
        let target = link_dir.join("target.txt");
        fs::write(&target, b"should not be reachable").unwrap();

        let p = WritePolicy::new(allowed_root.clone());

        // The intermediate symlink check should catch this escape
        let err = check_write(&target, &p).unwrap_err();
        assert!(
            matches!(err, PathSafetyError::SymlinkEscape { .. }),
            "expected SymlinkEscape for intermediate symlink escape, got {err:?}"
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn intermediate_symlink_within_root_allowed() {
        use std::fs;
        use std::os::unix;
        use std::sync::atomic::{AtomicU64, Ordering};

        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let stamp = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("agix_symlink_inside_{}", stamp));
        let allowed_root = dir.join("workdir");
        let real_sub = allowed_root.join("real_sub");
        let link_sub = allowed_root.join("link_sub");

        // Clean residual from any prior failed run, then set up fresh
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&real_sub).unwrap();
        // Symlink staying within the allowed root
        unix::fs::symlink(&real_sub, &link_sub).unwrap();

        let target = link_sub.join("file.txt");
        fs::write(&target, b"content").unwrap();

        let p = WritePolicy::new(allowed_root.clone());

        assert!(
            check_write(&target, &p).is_ok(),
            "symlink staying within root should pass"
        );

        fs::remove_dir_all(&dir).ok();
    }
}
