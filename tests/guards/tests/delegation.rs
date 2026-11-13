//! What a delegate's session is built from.
//!
//! DLG-3: every field of `runtime::agent::Builder` is on exactly one of three
//! lists — inherited whole through `HostContracts`, decided by each delegate
//! for itself, or not for delegates at all — so a field added to `Builder`
//! cannot go unclassified and quietly stop at the first delegation. The
//! inherited list is `HostContracts` field for field, and `Builder::inherit`
//! sets every builder field on it.
//!
//! DLG-4: what a delegate is built from is captured once, when its parent
//! session is assembled. Nothing in `agents` or `team` is a slot filled in
//! after construction: no `SetLater`, no `RwLock<Option<…>>` field, no
//! `set_*` on `AgentTool` or `Delegator`.
//!
//! DLG-6: `DelegateMode` is a closed enum, and every `match` on it in
//! production code names each mode: no `_` arm stands in for one a new mode
//! would fall into.
//!
//! DLG-7: in `agents`, a task is registered only through `TaskLease`, which
//! ends it however its holder goes away; nothing calls `register_task` or
//! holds a `TaskScope`.

use std::collections::BTreeSet;

use guards::scan;
use guards::{ws, View};
use syn::visit::Visit;

const BUILDER: &str = "crates/runtime/src/agent/builder/mod.rs";
const HOST: &str = "crates/runtime/src/agent/builder/host.rs";

/// (builder field, where it comes from in `HostContracts`).
const INHERITED: &[(&str, &str)] = &[
    ("exec", "exec"),
    ("token_counter", "token_counter"),
    ("rule_provider", "rules"),
    ("environment", "environment"),
    ("permission", "permission"),
    ("history_store", "history_store"),
    ("telemetry_handle_override", "telemetry"),
    ("task_router", "task_router"),
    ("scene_registry", "scene_registry"),
    ("memory_store", "memory_store"),
    ("memory_retriever", "memory_retriever"),
    ("retrieval_hooks", "retrieval_hooks"),
    ("instruction_provider", "instruction_provider"),
    ("plugins", "plugins"),
    ("modules", "modules"),
    ("tool_middleware", "host_layers.tool_middleware"),
    ("prompt_middleware", "host_layers.prompt_middleware"),
    ("result_transformers", "host_layers.result_transformers"),
    ("model_interceptors", "host_layers.model_interceptors"),
    ("recovery_policy", "host_layers.recovery_policy"),
    ("compactor", "host_layers.compactor"),
];

/// Decided per delegate by whoever spawns it.
const OWN: &[(&str, &str)] = &[
    ("scene", "its type's scene, or its parent's"),
    (
        "model",
        "its type's model, or the one routed for sub-agents",
    ),
    (
        "tools",
        "what it inherits of its parent's template, narrowed by its type and caller",
    ),
    (
        "settings",
        "its parent's, with its own model and its type's overrides",
    ),
    (
        "hooks",
        "the tree's one runner, which the delegating module hands down",
    ),
    (
        "session_id",
        "a fresh one, so its transcript is a file of its own",
    ),
    ("parent_session_id", "its lineage"),
    ("agent_type", "its lineage"),
    ("skip_warmup", "the spawner's choice"),
    ("output_schema", "the caller's"),
    (
        "contributed_tool_filter",
        "its type's and its caller's tool lists",
    ),
    (
        "mcp_servers",
        "the share of its parent's servers its type is granted",
    ),
    (
        "memory_extraction",
        "off for a one-shot delegate dropped as its turn returns",
    ),
    ("agent_depth", "its parent's plus one"),
    ("team_seat", "the team and name it works under"),
];

/// Not given to a delegate.
const NOT_FOR_DELEGATES: &[(&str, &str)] = &[
    (
        "telemetry_url",
        "resolved into the telemetry handle, which is inherited",
    ),
    (
        "instruction_file",
        "resolved into the instruction provider, which is inherited",
    ),
    ("frozen", "a replay snapshot of the root's first turn"),
    (
        "wake_rx",
        "the tree's wake channel is wired on the root's runner",
    ),
    (
        "commands_override",
        "shared across a host's root sessions; a delegate builds its own",
    ),
    (
        "elicitation",
        "a delegate's questions go to its parent's host",
    ),
    (
        "question_prompts",
        "a delegate's questions go to its parent's host",
    ),
    (
        "prompt_registry",
        "contributions to the root's prompt; a delegate's is its type's",
    ),
    (
        "prompt_assembler",
        "assembles the root's prompt; a delegate keeps the engine's",
    ),
    ("turn_policy", "a delegate has its own ceilings"),
    ("budget_policy", "a delegate has its own ceilings"),
    ("health_checks", "the host asks the root"),
    (
        "event_sinks",
        "a delegate's events reach them through its parent, tagged",
    ),
    (
        "cron_store",
        "scheduled jobs belong to the session the host keeps (D2 §7.7)",
    ),
    (
        "goal_rounds_from_owner",
        "a host drives the root's goal rounds",
    ),
    ("process_groups", "already in the inherited exec providers"),
    (
        "session_prompt",
        "the root's own prompt; a delegate keeps its type's",
    ),
];

