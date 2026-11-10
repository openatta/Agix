//! The support library against `tests/guards/fixtures`: one fixture for each
//! way a text scan used to read the workspace wrong, and the workspace itself
//! for the cases that only show at full size.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use guards::allow::{self, Finding};
use guards::scan::{self, Callee};
use guards::source::{load_crate, standalone};
use guards::{modgraph, ratchet, ws, Hit, SourceFile, View};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

fn fixture(name: &str) -> SourceFile {
    let text = std::fs::read_to_string(fixtures().join(name)).unwrap();
    standalone(&format!("fixtures/{name}"), text).unwrap()
}

fn static_names(f: &SourceFile, v: View) -> Vec<String> {
    scan::statics(f, v).into_iter().map(|s| s.name).collect()
}

fn bounds(f: &SourceFile) -> Vec<(usize, usize)> {
    f.test_lines
        .iter()
        .map(|r| (*r.start(), *r.end()))
        .collect()
}

fn hit() -> Hit {
    Hit {
        file: "a.rs".into(),
        line: 1,
        snippet: "x".into(),
    }
}

#[test]
fn a_test_const_does_not_hide_the_production_static_after_it() {
    let f = fixture("cfg_test_const.rs");
    assert_eq!(static_names(&f, View::Prod), ["AFTER_THE_TEST_CONST"]);
    assert_eq!(bounds(&f), [(1, 2)]);
}

#[test]
fn a_test_statement_is_pruned_and_the_rest_of_the_file_kept() {
    let f = fixture("stmt_cfg_test.rs");
    assert_eq!(static_names(&f, View::Prod), ["AFTER_THE_STATEMENT"]);
    assert_eq!(bounds(&f), [(2, 7)]);
    assert_eq!(f.production_lines(), 11 - 6);
}

#[test]
fn only_predicates_that_imply_test_are_test_code() {
    let f = fixture("cfg_predicates.rs");
    assert_eq!(
        static_names(&f, View::Prod),
        ["ANY_TEST_OR_UNIX", "NOT_TEST"]
    );
    assert_eq!(
        static_names(&f, View::Full),
        [
            "IN_UNIX_TESTS",
            "ANY_TEST_OR_UNIX",
            "ANY_ARM_TEST",
            "NOT_TEST",
            "IN_A_BARE_TEST"
        ]
    );
}

#[test]
fn a_test_module_declared_by_file_takes_its_whole_file() {
    let root = fixtures();
    let files = load_crate(&root, &root.join("tree/lib.rs"), "tree").unwrap();
    let by_rel: BTreeMap<&str, &SourceFile> = files.iter().map(|f| (f.rel.as_str(), f)).collect();
    assert_eq!(
        by_rel.keys().copied().collect::<Vec<_>>(),
        [
            "tree/lib.rs",
            "tree/server/a.rs",
            "tree/server/a/b.rs",
            "tree/server/mod.rs",
            "tree/server/rpc.rs",
            "tree/t.rs",
            "tree/tools.rs"
        ]
    );
    assert!(by_rel["tree/t.rs"].test_only);
    assert!(!by_rel["tree/tools.rs"].test_only);
    assert_eq!(by_rel["tree/server/a/b.rs"].module, ["server", "a", "b"]);
}

#[test]
fn a_mod_without_its_file_is_an_error_naming_the_declaration() {
    let root = fixtures();
    let err = load_crate(&root, &root.join("missing_mod/lib.rs"), "missing").unwrap_err();
    assert!(
        err.contains("missing_mod/lib.rs:1") && err.contains("mod gone;"),
        "{err}"
    );
}

#[test]
fn statics_are_found_however_they_are_written() {
    let f = fixture("statics.rs");
    let found = scan::statics(&f, View::Prod);
    let get = |name: &str| found.iter().find(|s| s.name == name).unwrap();

    let registry = get("REGISTRY");
    assert!(
        registry.ty_tokens.contains("LazyLock"),
        "{}",
        registry.ty_tokens
    );
    assert_eq!(registry.hit.line, 1);

    let stack = get("STACK");
    assert!(stack.thread_local && stack.ty_tokens.contains("RefCell"));
    assert_eq!(stack.hit.line, 6);

    assert_eq!(get("NEXT").enclosing, "counter");
    assert!(get("RAW").is_mut);
    assert!(!get("NAME").is_mut && !get("NAME").thread_local);
    assert_eq!(found.len(), 5);
}

