//! A session has one `SessionState`, whichever engine runs it.
//!
//! G-D3-1: every field of the session's state, of what an engine hangs on it,
//! of the pool's `SessionCore` and of its `EngineSlot` is on `FIELDS`, saying
//! whose it is — the session's, which outlives an engine; the engine's, which
//! the next engine replaces; or a turn's. A field that is not on the list
//! fails until somebody decides which; `SessionState` has no `pub` field.
//!
//! G-D3-2: production code creates a `SessionState` in `CREATED_AT` only —
//! the builder's default for an engine nobody handed a state to, and the
//! pool's `SessionCore`. A state created anywhere else would be a second copy
//! of a session's mode, todos, tasks and goal, which the session's own engine
//! never sees. Test-support packages under `tests/` are not scanned.
//!
//! G-D3-4: nothing carries state from one engine's copy to another's — no
//! `carry_*` function, no hub `absorb` — since there is only one copy.

use guards::allow::{self, Finding};
use guards::scan::{self, Callee};
use guards::{par_flat_map, ws, Hit, SourceFile, View};

const SESSION_STATE: &str = "crates/core/src/context/session.rs";
const LIVE: &str = "crates/assembly/src/session_pool/parts/live.rs";

const FIELDS: &[(&str, &str)] = &[
    (
        "SessionState::env",
        "Engine: the attached engine's settings-derived env, replaced whole on attach",
    ),
    (
        "SessionState::engine",
        "Engine: the attached engine's tools and providers, taken off when it ends",
    ),
    (
        "SessionState::permission_mode",
        "Session: plan mode outlives an engine; rebased onto each engine's mode",
    ),
    ("SessionState::todos", "Session: recorded in the log"),
    (
        "SessionState::recorded_durable",
        "Session: what the log has of the durable state",
    ),
    (
        "SessionState::activated_tools",
        "Session: recorded in the log with the durable state",
    ),
    (
        "SessionState::running_tasks",
        "Session: background work goes on across engines",
    ),
    (
        "SessionState::task_events",
        "Session: one forwarder per session reads it",
    ),
    ("SessionState::goal", "Session: recorded in the log"),
    (
        "SessionState::unrecorded_goal_changes",
        "Session: goal changes not yet in the log",
    ),
    (
        "SessionState::goal_recording",
        "Session: one writer of goal changes at a time",
    ),
    (
        "SessionState::goal_events",
        "Session: one forwarder per session reads it",
    ),
    (
        "SessionState::notices",
        "Session: what the agent has not read yet",
    ),
    (
        "SessionState::read_cache",
        "Session: what the model has read, for staleness checks",
    ),
    (
        "SessionState::last_read_path",
        "Session: for the repeated-read check",
    ),
    (
        "SessionState::ledger",
        "Session: spend, output target and invoked skills",
    ),
    (
        "SessionState::model_choice",
        "Session: the model a host chose, read by whichever engine runs the next turn",
    ),
    ("SessionEnv::cwd", "Engine: from the engine's settings"),
    (
        "SessionEnv::additional_writable_dirs",
        "Engine: from the engine's settings",
    ),
    (
        "SessionEnv::read_directories",
        "Engine: from the engine's settings",
    ),
    (
        "SessionEnv::read_anywhere",
        "Engine: from the engine's settings",
    ),
    (
        "SessionEnv::cache_directories",
        "Engine: from the engine's settings",
    ),
    ("SessionEnv::unattended", "Engine: from the engine's scene"),
    (
        "SessionEnv::sandbox_disabled",
        "Engine: from the engine's settings",
    ),
    (
        "SessionEnv::base_mode",
        "Engine: the mode the engine is configured with",
    ),
    ("SessionCore::id", "Session: its id"),
    (
        "SessionCore::scene",
        "Session: the scene it was created under",
    ),
    (
        "SessionCore::project",
        "Session: the project it is bound to",
    ),
    ("SessionCore::created_at", "Session: kept across rebuilds"),
    ("SessionCore::state", "Session: the one state"),
    (
        "SessionCore::hub",
        "Session: its watchers and open asks go on across engines",
    ),
    (
        "SessionCore::steer",
        "Session: messages for the running turn, whichever engine runs it",
    ),
    (
        "SessionCore::process_groups",
        "Session: reaped when the session ends",
    ),
    ("SessionCore::naming", "Session: its name"),
    (
        "SessionCore::context",
        "Session: how full the context was after the last turn",
    ),
    (
        "EngineSlot::generation",
        "Engine: the number it attached to the state under",
    ),
    ("EngineSlot::input_tx", "Engine: its input channel"),
    ("EngineSlot::cancel", "Engine: stops it"),
    (
        "EngineSlot::exited",
        "Engine: cancelled once it has stopped",
    ),
    (
        "EngineSlot::namer",
        "Engine: built from the engine's model routing",
    ),
    (
        "EngineSlot::binding",
        "Engine: the stamp of the binding it was built from",
    ),
    ("EngineSlot::delegator", "Engine: its delegator"),
    ("EngineSlot::scripts", "Engine: its bound scripts' record"),
    ("EngineSlot::health", "Engine: its plugins' checks"),
];

const CREATED_AT: &[(&str, &str)] = &[
    (
        "crates/runtime/src/agent/builder/build.rs",
        "an engine built without `Builder::session_state` gets a state of its own",
    ),
    (
        "crates/assembly/src/session_pool/parts/live.rs",
        "`SessionCore`: the state of a session the pool brings into memory",
    ),
    (
        "crates/core/src/context/session.rs::SessionState::for_tests_in",
        "a state for a test, which no session runs on",
    ),
];

