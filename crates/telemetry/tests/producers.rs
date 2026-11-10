//! The inventory in `docs/telemetry.md` is the code. That every kind has a
//! producer is checked in `tests/guards/tests/wiring.rs`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crates/telemetry sits two levels below the workspace root")
        .to_path_buf()
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
