//! Keeps `hooks::UNWIRED_EVENTS` honest.
//!
//! `HookEvent` has variants that parse, accept configuration, and never fire.
//! They are kept deliberately (see the const's doc comment for why deleting
//! one is worse than it looks), and `build_hook_runner` warns when someone
//! configures a hook for one. That warning is only worth anything if the list
//! it reads from is accurate — and a hand-maintained list of "things that are
//! not wired yet" is exactly the kind of comment that rots the moment someone
//! wires one up and forgets it exists.
//!
//! So the list is not trusted here: this recomputes it from the production
//! sources and fails if the two disagree **in either direction**. Wiring an
//! event without removing it from the const fails. Adding a variant that
//! nothing fires without listing it fails too.
//!
//! Same technique and the same caveat as `wiring_coverage.rs`: this is a
//! source-text scan, not call-graph analysis. It answers "does production
//! composition code name this event", which is the question that matters for
//! this particular bug class, and nothing more.

use std::path::{Path, PathBuf};

/// Sources that count as "production composition" — the places an event is
/// actually dispatched from.
///
/// `crates/hooks/src/runner/` is deliberately **excluded**: it is the
/// dispatcher itself, so every event name appears there by construction, and
/// counting it would mark all 30 as wired. `run_elicitation_result` is the
/// exact case that makes this distinction load-bearing — the wrapper fires
/// `ElicitationResult`, but nothing calls the wrapper.
const PRODUCTION_SOURCES: &[&str] = &[
    "crates/runtime/src",
    "crates/agents/src",
    "crates/mcp/src/module.rs",
    "crates/skills/src/module.rs",
    "crates/hooks/src/watcher.rs",
];

/// Events dispatched through a named wrapper method rather than by naming the
/// variant at the call site. Without these, an event fired only via its
/// wrapper reads as unwired.
const WRAPPER_DISPATCH: &[(&str, &str)] = &[
    ("Elicitation", "run_elicitation("),
    ("ElicitationResult", "run_elicitation_result("),
];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("daemon crate has a parent (repo root)")
        .to_path_buf()
}

/// Read a `.rs` file with everything from the first `#[cfg(test)]` onward
/// removed.
///
/// A test that fires an event says nothing about whether production does, and
/// this crate's convention is one trailing `mod tests` per file, so cutting at
/// the first occurrence is accurate here. It is a heuristic, not a parser: a
/// `#[cfg(test)]` item placed above production code would hide that code from
/// the scan, which fails toward reporting an event as unwired — the safe
/// direction, since that is a loud test failure rather than a silent gap.
fn read_without_tests(path: &Path) -> String {
    let Ok(content) = std::fs::read_to_string(path) else {
        return String::new();
    };
    match content.find("#[cfg(test)]") {
        Some(i) => content[..i].to_string(),
        None => content,
    }
}

/// Concatenate every `.rs` file under a path (or the file itself), tests
/// stripped.
fn concat_production_rs(path: &Path) -> String {
    if path.is_file() {
        return read_without_tests(path);
    }
    let mut out = String::new();
    let mut stack = vec![path.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                if p.file_name().is_some_and(|n| n == "tests") {
                    continue;
                }
                stack.push(p);
            } else if p.extension().is_some_and(|e| e == "rs") {
                out.push_str(&read_without_tests(&p));
                out.push('\n');
            }
        }
    }
    out
}

