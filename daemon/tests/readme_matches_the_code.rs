//! The countable claims of both READMEs (`README.md` and
//! `docs/README.zh-CN.md`) are counted from the code.
//!
//! The README is the one document every reader starts at, and a count in it
//! goes stale the moment a method, crate or tool is added. Only the numbers a
//! reader would act on are here, plus the tool table's names. Prose is not
//! checkable and pretending otherwise would make this test a formatting gate.
//!
//! Source-text scanning where a runtime value is not reachable from `daemon`'s
//! dependency graph, same as the sibling doc tests.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("daemon crate has a parent (repo root)")
        .to_path_buf()
}

fn read(rel: &str) -> String {
    let path = repo_root().join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{} is readable: {e}", path.display()))
}

const README: &str = "README.md";
const README_ZH: &str = "docs/README.zh-CN.md";

/// A README with every run of whitespace collapsed, so a claim can be
/// asserted without knowing where the paragraph happened to wrap.
fn collapsed(rel: &str) -> String {
    read(rel).split_whitespace().collect::<Vec<_>>().join(" ")
}

fn readme() -> String {
    collapsed(README)
}

fn readme_zh() -> String {
    collapsed(README_ZH)
}

fn claims(text: &str, claim: &str, derived_from: &str) {
    assert!(
        text.contains(claim),
        "a README does not say \"{claim}\".\n\
         That number comes from {derived_from} — either the README is stale or \
         this test's source is."
    );
}

/// Numbers the README spells out, because prose reads better that way.
fn spelled(n: usize) -> String {
    const TENS: [&str; 10] = [
        "", "", "Twenty", "Thirty", "Forty", "Fifty", "Sixty", "Seventy", "Eighty", "Ninety",
    ];
    const ONES: [&str; 10] = [
        "", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine",
    ];
    assert!((20..100).contains(&n), "no spelling for {n}");
    match n % 10 {
        0 => TENS[n / 10].to_string(),
        d => format!("{}-{}", TENS[n / 10], ONES[d]),
    }
}

/// The same numbers in Chinese numerals, for the zh-CN README's prose.
fn spelled_zh(n: usize) -> String {
    const DIGITS: [&str; 10] = ["", "一", "二", "三", "四", "五", "六", "七", "八", "九"];
    assert!((20..100).contains(&n), "no spelling for {n}");
    format!("{}十{}", DIGITS[n / 10], DIGITS[n % 10])
}

#[test]
fn the_workspace_is_the_size_the_readme_says() {
    let members: Vec<&str> = guards::ws()
        .packages()
        .iter()
        .map(|p| p.dir.as_str())
        .collect();

    let crates = members.iter().filter(|m| m.starts_with("crates/")).count();
    let readme = readme();

    claims(
        &readme,
        &format!("{crates} crates under `crates/`"),
        "the workspace member list",
    );
    claims(
        &readme,
        &format!("{} workspace members in total", members.len()),
        "the workspace member list",
    );

    let zh = readme_zh();
    claims(
        &zh,
        &format!("`crates/` 下 {crates} 个 crate"),
        "the workspace member list",
    );
    claims(
        &zh,
        &format!("工作区共 {} 个成员", members.len()),
        "the workspace member list",
    );
}

