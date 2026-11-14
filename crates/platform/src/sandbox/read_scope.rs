//! What a command under `ReadScope::Only` may read: the roots the policy
//! names, the system directories programs load from (`SYSTEM_READ_ROOTS`)
//! and the user's git configuration (`HOME_READ_ROOTS`,
//! `XDG_CONFIG_READ_ROOTS`). macOS denies `file-read*` and allows it back
//! under each root, metadata everywhere; Linux binds each root read-only in
//! place of the whole filesystem. A git those roots leave out — a shim under
//! the home directory, a configuration file included from elsewhere — is
//! reported unmet (`unmet`).
//!
//! The tables were measured on macOS with `ls`, `cat`, git directly and
//! through the xcrun shim; the Linux one is the design's, not yet measured.

use super::resolve_path;
use base::interface::exec::ReadScope;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

#[cfg(target_os = "macos")]
const SYSTEM_READ_ROOTS: &[&str] = &[
    "/usr",
    "/bin",
    "/sbin",
    "/System",
    "/Library/Developer/CommandLineTools",
    "/Applications/Xcode.app/Contents/Developer",
    "/private/etc",
    "/private/var/db/timezone",
    "/opt/homebrew",
    "/usr/local",
    "/dev",
];

#[cfg(target_os = "linux")]
const SYSTEM_READ_ROOTS: &[&str] = &[
    "/usr",
    "/bin",
    "/sbin",
    "/lib",
    "/lib32",
    "/lib64",
    "/libx32",
    "/etc",
    "/opt",
    "/nix/store",
];

/// Under the home directory. git stops with "unable to access" on a
/// configuration file that exists and cannot be read; CoreFoundation reads
/// the user's text encoding.
const HOME_READ_ROOTS: &[&str] = &[
    ".gitconfig",
    ".config/git",
    #[cfg(target_os = "macos")]
    ".CFUserTextEncoding",
];

/// Under `$XDG_CONFIG_HOME`, when it is set: git's own files there, one by
/// one, so that `git/credentials` beside them stays unreadable.
const XDG_CONFIG_READ_ROOTS: &[&str] = &["git/config", "git/ignore", "git/attributes"];

/// Where `xcode-select` points, which need not be one of the table's.
#[cfg(target_os = "macos")]
fn selected_developer_dir() -> Option<PathBuf> {
    std::fs::canonicalize("/private/var/db/xcode_select_link").ok()
}

#[cfg(not(target_os = "macos"))]
fn selected_developer_dir() -> Option<PathBuf> {
    None
}

/// The directories a command's own git looks in for the user's
/// configuration, from the command's environment, else the daemon's: the
/// profile and the strict-profile probe both read them from here.
#[derive(Debug, Clone, Default)]
pub(super) struct UserDirs {
    pub(super) home: Option<PathBuf>,
    pub(super) xdg_config_home: Option<PathBuf>,
}

impl UserDirs {
    pub(super) fn of(env: &[(String, String)]) -> Self {
        Self {
            home: var(env, "HOME").map(PathBuf::from),
            xdg_config_home: var(env, "XDG_CONFIG_HOME")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from),
        }
    }
}

/// `name` as the command's `env` sets it, else as the daemon's environment.
pub(super) fn var(env: &[(String, String)], name: &str) -> Option<OsString> {
    env.iter()
        .rev()
        .find(|(k, _)| k == name)
        .map(|(_, v)| OsString::from(v))
        .or_else(|| std::env::var_os(name))
}

/// Everything `ReadScope::Only(scope)` lets a command read, resolved.
pub(super) fn strict_read_roots(scope: &[PathBuf], dirs: &UserDirs) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = SYSTEM_READ_ROOTS
        .iter()
        .map(|root| resolve_path(Path::new(root)))
        .chain(selected_developer_dir())
        .chain(scope_read_roots(scope, dirs))
        .collect();
    roots.sort();
    roots.dedup();
    roots
}

/// `scope` and the user's git configuration under `dirs`, resolved.
fn scope_read_roots(scope: &[PathBuf], dirs: &UserDirs) -> Vec<PathBuf> {
    dirs.home
        .iter()
        .flat_map(|h| HOME_READ_ROOTS.iter().map(move |r| h.join(r)))
        .chain(
            dirs.xdg_config_home
                .iter()
                .flat_map(|x| XDG_CONFIG_READ_ROOTS.iter().map(move |r| x.join(r))),
        )
        .chain(scope.iter().cloned())
        .map(|p| resolve_path(&p))
        .collect()
}

