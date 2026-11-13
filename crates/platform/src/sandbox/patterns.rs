//! The macOS profile's patterns: what `sandbox-exec` matches by regular
//! expression over a resolved path because no fixed path names it — the
//! engine's credentials and the scene files of the state root, in every scene
//! directory including one created later or linked in from elsewhere.

use super::{project_agix_dirs, sandbox_escape, Roots};
use std::path::{Path, PathBuf};

/// Denies writing the state root's scene files.
pub(super) fn scene_file_write_rules(roots: &Roots) -> String {
    let Some(state_root) = &roots.state_root else {
        return String::new();
    };
    state_root_scene_files_regexes(state_root, roots.paths.linked_scene_dirs_resolved())
        .iter()
        .map(|regex| format!("(deny file-write* (regex \"{}\"))\n", sandbox_escape(regex)))
        .collect()
}

/// Denies reading the engine's credentials.
pub(super) fn credential_read_rules(roots: &Roots) -> String {
    roots
        .paths
        .credential_locations(&project_agix_dirs(&roots.cwd))
        .iter()
        .flat_map(credential_read_regexes)
        .map(|regex| format!("(deny file-read* (regex \"{}\"))\n", sandbox_escape(&regex)))
        .collect()
}

/// The paths `located` is, as regular expressions over a resolved path: one
/// for its directories under `base`, and one for each scene linked in from
/// elsewhere, whose files resolve outside `base`.
fn credential_read_regexes(located: &base::paths::Located) -> Vec<String> {
    let base = regex_escape(&located.base.display().to_string());
    let under_base = if located.under_each_scene {
        format!("^{base}/scenes/[^/]+/")
    } else {
        format!("^{base}/")
    };
    let linked = located
        .linked_dirs_resolved()
        .into_iter()
        .map(|dir| format!("^{}/", regex_escape(&dir.display().to_string())));
    std::iter::once(under_base)
        .chain(linked)
        .map(|dir| secret_regex(&dir, located.secret))
        .collect()
}

/// The paths `secret` is in the directories `dir`, a regex prefix, matches.
fn secret_regex(dir: &str, secret: base::paths::Secret) -> String {
    use base::paths::{KeyFileNames, Secret};
    match secret {
        Secret::KeyFiles(names @ KeyFileNames::StateRoot) => {
            format!(
                r"{dir}\.?({})[^/]*\.json[^/]*$",
                alternatives(names.stems())
            )
        }
        Secret::KeyFiles(names @ KeyFileNames::Project) => {
            format!(r"{dir}\.?({})[^/]*$", alternatives(names.stems()))
        }
        Secret::Tree(path) | Secret::DirName(path) => {
            format!("{dir}{}(/|$)", regex_escape(path))
        }
    }
}

/// `base::paths::STATE_ROOT_SCENE_FILES` in `state_root` and in every scene
/// directory under it, including one created later, and in each scene
/// directory `linked` in from elsewhere, whose files resolve outside it.
fn state_root_scene_files_regexes(state_root: &Path, linked: &[PathBuf]) -> Vec<String> {
    let files = alternatives(base::paths::STATE_ROOT_SCENE_FILES);
    std::iter::once(format!(
        "^{}/(scenes/[^/]+/)?({files})$",
        regex_escape(&state_root.display().to_string()),
    ))
    .chain(
        linked
            .iter()
            .map(|dir| format!("^{}/({files})$", regex_escape(&dir.display().to_string()))),
    )
    .collect()
}

fn alternatives(names: &[&str]) -> String {
    names
        .iter()
        .map(|name| regex_escape(name))
        .collect::<Vec<_>>()
        .join("|")
}

fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if r"\.^$|?*+()[]{}".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}
