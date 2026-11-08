//! Matching path-valued rules (`Read(...)`, `Edit(...)`) against the file a
//! tool call actually touches.
//!
//! Both sides are brought to the same shape before they meet: a tool's path
//! is resolved against the session's cwd and normalized (`..`, `.`), and so
//! is a relative rule pattern; `~` expands to the home directory on both
//! sides. Without this a deny on `/proj/.env` is walked around by asking for
//! `.env`, `./.env` or `src/../.env`.
//!
//! Globs follow gitignore conventions: `*` stays within one path segment,
//! `**` crosses any number of them. A pattern with no glob characters names
//! a file or a directory, and a directory covers everything beneath it.

use std::path::{Component, Path, PathBuf};

/// The file a tool call names, and other spellings of the same file.
pub struct ResolvedPath {
    /// The path the tool itself will open: relative input joined onto cwd,
    /// normalized.
    pub primary: PathBuf,
    /// The same file reached another way: `~` expanded to the home
    /// directory, or symlinks resolved — of the normalized path, and of the
    /// path as written, where a `..` after a symlink climbs out of the link's
    /// target. A deny or ask on any of these applies; an allow has to match
    /// `primary`.
    pub aliases: Vec<PathBuf>,
}

impl ResolvedPath {
    pub fn new(raw: &str, cwd: &Path) -> Self {
        let primary = absolutize(Path::new(raw), cwd);
        let mut aliases = Vec::new();
        if let Some(expanded) = expand_home(raw) {
            aliases.push(normalize(&expanded));
        }
        for real in [
            resolve_physically(&primary),
            resolve_physically(&cwd.join(raw)),
        ] {
            if real != primary && !aliases.contains(&real) {
                aliases.push(real);
            }
        }
        Self { primary, aliases }
    }

    pub fn all(&self) -> impl Iterator<Item = &Path> {
        std::iter::once(self.primary.as_path()).chain(self.aliases.iter().map(PathBuf::as_path))
    }
}

/// A rule pattern in the same coordinates as [`ResolvedPath`]: `~` expanded,
/// relative patterns joined onto cwd. A pattern that opens with a glob is
/// left unanchored so it matches at any depth (`**/.env`, and `*.env`, which
/// gitignore reads the same way).
pub fn resolve_pattern(pattern: &str, cwd: &Path) -> String {
    if let Some(expanded) = expand_home(pattern) {
        return normalize(&expanded).to_string_lossy().into_owned();
    }
    if starts_with_glob(pattern) {
        if pattern.contains('/') {
            return pattern.to_string();
        }
        return format!("**/{pattern}");
    }
    absolutize(Path::new(pattern), cwd)
        .to_string_lossy()
        .into_owned()
}

/// Whether a resolved pattern covers `path`.
pub fn matches(pattern: &str, path: &Path) -> bool {
    if has_glob(pattern) {
        return globset::GlobBuilder::new(pattern)
            .literal_separator(true)
            .build()
            .map(|g| g.compile_matcher().is_match(path))
            .unwrap_or(false);
    }
    path.starts_with(Path::new(pattern))
}

/// Whether `pattern` reaches less under gitignore-style `*` than under a `*`
/// that crosses `/`: true for any lone `*` in a pattern that has path segments
/// (`/a/*` covers `/a/b` but not `/a/b/c`). Rule files written for the wider
/// reading get a warning at load.
pub fn star_meaning_changed(pattern: &str) -> bool {
    let bytes = pattern.as_bytes();
    pattern.contains('/')
        && bytes.iter().enumerate().any(|(i, &b)| {
            b == b'*' && bytes.get(i + 1) != Some(&b'*') && (i == 0 || bytes[i - 1] != b'*')
        })
}

fn has_glob(s: &str) -> bool {
    s.contains(['*', '?', '['])
}

fn starts_with_glob(s: &str) -> bool {
    s.starts_with(['*', '?', '['])
}

fn absolutize(p: &Path, cwd: &Path) -> PathBuf {
    if p.is_absolute() {
        normalize(p)
    } else {
        normalize(&cwd.join(p))
    }
}