/// Ways production code may not move state between engines.
const CARRIES: &[(&str, &str)] = &[];

/// `file::function` around `hit`, or `file` at its top level.
fn place(f: &SourceFile, hit: &Hit) -> String {
    let around = scan::fns(f, View::Prod)
        .into_iter()
        .filter(|func| func.hit.line <= hit.line && hit.line < func.hit.line + func.lines)
        .max_by_key(|func| func.hit.line);
    match around {
        Some(func) => format!("{}::{}", f.rel, func.qual),
        None => f.rel.clone(),
    }
}

#[test]
fn every_field_of_a_sessions_state_says_whose_it_is() {
    let mut found = Vec::new();
    for (file, names) in [
        (SESSION_STATE, &["SessionState", "SessionEnv"][..]),
        (LIVE, &["SessionCore", "EngineSlot"][..]),
    ] {
        let f = ws().file(file);
        for st in scan::structs(f, View::Prod)
            .into_iter()
            .filter(|s| names.contains(&s.name.as_str()))
        {
            for field in st.fields {
                let name = field.name.expect("a named field");
                assert!(
                    st.name != "SessionState" || field.vis.is_empty(),
                    "SessionState::{name} is `{}`: the state's fields are private",
                    field.vis
                );
                found.push(Finding {
                    key: format!("{}::{name}", st.name),
                    hit: st.hit.clone(),
                });
            }
        }
    }
    allow::non_vacuous("session_state::FIELDS", found.len(), 40);
    for (key, why) in FIELDS {
        assert!(
            ["Session: ", "Engine: ", "Turn: "]
                .iter()
                .any(|owner| why.starts_with(owner)),
            "[session_state::FIELDS] `{key}` says `{why}`: start with `Session: `, \
             `Engine: ` or `Turn: `"
        );
    }
    allow::check(
        "session_state::FIELDS",
        found,
        FIELDS,
        "say whose the field is — the session's (outlives an engine, so a rebuild keeps \
         it), the engine's (the next engine brings its own) or a turn's",
    );
}

#[test]
fn a_session_state_is_created_in_one_place() {
    let files: Vec<_> = ws()
        .production()
        .filter(|f| !ws().package(&f.package).dir.starts_with("tests/"))
        .collect();
    allow::non_vacuous("session_state::CREATED_AT", files.len(), 200);
    let found: Vec<Finding> = par_flat_map(files, |f| {
        scan::calls(f, View::Prod)
            .into_iter()
            .filter_map(|c| {
                let Callee::Path(p) = &c.callee else {
                    return None;
                };
                match p.as_slice() {
                    [.., ty, new] if ty == "SessionState" && new == "new" => Some(Finding {
                        key: f.rel.clone(),
                        hit: c.hit,
                    }),
                    [ty, new] if ty == "Self" && new == "new" && f.rel == SESSION_STATE => {
                        let key = place(f, &c.hit);
                        key.starts_with(&format!("{SESSION_STATE}::SessionState::"))
                            .then_some(Finding { key, hit: c.hit })
                    }
                    _ => None,
                }
            })
            .collect()
    });
    allow::check(
        "session_state::CREATED_AT",
        found,
        CREATED_AT,
        "inject the session's state with `Builder::session_state` instead of creating another; \
         a test builds one with `SessionState::for_tests`",
    );
}

#[test]
fn nothing_carries_state_from_one_engine_to_the_next() {
    let files: Vec<_> = ws()
        .production()
        .filter(|f| !ws().package(&f.package).dir.starts_with("tests/"))
        .collect();
    allow::non_vacuous("session_state::CARRIES", files.len(), 200);
    let found: Vec<Finding> = par_flat_map(files, |f| {
        let in_pool =
            f.rel.starts_with("crates/assembly/src/session_pool/") && !f.rel.contains("/mcp_hub/");
        let mut found: Vec<Finding> = scan::fns(f, View::Prod)
            .into_iter()
            .filter(|func| {
                func.qual
                    .rsplit("::")
                    .next()
                    .is_some_and(|name| name.starts_with("carry_"))
            })
            .map(|func| Finding {
                key: format!("{}::{}", f.rel, func.qual),
                hit: func.hit,
            })
            .collect();
        for c in scan::calls(f, View::Prod) {
            let name = match &c.callee {
                Callee::Method(m) => m.as_str(),
                Callee::Path(p) => p.last().map(String::as_str).unwrap_or_default(),
            };
            if name.starts_with("carry_") || (in_pool && name == "absorb") {
                found.push(Finding {
                    key: format!("{}: {name}", f.rel),
                    hit: c.hit,
                });
            }
        }
        if in_pool {
            for (path, hit) in scan::expr_paths(f, View::Prod) {
                let joined = path.join("::");
                if joined.ends_with("InputMessage::Steer")
                    || joined.ends_with("EngineCommand::UpdateModel")
                {
                    found.push(Finding {
                        key: format!("{}: {joined}", f.rel),
                        hit,
                    });
                }
            }
        }
        found
    });
    allow::check(
        "session_state::CARRIES",
        found,
        CARRIES,
        "a session has one state, kept across its engines: inject it with \
         `Builder::session_state` instead of copying it over; a message for the running \
         turn goes through the session's steer queue, a model choice through its state",
    );
}
