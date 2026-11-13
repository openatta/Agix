//! The permission pipeline's guards (D1 PERM-*).
//!
//! PERM-2: a call is decided by the engine's policy, or relayed to a parent's
//! by a delegate; no other policy stands between a call and the rules.
//! PERM-3: a call is let through by the policy, or by the person the runtime
//! asked — nowhere else builds a `PermissionOutcome::Permit`.
//! PERM-4: whether anyone attends a session is read in one place; everything
//! else is handed the attendance, and a command the profile its approval
//! names.
//! PERM-5: only the shell's confinement, the engine's own processes and the
//! confinement report confine a process; the backends implement `confine`
//! and call nothing.
//! PERM-6: neither the permission engine nor a delegate's relay makes up a
//! tool context; they judge with the one the call runs with.
//! PERM-7: the permission engine does not reach the machine.
//! PERM-8: the decision matrix names every `Stage` the policy can refuse or
//! ask at, and its stage test reaches each one.

use std::collections::BTreeSet;

use guards::allow::{self, Finding};
use guards::scan::{self, Callee};
use guards::{par_flat_map, ws, Hit, SourceFile, View};

const MATRIX: &str = "crates/tools/tests/decision_matrix.rs";
const POLICY_DIR: &str = "crates/permissions/src/policy/";

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

const POLICIES: &[(&str, &str)] = &[
    (
        "crates/permissions/src/policy/mod.rs::Policy",
        "the engine's policy: rules, modes, the managed layer",
    ),
    (
        "crates/agents/src/agent_tool/permission.rs::DelegateRelay",
        "a delegate's calls, handed to its parent's policy",
    ),
];

#[test]
fn only_the_policy_and_the_delegate_relay_decide_a_call() {
    let found = par_flat_map(ws().production(), |f| {
        scan::impls(f, View::Prod)
            .into_iter()
            .filter(|imp| {
                imp.trait_path
                    .as_ref()
                    .is_some_and(|t| t.last().is_some_and(|s| s == "PermissionPolicy"))
            })
            .map(|imp| Finding {
                key: format!("{}::{}", f.rel, imp.self_ty),
                hit: imp.hit,
            })
            .collect()
    });
    allow::check(
        "permissions::POLICIES",
        found,
        POLICIES,
        "add rules to `permissions::policy::Policy`, or hand the call to it the way          `DelegateRelay` does",
    );
}

const MAKES_A_CONTEXT: &[(&str, &str)] = &[(
    "crates/permissions/src/policy/mod.rs::Policy::context_for",
    "a request that brought no context; D1-4 makes the context a required field",
)];

#[test]
fn the_policy_and_the_relay_judge_with_the_calls_own_context() {
    let found = par_flat_map(ws().production(), |f| {
        if !(f.rel.starts_with("crates/permissions/") || f.rel.starts_with("crates/agents/")) {
            return Vec::new();
        }
        scan::calls(f, View::Prod)
            .into_iter()
            .filter(|call| {
                matches!(&call.callee, Callee::Path(p)
                    if p.ends_with(&["ToolContext".into(), "from_engine_ctx_with".into()]))
            })
            .map(|call| Finding {
                key: place(f, &call.hit),
                hit: call.hit,
            })
            .collect()
    });
    allow::check(
        "permissions::MAKES_A_CONTEXT",
        found,
        MAKES_A_CONTEXT,
        "judge with `PermissionRequest::ctx`, the context the call runs with",
    );
}

const PERMITS: &[(&str, &str)] = &[
    (
        "crates/permissions/src/policy/**",
        "the policy, which is where a call is let through",
    ),
    (
        "crates/runtime/src/turn/tool_permission.rs::ask",
        "the answering step: the person the runtime asked approved the call",
    ),
];