fn struct_fields(file: &str, name: &str) -> BTreeSet<String> {
    let found = scan::structs(ws().file(file), View::Prod)
        .into_iter()
        .find(|s| s.name == name)
        .unwrap_or_else(|| panic!("no struct {name} in {file}"));
    found.fields.into_iter().filter_map(|f| f.name).collect()
}

/// Fields `inherit` assigns as `self.<field> = …`.
fn assigned_by_inherit() -> BTreeSet<String> {
    struct Assigns(BTreeSet<String>, bool);
    impl<'ast> Visit<'ast> for Assigns {
        fn visit_impl_item_fn(&mut self, f: &'ast syn::ImplItemFn) {
            let inside = f.sig.ident == "inherit";
            let was = std::mem::replace(&mut self.1, inside);
            syn::visit::visit_impl_item_fn(self, f);
            self.1 = was;
        }
        fn visit_expr_assign(&mut self, e: &'ast syn::ExprAssign) {
            if self.1 {
                if let syn::Expr::Field(field) = e.left.as_ref() {
                    if let (syn::Expr::Path(base), syn::Member::Named(name)) =
                        (field.base.as_ref(), &field.member)
                    {
                        if base.path.is_ident("self") {
                            self.0.insert(name.to_string());
                        }
                    }
                }
            }
            syn::visit::visit_expr_assign(self, e);
        }
    }
    let mut v = Assigns(BTreeSet::new(), false);
    v.visit_file(&ws().file(HOST).tree(View::Prod));
    v.0
}

#[test]
fn every_builder_field_is_classified_once() {
    let fields = struct_fields(BUILDER, "Builder");
    guards::allow::non_vacuous("delegation::builder_fields", fields.len(), 40);
    let listed: Vec<&str> = INHERITED
        .iter()
        .chain(OWN)
        .chain(NOT_FOR_DELEGATES)
        .map(|(f, _)| *f)
        .collect();
    let mut problems = Vec::new();
    for field in &fields {
        match listed.iter().filter(|l| *l == field).count() {
            1 => {}
            0 => problems.push(format!(
                "`Builder::{field}` is on none of INHERITED, OWN, NOT_FOR_DELEGATES"
            )),
            n => problems.push(format!("`Builder::{field}` is listed {n} times")),
        }
    }
    for name in &listed {
        if !fields.contains(*name) {
            problems.push(format!(
                "`{name}` is listed but `Builder` has no such field"
            ));
        }
    }
    assert!(
        problems.is_empty(),
        "{problems:#?}\nclassify it in tests/guards/tests/delegation.rs"
    );
}

#[test]
fn the_inherited_list_is_host_contracts_field_for_field() {
    let mut expected = BTreeSet::new();
    for field in struct_fields(HOST, "HostContracts") {
        if field == "host_layers" {
            expected.extend(
                struct_fields(HOST, "HostLayers")
                    .into_iter()
                    .map(|f| format!("host_layers.{f}")),
            );
        } else {
            expected.insert(field);
        }
    }
    let listed: BTreeSet<String> = INHERITED.iter().map(|(_, h)| h.to_string()).collect();
    assert_eq!(
        listed, expected,
        "INHERITED and HostContracts disagree: a contract no builder field takes, or a \
         field listed as inherited that HostContracts does not carry"
    );
}

#[test]
fn inherit_sets_every_inherited_builder_field() {
    let assigned = assigned_by_inherit();
    let wanted: BTreeSet<String> = INHERITED.iter().map(|(b, _)| b.to_string()).collect();
    assert_eq!(
        assigned, wanted,
        "`Builder::inherit` must set exactly the INHERITED builder fields"
    );
}

const LATE_BINDING_CRATES: &[&str] = &["agents", "team"];

/// Late-bound fields still waiting to go, as `(file, struct.field, why)`.
/// Only shrinks.
const LATE_BOUND_RATCHET: &[(&str, &str, &str)] = &[];

const NO_SETTERS_ON: &[&str] = &["AgentTool", "Delegator"];

