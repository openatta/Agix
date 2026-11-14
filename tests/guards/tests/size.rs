//! Production functions and files may not grow past a limit (SZ-1..3).
//!
//! A file counts its production lines (the physical lines minus the ranges
//! `cfg(test)` removes); a function counts from the line of `fn` to its closing
//! brace, a closure belonging to its enclosing function and a nested `fn`
//! counted by itself. Code already above the limit is listed in
//! `ratchet/size.txt` at its current size and may only shrink; anything new has
//! to fit. Test files are not limited.

use std::collections::BTreeMap;

use guards::{allow, par_flat_map, ratchet, scan, ws, View};

const FN_MAX: usize = 80;
const FILE_MAX: usize = 800;
const DATA: &str = "ratchet/size.txt";

const MEASURED_DIRS: &[&str] = &["crates/", "daemon/", "examples/"];

fn measured() -> BTreeMap<String, usize> {
    let files = ws().production().filter(|f| {
        let dir = format!("{}/", ws().package(&f.package).dir);
        MEASURED_DIRS.iter().any(|d| dir.starts_with(d))
    });
    par_flat_map(files, |f| {
        let mut out = vec![(format!("file {} -", f.rel), f.production_lines())];
        let mut seen: BTreeMap<String, usize> = BTreeMap::new();
        for func in scan::fns(f, View::Prod) {
            let n = seen.entry(func.qual.clone()).or_default();
            *n += 1;
            let item = if *n == 1 {
                func.qual.clone()
            } else {
                format!("{}#{n}", func.qual)
            };
            out.push((format!("fn {} {item}", f.rel), func.lines));
        }
        out
    })
    .into_iter()
    .collect()
}

#[test]
fn production_functions_and_files_stay_under_their_ceilings() {
    let all = measured();
    allow::non_vacuous("size::items", all.len(), 3000);
    ratchet::check_by(DATA, all, |key| {
        if key.starts_with("file ") {
            FILE_MAX
        } else {
            FN_MAX
        }
    });
}