#[test]
fn only_the_policy_and_the_person_asked_let_a_call_through() {
    let found = par_flat_map(ws().production(), |f| {
        scan::expr_paths(f, View::Prod)
            .into_iter()
            .filter(|(segments, _)| {
                segments.ends_with(&["PermissionOutcome".into(), "Permit".into()])
            })
            .map(|(_, hit)| Finding {
                key: if f.rel.starts_with(POLICY_DIR) {
                    format!("{POLICY_DIR}**")
                } else {
                    place(f, &hit)
                },
                hit,
            })
            .collect()
    });
    allow::check(
        "permissions::PERMITS",
        found,
        PERMITS,
        "let `permissions::policy::Policy` decide the call, or answer its question in the \
         runtime's answering step",
    );
}

const READS_UNATTENDED: &[(&str, &str)] = &[(
    "crates/runtime/src/turn/tool_env.rs::attendance_of",
    "the attendance of a session's calls, worked out once per call",
)];

#[test]
fn attendance_is_read_in_one_place_and_handed_on() {
    let found = par_flat_map(ws().production(), |f| {
        scan::field_accesses(f, View::Prod)
            .into_iter()
            .filter(|access| access.name == "unattended" && !access.write)
            .map(|access| Finding {
                key: place(f, &access.hit),
                hit: access.hit,
            })
            .collect()
    });
    allow::check(
        "permissions::READS_UNATTENDED",
        found,
        READS_UNATTENDED,
        "read `ToolContext::attendance` or `PermissionRequest::attendance` instead",
    );
}

const ENGINE_EXEC: &str = "crates/core/src/engine_exec.rs";

const CONFINES: &[(&str, &str)] = &[
    (
        "crates/tools/src/bash/sandbox.rs::confine_shell",
        "the command a shell-running tool spawns",
    ),
    (
        "crates/core/src/engine_exec.rs",
        "the engine's own git and helpers",
    ),
    (
        "crates/core/src/interface/exec/sandbox.rs::ConfinementReport::probe",
        "what the session's sandbox holds a command to, found once",
    ),
];

#[test]
fn only_the_sandbox_its_callers_and_the_report_confine_a_process() {
    let found = par_flat_map(ws().production(), |f| {
        scan::calls(f, View::Prod)
            .into_iter()
            .filter(|call| matches!(&call.callee, Callee::Method(m) if m == "confine"))
            .map(|call| Finding {
                key: match f.rel.as_str() {
                    rel @ ENGINE_EXEC => rel.to_string(),
                    _ => place(f, &call.hit),
                },
                hit: call.hit,
            })
            .collect()
    });
    allow::check(
        "permissions::CONFINES",
        found,
        CONFINES,
        "confine through `tools::bash::confine_shell`, or read the session's \
         `ConfinementReport` instead of probing the sandbox",
    );
}

#[test]
fn the_permission_engine_does_not_reach_the_machine() {
    let reaches: Vec<String> = ws()
        .package("permissions")
        .deps
        .iter()
        .filter(|d| d.name == "platform")
        .map(|d| format!("{:?}", d.kind))
        .collect();
    assert!(
        reaches.is_empty(),
        "[permissions::PERM-7] permissions depends on platform ({reaches:?}): judge with the \
         context the request brings, and in-process providers when it brings none"
    );
}

#[test]
fn every_stage_appears_in_the_decision_matrix() {
    let variants = scan::enum_variants(ws(), "base", "Stage");
    let named: BTreeSet<String> = scan::paths(ws().file(MATRIX), View::Full)
        .into_iter()
        .filter_map(|(segments, _)| match segments.as_slice() {
            [.., ty, variant] if ty == "Stage" => Some(variant.clone()),
            _ => None,
        })
        .collect();
    let missing: Vec<&String> = variants.iter().filter(|v| !named.contains(*v)).collect();
    assert!(
        missing.is_empty(),
        "[permissions::PERM-8] `base` `Stage` variants no call in {MATRIX} reaches: \
         {missing:?} — add a case for each to `every_stage_of_the_policy_is_reached`"
    );
}
