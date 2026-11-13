//! What a command under `ReadScope::Only` may read: the roots the policy
//! names, the system directories programs load from (`SYSTEM_READ_ROOTS`)
//! and the user's git configuration (`HOME_READ_ROOTS`). macOS denies
//! `file-read*` and allows it back under each root, metadata everywhere;
//! Linux binds each root read-only in place of the whole filesystem.
//!
//! The tables were measured on macOS with `ls`, `cat`, git directly and
//! through the xcrun shim; the Linux one is the design's, not yet measured.

use super::resolve_path;
use base::interface::exec::ReadScope;
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

/// Where `xcode-select` points, which need not be one of the table's.
#[cfg(target_os = "macos")]
fn selected_developer_dir() -> Option<PathBuf> {
    std::fs::canonicalize("/private/var/db/xcode_select_link").ok()
}

#[cfg(not(target_os = "macos"))]
fn selected_developer_dir() -> Option<PathBuf> {
    None
}

/// Everything `ReadScope::Only(scope)` lets a command read, resolved.
pub(super) fn strict_read_roots(scope: &[PathBuf], home: Option<&Path>) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = SYSTEM_READ_ROOTS
        .iter()
        .map(|root| resolve_path(Path::new(root)))
        .chain(selected_developer_dir())
        .chain(scope_read_roots(scope, home))
        .collect();
    roots.sort();
    roots.dedup();
    roots
}

/// `scope` and the user's git configuration under `home`, resolved.
fn scope_read_roots(scope: &[PathBuf], home: Option<&Path>) -> Vec<PathBuf> {
    home.iter()
        .flat_map(|h| HOME_READ_ROOTS.iter().map(move |r| h.join(r)))
        .chain(scope.iter().cloned())
        .map(|p| resolve_path(&p))
        .collect()
}

/// The profile rules for `read`, to go before the deny list so that it stays
/// in force inside the scope. Empty for `Everything`.
#[cfg(target_os = "macos")]
pub(super) fn macos_rules(read: &ReadScope, home: Option<&Path>) -> String {
    let ReadScope::Only(scope) = read else {
        return String::new();
    };
    let path = |p: &Path| super::sandbox_escape(&p.display().to_string());
    // dyld reads `/` itself before the program starts.
    let mut s = String::from(
        "(deny file-read*)\n(allow file-read-metadata)\n(allow file-read* (literal \"/\")",
    );
    for p in strict_read_roots(scope, home)
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
    home: Option<PathBuf>,
    readable: Option<Vec<PathBuf>>,
}

#[cfg(target_os = "linux")]
impl<'a> LinuxReadScope<'a> {
    pub(super) fn new(read: &'a ReadScope, home: Option<PathBuf>) -> Self {
        let scope = match read {
            ReadScope::Everything => None,
            ReadScope::Only(scope) => Some(scope.as_slice()),
        };
        Self {
            scope,
            readable: scope.map(|scope| strict_read_roots(scope, home.as_deref())),
            home,
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
        let roots = self
            .scope
            .map(|scope| scope_read_roots(scope, self.home.as_deref()));
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
