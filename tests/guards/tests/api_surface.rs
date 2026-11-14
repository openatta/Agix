//! The interface-surface guards (D5 §9). `Tool`'s method set is PERM-1's, in
//! `permissions.rs`.
//!
//! A1: `PermissionPolicy` only decides and `PermissionGrants` only grants and
//! clears; a pipeline that needs more widens `PermissionRequest`, not the
//! traits. A2: no default method of a contract trait reaches itself through
//! other defaults, so an implementor that overrides none of them cannot
//! recurse forever. A3: a production tool holds no `dyn Tool`, so no tool
//! forwards to another and forgets a method. A4: the contracts keep no
//! per-thread state. A5: every public enum of `base` and of
//! `session_pool::api` is `#[non_exhaustive]`, or listed in `CLOSED_ENUMS`
//! with the exhaustive match that must break when a variant is added. A6: the structs an embedder
//! builds are `#[non_exhaustive]` (a ratcheted list). A7: the
//! settings types a file is read into hold nothing a file cannot set — such a
//! field is a load result, which `LoadedSettings` holds. A8: no production
//! code is `#[deprecated]`; the owner's rule is to change the callers instead.

use std::collections::{BTreeMap, BTreeSet};

use guards::allow::{self, Finding};
use guards::scan;
use guards::{par_flat_map, ws, Hit, SourceFile, TargetKind, View};
use syn::visit::Visit;

const INTERFACE_DIR: &str = "crates/core/src/interface/";
const PERMISSION_TRAITS: &str = "crates/core/src/interface/permission.rs";

const PERMISSION_SURFACE: &[(&str, &[&str])] = &[
    ("PermissionPolicy", &["decide"]),
    ("PermissionGrants", &["clear", "grant"]),
];

#[test]
fn the_permission_traits_decide_and_grant_and_nothing_else() {
    let traits = scan::traits(ws().file(PERMISSION_TRAITS), View::Prod);
    for (name, expected) in PERMISSION_SURFACE {
        let found = traits.iter().find(|t| t.name == *name).unwrap_or_else(|| {
            panic!("[api_surface::A1] no `trait {name}` in {PERMISSION_TRAITS}")
        });
        let mut actual = found.items.clone();
        actual.sort();
        assert_eq!(
            actual, *expected,
            "[api_surface::A1] `{name}` must have exactly {expected:?} (D5 §5.1): what a decision \
             needs goes into `PermissionRequest`, not onto the trait"
        );
    }
}

fn default_method_cycles(f: &SourceFile) -> Vec<String> {
    #[derive(Default)]
    struct SelfCalls(BTreeSet<String>);
    impl<'ast> Visit<'ast> for SelfCalls {
        fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
            if let syn::Expr::Path(p) = &*call.receiver {
                if p.path.is_ident("self") {
                    self.0.insert(call.method.to_string());
                }
            }
            syn::visit::visit_expr_method_call(self, call);
        }
        fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
            if let syn::Expr::Path(p) = &*call.func {
                if let [first, method] = p.path.segments.iter().collect::<Vec<_>>().as_slice() {
                    if first.ident == "Self" {
                        self.0.insert(method.ident.to_string());
                    }
                }
            }
            syn::visit::visit_expr_call(self, call);
        }
    }
    struct Traits<'a> {
        f: &'a SourceFile,
        out: Vec<String>,
    }
    impl<'ast> Visit<'ast> for Traits<'_> {
        fn visit_item_trait(&mut self, t: &'ast syn::ItemTrait) {
            let fns: Vec<&syn::TraitItemFn> = t
                .items
                .iter()
                .filter_map(|i| match i {
                    syn::TraitItem::Fn(func) => Some(func),
                    _ => None,
                })
                .collect();
            let names: BTreeSet<String> = fns.iter().map(|m| m.sig.ident.to_string()).collect();
            let mut calls: BTreeMap<String, Vec<String>> = BTreeMap::new();
            for func in &fns {
                let Some(body) = &func.default else { continue };
                let mut found = SelfCalls::default();
                found.visit_block(body);
                calls.insert(
                    func.sig.ident.to_string(),
                    found.0.into_iter().filter(|m| names.contains(m)).collect(),
                );
            }
            let at = self.f.hit(syn::spanned::Spanned::span(&t.ident));
            for cycle in cycles(&calls) {
                self.out.push(format!(
                    "{}:{}: trait {}: {}",
                    at.file,
                    at.line,
                    t.ident,
                    cycle.join(" → ")
                ));
            }
            syn::visit::visit_item_trait(self, t);
        }
    }
    let tree = f.tree(View::Prod);
    let mut traits = Traits { f, out: Vec::new() };
    traits.visit_file(&tree);
    traits.out
}

