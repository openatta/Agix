//! Every telemetry event kind has a producer: some non-test code outside this
//! crate constructs it. A kind nothing emits reads, to whoever configures
//! `event_toggles` or counts the kinds in the README, as something they will
//! see; it is added together with the place that sends it, or not at all.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crates/telemetry sits two levels below the workspace root")
        .to_path_buf()
}

fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if !matches!(name.as_ref(), "tests" | "target" | "benches" | "examples") {
                sources(&path, out);
            }
        } else if name.ends_with(".rs") {
            out.push(path);
        }
    }
}

/// The file's text up to its first test module.
fn production_code(path: &Path) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    match text.find("#[cfg(test)]") {
        Some(at) => text[..at].to_string(),
        None => text,
    }
}

#[test]
fn every_event_kind_has_a_producer() {
    let root = workspace_root();
    let mut files = Vec::new();
    for dir in ["crates", "daemon"] {
        sources(&root.join(dir), &mut files);
    }
    let telemetry_crate = root.join("crates").join("telemetry");
    let code: Vec<String> = files
        .iter()
        .filter(|f| !f.starts_with(&telemetry_crate))
        .map(|f| production_code(f))
        .collect();
    assert!(code.len() > 50, "found only {} source files", code.len());

    let orphans: Vec<&str> = telemetry::EventPayload::KINDS
        .iter()
        .copied()
        .filter(|kind| {
            let constructor = format!("TelemetryEvent::{kind}(");
            !code.iter().any(|text| text.contains(&constructor))
        })
        .collect();
    assert!(
        orphans.is_empty(),
        "event kinds nothing emits: {orphans:?} — emit them where they happen, or delete them"
    );
}

const BEGIN: &str =
    "<!-- BEGIN GENERATED TABLE — see telemetry::events::inventory::render_markdown -->";
const END: &str = "<!-- END GENERATED TABLE -->";

/// The inventory in `docs/telemetry.md` is rendered from the code, not
/// written. `AGIX_UPDATE_DOCS=1` rewrites it; read the diff before committing.
#[test]
fn the_documented_inventory_is_the_code() {
    let path = workspace_root().join("docs").join("telemetry.md");
    let doc = std::fs::read_to_string(&path).expect("docs/telemetry.md exists");
    let generated = telemetry::events::inventory::render_markdown();

    let start = doc.find(BEGIN).expect("the generated-table begin marker");
    let end = doc.find(END).expect("the generated-table end marker");
    assert!(start < end, "the markers are in the wrong order");
    let current = doc[start + BEGIN.len()..end].trim();
    if current == generated.trim() {
        return;
    }

    if std::env::var("AGIX_UPDATE_DOCS").is_ok() {
        let rewritten = format!(
            "{}{}\n{}\n{}",
            &doc[..start],
            BEGIN,
            generated.trim(),
            &doc[end..]
        );
        std::fs::write(&path, rewritten).expect("rewrite docs/telemetry.md");
        return;
    }

    panic!(
        "docs/telemetry.md is out of date with telemetry::EventPayload.\n\
         Re-run with AGIX_UPDATE_DOCS=1 and read the diff.\n\n\
         expected:\n{generated}\n\nfound:\n{current}\n"
    );
}
