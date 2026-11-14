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
//!
//! G-D3-3: a `ModelErrorKind` is made only where model errors are classified
//! — the model crate and the constructors beside the type; everything else
//! reads the kind it was handed.
//!
//! G-D3-5: a task the session pool spawns does not take the pool along —
//! neither an `Arc<SessionPool>` nor the result of an upgrade made before it
//! started — except as `HOLDS_THE_POOL` says.

use guards::allow::{self, Finding};
use guards::scan::{self, Callee};
use guards::{par_flat_map, ws, Hit, SourceFile, View};
use quote::ToTokens;
use syn::spanned::Spanned;
use syn::visit::Visit;

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
        "SessionCore::log",
        "Session: whether its log has everything it holds, whichever engine wrote it",
    ),
    (
        "SessionCore::last_failure",
        "Session: how its last failed turn failed, whichever engine ran it",
    ),
    (
        "EngineSlot::generation",
        "Engine: the number it attached to the state under",
    ),
    ("EngineSlot::input_tx", "Engine: its input channel"),
    ("EngineSlot::cancel", "Engine: stops it"),
    ("EngineSlot::end_reason", "Engine: why it was stopped"),
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
        "crates/runtime/src/agent/builder/build/wiring.rs",
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

/// Production code outside the model crate that makes a `ModelErrorKind` or
/// reads a provider's words for "too long". Comparing against a kind reads
/// it; anything else makes one.
const CLASSIFIED_AT: &[(&str, &str)] = &[];

/// Where a model error is classified: the model crate, and the constructors
/// next to the type.
fn classifies_model_errors(rel: &str) -> bool {
    rel.starts_with("crates/model/") || rel == "crates/core/src/interface/model.rs"
}

struct MadeKinds<'a> {
    f: &'a SourceFile,
    found: Vec<Hit>,
}

fn names_a_kind(path: &syn::Path) -> bool {
    let segments: Vec<_> = path.segments.iter().map(|s| s.ident.to_string()).collect();
    segments.len() >= 2 && segments[segments.len() - 2] == "ModelErrorKind"
}

impl<'ast> Visit<'ast> for MadeKinds<'_> {
    fn visit_expr_binary(&mut self, e: &'ast syn::ExprBinary) {
        if matches!(e.op, syn::BinOp::Eq(_) | syn::BinOp::Ne(_)) {
            for side in [&*e.left, &*e.right] {
                match side {
                    syn::Expr::Path(p) if names_a_kind(&p.path) => {}
                    other => self.visit_expr(other),
                }
            }
            return;
        }
        syn::visit::visit_expr_binary(self, e);
    }

    fn visit_expr_path(&mut self, e: &'ast syn::ExprPath) {
        let last = e.path.segments.last().map(|s| s.ident.to_string());
        if names_a_kind(&e.path) || last.as_deref() == Some("says_context_too_long") {
            self.found.push(self.f.hit(e.span()));
        }
        syn::visit::visit_expr_path(self, e);
    }

    fn visit_signature(&mut self, s: &'ast syn::Signature) {
        if s.ident == "says_context_too_long" {
            self.found.push(self.f.hit(s.ident.span()));
        }
        syn::visit::visit_signature(self, s);
    }
}

/// G-D3-3: a model error is classified once, in the model crate; the engine
/// and the pool read the kind it was given and never make one of their own.
#[test]
fn a_model_error_is_classified_in_one_place() {
    let files: Vec<_> = ws()
        .production()
        .filter(|f| !ws().package(&f.package).dir.starts_with("tests/"))
        .collect();
    allow::non_vacuous("session_state::CLASSIFIED_AT", files.len(), 200);
    let made: Vec<(bool, Finding)> = par_flat_map(files, |f| {
        let mut kinds = MadeKinds {
            f,
            found: Vec::new(),
        };
        kinds.visit_file(&f.tree(View::Prod));
        let classifying = classifies_model_errors(&f.rel);
        kinds
            .found
            .into_iter()
            .map(|hit| {
                (
                    classifying,
                    Finding {
                        key: f.rel.clone(),
                        hit,
                    },
                )
            })
            .collect()
    });
    let (classifying, elsewhere): (Vec<_>, Vec<_>) = made.into_iter().partition(|(at, _)| *at);
    allow::non_vacuous("session_state::CLASSIFIED_AT", classifying.len(), 20);
    allow::check(
        "session_state::CLASSIFIED_AT",
        elsewhere.into_iter().map(|(_, finding)| finding).collect(),
        CLASSIFIED_AT,
        "classify the error in the model crate (`model::error`), where the status and the \
         provider's words are, and read `ModelError::kind` here; compare against a kind \
         with `==` or `matches!` rather than building one",
    );
}