/// Each cycle once, starting from its smallest name.
fn cycles(calls: &BTreeMap<String, Vec<String>>) -> Vec<Vec<String>> {
    fn walk(
        calls: &BTreeMap<String, Vec<String>>,
        path: &mut Vec<String>,
        out: &mut BTreeSet<Vec<String>>,
    ) {
        let here = path.last().cloned().unwrap_or_default();
        for next in calls.get(&here).into_iter().flatten() {
            if let Some(at) = path.iter().position(|p| p == next) {
                let mut cycle = path[at..].to_vec();
                let start = cycle
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, m)| m.as_str())
                    .map_or(0, |(i, _)| i);
                cycle.rotate_left(start);
                cycle.push(cycle[0].clone());
                out.insert(cycle);
            } else if calls.contains_key(next) {
                path.push(next.clone());
                walk(calls, path, out);
                path.pop();
            }
        }
    }
    let mut out = BTreeSet::new();
    for start in calls.keys() {
        walk(calls, &mut vec![start.clone()], &mut out);
    }
    out.into_iter().collect()
}

#[test]
fn no_contract_default_reaches_itself_through_other_defaults() {
    let files: Vec<&SourceFile> = ws()
        .production()
        .filter(|f| f.rel.starts_with(INTERFACE_DIR))
        .collect();
    allow::non_vacuous("api_surface::A2", files.len(), 20);
    let found = par_flat_map(files, default_method_cycles);
    assert!(
        found.is_empty(),
        "[api_surface::A2] default methods that call each other round in a circle — an \
         implementor that overrides none of them recurses forever. Make one of them required:\n{}",
        found.join("\n")
    );
}

/// Whether `ty`'s tokens name `dyn Tool` (any path ending in `Tool`).
fn holds_dyn_tool(ty: &str) -> bool {
    let tokens: Vec<&str> = ty.split_whitespace().collect();
    tokens.iter().enumerate().any(|(i, t)| {
        *t == "dyn"
            && tokens[i + 1..]
                .iter()
                .take_while(|t| **t == "::" || t.chars().all(|c| c.is_alphanumeric() || c == '_'))
                .filter(|t| **t != "::")
                .last()
                == Some(&"Tool")
    })
}

