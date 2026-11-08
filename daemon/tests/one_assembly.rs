//! There is one place that knows how a daemon is put together.
//!
//! `SessionPool`'s constructor takes eleven positional arguments and is
//! followed by four startup steps a session is silently worse without. A copy
//! of that knowledge is not merely duplication: when the assembly changes,
//! nothing tells the copy, because there is no compile error for a step nobody
//! performs. `daemon::assemble::pool` is the single copy, and this test keeps
//! it single, since a hand-assembled pool would compile, pass, and quietly
//! skip whatever gets added next.
//!
//! Source-text scanning, like `protocol_doc_matches_dispatch`. It cannot tell
//! a good reason from a bad one; it can only say that a second assembly
//! exists, which is the thing that kept happening.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("daemon crate has a parent (repo root)")
        .to_path_buf()
}

/// Files allowed to construct a pool directly.
///
/// `assemble.rs` is the assembly. `session_pool.rs`'s own unit tests are a
/// layer below it — they exercise the type this module assembles, and routing
/// them through the assembly would test the wrong thing.
const MAY_ASSEMBLE: &[&str] = &[
    "crates/assembly/src/assemble.rs",
    "crates/assembly/src/session_pool.rs",
];

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn nothing_assembles_a_session_pool_but_the_assembly() {
    let root = repo_root();
    let mut sources = Vec::new();
    for dir in ["daemon", "crates", "tests"] {
        rust_sources(&root.join(dir), &mut sources);
    }
    assert!(
        sources.len() > 50,
        "the scan found almost nothing to read: {} files",
        sources.len()
    );

    // Spelled in pieces so this file is not its own first offender.
    let needle = format!("SessionPool{}new(", "::");
    let offenders: Vec<String> = sources
        .iter()
        .filter(|path| {
            let rel = path.strip_prefix(&root).unwrap_or(path).to_string_lossy();
            !MAY_ASSEMBLE.iter().any(|allowed| rel == *allowed)
        })
        .filter(|path| {
            std::fs::read_to_string(path)
                .map(|src| src.contains(&needle))
                .unwrap_or(false)
        })
        .map(|path| {
            path.strip_prefix(&root)
                .unwrap_or(path)
                .to_string_lossy()
                .into_owned()
        })
        .collect();

    assert!(
        offenders.is_empty(),
        "these build a pool without `daemon::assemble::pool`: {offenders:?}\n\
         A second assembly is one nobody updates when the first one changes."
    );
}
