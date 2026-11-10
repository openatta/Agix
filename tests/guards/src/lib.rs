//! The shared support for the architecture guards in `tests/guards/tests`.
//!
//! Every guard reads the same picture of the workspace: `cargo metadata` for
//! packages, targets, dependency edges and features, and each target's module
//! tree walked from its root file along `mod` declarations — so only files the
//! compiler would read are scanned, and test code is told apart by `cfg(test)`
//! in the syntax tree rather than by where a file sits. The scanners in
//! [`scan`] and [`modgraph`] answer "where is X referenced" and "what does this
//! item look like" over that picture; [`allow`] and [`ratchet`] turn findings
//! into failures that say what to change and where the list is.

pub mod allow;
pub mod cfg;
pub mod fswalk;
pub mod modgraph;
pub mod ratchet;
pub mod scan;
pub mod source;
pub mod workspace;

pub use source::{SourceFile, View};
pub use workspace::{ws, Dep, DepKind, Package, Target, TargetKind, Workspace};

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Hit {
    pub file: String,
    pub line: usize,
    pub snippet: String,
}

impl std::fmt::Display for Hit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}: {}", self.file, self.line, self.snippet)
    }
}

/// `scan` over `files` split across the machine's threads, results in file
/// order. Each worker parses its own files: syntax trees cannot cross threads,
/// so only what `scan` returns does.
pub fn par_flat_map<'a, T, I, F>(files: I, scan: F) -> Vec<T>
where
    T: Send,
    I: IntoIterator<Item = &'a SourceFile>,
    F: Fn(&SourceFile) -> Vec<T> + Sync,
{
    let files: Vec<&SourceFile> = files.into_iter().collect();
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    let chunk = files.len().div_ceil(threads).max(1);
    let scan = &scan;
    std::thread::scope(|s| {
        let handles: Vec<_> = files
            .chunks(chunk)
            .map(|part| s.spawn(move || part.iter().flat_map(|f| scan(f)).collect::<Vec<T>>()))
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap_or_else(|e| std::panic::resume_unwind(e)))
            .collect()
    })
}
