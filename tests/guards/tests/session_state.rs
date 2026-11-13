//! A session has one `SessionState`, whichever engine runs it.
//!
//! G-D3-2: production code creates a `SessionState` in `CREATED_AT` only —
//! the builder's default for an engine nobody handed a state to. A state
//! created anywhere else would be a second copy of a session's mode, todos,
//! tasks and goal, which the session's own engine never sees. Test-support
//! packages under `tests/` are not scanned.

use guards::allow::{self, Finding};
use guards::scan::{self, Callee};
use guards::{par_flat_map, ws, View};

const CREATED_AT: &[(&str, &str)] = &[(
    "crates/runtime/src/agent/builder/build.rs",
    "an engine built without `Builder::session_state` gets a state of its own",
)];

#[test]
fn a_session_state_is_created_in_one_place() {
    let files: Vec<_> = ws()
        .production()
        .filter(|f| !ws().package(&f.package).dir.starts_with("tests/"))
        .collect();
    allow::non_vacuous("session_state::CREATED_AT", files.len(), 200);
    let found: Vec<Finding> = par_flat_map(files.into_iter(), |f| {
        scan::calls(f, View::Prod)
            .into_iter()
            .filter(|c| match &c.callee {
                Callee::Path(p) => {
                    matches!(p.as_slice(), [.., ty, new] if ty == "SessionState" && new == "new")
                }
                Callee::Method(_) => false,
            })
            .map(|c| Finding {
                key: f.rel.clone(),
                hit: c.hit,
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