#[test]
fn a_hook_event_is_matched_by_whole_segments() {
    let f = fixture("hook_events.rs");
    let fired: Vec<Vec<String>> = scan::calls(&f, View::Prod)
        .into_iter()
        .filter(|c| matches!(&c.callee, Callee::Method(m) if m == "run"))
        .flat_map(|c| c.args)
        .filter_map(|arg| match arg {
            syn::Expr::Path(p) => Some(
                p.path
                    .segments
                    .iter()
                    .map(|s| s.ident.to_string())
                    .collect(),
            ),
            _ => None,
        })
        .collect();
    assert_eq!(
        fired,
        [
            vec!["HookEvent".to_string(), "StopFailure".to_string()],
            vec!["HookEvent".to_string(), "PostToolUseFailure".to_string()],
        ]
    );
    let stop = ["HookEvent".to_string(), "Stop".to_string()];
    let stops: Vec<usize> = scan::expr_paths(&f, View::Prod)
        .into_iter()
        .filter(|(p, _)| p.ends_with(&stop))
        .map(|(_, h)| h.line)
        .collect();
    assert_eq!(
        stops,
        [3],
        "only `has_hooks_for(HookEvent::Stop)` names Stop itself"
    );
}

#[test]
fn a_local_module_shadows_the_dependency_of_the_same_name() {
    let root = fixtures();
    let files = load_crate(&root, &root.join("tree/lib.rs"), "tree").unwrap();
    let lib = files.iter().find(|f| f.rel == "tree/lib.rs").unwrap();
    let refs: Vec<Vec<String>> = modgraph::module_refs(lib, View::Prod)
        .into_iter()
        .map(|(p, _)| p)
        .collect();
    assert!(
        refs.contains(&vec!["tools".into(), "helper".into()]),
        "{refs:?}"
    );
    assert!(
        !refs.iter().any(|p| p.last().is_some_and(|s| s == "Tool")),
        "{refs:?}"
    );
    let paths = scan::paths(lib, View::Prod);
    assert!(paths
        .iter()
        .any(|(p, _)| p == &["::".to_string(), "tools".into(), "Tool".into()]));
}

#[test]
fn a_function_is_measured_from_fn_to_its_closing_brace() {
    let f = fixture("doc_fn.rs");
    let fns = scan::fns(&f, View::Prod);
    assert_eq!(fns[0].qual, "five_lines");
    assert_eq!((fns[0].hit.line, fns[0].lines), (5, 5));
    assert_eq!(fns[1].qual, "Wire::method");
    assert_eq!(fns[1].lines, 1);
}

#[test]
fn use_groups_and_super_chains_resolve_to_crate_modules() {
    let root = fixtures();
    let files = load_crate(&root, &root.join("tree/lib.rs"), "tree").unwrap();
    let lib = files.iter().find(|f| f.rel == "tree/lib.rs").unwrap();
    let b = files
        .iter()
        .find(|f| f.rel == "tree/server/a/b.rs")
        .unwrap();
    let refs: Vec<Vec<String>> = modgraph::module_refs_with_root(b, View::Prod, Some(lib))
        .into_iter()
        .map(|(p, _)| p)
        .collect();
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    assert!(refs.contains(&s(&["server"])), "{refs:?}");
    assert!(refs.contains(&s(&["tools"])), "{refs:?}");
    assert!(refs.contains(&s(&["server", "rpc", "Handler"])), "{refs:?}");
    assert_eq!(
        refs.iter()
            .filter(|p| **p == s(&["server", "rpc", "Handler"]))
            .count(),
        2,
        "`super::super::rpc::Handler` and the root re-export `crate::Handler`: {refs:?}"
    );
}

#[test]
fn macros_attributes_and_aliases_are_scanned() {
    let f = fixture("macros.rs");
    let strings: Vec<String> = scan::string_literals(&f, View::Prod)
        .into_iter()
        .map(|(s, _)| s)
        .collect();
    assert!(
        strings.contains(&"crate::codec::base64".to_string()),
        "{strings:?}"
    );
    assert!(strings.contains(&"k".to_string()), "{strings:?}");

    let produced: Vec<String> = scan::calls(&f, View::Prod)
        .into_iter()
        .filter_map(|c| match c.callee {
            Callee::Path(p) if p.len() == 3 && p[1] == "TelemetryEvent" => Some(p[2].clone()),
            _ => None,
        })
        .collect();
    assert_eq!(produced, ["started", "finished"]);

    let paths: Vec<Vec<String>> = scan::paths(&f, View::Prod)
        .into_iter()
        .map(|(p, _)| p)
        .collect();
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    assert!(
        paths.contains(&s(&["std", "process", "Command", "new"])),
        "{paths:?}"
    );
    assert!(paths.contains(&s(&["serde", "Serialize"])), "{paths:?}");

    assert_eq!(scan::cfg_feature_uses(&f, View::Prod, "mcp").len(), 3);
    assert_eq!(scan::macro_calls(&f, View::Prod, "json").len(), 1);
}