/// What a command under `read` would expect of git and cannot have, one line
/// each: the `git` first on `path` lies outside what it reads, or the user's
/// git configuration — `~/.gitconfig`, and `git/config` under
/// `xdg_config_home` or else `~/.config`, where git looks — is or includes a
/// file from outside it, which git stops on. Empty for `Everything`.
pub(super) fn unmet(read: &ReadScope, dirs: &UserDirs, path: Option<&OsStr>) -> Vec<String> {
    let ReadScope::Only(scope) = read else {
        return Vec::new();
    };
    let home = dirs.home.as_deref();
    let roots = strict_read_roots(scope, dirs);
    let readable = |p: &Path| roots.iter().any(|root| p.starts_with(root));
    let mut unmet = Vec::new();
    if let Some(git) = path.and_then(git_on_path) {
        let real = resolve_path(&git);
        if !readable(&real) {
            unmet.push(format!(
                "git: the `git` on PATH is {}, which a command under the strict profile cannot \
                 read, so git does not run there",
                real.display()
            ));
        }
    }
    let mut seen = Vec::new();
    let xdg_config = dirs
        .xdg_config_home
        .clone()
        .or_else(|| home.map(|h| h.join(".config")))
        .map(|dir| dir.join("git/config"));
    let configs = home
        .map(|h| h.join(".gitconfig"))
        .into_iter()
        .chain(xdg_config);
    for config in configs {
        let real = resolve_path(&config);
        if real.is_file() && !readable(&real) {
            unmet.push(format!(
                "git: the user's git configuration is {}, which a command under the strict \
                 profile cannot read, so git stops there",
                real.display()
            ));
        }
        for included in git_includes(&config, home, &mut seen) {
            if !readable(&included) {
                unmet.push(format!(
                    "git: the user's git configuration includes {}, which a command under the \
                     strict profile cannot read, so git stops there",
                    included.display()
                ));
            }
        }
    }
    unmet
}

fn git_on_path(path: &OsStr) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    std::env::split_paths(path)
        .map(|dir| dir.join("git"))
        .find(|git| {
            std::fs::metadata(git).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}

/// The existing files `config` includes, its includes' includes among them,
/// resolved. `seen` keeps a file from being read twice.
fn git_includes(config: &Path, home: Option<&Path>, seen: &mut Vec<PathBuf>) -> Vec<PathBuf> {
    let config = resolve_path(config);
    if seen.contains(&config) {
        return Vec::new();
    }
    seen.push(config.clone());
    let Ok(text) = std::fs::read_to_string(&config) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    let mut including = false;
    for line in text.lines().map(str::trim) {
        if let Some(header) = line.strip_prefix('[') {
            let name = header
                .split(|c: char| c == ']' || c.is_whitespace() || c == '"')
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase();
            including = name == "include" || name == "includeif";
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if !including || !key.trim().eq_ignore_ascii_case("path") {
            continue;
        }
        let value = value.trim().trim_matches('"');
        let named = match (value.strip_prefix("~/"), home) {
            (Some(rest), Some(home)) => home.join(rest),
            _ => config.parent().unwrap_or(Path::new("/")).join(value),
        };
        let included = resolve_path(&named);
        if included.is_file() {
            found.extend(git_includes(&included, home, seen));
            found.push(included);
        }
    }
    found
}

/// The profile rules for `read`, to go before the deny list so that it stays
/// in force inside the scope. Empty for `Everything`.
#[cfg(target_os = "macos")]
pub(super) fn macos_rules(read: &ReadScope, dirs: &UserDirs) -> String {
    let ReadScope::Only(scope) = read else {
        return String::new();
    };
    let path = |p: &Path| super::sandbox_escape(&p.display().to_string());
    // dyld reads `/` itself before the program starts.
    let mut s = String::from(
        "(deny file-read*)\n(allow file-read-metadata)\n(allow file-read* (literal \"/\")",
    );
    for p in strict_read_roots(scope, dirs)
        .iter()
        .flat_map(|p| super::spellings(p))
    {
        s.push_str(&format!(" (subpath \"{}\")", path(&p)));
    }
    // The xcrun shim (`/usr/bin/git`) looks its tool up in this cache and
    // prints an error on every run when it cannot read it. Not writable: the
    // unconfined xcrun reads it too.
    if let Some(tmp) = super::darwin_user_temp_dir() {
        s.push_str(&format!(" (literal \"{}\")", path(&tmp.join("xcrun_db"))));
    }
    s.push_str(")\n");
    s
}

/// How a bwrap command line holds `read`.
#[cfg(target_os = "linux")]
pub(super) struct LinuxReadScope<'a> {
    scope: Option<&'a [PathBuf]>,
    dirs: UserDirs,
    readable: Option<Vec<PathBuf>>,
}

#[cfg(target_os = "linux")]
impl<'a> LinuxReadScope<'a> {
    pub(super) fn new(read: &'a ReadScope, dirs: UserDirs) -> Self {
        let scope = match read {
            ReadScope::Everything => None,
            ReadScope::Only(scope) => Some(scope.as_slice()),
        };
        Self {
            scope,
            readable: scope.map(|scope| strict_read_roots(scope, &dirs)),
            dirs,
        }
    }

    /// The first mounts: the whole filesystem, or the system roots.
    pub(super) fn base_mounts(&self, args: &mut Vec<String>) {
        match self.scope {
            None => super::bind(args, "--ro-bind", Path::new("/")),
            Some(_) => {
                for root in SYSTEM_READ_ROOTS {
                    super::bind(args, "--ro-bind-try", Path::new(root));
                }
            }
        }
    }

    /// The scope's own roots, after the `/tmp` tmpfs so that one under
    /// `/tmp` is not hidden by it.
    pub(super) fn scope_mounts(&self, args: &mut Vec<String>) {
        let roots = self.scope.map(|scope| scope_read_roots(scope, &self.dirs));
        for root in roots.iter().flatten() {
            super::bind(args, "--ro-bind-try", root);
        }
    }

    /// Whether `path` is in the sandbox at all.
    pub(super) fn shows(&self, path: &Path) -> bool {
        self.readable
            .as_ref()
            .is_none_or(|readable| readable.iter().any(|r| path.starts_with(r)))
    }
}
