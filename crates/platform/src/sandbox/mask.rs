//! How a mount-based sandbox hides the credentials under a directory: the
//! mounts that cover each directory holding one with a tmpfs and bind its
//! other entries back read-only. Computed from `base::paths`' credential
//! locations and the directories as they are on disk, independent of the
//! backend, so it is tested on every platform; the Linux backend turns each
//! [`Mount`] into a bwrap argument.
//!
//! What has to stay writable inside a masked directory is bound back over
//! the mask, and a credential in that is hidden where it is now
//! ([`Mount::Hide`]): one written there later, or rename-written over it, is
//! readable. A writable directory that holds a credential directly — a
//! project that is the state root, a scene directory opened as a project —
//! cannot itself be masked without its new entries landing in the tmpfs, so
//! that is where the residue stays.

use base::paths::{KeyFileNames, Located, Secret};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Mount {
    Tmpfs(PathBuf),
    RoBind(PathBuf),
    /// Bound back writable over the masked tree.
    Bind(PathBuf),
    /// A credential inside what is bound back writable, hidden on its own.
    Hide(PathBuf),
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
/// that is not a real directory stays covered. Then each of `holes` that
/// [`fits_a_hole`] is bound back writable, and last each credential now in
/// one of them is hidden.
pub(super) fn mask_plan(root: &Path, locs: &[Located], holes: &[PathBuf]) -> Vec<Mount> {
    let tree = tree_of(root, locs);
    let mut plan = Vec::new();
    plan_dir(root, &tree, &mut plan);
    if plan.is_empty() {
        return plan;
    }
    let holes: Vec<&PathBuf> = holes
        .iter()
        .filter(|hole| hole_fits(root, &tree, hole))
        .collect();
    plan.extend(holes.iter().map(|hole| Mount::Bind(hole.to_path_buf())));
    let mut hidden: Vec<PathBuf> = locs
        .iter()
        .flat_map(Located::existing)
        .filter(|p| holes.iter().any(|hole| p.starts_with(hole)))
        .collect();
    hidden.sort();
    hidden.dedup();
    plan.extend(hidden.into_iter().map(Mount::Hide));
    plan
}

/// The mounts that mask `dir`, a directory below `root`, around the
/// credentials of `locs` in it, as [`mask_plan`] masks `root`. Empty where
/// none lies in it or it is not a real directory.
pub(super) fn mask_below(root: &Path, locs: &[Located], dir: &Path) -> Vec<Mount> {
    let tree = tree_of(root, locs);
    let mut plan = Vec::new();
    if let Reach::Holds(node) = reach(root, &tree, dir) {
        plan_dir(dir, node, &mut plan);
    }
    plan
}

/// The state root no fence shuts — one a project lives in, or any under a
/// policy that writes only its named roots — and what has to stay writable
/// in it — the project, when it is writable, and the writable roots under
/// the root — when the root can be masked around them: the root is in the
/// sandbox at all (`shown`), and each of them [`fits_a_hole`].
pub(super) fn holes_in_state_root<'a>(
    roots: &'a super::Roots,
    secrets: &[Located],
    shown: impl Fn(&Path) -> bool,
) -> Option<(&'a Path, Vec<PathBuf>)> {
    let root = roots.state_root.as_deref().filter(|root| {
        (roots.only_additional || roots.project_in(root)) && shown(root) && root.is_dir()
    })?;
    let holes: Vec<PathBuf> = (!roots.only_additional)
        .then_some(&roots.cwd)
        .into_iter()
        .chain(roots.writable.iter().filter(|p| p.starts_with(root)))
        .cloned()
        .collect();
    holes
        .iter()
        .all(|hole| fits_a_hole(root, secrets, hole))
        .then_some((root, holes))
}

/// Whether `hole` can be bound back writable over the mask of `locs` under
/// `root`: it is below `root`, not `root` itself, and neither a credential
/// nor inside one. A credential below it is hidden on its own.
pub(super) fn fits_a_hole(root: &Path, locs: &[Located], hole: &Path) -> bool {
    hole_fits(root, &tree_of(root, locs), hole)
}

fn hole_fits(root: &Path, tree: &Node, hole: &Path) -> bool {
    hole != root && matches!(reach(root, tree, hole), Reach::Clear | Reach::Holds(_))
}

/// Where a path stands to the credentials under a directory.
enum Reach<'a> {
    Outside,
    /// A credential, or inside one.
    Secret,
    /// No credential lies in or below it.
    Clear,
    /// Credentials lie in or below it, as the node says.
    Holds(&'a Node),
}

fn reach<'a>(root: &Path, tree: &'a Node, path: &Path) -> Reach<'a> {
    let Ok(below) = path.strip_prefix(root) else {
        return Reach::Outside;
    };
    let mut node = tree;
    for part in below.iter() {
        let lowered = part.to_string_lossy().to_lowercase();
        if node.is_secret(&lowered) {
            return Reach::Secret;
        }
        match node.children.get(&lowered).or(node.each.as_deref()) {
            Some(inner) => node = inner,
            None => return Reach::Clear,
        }
    }
    Reach::Holds(node)
}

