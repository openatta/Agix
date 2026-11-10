//! Ceilings that only go down.
//!
//! A data file lists the items allowed above the default limit, one per line:
//! whitespace-separated key columns, then the ceiling. Anything measured above
//! its ceiling (or above the default when unlisted) fails, and so does an entry
//! whose ceiling is not above the default or whose item is gone.
//!
//! `AGIX_UPDATE_RATCHET=1` lowers every ceiling to what is measured and drops
//! stale entries, never raising one; `AGIX_UPDATE_RATCHET=seed` writes the file
//! from scratch. Both are environment switches rather than `#[ignore]` tests
//! because CI runs `-- --ignored`, which would rewrite the files there.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// `data` is relative to `tests/guards`, e.g. `ratchet/size.txt`. A measured
/// key is the key columns joined by single spaces.
pub fn check(data: &str, measured: BTreeMap<String, usize>, default_limit: usize) {
    check_by(data, measured, |_| default_limit);
}

/// [`check`] for a file whose default limit depends on the key, such as one
/// limit for functions and another for files.
pub fn check_by(data: &str, measured: BTreeMap<String, usize>, limit_of: impl Fn(&str) -> usize) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(data);
    let mode = std::env::var("AGIX_UPDATE_RATCHET").unwrap_or_default();
    check_at_by(&path, &mode, measured, limit_of);
}

pub fn check_at(path: &Path, mode: &str, measured: BTreeMap<String, usize>, default_limit: usize) {
    check_at_by(path, mode, measured, |_| default_limit);
}

pub fn check_at_by(
    path: &Path,
    mode: &str,
    measured: BTreeMap<String, usize>,
    limit_of: impl Fn(&str) -> usize,
) {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let (header, mut entries) = parse(&text, path);
    match mode {
        "1" => {
            entries = entries
                .into_iter()
                .filter_map(|(k, ceiling)| {
                    let now = *measured.get(&k)?;
                    let lowered = now.min(ceiling);
                    (lowered > limit_of(&k)).then_some((k, lowered))
                })
                .collect();
            write(path, &header, &entries);
        }
        "seed" => {
            entries = measured
                .iter()
                .filter(|(k, n)| **n > limit_of(k))
                .map(|(k, n)| (k.clone(), *n))
                .collect();
            write(path, &header, &entries);
        }
        _ => {}
    }

    let shown = shown(path);
    let mut problems = Vec::new();
    for (key, n) in &measured {
        let ceiling = entries.get(key).copied().unwrap_or_else(|| limit_of(key));
        if *n > ceiling {
            problems.push(format!(
                "{key}: {n}, above its ceiling {ceiling}. Split it; or raise the ceiling in \
                 {shown} in the same commit and say why in the commit message"
            ));
        } else if *n < ceiling && entries.contains_key(key) {
            eprintln!("note: {key} is at {n}, under its ceiling {ceiling}; AGIX_UPDATE_RATCHET=1 lowers it");
        }
    }
    for (key, ceiling) in &entries {
        let default_limit = limit_of(key);
        if *ceiling <= default_limit {
            problems.push(format!(
                "{key}: the ceiling {ceiling} is not above the default {default_limit}; delete the entry from {shown}"
            ));
        }
        if !measured.contains_key(key) {
            problems.push(format!(
                "{key}: no such item any more; delete the entry from {shown}"
            ));
        }
    }
    assert!(
        problems.is_empty(),
        "{} ratchet problem(s):\n{}",
        problems.len(),
        problems
            .iter()
            .map(|p| format!("  - {p}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

fn shown(path: &Path) -> String {
    let guards = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    match path.strip_prefix(&guards) {
        Ok(rel) => format!("tests/guards/{}", rel.display()),
        Err(_) => path.display().to_string(),
    }
}

fn parse(text: &str, path: &Path) -> (Vec<String>, BTreeMap<String, usize>) {
    let mut header = Vec::new();
    let mut entries = BTreeMap::new();
    for (i, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with('#') {
            header.push(trimmed.to_string());
            continue;
        }
        let cols: Vec<&str> = trimmed.split_whitespace().collect();
        let parsed = cols.split_last().and_then(|(last, keys)| {
            let n = last.parse::<usize>().ok()?;
            (!keys.is_empty()).then(|| (keys.join(" "), n))
        });
        let Some((key, n)) = parsed else {
            panic!(
                "{}:{}: not `<key columns> <ceiling>`: {trimmed}",
                path.display(),
                i + 1
            );
        };
        assert!(
            entries.insert(key.clone(), n).is_none(),
            "{}:{}: `{key}` is listed twice",
            path.display(),
            i + 1
        );
    }
    (header, entries)
}

fn write(path: &Path, header: &[String], entries: &BTreeMap<String, usize>) {
    let rows: Vec<Vec<String>> = entries
        .iter()
        .map(|(k, n)| {
            let mut cols: Vec<String> = k.split(' ').map(str::to_string).collect();
            cols.push(n.to_string());
            cols
        })
        .collect();
    let width = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut widths = vec![0; width];
    for row in &rows {
        for (i, col) in row.iter().enumerate() {
            widths[i] = widths[i].max(col.len());
        }
    }
    let mut out = String::new();
    for line in header {
        out.push_str(line);
        out.push('\n');
    }
    for row in &rows {
        let line: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(i, col)| format!("{col:<w$}", w = widths[i]))
            .collect();
        out.push_str(line.join("  ").trim_end());
        out.push('\n');
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    std::fs::write(path, out).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
}
