//! Keeps the extension-point index's reference table generated rather than
//! written.
//!
//! A hand-maintained table of what can be extended is a table that is wrong
//! within two work orders — a point gets added and nobody remembers the
//! document, or a point's trust rules change and the row stays. So the table
//! in `docs/extension_points.md` is rendered from `base::interface::catalog`,
//! and this fails if the file and the catalog disagree.
//!
//! Set `AGIX_UPDATE_DOCS=1` to rewrite the file from the catalog, and read the
//! diff before committing it — a generated table regenerated reflexively is a
//! slower way of having a wrong one.

use std::path::PathBuf;

const BEGIN: &str =
    "<!-- BEGIN GENERATED TABLE — see base::interface::catalog::render_markdown -->";
const END: &str = "<!-- END GENERATED TABLE -->";

fn doc_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("daemon/ has a parent")
        .join("docs/extension_points.md")
}

#[test]
fn the_reference_table_matches_the_catalog() {
    let path = doc_path();
    let doc = std::fs::read_to_string(&path).expect("the extension point index exists");
    let generated = base::interface::catalog::render_markdown();

    let start = doc.find(BEGIN).expect("the generated-table begin marker");
    let end = doc.find(END).expect("the generated-table end marker");
    assert!(start < end, "the markers are in the wrong order");
    let current = doc[start + BEGIN.len()..end].trim();

    if current == generated.trim() {
        return;
    }

    if std::env::var("AGIX_UPDATE_DOCS").is_ok() {
        let rewritten = format!(
            "{}{}\n\n{}\n{}",
            &doc[..start],
            BEGIN,
            generated.trim(),
            &doc[end..]
        );
        std::fs::write(&path, rewritten).expect("rewrite the index");
        return;
    }

    panic!(
        "docs/extension_points.md is out of date with base::interface::catalog.\n\
         Re-run with AGIX_UPDATE_DOCS=1 and read the diff.\n\n\
         expected:\n{generated}\n\nfound:\n{current}\n"
    );
}

/// Every point the catalog names has a section in the index, so a reader who
/// finds a row can find out how to use it.
#[test]
fn every_point_in_the_table_is_written_about_somewhere_in_the_document() {
    let doc = std::fs::read_to_string(doc_path()).expect("the extension point index exists");
    // The prose, with the generated table cut out. Counting mentions across
    // the whole file would make this test depend on whether the sibling above
    // has rewritten the table yet, which under `AGIX_UPDATE_DOCS=1` is a race
    // between two tests in the same binary.
    let prose = match (doc.find(BEGIN), doc.find(END)) {
        (Some(start), Some(end)) if start < end => {
            format!("{}{}", &doc[..start], &doc[end + END.len()..])
        }
        _ => doc.clone(),
    };
    for point in base::interface::catalog::all() {
        assert!(
            prose.contains(&format!("`{}`", point.id)),
            "`{}` appears in the generated table and nowhere else — a row a reader \
             cannot act on is worse than no row",
            point.id
        );
    }
}

/// The module each point claims to be defined in has to exist. A `defined_in`
/// pointing at a path that moved is the failure this index exists to prevent.
#[test]
fn every_point_names_a_module_that_exists() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("daemon/ has a parent")
        .to_path_buf();

    let crate_dir = |name: &str| -> Option<PathBuf> {
        let dir = match name {
            "base" => root.join("crates/core/src"),
            "history" => root.join("crates/history/src"),
            "compaction" => root.join("crates/compaction/src"),
            "hooks" => root.join("crates/hooks/src"),
            _ => return None,
        };
        Some(dir)
    };

    for point in base::interface::catalog::all() {
        let mut segments = point.defined_in.split("::");
        let krate = segments.next().expect("a crate name");
        let Some(dir) = crate_dir(krate) else {
            panic!(
                "`{}` names crate `{krate}`, which this test does not know how to \
                   locate — teach it, or fix the path",
                point.id
            );
        };
        // Walk down to the last segment that names a module rather than a
        // type: modules are lowercase, types are not.
        let module_path: Vec<&str> = segments
            .take_while(|s| s.chars().next().is_some_and(|c| c.is_lowercase()))
            .collect();
        assert!(
            !module_path.is_empty(),
            "`{}` has no module path in `{}`",
            point.id,
            point.defined_in
        );
        let as_file = dir.join(format!("{}.rs", module_path.join("/")));
        let as_dir = dir.join(module_path.join("/")).join("mod.rs");
        assert!(
            as_file.exists() || as_dir.exists(),
            "`{}` says it is defined in `{}`, but neither {} nor {} exists",
            point.id,
            point.defined_in,
            as_file.display(),
            as_dir.display()
        );
    }
}

/// The carrier's list and the document's table say the same thing.
///
/// These drift the way every pair of hand-maintained lists drifts, and the
/// consequence is specific: the table is what a script author reads to decide
/// what to write, so a point missing from it is a capability nobody uses, and
/// a point in it that the carrier cannot bind is a startup error somebody hits
/// after writing the script.
#[cfg(feature = "scripts")]
#[test]
fn the_bindable_points_and_the_table_that_lists_them_agree() {
    let doc = std::fs::read_to_string(doc_path()).expect("the extension point index exists");
    let section = doc
        .split("## 一个脚本能绑到哪些点上")
        .nth(1)
        .expect("the section listing what a script can be bound to")
        .split("### 保持关闭的那四个")
        .next()
        .expect("the closed-points subsection ends it");

    for point in script_host::bindings::BINDABLE_POINTS {
        assert!(
            section.contains(&format!("`{point}`")),
            "`{point}` can be bound and the table does not say so"
        );
    }

    let listed = section
        .lines()
        .filter(|l| l.starts_with("| `"))
        .filter_map(|l| l.split('`').nth(1))
        .count();
    assert_eq!(
        listed,
        script_host::bindings::BINDABLE_POINTS.len(),
        "the table lists {listed} points and the carrier binds {}",
        script_host::bindings::BINDABLE_POINTS.len()
    );
}