#[test]
fn no_production_tool_wraps_another() {
    let files: Vec<&SourceFile> = ws().production().collect();
    let tools: BTreeSet<(String, String)> = par_flat_map(files.iter().copied(), |f| {
        scan::impls(f, View::Prod)
            .into_iter()
            .filter(|imp| {
                imp.trait_path
                    .as_ref()
                    .is_some_and(|t| t.last().is_some_and(|s| s == "Tool"))
            })
            .map(|imp| (f.package.clone(), imp.self_ty))
            .collect()
    })
    .into_iter()
    .collect();
    allow::non_vacuous("api_surface::A3", tools.len(), 30);
    let found: Vec<String> = par_flat_map(files, |f| {
        scan::structs(f, View::Prod)
            .into_iter()
            .filter(|s| tools.contains(&(f.package.clone(), s.name.clone())))
            .flat_map(|s| {
                s.fields
                    .iter()
                    .filter(|field| holds_dyn_tool(&field.ty))
                    .map(|field| {
                        format!(
                            "{}: {}.{}: {}",
                            s.hit,
                            s.name,
                            field.name.as_deref().unwrap_or("_"),
                            field.ty
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    });
    assert!(
        found.is_empty(),
        "[api_surface::A3] a tool holding another `dyn Tool` forwards to it, and a wrapper \
         forgets the methods added after it was written. Put what it adds on the tool's \
         `ToolSpec` or in a `ToolMiddleware` instead:\n{}",
        found.join("\n")
    );
}

#[test]
fn the_contracts_keep_no_per_thread_state() {
    let found = par_flat_map(
        ws().production()
            .filter(|f| f.rel.starts_with(INTERFACE_DIR)),
        |f| scan::macro_calls(f, View::Prod, "thread_local"),
    );
    assert!(
        found.is_empty(),
        "[api_surface::A4] `thread_local!` in a contract: an implementation runs on whichever \
         thread the runtime polls it on, so the state belongs on the session or the call:\n{}",
        found
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    );
}

const FILE_SHAPED: &[&str] = &["Settings", "SettingsFile"];
const SETTINGS_FILE: &str = "crates/core/src/settings.rs";

fn deprecated_attrs(f: &SourceFile) -> Vec<Hit> {
    struct Find<'a> {
        f: &'a SourceFile,
        out: Vec<Hit>,
    }
    impl<'ast> Visit<'ast> for Find<'_> {
        fn visit_attribute(&mut self, attr: &'ast syn::Attribute) {
            if attr.path().is_ident("deprecated") {
                self.out.push(self.f.hit(syn::spanned::Spanned::span(attr)));
            }
        }
    }
    let tree = f.tree(View::Prod);
    let mut find = Find { f, out: Vec::new() };
    find.visit_file(&tree);
    find.out
}

#[test]
fn no_production_code_is_marked_deprecated() {
    let found = par_flat_map(ws().production(), deprecated_attrs);
    assert!(
        found.is_empty(),
        "[api_surface::A8] `#[deprecated]` in production code — change the callers and \
         remove the old item instead of keeping it alive:\n{}",
        found
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    );
}

fn skips_serde(field: &syn::Field) -> bool {
    field.attrs.iter().any(|attr| {
        if !attr.path().is_ident("serde") {
            return false;
        }
        let mut skip = false;
        let _ = attr.parse_nested_meta(|meta| {
            skip |= meta.path.is_ident("skip") || meta.path.is_ident("skip_deserializing");
            if meta.input.peek(syn::Token![=]) {
                let _: syn::Expr = meta.value()?.parse()?;
            }
            Ok(())
        });
        skip
    })
}

#[test]
fn the_file_shaped_settings_types_hold_only_what_a_file_sets() {
    let f = ws().file(SETTINGS_FILE);
    let tree = f.tree(View::Prod);
    let mut seen = Vec::new();
    let mut found = Vec::new();
    for item in &tree.items {
        let syn::Item::Struct(s) = item else { continue };
        let name = s.ident.to_string();
        if !FILE_SHAPED.contains(&name.as_str()) {
            continue;
        }
        seen.push(name.clone());
        for field in &s.fields {
            if skips_serde(field) {
                found.push(format!(
                    "{}: {name}.{}",
                    f.hit(syn::spanned::Spanned::span(field)),
                    field
                        .ident
                        .as_ref()
                        .map_or(String::new(), ToString::to_string)
                ));
            }
        }
    }
    seen.sort();
    assert_eq!(
        seen, FILE_SHAPED,
        "[api_surface::A7] {SETTINGS_FILE} no longer defines {FILE_SHAPED:?} at the top level; \
         point the guard at where they moved"
    );
    assert!(
        found.is_empty(),
        "[api_surface::A7] a settings file cannot set these, so they belong on \
         `LoadedSettings`, not on the file-shaped types:\n{}",
        found.join("\n")
    );
}

const PROJECTED: &str = "projected variant by variant by `assembly::rpc::events` (D4 §3.4); a new \
     variant has to be decided there";
const PROTOCOL: &str =
    "the conversation's wire shape; the model adapters and the transcript convert every variant";
const SETTING: &str = "a settings value the deployment writes and the engine matches exhaustively";
const SANDBOX: &str =
    "what a confined command may reach; the sandbox profile compiler must place every variant";
const TRUST: &str =
    "a project-trust decision; what an untrusted tier may do is decided per variant";
const PERMISSION: &str = "a permission decision; every variant is decided by the policy";
const DELEGATE_ASK: &str =
    "part of a delegate request; a `Delegator` must honour every variant it is handed";
const HOST_ANSWER: &str = "a host's answer through a contract; the engine acts on every variant";
const HOST_INPUT: &str = "the host picks it; the engine must handle every variant it can be given";
const WIRE_CODE: &str = "a code on the wire; its mapping names every variant";

const CLOSED_ENUMS: &[(&str, &str)] = &[
    (
        "Admission",
        "whether the engine's git may start; a security decision",
    ),
    ("AgentEvent", PROJECTED),
    (
        "ApiType",
        "picks the wire protocol; the model adapters and the recorder match it exhaustively",
    ),
    ("ArchiveFormat", HOST_INPUT),
    ("AskRoute", DELEGATE_ASK),
    ("Backoff", HOST_ANSWER),
    ("BlockRole", WIRE_CODE),
    ("CacheEdit", PROTOCOL),
    ("CacheStrategy", WIRE_CODE),
    ("ConfigTierName", HOST_INPUT),
    (
        "Confinement",
        "whether the engine's subprocesses are sandboxed; a security decision",
    ),
    ("ContentBlock", PROTOCOL),
    ("DelegateEnd", HOST_ANSWER),
    ("DelegateMode", DELEGATE_ASK),
    ("Delegated", HOST_ANSWER),
    ("Divergence", SETTING),
    ("Effort", SETTING),
    ("ElicitOutcome", HOST_ANSWER),
    ("Enforcement", SANDBOX),
    (
        "EnvPlan",
        "what a subprocess inherits; the platform applies every variant",
    ),
    (
        "EnvScope",
        "what a subprocess inherits; the platform applies every variant",
    ),
    ("ErrorCode", WIRE_CODE),
    (
        "GitAccess",
        "what the engine's git may write; a security decision",
    ),
    ("ImageSource", PROTOCOL),
    ("ImportDecision", HOST_ANSWER),
    (
        "Initiator",
        "who asked for a turn decides whether it may cross a scene boundary",
    ),
    (
        "KeyFileNames",
        "a credential the sandbox masks; the mask compiler places every variant",
    ),
    ("McpIdentity", HOST_INPUT),
    ("McpRedirect", HOST_INPUT),
    ("MemberLifetime", DELEGATE_ASK),
    ("Message", PROTOCOL),
    ("MessageRole", PROTOCOL),
    ("ModelContentBlock", PROTOCOL),
    ("ModelEvent", PROTOCOL),
    ("NetworkMode", SANDBOX),
    ("NetworkModeConfig", SETTING),
    ("Order", TRUST),
    (
        "Origin",
        "who directed a request decides which hosts it may reach",
    ),
    (
        "OutputStream",
        "a process has stdout and stderr, nothing else",
    ),
    ("OutputTarget", HOST_ANSWER),
    ("PathStyle", SETTING),
    ("PermissionMode", PERMISSION),
    ("PermissionOutcome", PERMISSION),
    ("PluginScope", HOST_INPUT),
    ("ProjectKey", TRUST),
    ("ProjectTrust", TRUST),
    ("PromptAttachmentKind", HOST_INPUT),
    ("PromptContent", HOST_INPUT),
    ("PromptDecision", HOST_ANSWER),
    ("PromptDelivery", HOST_ANSWER),
    ("ProxyEndpoint", SANDBOX),
    ("ReadScope", SANDBOX),
    ("RecorderMode", SETTING),
    ("Recovery", HOST_ANSWER),
    ("Role", PROTOCOL),
    ("RuleBehavior", PERMISSION),
    ("RuleSource", PERMISSION),
    (
        "RunningStatus",
        "projected variant by variant into `TaskOutput`'s `status` by `tools::task_output`",
    ),
    ("SandboxMode", SANDBOX),
    (
        "Scope",
        "where a credential the sandbox masks lives; the mask compiler places every variant",
    ),
    ("ScriptOutcome", HOST_ANSWER),
    (
        "Secret",
        "a credential the sandbox masks; the mask compiler places every variant",
    ),
    ("SkillPrecedence", HOST_INPUT),
    (
        "SkillSource",
        "projected variant by variant into `commands.list`'s `source` by `runtime::commands`",
    ),
    ("Spending", HOST_ANSWER),
    ("StopReason", PROTOCOL),
    ("StopRecovery", HOST_ANSWER),
    ("StopsWith", DELEGATE_ASK),
    (
        "SubprocessEnvMode",
        "what a subprocess inherits; the platform applies every variant",
    ),
    ("SystemKind", PROTOCOL),
    ("TaskEvent", PROJECTED),
    ("TaskModelOverride", SETTING),
    ("TeamStageStatus", PROJECTED),
    ("ThinkingMode", SETTING),
    ("ThinkingModeConfig", SETTING),
    ("ThinkingToggle", SETTING),
    (
        "Tier",
        "a layer's tier decides what it may set; the layering decides every variant",
    ),
    (
        "TodoStatus",
        "the model's `TodoWrite` input; the tool counts every status",
    ),
    ("ToolPattern", HOST_INPUT),
    (
        "ToolResultContent",
        "a tool's result (`base::tool`) and its form in the conversation (`base::message`); \
         both are converted variant by variant",
    ),
    ("TrustLevel", TRUST),
    ("TurnStep", HOST_ANSWER),
    ("ValidationResult", HOST_ANSWER),
    ("Workdir", DELEGATE_ASK),
    (
        "Workspace",
        "whether a session has a project tier; decided per variant",
    ),
];

#[test]
fn every_public_enum_is_non_exhaustive_or_closed_for_a_reason() {
    let in_scope: Vec<&SourceFile> = ws()
        .production()
        .filter(|f| {
            (f.package == "base" && f.target == TargetKind::Lib)
                || f.rel.starts_with("crates/assembly/src/session_pool/api/")
        })
        .collect();
    let enums: Vec<(bool, Finding)> = par_flat_map(in_scope, |f| {
        scan::enums(f, View::Prod)
            .into_iter()
            .filter(|e| e.vis == "pub")
            .map(|e| {
                let open = e
                    .attrs
                    .iter()
                    .any(|a| a.replace(' ', "") == "#[non_exhaustive]");
                (
                    open,
                    Finding {
                        key: e.name,
                        hit: e.hit,
                    },
                )
            })
            .collect()
    });
    allow::non_vacuous("api_surface::CLOSED_ENUMS", enums.len(), 100);
    let closed = enums
        .into_iter()
        .filter(|(open, _)| !open)
        .map(|(_, f)| f)
        .collect();
    allow::check(
        "api_surface::CLOSED_ENUMS",
        closed,
        CLOSED_ENUMS,
        "mark the enum `#[non_exhaustive]`, or list it with who matches it exhaustively and why \
         that match must break when a variant is added (D5 §3.1)",
    );
}

/// A6's list, as (struct, file). A ratchet: a struct an embedder builds goes on
/// it when it gains `#[non_exhaustive]`, and nothing comes off except by the
/// struct's removal.
const NON_EXHAUSTIVE_STRUCTS: &[(&str, &str)] = &[
    ("AgentDef", "crates/plugin/src/manifest.rs"),
    ("CallTraits", "crates/core/src/interface/tool.rs"),
    ("CompactionConfig", "crates/core/src/settings.rs"),
    ("ExecutionParams", "crates/core/src/interface/scene.rs"),
    ("ExecutionSettings", "crates/core/src/settings.rs"),
    ("Grant", "crates/core/src/interface/permission.rs"),
    ("LoadedRule", "crates/core/src/settings/loaded.rs"),
    ("LoadedSettings", "crates/core/src/settings/loaded.rs"),
    ("McpPayload", "crates/plugin/src/manifest.rs"),
    ("ModelSettings", "crates/core/src/settings.rs"),
    ("ModuleSurface", "crates/core/src/interface/scene.rs"),
    ("PathSettings", "crates/core/src/settings.rs"),
    (
        "PermissionRequest",
        "crates/core/src/interface/permission.rs",
    ),
    ("PluginMeta", "crates/plugin/src/manifest.rs"),
    ("PluginsConfig", "crates/core/src/settings.rs"),
    ("ProgressUpdate", "crates/core/src/interface/tool.rs"),
    ("PromptContext", "crates/core/src/interface/tool.rs"),
    ("ReminderContext", "crates/core/src/interface/scene.rs"),
    ("SandboxConfig", "crates/core/src/settings.rs"),
    ("SceneSection", "crates/plugin/src/manifest.rs"),
    ("SceneSpec", "crates/core/src/interface/scene.rs"),
    ("ScenePromptContext", "crates/core/src/interface/scene.rs"),
    ("ScriptPayload", "crates/plugin/src/manifest.rs"),
    ("Settings", "crates/core/src/settings.rs"),
    ("SkillsSection", "crates/plugin/src/manifest.rs"),
    ("StartedAgent", "crates/core/src/interface/delegate.rs"),
    ("TokenBudget", "crates/core/src/interface/scene.rs"),
    ("ToolContext", "crates/core/src/interface/tool.rs"),
    ("ToolDef", "crates/core/src/interface/model.rs"),
    ("ToolResult", "crates/core/src/interface/tool.rs"),
    ("ToolSpec", "crates/core/src/interface/tool.rs"),
    ("ToolSurface", "crates/core/src/interface/scene.rs"),
];

const NON_EXHAUSTIVE_STRUCTS_AT_LEAST: usize = 32;

#[test]
fn the_structs_an_embedder_builds_are_non_exhaustive() {
    assert!(
        NON_EXHAUSTIVE_STRUCTS.len() >= NON_EXHAUSTIVE_STRUCTS_AT_LEAST,
        "[api_surface::A6] NON_EXHAUSTIVE_STRUCTS shrank to {}, below \
         {NON_EXHAUSTIVE_STRUCTS_AT_LEAST}: the list only grows (D5 §3.3)",
        NON_EXHAUSTIVE_STRUCTS.len()
    );
    let mut problems = Vec::new();
    for (name, file) in NON_EXHAUSTIVE_STRUCTS {
        let found = scan::structs(ws().file(file), View::Prod)
            .into_iter()
            .find(|s| s.name == *name && s.vis == "pub");
        match found {
            None => problems.push(format!(
                "`{name}` is no longer a public struct of {file}: drop it from the list, and \
                 lower NON_EXHAUSTIVE_STRUCTS_AT_LEAST only if the struct is gone"
            )),
            Some(s) => {
                if !s
                    .attrs
                    .iter()
                    .any(|a| a.replace(' ', "") == "#[non_exhaustive]")
                {
                    problems.push(format!(
                        "{}\n      `{name}` lost `#[non_exhaustive]`: a field added to it would \
                         break every embedder that builds one with a struct literal",
                        s.hit
                    ));
                }
            }
        }
    }
    assert!(
        problems.is_empty(),
        "[api_surface::A6] (list in tests/guards/tests/api_surface.rs)\n  {}",
        problems.join("\n  ")
    );
}
