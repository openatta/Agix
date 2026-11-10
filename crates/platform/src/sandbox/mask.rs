//! How a mount-based sandbox hides the credentials under a directory: the
//! mounts that cover each directory holding one with a tmpfs and bind its
//! other entries back read-only. Computed from `base::paths`' credential
//! locations and the directories as they are on disk, independent of the
//! backend, so it is tested on every platform; the Linux backend turns each
//! [`Mount`] into a bwrap argument.

use base::paths::{KeyFileNames, Located, Secret};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Mount {
    Tmpfs(PathBuf),
    RoBind(PathBuf),
}

/// The credential locations below one directory, as a tree of names.
#[derive(Default)]
struct Node {
    children: BTreeMap<String, Node>,
    /// What every real subdirectory holds: `scenes/<any>`.
    each: Option<Box<Node>>,
    key_files: Vec<KeyFileNames>,
    secret_names: Vec<String>,
}

impl Node {
    fn child(&mut self, name: &str) -> &mut Node {
        self.children.entry(name.to_lowercase()).or_default()
    }

    fn is_secret(&self, lowered: &str) -> bool {
        self.key_files.iter().any(|names| names.admits(lowered))
            || self.secret_names.iter().any(|name| name == lowered)
    }
}

/// The mounts that leave every location in `locs` under `root` unreadable
/// and the rest of `root` readable as it is now, in the order they have to
/// be applied: each directory that holds a credential, or a directory on the
/// way to one, is covered with a tmpfs; its entries that are neither are
/// bound back over it. A credential written there later lands under the
/// tmpfs, on no path a command can read. An entry on the way to a credential
/// that is not a real directory stays covered.
pub(super) fn mask_plan(root: &Path, locs: &[Located]) -> Vec<Mount> {
    let mut tree = Node::default();
    for loc in locs {
        let Ok(below) = loc.base.strip_prefix(root) else {
            continue;
        };
        let mut node = &mut tree;
        for part in below.iter() {
            node = node.child(&part.to_string_lossy());
        }
        if loc.under_each_scene {
            node = node
                .child("scenes")
                .each
                .get_or_insert_with(Default::default);
        }
        match loc.secret {
            Secret::KeyFiles(names) => node.key_files.push(names),
            Secret::Tree(path) | Secret::DirName(path) => {
                let parts: Vec<&str> = path.split('/').collect();
                let Some((last, above)) = parts.split_last() else {
                    continue;
                };
                for part in above {
                    node = node.child(part);
                }
                node.secret_names.push(last.to_lowercase());
            }
        }
    }
    let mut plan = Vec::new();
    plan_dir(root, &tree, &mut plan);
    plan
}

fn plan_dir(dir: &Path, node: &Node, plan: &mut Vec<Mount>) {
    if !is_real_dir(dir) {
        return;
    }
    plan.push(Mount::Tmpfs(dir.to_path_buf()));
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    entries.sort();
    let mut below = Vec::new();
    for path in entries {
        let lowered = path
            .file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if node.is_secret(&lowered) {
            continue;
        }
        match node.children.get(&lowered).or(node.each.as_deref()) {
            Some(inner) => below.push((path, inner)),
            None => plan.push(Mount::RoBind(path)),
        }
    }
    for (path, inner) in below {
        plan_dir(&path, inner, plan);
    }
}

fn is_real_dir(path: &Path) -> bool {
    path.symlink_metadata().is_ok_and(|m| m.is_dir())
}