#[test]
fn an_allow_list_passes_when_it_matches_exactly() {
    allow::check(
        "selftest::LIST",
        vec![Finding {
            key: "a".into(),
            hit: hit(),
        }],
        &[("a", "a reason")],
        "nothing",
    );
}

#[test]
#[should_panic(expected = "not on the list")]
fn an_allow_list_reports_what_it_does_not_name() {
    allow::check(
        "selftest::LIST",
        vec![Finding {
            key: "b".into(),
            hit: hit(),
        }],
        &[],
        "list it",
    );
}

#[test]
#[should_panic(expected = "matches nothing any more")]
fn an_allow_list_reports_its_stale_entries() {
    allow::check(
        "selftest::LIST",
        Vec::new(),
        &[("gone", "was here")],
        "drop it",
    );
}

#[test]
#[should_panic(expected = "gives no reason")]
fn an_allow_list_entry_needs_a_reason() {
    allow::check(
        "selftest::LIST",
        vec![Finding {
            key: "a".into(),
            hit: hit(),
        }],
        &[("a", " ")],
        "say why",
    );
}

#[test]
fn a_ratchet_only_goes_down() {
    let dir = std::env::temp_dir().join(format!("agix-guards-ratchet-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let data = dir.join("size.txt");
    std::fs::write(
        &data,
        "# kind path item ceiling\nfn a.rs big 150\nfn a.rs gone 120\n",
    )
    .unwrap();
    let measured = |n: usize| {
        BTreeMap::from([
            ("fn a.rs big".to_string(), n),
            ("fn a.rs small".to_string(), 10),
        ])
    };

    let stale = std::panic::catch_unwind(|| ratchet::check_at(&data, "", measured(140), 100));
    assert!(
        stale.is_err(),
        "the entry for a function that is gone fails"
    );

    ratchet::check_at(&data, "1", measured(140), 100);
    let lowered = std::fs::read_to_string(&data).unwrap();
    assert!(
        lowered.contains("fn  a.rs  big  140") && !lowered.contains("gone"),
        "{lowered}"
    );

    let over = std::panic::catch_unwind(|| ratchet::check_at(&data, "1", measured(160), 100));
    assert!(over.is_err(), "above the ceiling fails, updating or not");
    assert!(
        std::fs::read_to_string(&data).unwrap().contains("140"),
        "an update never raises"
    );

    ratchet::check_at(&data, "seed", measured(130), 100);
    assert!(std::fs::read_to_string(&data).unwrap().contains("130"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_workspace_is_read_through_its_module_trees() {
    let started = std::time::Instant::now();
    let ws = ws();
    let loaded = started.elapsed();

    let production = ws.production().count();
    allow::non_vacuous("support_selftest::production files", production, 300);
    allow::non_vacuous("support_selftest::test files", ws.tests().count(), 100);
    for p in ws.packages() {
        for t in &p.targets {
            assert_eq!(
                ws.file(&t.src).package,
                p.name,
                "{} is read as {}'s",
                t.src,
                p.name
            );
        }
    }
    assert!(ws.file("daemon/tests/integration.rs").target == guards::TargetKind::Test);
    assert!(!ws
        .files()
        .any(|f| f.rel.starts_with(".claude/") || f.rel.starts_with("target/")));

    let started = std::time::Instant::now();
    let statics = guards::par_flat_map(ws.production(), |f| scan::statics(f, View::Prod)).len();
    eprintln!(
        "guards: cargo metadata + {} files parsed in parallel: {loaded:?}; \
         {production} production files parsed again in parallel and scanned: {:?} ({statics} statics)",
        ws.files().count(),
        started.elapsed()
    );
}

#[test]
fn a_test_marker_early_in_a_file_hides_nothing_after_it() {
    let ws = ws();
    let names: Vec<String> = ws
        .production()
        .filter(|f| f.rel == "crates/core/src/frozen/git.rs")
        .flat_map(|f| static_names(f, View::Prod))
        .collect();
    assert!(names.iter().any(|n| n == "WARNED"), "{names:?}");
    let store = ws.file("crates/history/src/store.rs");
    assert!(
        store.production_lines() > 1000,
        "the code after store.rs's statement-level `#[cfg(test)]` is production: {}",
        store.production_lines()
    );
}