/// Every `HookEvent` variant name, read out of the enum's own source.
///
/// Taken from the source rather than a hand-written list so that adding a
/// variant cannot quietly escape this check.
fn all_event_names(root: &Path) -> Vec<String> {
    let src = std::fs::read_to_string(root.join("crates/hooks/src/config.rs"))
        .expect("crates/hooks/src/config.rs should exist");
    let start = src
        .find("pub enum HookEvent {")
        .expect("HookEvent enum should be declared in config.rs");
    let body = &src[start..];
    let end = body
        .find("\n}")
        .expect("HookEvent enum should have a closing brace");

    let names: Vec<String> = body[..end]
        .lines()
        .map(str::trim)
        // Variant lines are bare `Name,` — doc comments, attributes and the
        // declaration line itself all fail this shape.
        .filter(|l| l.ends_with(',') && !l.contains(' ') && !l.starts_with('/'))
        .map(|l| l.trim_end_matches(',').to_string())
        .filter(|n| n.chars().next().is_some_and(char::is_uppercase))
        .collect();

    assert!(
        names.len() > 20,
        "variant scan found only {} names — the enum's formatting probably changed \
         and this parser needs updating, which would otherwise make the whole check \
         vacuous: {names:?}",
        names.len()
    );
    names
}

#[test]
fn unwired_events_const_matches_what_the_engine_actually_fires() {
    let root = repo_root();
    let production: String = PRODUCTION_SOURCES
        .iter()
        .map(|p| concat_production_rs(&root.join(p)))
        .collect::<Vec<_>>()
        .join("\n");

    let mut computed_unwired = Vec::new();
    for name in all_event_names(&root) {
        let named_at_call_site = production.contains(&format!("HookEvent::{name}"));
        let fired_via_wrapper = WRAPPER_DISPATCH
            .iter()
            .find(|(event, _)| *event == name)
            .is_some_and(|(_, wrapper)| production.contains(wrapper));
        if !named_at_call_site && !fired_via_wrapper {
            computed_unwired.push(name);
        }
    }
    computed_unwired.sort();

    let mut declared: Vec<String> = hooks::UNWIRED_EVENTS
        .iter()
        .map(|e| format!("{e:?}"))
        .collect();
    declared.sort();

    let newly_unwired: Vec<_> = computed_unwired
        .iter()
        .filter(|e| !declared.contains(e))
        .collect();
    assert!(
        newly_unwired.is_empty(),
        "these hook events are not fired anywhere in production code but are not \
         declared in `hooks::UNWIRED_EVENTS`: {newly_unwired:?}. A hook configured \
         for one of them would wait forever with nothing said. Either fire the event, \
         or add it to the const with the reason written on the variant."
    );

    let now_wired: Vec<_> = declared
        .iter()
        .filter(|e| !computed_unwired.contains(e))
        .collect();
    assert!(
        now_wired.is_empty(),
        "`hooks::UNWIRED_EVENTS` still lists {now_wired:?}, but production code now \
         fires them. Remove them from the const (and from the variants' doc comments) \
         so the startup warning stops lying about a gap that has been closed."
    );
}

/// The scan must be able to fail. If `PRODUCTION_SOURCES` ever resolves to
/// nothing — a moved directory, a bad join — every event reads as unwired and
/// the real check above would still pass its first assertion for any event
/// already in the const. Prove the machinery detects a name that genuinely
/// appears nowhere.
#[test]
fn wiring_scan_detects_an_event_that_is_fired_nowhere() {
    let root = repo_root();
    let production: String = PRODUCTION_SOURCES
        .iter()
        .map(|p| concat_production_rs(&root.join(p)))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        !production.is_empty(),
        "PRODUCTION_SOURCES resolved to no source at all — the paths are wrong and \
         this check is vacuous"
    );
    assert!(
        !production.contains("HookEvent::NoSuchEventExists"),
        "scan claims to find an event that does not exist"
    );
    // And the positive control: an event everyone agrees is wired.
    assert!(
        production.contains("HookEvent::PreToolUse"),
        "scan cannot find PreToolUse, which is fired from the tool dispatch path — \
         the scan is broken"
    );
}

/// `is_wired()` and the const must not drift apart.
#[test]
fn is_wired_agrees_with_the_const() {
    for &event in hooks::UNWIRED_EVENTS {
        assert!(
            !event.is_wired(),
            "{event:?} is in UNWIRED_EVENTS but reports wired"
        );
    }
    assert!(hooks::HookEvent::PreToolUse.is_wired());
}
