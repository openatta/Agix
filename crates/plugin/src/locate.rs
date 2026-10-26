//! Finding the companion files a build ships *beside* its executable rather
//! than inside it — the DSH bridge module, the out-of-process component
//! compiler.
//!
//! Lives in this crate rather than in `plugin-host` because the package layer
//! needs the same search and must stay compilable without a WebAssembly
//! runtime; two copies of "walk up from the executable" would drift the day
//! one of them learned about a new install layout.

use std::path::{Path, PathBuf};

/// Look for a sibling file at each of `relatives`, walking up from the
/// running executable.
///
/// Walking rather than counting `..`s because the executable's depth is not
/// fixed: `target/debug/`, `target/debug/deps/` under test, and an installed
/// `bin/` are all different distances from the same file. Bounded, because
/// an unbounded walk eventually inspects `/`.
pub fn search_near_executable(relatives: &[&str]) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let mut dir: &Path = exe.parent()?;
    for _ in 0..4 {
        for rel in relatives {
            let candidate = dir.join(rel);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        dir = dir.parent()?;
    }
    None
}

/// Find a companion executable this build needs but does not contain.
///
/// The environment variable is named rather than derived from `name`: every
/// one of these tools is already called `agix-something`, so deriving it
/// produced `AGIX_AGIX_PLUGIN_COMPILE` and nothing anyone was told to set
/// was ever read.
pub fn locate_tool(name: &str, var: &str) -> Option<PathBuf> {
    if let Ok(p) = std::env::var(var) {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    search_near_executable(&[name])
}