/// Tasks the pool spawns that hold the pool itself. A task holding an
/// `Arc<SessionPool>` keeps the pool alive for as long as it runs; one that
/// runs as long as the pool does keeps it alive for ever.
const HOLDS_THE_POOL: &[(&str, &str)] = &[
    (
        "crates/assembly/src/session_pool/facade/goal.rs::SessionPool::start_goal_round",
        "a goal round is a `run_turn`, which needs the pool for as long as the \
         round runs; it ends with the round",
    ),
    (
        "crates/assembly/src/session_pool/parts/scheduler.rs::Scheduler::start",
        "one minute's due jobs run as turns of the pool; the task ends with them, \
         and the loop around it holds only a `Weak`",
    ),
    (
        "crates/assembly/src/session_pool/parts/mcp_hub/connect.rs::SessionPool::connect_mcp_servers_in_background",
        "the default project's first MCP connect, reported as daemon events; \
         the task ends once every server has connected or failed",
    ),
    (
        "crates/assembly/src/session_pool/parts/mcp_hub/auth.rs::SessionPool::watch_mcp_auth",
        "taking a server whose tokens stopped working off the pool's connections; \
         the task ends once it has",
    ),
];

/// What a function binds to the pool itself: names whose value is
/// `self.clone()` on an `Arc<Self>`, an upgraded `Weak`, or made from either.
fn strong_names(body: &syn::Block) -> std::collections::BTreeSet<String> {
    let mut strong = std::collections::BTreeSet::new();
    loop {
        let mut bindings = Bindings {
            strong: &strong,
            found: Vec::new(),
        };
        bindings.visit_block(body);
        let before = strong.len();
        strong.extend(bindings.found);
        if strong.len() == before {
            return strong;
        }
    }
}

struct Bindings<'a> {
    strong: &'a std::collections::BTreeSet<String>,
    found: Vec<String>,
}

impl Bindings<'_> {
    fn bind(&mut self, pat: &syn::Pat, init: &syn::Expr) {
        if let (syn::Pat::Tuple(pats), syn::Expr::Tuple(inits)) = (pat, init) {
            if pats.elems.len() == inits.elems.len() {
                for (pat, init) in pats.elems.iter().zip(&inits.elems) {
                    self.bind(pat, init);
                }
                return;
            }
        }
        let typed = matches!(pat, syn::Pat::Type(t)
            if t.ty.to_token_stream().to_string().contains("SessionPool"));
        if typed || yields_the_pool(init, self.strong) {
            self.found.extend(names_bound(pat));
        }
    }
}

/// Whether `e`'s value is the pool itself: an upgraded `Weak`, `self.clone()`
/// on an `Arc<Self>`, or a name already bound to the pool, passed through
/// `clone`, `as_ref`, `Some`, `?` and the like — not anything merely computed
/// with its help.
fn yields_the_pool(e: &syn::Expr, strong: &std::collections::BTreeSet<String>) -> bool {
    match e {
        syn::Expr::MethodCall(m) if m.method == "upgrade" => true,
        syn::Expr::MethodCall(m)
            if m.method == "clone"
                && matches!(&*m.receiver, syn::Expr::Path(p) if p.path.is_ident("self")) =>
        {
            true
        }
        syn::Expr::MethodCall(m)
            if ["clone", "as_ref", "unwrap", "expect", "cloned"]
                .iter()
                .any(|passes| m.method == passes) =>
        {
            yields_the_pool(&m.receiver, strong)
        }
        syn::Expr::Path(p) => {
            p.path
                .get_ident()
                .is_some_and(|i| strong.contains(&i.to_string()))
                || p.path.segments.iter().any(|s| s.ident == "SessionPool")
        }
        syn::Expr::Call(c) => {
            let through = matches!(&*c.func, syn::Expr::Path(p)
                if p.path.segments.last().is_some_and(|s| s.ident == "Some" || s.ident == "clone"));
            through && c.args.iter().any(|arg| yields_the_pool(arg, strong))
        }
        syn::Expr::Reference(r) => yields_the_pool(&r.expr, strong),
        syn::Expr::Paren(p) => yields_the_pool(&p.expr, strong),
        syn::Expr::Try(t) => yields_the_pool(&t.expr, strong),
        syn::Expr::Await(a) => yields_the_pool(&a.base, strong),
        _ => false,
    }
}

