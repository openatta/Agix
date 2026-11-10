//! Capabilities that exist in a library and are never reached from where the
//! engine is put together: a function that compiles, passes its own tests, and
//! does nothing in a real session because no composition root calls it.
//!
//! WIR-1 keeps `hooks::UNWIRED_EVENTS` honest in both directions, WIR-2 wants a
//! producer for every telemetry kind, WIR-3 wants the composition roots to
//! call the wiring-shaped functions, WIR-4 wants one place that spawns a
//! delegate's session. All of them read the production syntax tree: a call in
//! test code, in a comment, or one that merely asks `has_hooks_for` does not
//! count.

use std::collections::BTreeSet;

use guards::allow::{self, Finding};
use guards::scan::{self, Call, Callee};
use guards::{par_flat_map, ws, Hit, SourceFile, View};
use syn::visit::Visit;

const FIRE_CALLS: &[&str] = &[
    "run",
    "fire",
    "fire_lifecycle_hook",
    "fire_ctx_lifecycle_hook",
    "fire_subagent_hook",
];

/// Wrappers that fire one fixed event without naming it.
const WRAPPER_DISPATCH: &[(&str, &str)] = &[
    ("run_elicitation", "Elicitation"),
    ("run_elicitation_result", "ElicitationResult"),
];

/// Events chosen in one place and passed to a fire call from another: the
/// variant, the file that names it, why the fire call cannot.
const INDIRECT_FIRES: &[(&str, &str, &str)] = &[
    (
        "WorktreeCreate",
        "crates/runtime/src/turn/tool_exec.rs",
        "picked from the tool's name, then handed to `fire_ctx_lifecycle_hook` as a variable",
    ),
    (
        "WorktreeRemove",
        "crates/runtime/src/turn/tool_exec.rs",
        "picked from the tool's name, then handed to `fire_ctx_lifecycle_hook` as a variable",
    ),
];

/// Where a hook event is dispatched from is everywhere but the dispatcher: in
/// `hooks`, the runner, the config and the catalog name every event by
/// construction.
const HOOKS_DISPATCHER_MODULES: &[&str] = &["runner", "config", "catalog"];

/// (defining crate, function): each exists so a composition root can call it
/// to switch a capability on.
const WIRING_CHECKS: &[(&str, &str)] = &[
    ("tools", "register_builtin_tools"),
    ("tools", "register_web_search"),
    ("tools", "register_cron_tools"),
    ("tools", "register_worktree_tools"),
    ("hooks", "enable_file_watching"),
    ("mcp", "set_elicitation_callback"),
    ("script-host", "bind_into"),
    ("script-host", "bind_lenient"),
    ("history", "migrate_layout"),
];

/// Functions that are for external embedders of the library rather than this
/// repository's own daemon, with the reason.
const INTENTIONALLY_UNWIRED: &[(&str, &str, &str)] = &[];

/// Files that are composition roots besides the whole of `daemon` and
/// `assembly`; a module wires its capability into the session in `on_session`.
const COMPOSITION_ROOT_FILES: &[&str] = &[
    "crates/agents/src/lib.rs",
    "crates/mcp/src/module.rs",
    "crates/skills/src/module.rs",
];
const COMPOSITION_ROOT_DIRS: &[&str] = &["crates/runtime/src/agent/"];

const SPAWN_SITE_FILE: &str = "crates/agents/src/agent_tool/build.rs";
const SPAWN_INHERITANCE: &str = "inherited_by_a_delegate";

fn not_test_support(f: &&SourceFile) -> bool {
    !ws().package(&f.package).dir.starts_with("tests/")
}

fn callee_name(c: &Call) -> Option<&str> {
    match &c.callee {
        Callee::Path(p) => p.last().map(String::as_str),
        Callee::Method(m) => Some(m),
    }
}

#[derive(Default)]
struct EventPaths(Vec<String>);

impl<'ast> Visit<'ast> for EventPaths {
    fn visit_expr_path(&mut self, p: &'ast syn::ExprPath) {
        let segs: Vec<String> = p
            .path
            .segments
            .iter()
            .map(|s| s.ident.to_string())
            .collect();
        if let [.., event, variant] = segs.as_slice() {
            if event == "HookEvent" {
                self.0.push(variant.clone());
            }
        }
        syn::visit::visit_expr_path(self, p);
    }
}

fn events_fired_in(f: &SourceFile) -> Vec<String> {
    let mut out = Vec::new();
    for call in scan::calls(f, View::Prod) {
        let Some(name) = callee_name(&call) else {
            continue;
        };
        if FIRE_CALLS.contains(&name) {
            let mut found = EventPaths::default();
            for arg in &call.args {
                found.visit_expr(arg);
            }
            out.extend(found.0);
        }
        if let Some((_, event)) = WRAPPER_DISPATCH.iter().find(|(w, _)| *w == name) {
            out.push(event.to_string());
        }
    }
    out
}