fn expand_home(raw: &str) -> Option<PathBuf> {
    let rest = if raw == "~" {
        ""
    } else {
        raw.strip_prefix("~/")?
    };
    Some(home_dir()?.join(rest))
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

/// Where `path` lands on this machine's disk: its deepest prefix that exists,
/// resolved by the OS, with the rest put back on lexically. The rest does not
/// exist, so it holds no symlink a lexical `..` could misjudge.
pub fn resolve_physically(path: &Path) -> PathBuf {
    let parts: Vec<Component> = path.components().collect();
    for split in (1..=parts.len()).rev() {
        let prefix: PathBuf = parts[..split].iter().collect();
        if let Ok(mut out) = std::fs::canonicalize(&prefix) {
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
    normalize(path)
}

fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> PathBuf {
        home_dir().expect("tests need HOME")
    }

    #[test]
    fn relative_and_dotted_paths_resolve_onto_cwd() {
        let cwd = Path::new("/proj");
        for raw in [".env", "./.env", "src/../.env", "/proj/x/../.env"] {
            assert_eq!(
                ResolvedPath::new(raw, cwd).primary,
                PathBuf::from("/proj/.env"),
                "{raw}"
            );
        }
    }

    #[test]
    fn a_tilde_path_carries_its_home_expansion_as_an_alias() {
        let r = ResolvedPath::new("~/.ssh/id_rsa", Path::new("/proj"));
        assert_eq!(r.primary, PathBuf::from("/proj/~/.ssh/id_rsa"));
        assert!(r.aliases.contains(&home().join(".ssh/id_rsa")));
    }

    #[test]
    fn a_symlinked_file_carries_its_real_path_as_an_alias() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("secret");
        std::fs::write(&real, "x").unwrap();
        let link = dir.path().join("innocent");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).unwrap();
        #[cfg(unix)]
        {
            let r = ResolvedPath::new(link.to_str().unwrap(), Path::new("/"));
            assert!(r.aliases.contains(&std::fs::canonicalize(&real).unwrap()));
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_dotdot_after_a_symlink_carries_where_it_really_lands_as_an_alias() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let (proj, outside) = (root.join("proj"), root.join("outside"));
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::create_dir_all(outside.join("sub")).unwrap();
        std::os::unix::fs::symlink(outside.join("sub"), proj.join("link")).unwrap();

        let r = ResolvedPath::new("link/../x", &proj);
        assert_eq!(r.primary, proj.join("x"));
        assert!(r.aliases.contains(&outside.join("x")), "{:?}", r.aliases);
    }

    #[test]
    fn patterns_resolve_into_the_same_coordinates() {
        let cwd = Path::new("/proj");
        assert_eq!(resolve_pattern(".env", cwd), "/proj/.env");
        assert_eq!(resolve_pattern("./src/**", cwd), "/proj/src/**");
        assert_eq!(resolve_pattern("/etc/**", cwd), "/etc/**");
        assert_eq!(resolve_pattern("**/.env", cwd), "**/.env");
        assert_eq!(resolve_pattern("*.pem", cwd), "**/*.pem");
        assert_eq!(
            resolve_pattern("~/.ssh/**", cwd),
            home().join(".ssh/**").to_string_lossy()
        );
    }

    #[test]
    fn single_star_stays_in_its_segment_and_double_star_crosses() {
        assert!(matches("/a/*", Path::new("/a/b")));
        assert!(!matches("/a/*", Path::new("/a/b/c")));
        assert!(matches("/a/**", Path::new("/a/b/c")));
        assert!(matches("**/.env", Path::new("/proj/deep/.env")));
        assert!(matches("**/.env", Path::new("/.env")));
    }

    #[test]
    fn a_plain_pattern_covers_the_directory_it_names() {
        assert!(matches("/proj/secret", Path::new("/proj/secret")));
        assert!(matches("/proj/secret", Path::new("/proj/secret/key")));
        assert!(!matches("/proj/secret", Path::new("/proj/secrets")));
    }

    #[test]
    fn star_meaning_change_is_detected_only_for_lone_stars() {
        assert!(star_meaning_changed("/a/*"));
        assert!(star_meaning_changed("/a/*/b"));
        assert!(!star_meaning_changed("*.env"));
        assert!(!star_meaning_changed("/a/**"));
        assert!(!star_meaning_changed("**/.env"));
        assert!(!star_meaning_changed("/a/b"));
    }
}