#[test]
fn the_method_count_is_the_documented_method_count() {
    // The set itself is tied to `dispatch` by `protocol_doc_matches_dispatch`,
    // so counting the protocol document here cannot drift from the daemon
    // without that test failing first.
    let doc = read("docs/daemon_rpc_protocol.md");
    let mut methods: Vec<String> = doc
        .lines()
        .filter(|line| line.starts_with("#### "))
        .flat_map(|line| {
            line.split('`')
                .skip(1)
                .step_by(2)
                .map(str::trim)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .filter(|name| name.contains('.'))
        .collect();
    methods.sort();
    methods.dedup();

    claims(
        &readme(),
        &format!("{} methods are documented", spelled(methods.len())),
        "the method headings in docs/daemon_rpc_protocol.md §6",
    );
    claims(
        &readme_zh(),
        &format!("{}个方法", spelled_zh(methods.len())),
        "the method headings in docs/daemon_rpc_protocol.md §6",
    );
}

#[test]
fn the_extension_surface_is_the_size_the_readme_says() {
    let points = base::interface::catalog::all().len();
    claims(
        &readme(),
        &format!("{points} extension points"),
        "base::interface::catalog",
    );
    claims(
        &readme_zh(),
        &format!("{points} 个扩展点"),
        "base::interface::catalog",
    );
}

#[test]
fn the_hook_and_telemetry_counts_are_the_enums() {
    let hook_events = guards::scan::enum_variants(guards::ws(), "hooks", "HookEvent").len();
    let event_types = telemetry::EventPayload::KINDS.len();
    let readme = readme();

    claims(
        &readme,
        &format!("`HookEvent` ({hook_events})"),
        "hooks::HookEvent",
    );
    claims(
        &readme,
        &format!("{} named moments", spelled(hook_events)),
        "hooks::HookEvent",
    );
    claims(
        &readme,
        &format!("`EventPayload` ({event_types})"),
        "telemetry::EventPayload",
    );
    claims(
        &readme,
        &format!("{event_types} structured event types"),
        "telemetry::EventPayload",
    );

    let zh = readme_zh();
    claims(
        &zh,
        &format!("{}个时点", spelled_zh(hook_events)),
        "hooks::HookEvent",
    );
    claims(
        &zh,
        &format!("{event_types} 种结构化事件"),
        "telemetry::EventPayload",
    );
}

#[test]
fn the_self_contained_tool_count_is_what_registration_installs() {
    let reg = std::sync::Arc::new(base::tool::InMemoryToolRegistry::new());
    tools::register_builtin_tools(&reg);

    claims(
        &readme(),
        &format!("installs the {} self-contained ones", reg.len()),
        "tools::register_builtin_tools",
    );
    claims(
        &readme_zh(),
        &format!("装上其中 {} 个自给自足的", reg.len()),
        "tools::register_builtin_tools",
    );
}

/// Every tool a README's table names is a tool that exists.
///
/// The table is the README's longest list of claims and the one most likely to
/// outlive a rename, which a count alone would not catch.
#[test]
fn every_tool_in_the_table_exists() {
    let real = tool_names_in_source();
    for (rel, header, count_claim) in [
        (README, "| Category | Tools |", "Tools — {} built in"),
        (README_ZH, "| 类别 | 工具 |", "工具——{} 个内建"),
    ] {
        let source = read(rel);
        let table = source
            .split_once(header)
            .unwrap_or_else(|| panic!("{rel}'s tool table still has its header"))
            .1
            .split_once("\n\n")
            .expect("the tool table ends at a blank line")
            .0;

        let named: Vec<&str> = table
            .split('`')
            .skip(1)
            .step_by(2)
            // `mcp__<server>__<tool>` is a shape, not a name.
            .filter(|n| !n.contains('<'))
            .collect();

        let missing: Vec<&&str> = named
            .iter()
            .filter(|n| !real.contains(&n.to_string()))
            .collect();
        assert!(
            missing.is_empty(),
            "{rel}'s tool table names tools nothing implements: {missing:?}\n\
             A tool a reader cannot call is worse than one that was never advertised."
        );

        claims(
            &collapsed(rel),
            &count_claim.replace("{}", &named.len().to_string()),
            "the rows of that README's own tool table",
        );
    }
}

/// What each `impl Tool for X`'s `fn name` returns, in production code: a
/// literal, or a constant holding one.
fn tool_names_in_source() -> Vec<String> {
    use guards::scan::{self, Returned};
    let consts: std::collections::BTreeMap<String, String> =
        guards::par_flat_map(guards::ws().production(), |f| {
            scan::str_consts(f, guards::View::Prod)
        })
        .into_iter()
        .collect();
    guards::par_flat_map(guards::ws().production(), |f| {
        scan::trait_fn_returns(f, guards::View::Prod, "Tool", "name")
    })
    .into_iter()
    .filter_map(|r| match r {
        Returned::Literal(name) => Some(name),
        Returned::Const(id) => consts.get(&id).cloned(),
    })
    .collect()
}