fn dispatch_sources() -> impl Iterator<Item = &'static SourceFile> {
    ws().production().filter(not_test_support).filter(|f| {
        !(f.package == "hooks"
            && f.module
                .first()
                .is_some_and(|m| HOOKS_DISPATCHER_MODULES.contains(&m.as_str())))
    })
}

fn fired() -> BTreeSet<String> {
    par_flat_map(dispatch_sources(), events_fired_in)
        .into_iter()
        .collect()
}

#[test]
fn unwired_events_const_matches_what_the_engine_actually_fires() {
    let fired = fired();
    allow::non_vacuous("wiring::hook_events", fired.len(), 20);
    assert!(
        fired.contains("PreToolUse"),
        "the scan cannot see PreToolUse fired from the tool dispatch path: the scan is broken"
    );

    let mut direct_and_indirect = fired.clone();
    for (variant, file, _) in INDIRECT_FIRES {
        let named = scan::expr_paths(ws().file(file), View::Prod)
            .into_iter()
            .any(|(p, _)| matches!(p.as_slice(), [.., e, v] if e == "HookEvent" && v == variant));
        assert!(
            named,
            "INDIRECT_FIRES lists {variant} in {file}, which no longer names `HookEvent::{variant}`: \
             take the entry off the list in tests/guards/tests/wiring.rs"
        );
        assert!(
            !fired.contains(*variant),
            "INDIRECT_FIRES lists {variant}, but a fire call now names it directly: take the \
             entry off the list in tests/guards/tests/wiring.rs"
        );
        direct_and_indirect.insert(variant.to_string());
    }

    let computed_unwired: BTreeSet<String> = hooks::HookEvent::ALL
        .iter()
        .map(|e| format!("{e:?}"))
        .filter(|name| !direct_and_indirect.contains(name))
        .collect();
    let declared: BTreeSet<String> = hooks::UNWIRED_EVENTS
        .iter()
        .map(|e| format!("{e:?}"))
        .collect();

    let newly_unwired: Vec<_> = computed_unwired.difference(&declared).collect();
    assert!(
        newly_unwired.is_empty(),
        "these hook events are not fired anywhere in production code but are not declared in \
         `hooks::UNWIRED_EVENTS`: {newly_unwired:?}. A hook configured for one of them would \
         wait forever with nothing said. Either fire the event, or add it to the const with the \
         reason written on the variant."
    );
    let now_wired: Vec<_> = declared.difference(&computed_unwired).collect();
    assert!(
        now_wired.is_empty(),
        "`hooks::UNWIRED_EVENTS` still lists {now_wired:?}, but production code now fires them. \
         Remove them from the const (and from the variants' doc comments) so the startup warning \
         stops lying about a gap that has been closed."
    );
}

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

#[test]
fn every_telemetry_kind_has_a_producer() {
    let produced: BTreeSet<String> = par_flat_map(
        ws().production()
            .filter(not_test_support)
            .filter(|f| f.package != "telemetry"),
        |f| {
            scan::calls(f, View::Prod)
                .into_iter()
                .filter_map(|c| match c.callee {
                    Callee::Path(p) => match p.as_slice() {
                        [.., ty, kind] if ty == "TelemetryEvent" => Some(kind.clone()),
                        _ => None,
                    },
                    Callee::Method(_) => None,
                })
                .collect()
        },
    )
    .into_iter()
    .collect();
    allow::non_vacuous("wiring::telemetry_producers", produced.len(), 10);

    let orphans: Vec<&str> = telemetry::EventPayload::KINDS
        .iter()
        .copied()
        .filter(|kind| !produced.contains(*kind))
        .collect();
    assert!(
        orphans.is_empty(),
        "event kinds nothing emits: {orphans:?}. A kind nothing emits reads, to whoever \
         configures `event_toggles`, as something they will see: emit it where it happens, or \
         delete it."
    );
}

fn is_composition_root(f: &SourceFile) -> bool {
    let package = ws().package(&f.package);
    matches!(package.name.as_str(), "daemon" | "assembly")
        || COMPOSITION_ROOT_FILES.contains(&f.rel.as_str())
        || COMPOSITION_ROOT_DIRS.iter().any(|d| f.rel.starts_with(d))
}