/// Each [`base::paths::CREDENTIALS`] entry a backend masks, placed under
/// `root` and the project's `agix`, and a file it names: in a scene that
/// may not exist yet, each key-file stem.
#[cfg(test)]
pub(super) fn credential_examples(
    root: &Path,
    agix: &Path,
) -> Vec<(base::paths::Credential, PathBuf)> {
    use base::paths::{Scope, CREDENTIALS};
    let mut examples = Vec::new();
    for credential in CREDENTIALS {
        let dir = match credential.scope {
            Scope::StateRoot => root.to_path_buf(),
            Scope::EachSceneDir => root.join("scenes").join("added-later"),
            Scope::AnyAgixDir => agix.to_path_buf(),
            Scope::Anywhere => continue,
        };
        let names: Vec<String> = match credential.secret {
            Secret::KeyFiles(KeyFileNames::StateRoot) => KeyFileNames::StateRoot
                .stems()
                .iter()
                .map(|stem| format!("{stem}.json"))
                .collect(),
            Secret::KeyFiles(names) => names.stems().iter().map(|s| s.to_string()).collect(),
            Secret::Tree(path) | Secret::DirName(path) => vec![format!("{path}/x")],
        };
        examples.extend(names.into_iter().map(|name| (*credential, dir.join(name))));
    }
    examples
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use base::paths::{Scope, SensitivePaths};

    /// Whether `path` is readable after `plan`: the last mount over it or a
    /// directory above it decides.
    fn readable(plan: &[Mount], path: &Path) -> bool {
        plan.iter()
            .rev()
            .find_map(|mount| match mount {
                Mount::Tmpfs(at) if path.starts_with(at) && path != at => Some(false),
                Mount::RoBind(at) if path.starts_with(at) => Some(true),
                _ => None,
            })
            .unwrap_or(true)
    }

    fn write(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "").unwrap();
    }

    fn state() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        (dir, root)
    }

    #[test]
    fn the_root_and_every_scene_are_masked_and_the_rest_bound_back() {
        let (_dir, state) = state();
        for scene in ["code", "triage"] {
            let d = state.join("scenes").join(scene);
            std::fs::create_dir_all(d.join("skills")).unwrap();
            write(&d.join("settings.json"));
            write(&d.join("settings.local.json"));
            write(&d.join("scene.json"));
        }
        std::fs::create_dir_all(state.join("skills")).unwrap();
        write(&state.join("settings.json"));
        write(&state.join("settings.local.json"));
        let locs = SensitivePaths::new(Some(&state), None).credential_locations(&[]);
        let plan = mask_plan(&state, &locs);

        assert_eq!(plan.first(), Some(&Mount::Tmpfs(state.clone())));
        for scene in ["code", "triage"] {
            assert!(
                plan.contains(&Mount::Tmpfs(state.join("scenes").join(scene))),
                "scene `{scene}` is not masked: {plan:?}"
            );
            assert!(plan.contains(&Mount::RoBind(
                state.join("scenes").join(scene).join("scene.json")
            )));
        }
        assert!(
            !plan.iter().any(|m| matches!(m, Mount::RoBind(p)
                if p.ends_with("settings.json") || p.ends_with("settings.local.json"))),
            "a key file was bound back over the tmpfs: {plan:?}"
        );
        assert!(plan.contains(&Mount::RoBind(state.join("skills"))));
    }

    #[test]
    fn a_token_directory_stays_under_the_tmpfs_of_the_directory_holding_it() {
        let (_dir, state) = state();
        write(&state.join("mcp/oauth/x.json"));
        write(&state.join("mcp/servers.json"));
        write(&state.join("daemon/auth/desktop.token"));
        write(&state.join("daemon/instances.d/a.json"));
        let locs = SensitivePaths::new(Some(&state), None).credential_locations(&[]);
        let plan = mask_plan(&state, &locs);

        assert!(plan.contains(&Mount::Tmpfs(state.join("mcp"))));
        assert!(plan.contains(&Mount::RoBind(state.join("mcp/servers.json"))));
        assert!(plan.contains(&Mount::RoBind(state.join("daemon/instances.d"))));
        assert!(!readable(&plan, &state.join("mcp/oauth/x.json")));
        assert!(!readable(&plan, &state.join("daemon/auth/desktop.token")));
        assert!(!readable(&plan, &state.join("daemon/auth/written-later")));
    }

    #[test]
    fn what_is_not_a_real_directory_on_the_way_to_a_credential_stays_covered() {
        let (_dir, state) = state();
        let elsewhere = tempfile::tempdir().unwrap();
        write(&elsewhere.path().join("settings.json"));
        std::fs::create_dir_all(state.join("scenes")).unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), state.join("scenes/linked")).unwrap();
        write(&state.join("scenes/notes.txt"));
        let locs = SensitivePaths::new(Some(&state), None).credential_locations(&[]);
        let plan = mask_plan(&state, &locs);

        assert!(!readable(&plan, &state.join("scenes/linked/settings.json")));
        assert!(!readable(&plan, &state.join("scenes/notes.txt")));
    }

    #[test]
    fn an_absent_root_needs_no_mounts() {
        let (_dir, state) = state();
        let missing = state.join("missing");
        let locs = SensitivePaths::new(Some(&missing), None).credential_locations(&[]);
        assert!(mask_plan(&missing, &locs).is_empty());
    }

    /// The Linux backend masks the state root's credentials through
    /// [`mask_plan`] and hides the project's key files one by one
    /// ([`Located::existing`]): every entry of the table reaches one of them.
    #[test]
    fn every_credential_is_masked_on_linux() {
        let (_dir, root) = state();
        let project = tempfile::tempdir().unwrap();
        let agix = project.path().canonicalize().unwrap().join(".agix");
        std::fs::create_dir_all(&agix).unwrap();
        let examples = credential_examples(&root, &agix);
        for (_, path) in &examples {
            write(path);
        }
        write(&root.join("skills/x/SKILL.md"));
        let locs = SensitivePaths::new(Some(&root), None)
            .credential_locations(std::slice::from_ref(&agix));
        let plan = mask_plan(&root, &locs);
        let hidden: Vec<PathBuf> = locs
            .iter()
            .filter(|l| l.base == agix)
            .flat_map(Located::existing)
            .collect();

        assert!(!examples.is_empty());
        for (credential, path) in &examples {
            let masked = match credential.scope {
                Scope::AnyAgixDir => hidden.contains(path),
                _ => !readable(&plan, path),
            };
            assert!(masked, "{credential:?}: {} is readable", path.display());
        }
        assert!(readable(&plan, &root.join("skills/x/SKILL.md")));
    }
}