impl<'ast> Visit<'ast> for Bindings<'_> {
    fn visit_local(&mut self, l: &'ast syn::Local) {
        if let Some(init) = &l.init {
            self.bind(&l.pat, &init.expr);
        }
        syn::visit::visit_local(self, l);
    }

    fn visit_expr_let(&mut self, l: &'ast syn::ExprLet) {
        self.bind(&l.pat, &l.expr);
        syn::visit::visit_expr_let(self, l);
    }
}

/// Whether evaluating `e` — not the closures and async blocks in it, which
/// run later — takes hold of the pool: upgrades a `Weak`, clones `self`.
fn takes_the_pool_up_front(e: &syn::Expr) -> bool {
    struct Finds {
        hit: bool,
    }
    impl<'ast> Visit<'ast> for Finds {
        fn visit_expr_closure(&mut self, _: &'ast syn::ExprClosure) {}
        fn visit_expr_async(&mut self, _: &'ast syn::ExprAsync) {}
        fn visit_expr_method_call(&mut self, m: &'ast syn::ExprMethodCall) {
            let on_self = matches!(&*m.receiver, syn::Expr::Path(p) if p.path.is_ident("self"));
            if m.method == "upgrade" || (m.method == "clone" && on_self) {
                self.hit = true;
            }
            syn::visit::visit_expr_method_call(self, m);
        }
        fn visit_expr_path(&mut self, p: &'ast syn::ExprPath) {
            if p.path.segments.iter().any(|s| s.ident == "SessionPool") {
                self.hit = true;
            }
        }
    }
    let mut finds = Finds { hit: false };
    finds.visit_expr(e);
    finds.hit
}

fn names_bound(pat: &syn::Pat) -> Vec<String> {
    struct Names(Vec<String>);
    impl<'ast> Visit<'ast> for Names {
        fn visit_pat_ident(&mut self, p: &'ast syn::PatIdent) {
            self.0.push(p.ident.to_string());
            syn::visit::visit_pat_ident(self, p);
        }
    }
    let mut names = Names(Vec::new());
    names.visit_pat(pat);
    names.0
}

/// The names `e` binds anywhere inside it, and the names it reads — not
/// counting one only handed to `Arc::downgrade`, which takes a `Weak` along.
fn bound_and_read(
    e: &syn::Expr,
) -> (
    std::collections::BTreeSet<String>,
    std::collections::BTreeSet<String>,
) {
    #[derive(Default)]
    struct Names {
        bound: std::collections::BTreeSet<String>,
        read: std::collections::BTreeSet<String>,
    }
    impl<'ast> Visit<'ast> for Names {
        fn visit_pat_ident(&mut self, p: &'ast syn::PatIdent) {
            self.bound.insert(p.ident.to_string());
            syn::visit::visit_pat_ident(self, p);
        }
        fn visit_expr_path(&mut self, p: &'ast syn::ExprPath) {
            if let Some(name) = p.path.get_ident() {
                self.read.insert(name.to_string());
            }
        }
        fn visit_expr_call(&mut self, c: &'ast syn::ExprCall) {
            let downgrades = matches!(&*c.func, syn::Expr::Path(p)
                if p.path.segments.last().is_some_and(|s| s.ident == "downgrade"));
            if !downgrades {
                syn::visit::visit_expr_call(self, c);
            }
        }
    }
    let mut names = Names::default();
    names.visit_expr(e);
    (names.bound, names.read)
}

/// The spawns in one function, and which of them hold the pool.
struct Spawns<'a> {
    strong: std::collections::BTreeSet<String>,
    spawned: Vec<Hit>,
    holding: Vec<Hit>,
    f: &'a SourceFile,
}

impl Spawns<'_> {
    fn spawn(&mut self, args: &syn::punctuated::Punctuated<syn::Expr, syn::Token![,]>) {
        let Some(first) = args.first() else { return };
        let hit = self.f.hit(first.span());
        self.spawned.push(hit.clone());
        let holds = args.iter().any(|arg| {
            let (bound, read) = bound_and_read(arg);
            takes_the_pool_up_front(arg)
                || read
                    .iter()
                    .any(|name| self.strong.contains(name) && !bound.contains(name))
        });
        if holds {
            self.holding.push(hit);
        }
    }
}