#[test]
fn wiring_shaped_functions_are_called_from_a_composition_root() {
    let called: BTreeSet<String> =
        par_flat_map(ws().production().filter(|f| is_composition_root(f)), |f| {
            scan::calls(f, View::Prod)
                .into_iter()
                .filter_map(|c| callee_name(&c).map(str::to_string))
                .collect()
        })
        .into_iter()
        .collect();
    allow::non_vacuous("wiring::composition_roots", called.len(), 200);

    let found: Vec<Finding> = WIRING_CHECKS
        .iter()
        .filter(|(_, func)| !called.contains(*func))
        .filter(|(krate, func)| {
            !INTENTIONALLY_UNWIRED
                .iter()
                .any(|(c, f, _)| c == krate && f == func)
        })
        .map(|(krate, func)| Finding {
            key: format!("{krate}::{func}"),
            hit: Hit {
                file: format!("crates of `{krate}`"),
                line: 0,
                snippet: format!("`{func}` is called from no composition root"),
            },
        })
        .collect();
    assert!(
        found.is_empty(),
        "these functions exist to be called by a composition root but nothing in daemon, \
         assembly, a module's session hook or the agent builder calls them:\n  {}\n\nWire the \
         call in (the usual fix); if one is for external embedders of the library rather than \
         this repo's own daemon, add it to INTENTIONALLY_UNWIRED in \
         tests/guards/tests/wiring.rs with the reason.",
        found
            .iter()
            .map(|f| f.key.clone())
            .collect::<Vec<_>>()
            .join("\n  ")
    );
}

#[test]
fn every_wiring_check_names_a_function_that_exists() {
    for (krate, func) in WIRING_CHECKS {
        ws().package(krate);
        let defined = par_flat_map(ws().production().filter(|f| f.package == *krate), |f| {
            scan::fns(f, View::Prod)
                .into_iter()
                .filter(|s| s.qual.rsplit("::").next() == Some(func))
                .map(|s| s.qual)
                .collect()
        });
        assert!(
            !defined.is_empty(),
            "WIRING_CHECKS names `{krate}::{func}`, which `{krate}` no longer defines: renamed \
             or removed, so take it off the list in tests/guards/tests/wiring.rs"
        );
    }
}

fn spawn_sites() -> Vec<(String, Hit, bool)> {
    par_flat_map(ws().production().filter(|f| f.package == "agents"), |f| {
        let fns = scan::fns(f, View::Prod);
        let calls = scan::calls(f, View::Prod);
        calls
            .iter()
            .filter(|c| {
                matches!(&c.callee, Callee::Path(p)
                        if matches!(p.as_slice(), [.., ty, new] if ty == "Builder" && new == "new")
                            && !p.iter().any(|s| s == "thread"))
            })
            .map(|spawn| {
                let line = spawn.hit.line;
                let enclosing = fns
                    .iter()
                    .filter(|s| (s.hit.line..s.hit.line + s.lines).contains(&line))
                    .min_by_key(|s| s.lines);
                let inherits = enclosing.is_some_and(|s| {
                    calls.iter().any(|c| {
                        callee_name(c) == Some(SPAWN_INHERITANCE)
                            && (s.hit.line..s.hit.line + s.lines).contains(&c.hit.line)
                    })
                });
                (
                    enclosing.map_or_else(String::new, |s| s.qual.clone()),
                    spawn.hit.clone(),
                    inherits,
                )
            })
            .collect()
    })
}

#[test]
fn a_fire_call_is_told_from_a_question_about_the_event() {
    let src = r#"
        fn f(runner: &Runner) {
            runner.run(HookEvent::StopFailure, ());
            if runner.has_hooks_for(HookEvent::Stop) {}
            fire_lifecycle_hook(hooks::config::HookEvent::TurnStart, |i| i);
            runner.run_elicitation(&a, &b, &c);
            tokio::select! { _ = runner.run(HookEvent::PostToolUseFailure, ()) => {} }
        }
    "#;
    let f = guards::source::standalone("fixture.rs", src.to_string()).expect("parses");
    let mut fired = events_fired_in(&f);
    fired.sort();
    assert_eq!(
        fired,
        [
            "Elicitation",
            "PostToolUseFailure",
            "StopFailure",
            "TurnStart"
        ]
    );
}

#[test]
fn every_spawn_site_takes_the_whole_inheritance() {
    let sites = spawn_sites();
    let places: Vec<String> = sites.iter().map(|(_, h, _)| h.to_string()).collect();
    assert!(
        sites.len() == 1 && sites[0].1.file == SPAWN_SITE_FILE,
        "the agents crate builds a session at {places:?}; a delegated session is assembled in \
         exactly one place, `Inner::build_delegate` in {SPAWN_SITE_FILE}. Route the new spawn \
         site through it, with what differs expressed in `DelegatePlan`."
    );
    let (function, hit, inherits) = &sites[0];
    assert!(
        *inherits,
        "{hit} builds a session in `{function}` without calling `{SPAWN_INHERITANCE}` in the \
         same function. Everything a delegated session inherits is on that one list precisely \
         so a spawn site cannot take some of it."
    );
}
