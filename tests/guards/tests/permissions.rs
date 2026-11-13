//! The permission pipeline's guards (D1 PERM-*).
//!
//! PERM-3: a call is let through by the policy, or by the person the runtime
//! asked — nowhere else builds a `PermissionOutcome::Permit`.
//! PERM-4: whether anyone attends a session is read in one place, and the
//! profile a command runs under in one more; everything else is handed the
//! attendance.
//! PERM-5: only the shell's confinement, the engine's own processes and the
//! confinement report confine a process; the backends implement `confine`
//! and call nothing.
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

const PERMITS: &[(&str, &str)] = &[
    (
        "crates/permissions/src/policy/**",
        "the policy, which is where a call is let through",
    ),
    (
        "crates/runtime/src/turn/tool_permission.rs::ask",
        "the answering step: the person the runtime asked approved the call",
    ),
    (
        "crates/agents/src/agent_tool/permission.rs::permitted",
        "ParentPermission's own answering loop and AlwaysPermit; D1-3c replaces them",
    ),
    (
        "crates/assembly/src/assemble.rs::AllowAllPermission::decide",
        "the host's allow-everything permission; D1-5a makes the host a layer of rules",
    ),
    (
        "tests/live/src/handle.rs::AllowAll::decide",
        "the live tier's stand-in for the daemon's own allow-everything host permission",
    ),
    (
        "crates/runtime/src/agent/builder/build.rs::Builder::build::AllowAll::decide",
        "the builder's allow-everything default; D1-5a makes the policy the default",
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

const READS_UNATTENDED: &[(&str, &str)] = &[
    (
        "crates/runtime/src/turn/tool_env.rs::attendance_of",
        "the attendance of a session's calls, worked out once per call",
    ),
    (
        "crates/tools/src/bash/sandbox.rs::sandbox_policy",
        "the profile a command runs under; D1-6a moves the choice to the runtime",
    ),
];

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
