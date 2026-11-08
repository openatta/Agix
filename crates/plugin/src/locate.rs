//! Finding the companion files a build ships *beside* its executable rather
//! than inside it, such as the DSH bridge module.

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
