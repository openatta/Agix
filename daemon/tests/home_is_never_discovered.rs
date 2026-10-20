//! No crate may find its own state directory.
//!
//! Where an instance keeps its state is decided once, by the process entry
//! point, and passed down. A module that reads `$HOME` instead has quietly
//! opted out: it writes to the invoking user's home no matter what the
//! instance was configured with, which breaks redirected deployments and is
//! why test runs used to leave files in a real `~/.atta`.
//!
//! Injecting the roots fixed the readers that existed. This test is what
//! keeps the next one from being added — the failure mode is not a bug
//! anyone notices, it is a file appearing somewhere nobody looks.
//!
//! Source-text scanning, not real analysis: it greps for the env lookups
//! that resolve a home directory. That is enough, because the point is to
//! make adding one a deliberate act — you have to come here and write down
//! why.

use std::path::{Path, PathBuf};

/// The lookups that resolve a user's home directory.
const HOME_LOOKUPS: &[&str] = &[
    "var(\"HOME\")",
    "var_os(\"HOME\")",
    "var(\"USERPROFILE\")",
    "var_os(\"USERPROFILE\")",
    "home_dir()",
];

/// Files allowed to resolve a home directory, and why.
///
/// Two things earn a place here. **Entry points**, which is where the
/// decision belongs — exactly one of them. And **facts about the machine**:
/// the sandbox has to know the real home to protect `~/.ssh`, and a system
/// prompt describing the environment is describing the user's environment.
/// Neither is choosing where AttaCore keeps its files.
///
/// A storage root never qualifies. If a new entry is a place to write state,
/// the answer is to take the root as a parameter.
const ALLOWED: &[(&str, &str)] = &[
    (
        "daemon/src/config.rs",
        "the entry point: resolves ATTA_CONFIG_HOME (or $HOME/.atta) once, \
         then passes it down. Everything else receives what this decides.",
    ),
    (
        "crates/core/src/frozen/mod.rs",
        "collects the environment snapshot — home alongside os, shell and \
         git state. A fact about the machine, put into the system prompt, \
         and replaceable by tests because it lives in the snapshot.",
    ),
    (
        "crates/core/src/interface/exec/sandbox.rs",
        "the default deny-read list is ~/.ssh and friends, so building it \
         needs this machine's home. It is policy, which is why it sits with \
         the contract rather than with a backend.",
    ),
    (
        "crates/core/src/interface/exec/local/sandbox.rs",
        "a backend confines a process on this machine, and the state root it \
         protects falls back to $HOME/.atta when the policy does not name one. \
         Where AttaCore's own settings.json lives comes from \
         `SandboxPolicy::state_root` first.",
    ),
    (
        "crates/permissions/src/path_rule.rs",
        "expands `~` in permission rules and in the paths tools are asked to \
         touch — the home a shell expands `~` to when the command runs. A fact \
         about the machine, never a place AttaCore writes.",
    ),
    (
        "crates/tools/src/bash.rs",
        "same deny-list, reused by the command classifier so `cat ~/.ssh/id_rsa` \
         is not waved through as a read-only command.",
    ),
];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("daemon crate has a parent (repo root)")
        .to_path_buf()
}

fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            rs_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn only_the_entry_point_and_environment_facts_resolve_a_home_directory() {
    let root = repo_root();
    let mut files = Vec::new();
    for dir in ["crates", "daemon", "tests"] {
        rs_files(&root.join(dir), &mut files);
    }
    assert!(!files.is_empty(), "found no sources to scan");

    let mut offenders: Vec<String> = Vec::new();
    for file in &files {
        let rel = file
            .strip_prefix(&root)
            .unwrap_or(file)
            .to_string_lossy()
            .replace('\\', "/");
        if ALLOWED.iter().any(|(allowed, _)| *allowed == rel) {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(file) else {
            continue;
        };
        for (n, line) in content.lines().enumerate() {
            // The list in this file is the list, not a use of it.
            if rel == "daemon/tests/home_is_never_discovered.rs" {
                continue;
            }
            if HOME_LOOKUPS.iter().any(|needle| line.contains(needle)) {
                offenders.push(format!("{rel}:{}: {}", n + 1, line.trim()));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "these resolve a home directory instead of being told where to write:\n  {}\n\n\
         Take the root as a parameter — see `base::paths::ConfigPaths`. If this really is \
         a fact about the machine rather than a place to store things, add the file to \
         ALLOWED in {} with the reason.",
        offenders.join("\n  "),
        file!(),
    );
}

/// The allow-list is a statement about specific files; a stale entry makes it
/// a weaker statement than it looks.
#[test]
fn every_allowed_file_exists_and_still_resolves_a_home() {
    let root = repo_root();
    for (rel, reason) in ALLOWED {
        let path = root.join(rel);
        assert!(
            path.exists(),
            "{rel} is on the allow-list but does not exist"
        );
        assert!(!reason.trim().is_empty(), "{rel} needs a reason");

        let content = std::fs::read_to_string(&path).expect("allowed file is readable");
        assert!(
            HOME_LOOKUPS.iter().any(|needle| content.contains(needle)),
            "{rel} no longer resolves a home directory — drop it from ALLOWED"
        );
    }
}
