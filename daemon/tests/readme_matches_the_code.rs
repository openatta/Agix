//! The README's countable claims are counted from the code.
//!
//! Four documents in this repository are already compared against the code by
//! a test, and the README — the one document every reader starts at — was not
//! one of them. It says "nothing is claimed that is not checked, including by
//! these documents" while being the exception, and it had drifted in exactly
//! the way that invites: a method count from before two methods were added, a
//! workspace member count from before the harness crate, a builder argument
//! described as concrete a week after it became a trait object.
//!
//! Only the numbers a reader would act on are here. Prose is not checkable and
//! pretending otherwise would make this test a formatting gate.
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

/// The README with every run of whitespace collapsed, so a claim can be
/// asserted without knowing where the paragraph happened to wrap.
fn readme() -> String {
    read("README.md")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn claims(text: &str, claim: &str, derived_from: &str) {
    assert!(
        text.contains(claim),
        "the README does not say \"{claim}\".\n\
         That number comes from {derived_from} — either the README is stale or \
         this test's source is."
    );
}

/// Variant names of `pub enum <name>`, by source scan.
fn variants(src: &str, name: &str) -> Vec<String> {
    let head = format!("pub enum {name} {{");
    let body = src
        .split_once(&head)
        .unwrap_or_else(|| panic!("`{head}` still exists"))
        .1;
    let body = body.split_once("\n}").expect("the enum closes").0;

    body.lines()
        .filter_map(|line| {
            let indent = line.len() - line.trim_start().len();
            if indent != 4 {
                return None;
            }
            let trimmed = line.trim();
            let name: String = trimmed
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .collect();
            let rest = &trimmed[name.len()..];
            let starts_upper = name.chars().next().is_some_and(|c| c.is_ascii_uppercase());
            let ends_a_variant =
                rest.starts_with(',') || rest.starts_with(" {") || rest.starts_with('(');
            (starts_upper && ends_a_variant).then_some(name)
        })
        .collect()
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

#[test]
fn the_workspace_is_the_size_the_readme_says() {
    let manifest = read("Cargo.toml");
    let members: Vec<&str> = manifest
        .split_once("members = [")
        .expect("the workspace still lists members")
        .1
        .split_once(']')
        .expect("the member list closes")
        .0
        .lines()
        .filter_map(|l| l.trim().strip_prefix('"'))
        .filter_map(|l| l.split('"').next())
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
}

#[test]
fn the_extension_surface_is_the_size_the_readme_says() {
    claims(
        &readme(),
        &format!("{} extension points", base::interface::catalog::all().len()),
        "base::interface::catalog",
    );
}

#[test]
fn the_hook_and_telemetry_counts_are_the_enums() {
    let hook_events = variants(&read("crates/hooks/src/config.rs"), "HookEvent").len();
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
}

#[test]
fn the_self_contained_tool_count_is_what_registration_installs() {
    let reg = std::sync::Arc::new(base::tool::InMemoryToolRegistry::new());
    tools::register_builtin_tools(&reg);

    claims(
        &readme(),
        &format!("installs the {} self-contained ones", reg.all().len()),
        "tools::register_builtin_tools",
    );
}

/// Every tool the README's table names is a tool that exists.
///
/// The table is the README's longest list of claims and the one most likely to
/// outlive a rename, which a count alone would not catch.
#[test]
fn every_tool_in_the_table_exists() {
    let readme_src = read("README.md");
    let table = readme_src
        .split_once("| Category | Tools |")
        .expect("the tool table still has its header")
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

    let real = tool_names_in_source();
    let missing: Vec<&&str> = named
        .iter()
        .filter(|n| !real.contains(&n.to_string()))
        .collect();
    assert!(
        missing.is_empty(),
        "the README's tool table names tools nothing implements: {missing:?}\n\
         A tool a reader cannot call is worse than one that was never advertised."
    );

    claims(
        &readme(),
        &format!("Tools — {} built in", named.len()),
        "the rows of the README's own tool table",
    );
}

/// Names from every `fn name(&self) -> &str { "…" }` in the workspace.
fn tool_names_in_source() -> Vec<String> {
    const SIGNATURE: &str = "fn name(&self) -> &str {";
    let mut out = Vec::new();
    let mut stack = vec![repo_root().join("crates")];

    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let Ok(src) = std::fs::read_to_string(&path) else {
                    continue;
                };
                for hit in src.split(SIGNATURE).skip(1) {
                    let Some(open) = hit.find('"') else { continue };
                    // Only a literal on the same or next line is the name; a
                    // body that computes one has no name to collect here.
                    if hit[..open].contains('}') {
                        continue;
                    }
                    if let Some(name) = hit[open + 1..].split('"').next() {
                        out.push(name.to_string());
                    }
                }
            }
        }
    }
    out
}