#[test]
fn delegation_binds_nothing_late() {
    let mut problems = Vec::new();
    let mut ratcheted = BTreeSet::new();
    let mut scanned = 0;
    for f in ws()
        .production()
        .filter(|f| LATE_BINDING_CRATES.contains(&f.package.as_str()))
    {
        scanned += 1;
        for st in scan::structs(f, View::Prod) {
            if st.name == "SetLater" {
                problems.push(format!("{}: `SetLater` is back", st.hit));
            }
            for field in &st.fields {
                let ty: String = field.ty.chars().filter(|c| !c.is_whitespace()).collect();
                if !ty.contains("RwLock<Option<") {
                    continue;
                }
                let key = format!("{}.{}", st.name, field.name.as_deref().unwrap_or("?"));
                if LATE_BOUND_RATCHET
                    .iter()
                    .any(|(file, entry, _)| *file == f.rel && *entry == key)
                {
                    ratcheted.insert(key);
                } else {
                    problems.push(format!(
                        "{}: `{key}` is a `RwLock<Option<…>>` filled in after construction; \
                         take the value when the struct is built",
                        st.hit
                    ));
                }
            }
        }
        for imp in scan::impls(f, View::Prod) {
            if imp.trait_path.is_some() || !NO_SETTERS_ON.contains(&imp.self_ty.as_str()) {
                continue;
            }
            for item in imp.items.iter().filter(|i| i.starts_with("set_")) {
                problems.push(format!(
                    "{}: `{}::{item}` sets what the delegator captured at construction",
                    imp.hit, imp.self_ty
                ));
            }
        }
    }
    guards::allow::non_vacuous("delegation::late_binding_files", scanned, 10);
    for (_, entry, _) in LATE_BOUND_RATCHET {
        if !ratcheted.contains(*entry) {
            problems.push(format!(
                "`{entry}` is no longer late-bound: take it off LATE_BOUND_RATCHET"
            ));
        }
    }
    assert!(problems.is_empty(), "{problems:#?}");
}

/// Enums in `base` matched exhaustively across crates, so without
/// `#[non_exhaustive]`: `(enum, file, why)`. D5's base-wide list takes these
/// over when it lands.
const CLOSED_ENUMS: &[(&str, &str, &str)] = &[(
    "DelegateMode",
    "crates/core/src/interface/delegate.rs",
    "the engine's one dispatch, `agents::delegate::Delegator::delegate`, matches it exhaustively",
)];

#[test]
fn closed_enums_are_not_non_exhaustive() {
    for (name, file, _) in CLOSED_ENUMS {
        let found = scan::enums(ws().file(file), View::Prod)
            .into_iter()
            .find(|e| e.name == *name)
            .unwrap_or_else(|| panic!("CLOSED_ENUMS names {name}, which {file} no longer has"));
        assert!(
            !found.attrs.iter().any(|a| a.contains("non_exhaustive")),
            "{}: `{name}` is on CLOSED_ENUMS; a `#[non_exhaustive]` would make every \
             match on it across crates take a `_` arm",
            found.hit
        );
    }
}

/// `match` expressions with a `DelegateMode::` pattern, and whether one of
/// their arms is a wildcard.
#[derive(Default)]
struct ModeMatches(Vec<(proc_macro2::Span, bool)>);

fn names_mode(p: &syn::Pat) -> bool {
    struct Finds(bool);
    impl<'ast> Visit<'ast> for Finds {
        fn visit_path(&mut self, path: &'ast syn::Path) {
            if path.segments.iter().any(|s| s.ident == "DelegateMode") {
                self.0 = true;
            }
            syn::visit::visit_path(self, path);
        }
    }
    let mut f = Finds(false);
    f.visit_pat(p);
    f.0
}

fn is_catch_all(p: &syn::Pat) -> bool {
    match p {
        syn::Pat::Wild(_) => true,
        syn::Pat::Ident(i) => i.subpat.is_none(),
        _ => false,
    }
}

impl<'ast> Visit<'ast> for ModeMatches {
    fn visit_expr_match(&mut self, m: &'ast syn::ExprMatch) {
        if m.arms.iter().any(|a| names_mode(&a.pat)) {
            let wild = m.arms.iter().any(|a| is_catch_all(&a.pat));
            self.0.push((m.match_token.span, wild));
        }
        syn::visit::visit_expr_match(self, m);
    }
}

#[test]
fn every_match_on_a_delegate_mode_names_each_mode() {
    let mut matches = 0;
    let mut problems = Vec::new();
    for f in ws().production() {
        if !f.text.contains("DelegateMode") {
            continue;
        }
        let mut v = ModeMatches::default();
        v.visit_file(&f.tree(View::Prod));
        for (span, wild) in v.0 {
            matches += 1;
            if wild {
                problems.push(format!(
                    "{}: a `match` on `DelegateMode` with a catch-all arm",
                    f.hit(span)
                ));
            }
        }
    }
    guards::allow::non_vacuous("delegation::mode_matches", matches, 1);
    assert!(problems.is_empty(), "{problems:#?}");
}

#[test]
fn agents_register_tasks_only_through_a_lease() {
    let mut problems = Vec::new();
    let mut scanned = 0;
    for f in ws().production().filter(|f| f.package == "agents") {
        scanned += 1;
        for call in scan::calls(f, View::Prod) {
            let name = match &call.callee {
                scan::Callee::Path(p) => p.last().cloned(),
                scan::Callee::Method(m) => Some(m.clone()),
            };
            if name.as_deref() == Some("register_task") {
                problems.push(format!(
                    "{}: `register_task` outside a `TaskLease`",
                    call.hit
                ));
            }
        }
        for (path, hit) in scan::paths(f, View::Prod) {
            if path.iter().any(|s| s == "TaskScope") {
                problems.push(format!("{hit}: a `TaskScope`; take a `TaskLease`"));
            }
        }
    }
    guards::allow::non_vacuous("delegation::agents_files", scanned, 10);
    assert!(problems.is_empty(), "{problems:#?}");
}