impl<'ast> Visit<'ast> for Spawns<'_> {
    fn visit_expr_call(&mut self, c: &'ast syn::ExprCall) {
        if let syn::Expr::Path(p) = &*c.func {
            if p.path.segments.last().is_some_and(|s| s.ident == "spawn") {
                self.spawn(&c.args);
            }
        }
        syn::visit::visit_expr_call(self, c);
    }

    fn visit_expr_method_call(&mut self, m: &'ast syn::ExprMethodCall) {
        if m.method == "spawn" {
            self.spawn(&m.args);
        }
        syn::visit::visit_expr_method_call(self, m);
    }
}

/// Every function body in a file, the file's own `fn`s and its methods.
fn bodies(file: &syn::File) -> Vec<&syn::Block> {
    struct Bodies<'ast>(Vec<&'ast syn::Block>);
    impl<'ast> Visit<'ast> for Bodies<'ast> {
        fn visit_item_fn(&mut self, f: &'ast syn::ItemFn) {
            self.0.push(&f.block);
            syn::visit::visit_item_fn(self, f);
        }
        fn visit_impl_item_fn(&mut self, f: &'ast syn::ImplItemFn) {
            self.0.push(&f.block);
            syn::visit::visit_impl_item_fn(self, f);
        }
    }
    let mut bodies = Bodies(Vec::new());
    bodies.visit_file(file);
    bodies.0
}

/// G-D3-5: a task the pool spawns holds a `Weak` of it and upgrades it when
/// it needs the pool, rather than taking an `Arc<SessionPool>` along — or the
/// result of an upgrade made before it started. What a task may hold is the
/// session's own parts: its `SessionCore`, its `SessionHub`.
#[test]
fn a_task_the_pool_spawns_does_not_hold_the_pool() {
    let files: Vec<_> = ws()
        .production()
        .filter(|f| f.rel.starts_with("crates/assembly/src/session_pool/"))
        .collect();
    let scanned: Vec<(usize, Vec<Finding>)> = par_flat_map(files, |f| {
        let tree = f.tree(View::Prod);
        let mut spawned = 0;
        let mut found = Vec::new();
        for body in bodies(&tree) {
            let mut spawns = Spawns {
                strong: strong_names(body),
                spawned: Vec::new(),
                holding: Vec::new(),
                f,
            };
            spawns.visit_block(body);
            spawned += spawns.spawned.len();
            found.extend(spawns.holding.into_iter().map(|hit| Finding {
                key: place(f, &hit),
                hit,
            }));
        }
        vec![(spawned, found)]
    });
    let spawned: usize = scanned.iter().map(|(n, _)| n).sum();
    allow::non_vacuous("session_state::HOLDS_THE_POOL", spawned, 5);
    allow::check(
        "session_state::HOLDS_THE_POOL",
        scanned.into_iter().flat_map(|(_, found)| found).collect(),
        HOLDS_THE_POOL,
        "take `self.this.clone()` (a `Weak`) into the task and upgrade it there, when the \
         task needs the pool; or take only the session's `SessionCore` / `SessionHub`",
    );
}