fn tree_of(root: &Path, locs: &[Located]) -> Node {
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
    tree
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
                Mount::RoBind(at) | Mount::Bind(at) if path.starts_with(at) => Some(true),
                Mount::Hide(at) if path.starts_with(at) => Some(false),
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
        let plan = mask_plan(&state, &locs, &[]);

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
        let plan = mask_plan(&state, &locs, &[]);

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
        let plan = mask_plan(&state, &locs, &[]);

        assert!(!readable(&plan, &state.join("scenes/linked/settings.json")));
        assert!(!readable(&plan, &state.join("scenes/notes.txt")));
    }

    /// A project in the state root: the root is masked as for any project,
    /// and the project is bound back writable over it, so a key file the
    /// engine rename-writes later is not readable.
    #[test]
    fn a_project_in_the_state_root_is_a_hole_in_its_mask() {
        let (_dir, state) = state();
        write(&state.join("settings.json"));
        write(&state.join("scenes/code/settings.json"));
        write(&state.join("projects/app/src/main.rs"));
        write(&state.join("notes.md"));
        let project = state.join("projects/app");
        let tmp = state.join("tmp/session");
        std::fs::create_dir_all(&tmp).unwrap();
        let locs = SensitivePaths::new(Some(&state), None).credential_locations(&[]);
        assert!(fits_a_hole(&state, &locs, &project));
        assert!(fits_a_hole(&state, &locs, &tmp));
        let plan = mask_plan(&state, &locs, &[project.clone(), tmp.clone()]);

        assert_eq!(plan.first(), Some(&Mount::Tmpfs(state.clone())));
        assert_eq!(
            plan[plan.len() - 2..],
            [Mount::Bind(project.clone()), Mount::Bind(tmp)]
        );
        assert!(readable(&plan, &project.join("src/main.rs")));
        assert!(readable(&plan, &state.join("notes.md")));
        assert!(!readable(&plan, &state.join("settings.json")));
        assert!(!readable(&plan, &state.join("settings.local.json")));
        assert!(!readable(&plan, &state.join("scenes/code/settings.json")));
    }

    /// A command that writes only its named roots — the engine's own git,
    /// a filter driver it runs — is no reason to bind the state root's
    /// credentials shut one by one, where a key file rename-written later
    /// slips out from under its bind: the root is masked, the named roots
    /// in it bound back. One its read scope leaves out is not in the sandbox
    /// at all.
    #[test]
    fn a_policy_writing_only_its_named_roots_masks_the_state_root_around_them() {
        use base::interface::exec::SandboxPolicy;
        let (_dir, state) = state();
        write(&state.join("settings.local.json"));
        let tmp = state.join("tmp/session");
        std::fs::create_dir_all(&tmp).unwrap();
        let project = tempfile::tempdir().unwrap();
        let policy = SandboxPolicy {
            state_root: Some(state.clone()),
            only_additional_writable: true,
            ..Default::default()
        };
        let roots =
            super::super::Roots::resolve(project.path(), std::slice::from_ref(&tmp), &policy);
        let locs = roots.paths.credential_locations(&[]);

        let (root, holes) = holes_in_state_root(&roots, &locs, |_| true).expect("masked");
        assert_eq!(
            (root, holes.as_slice()),
            (state.as_path(), [tmp].as_slice())
        );
        assert!(holes_in_state_root(&roots, &locs, |_| false).is_none());
    }

    /// A project that holds credentials of the state root — a scene
    /// directory opened as a project — is still a hole in the root's mask:
    /// what lies outside it stays under the tmpfs, the root's own key files
    /// and one written later among them; those in it are hidden one by one.
    #[test]
    fn a_project_holding_credentials_is_a_hole_whose_credentials_are_hidden() {
        use base::interface::exec::SandboxPolicy;
        let (_dir, state) = state();
        write(&state.join("settings.json"));
        write(&state.join("scenes/code/settings.local.json"));
        write(&state.join("scenes/code/skills/s/SKILL.md"));
        write(&state.join("scenes/triage/settings.json"));
        let project = state.join("scenes/code");
        let policy = SandboxPolicy {
            state_root: Some(state.clone()),
            ..Default::default()
        };
        let roots = super::super::Roots::resolve(&project, &[], &policy);
        let locs = roots.paths.credential_locations(&[]);
        let (root, holes) = holes_in_state_root(&roots, &locs, |_| true).expect("masked");
        let plan = mask_plan(root, &locs, &holes);

        assert!(plan.contains(&Mount::Bind(project.clone())), "{plan:?}");
        assert!(readable(&plan, &project.join("skills/s/SKILL.md")));
        assert!(!readable(&plan, &project.join("settings.local.json")));
        for later in [
            state.join("settings.json"),
            state.join("settings.local.json"),
            state.join("scenes/triage/settings.local.json"),
            state.join("scenes/new/settings.json"),
        ] {
            assert!(!readable(&plan, &later), "{}: {plan:?}", later.display());
        }
    }

    /// A project that is the state root keeps the root itself from being
    /// masked, but not the configuration directories in it that hold
    /// credentials, which a command cannot write anyway: a scene's key file
    /// or token written later, or one in a scene created later, lands under
    /// their tmpfs.
    #[test]
    fn a_directory_below_the_root_is_masked_on_its_own() {
        let (_dir, state) = state();
        write(&state.join("scenes/code/settings.json"));
        write(&state.join("scenes/code/scene.json"));
        write(&state.join("mcp/servers.json"));
        write(&state.join("skills/s/SKILL.md"));
        let locs = SensitivePaths::new(Some(&state), None).credential_locations(&[]);
        let scenes = mask_below(&state, &locs, &state.join("scenes"));
        let mcp = mask_below(&state, &locs, &state.join("mcp"));

        assert_eq!(scenes.first(), Some(&Mount::Tmpfs(state.join("scenes"))));
        assert!(readable(&scenes, &state.join("scenes/code/scene.json")));
        for later in [
            "scenes/code/settings.json",
            "scenes/code/settings.local.json",
            "scenes/code/mcp/oauth/x.json",
            "scenes/new/settings.json",
        ] {
            assert!(
                !readable(&scenes, &state.join(later)),
                "{later}: {scenes:?}"
            );
        }
        assert!(readable(&mcp, &state.join("mcp/servers.json")));
        assert!(!readable(&mcp, &state.join("mcp/oauth/x.json")));
        for none in ["skills", "scenes/code/settings.json", "missing"] {
            assert!(
                mask_below(&state, &locs, &state.join(none)).is_empty(),
                "{none}"
            );
        }
    }

    /// A project's `.agix` masked around its key files: the rest is bound
    /// back, the working entries writable, and a key file written later or
    /// rename-written over the old one is not readable.
    #[test]
    fn a_projects_agix_is_masked_around_its_key_files() {
        let project = tempfile::tempdir().unwrap();
        let project = project.path().canonicalize().unwrap();
        let agix = project.join(".agix");
        write(&agix.join("settings.json"));
        write(&agix.join("settings.local.json"));
        write(&agix.join("scratchpad/notes.md"));
        let locs = SensitivePaths::new(None, Some(&project))
            .credential_locations(std::slice::from_ref(&agix));
        let tmp = agix.join(".settings.local.json.1.tmp");
        let plan = mask_plan(&agix, &locs, &[agix.join("scratchpad"), tmp.clone()]);

        assert_eq!(plan.first(), Some(&Mount::Tmpfs(agix.clone())));
        assert!(plan.contains(&Mount::Bind(agix.join("scratchpad"))));
        assert!(!plan.contains(&Mount::Bind(tmp.clone())), "{plan:?}");
        assert!(readable(&plan, &agix.join("settings.json")));
        assert!(readable(&plan, &agix.join("scratchpad/notes.md")));
        for secret in [
            "settings.local.json",
            "mcp.local.json",
            ".settings.local.json.1.tmp",
        ] {
            assert!(!readable(&plan, &agix.join(secret)), "{secret}: {plan:?}");
        }
    }

    #[test]
    fn the_root_and_what_is_or_is_in_a_credential_are_no_hole() {
        let (_dir, state) = state();
        write(&state.join("scenes/code/settings.json"));
        write(&state.join("daemon/auth/desktop.token"));
        let locs = SensitivePaths::new(Some(&state), None).credential_locations(&[]);
        for hole in [
            state.clone(),
            state.join("daemon/auth"),
            state.join("daemon/auth/x"),
            state.join("scenes/code/settings.json"),
            PathBuf::from("/elsewhere"),
        ] {
            assert!(!fits_a_hole(&state, &locs, &hole), "{}", hole.display());
        }
        for hole in [
            state.join("scenes"),
            state.join("scenes/code"),
            state.join("scenes/code/skills"),
            state.join("daemon"),
        ] {
            assert!(fits_a_hole(&state, &locs, &hole), "{}", hole.display());
        }
        let plan = mask_plan(&state, &locs, &[state.clone(), state.join("daemon/auth")]);
        assert!(
            !plan.iter().any(|m| matches!(m, Mount::Bind(_))),
            "{plan:?}"
        );
    }

    #[test]
    fn an_absent_root_needs_no_mounts() {
        let (_dir, state) = state();
        let missing = state.join("missing");
        let locs = SensitivePaths::new(Some(&missing), None).credential_locations(&[]);
        assert!(mask_plan(&missing, &locs, &[]).is_empty());
    }

    /// The Linux backend masks the state root's credentials and the
    /// project's `.agix` through [`mask_plan`]: every entry of the table
    /// reaches one of them.
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
        let plan = mask_plan(&root, &locs, &[]);
        let agix_plan = mask_plan(&agix, &locs, &[]);

        assert!(!examples.is_empty());
        for (credential, path) in &examples {
            let masked = match credential.scope {
                Scope::AnyAgixDir => !readable(&agix_plan, path),
                _ => !readable(&plan, path),
            };
            assert!(masked, "{credential:?}: {} is readable", path.display());
        }
        assert!(readable(&plan, &root.join("skills/x/SKILL.md")));
    }
}
