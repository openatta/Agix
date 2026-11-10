//! Keeps the event table in `docs/hooks.md` generated from `hooks::catalog`
//! rather than written, the way `extension_points_doc.rs` keeps the
//! extension-point index.
//!
//! Set `AGIX_UPDATE_DOCS=1` to rewrite the table from the catalog, and read
//! the diff before committing it.

use std::path::{Path, PathBuf};

const BEGIN: &str = "<!-- BEGIN GENERATED TABLE — see hooks::catalog::render_markdown -->";
const END: &str = "<!-- END GENERATED TABLE -->";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("daemon/ has a parent")
        .to_path_buf()
}

#[test]
fn the_event_table_matches_the_catalog() {
    let path = repo_root().join("docs/hooks.md");
    let doc = std::fs::read_to_string(&path).expect("docs/hooks.md exists");
    let generated = hooks::catalog::render_markdown();

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
        std::fs::write(&path, rewritten).expect("rewrite docs/hooks.md");
        return;
    }

    panic!(
        "docs/hooks.md is out of date with hooks::catalog.\n\
         Re-run with AGIX_UPDATE_DOCS=1 and read the diff.\n\n\
         expected:\n{generated}\n\nfound:\n{current}\n"
    );
}

/// `HookEvent::ALL` is typed out; the enum is the authority. A variant
/// missing from the list would be missing from the table without anything
/// failing to compile.
#[test]
fn the_catalog_lists_every_variant_in_declaration_order() {
    let declared = guards::scan::enum_variants(guards::ws(), "hooks", "HookEvent");
    let listed: Vec<String> = hooks::HookEvent::ALL.iter().map(|e| e.name()).collect();
    assert_eq!(listed, declared);
}
