//! Two session pools in one process are as separate as two daemons, and a
//! `static` is what would break that: whatever it holds is shared by every
//! pool. So every `static` in production code — at module level, inside a
//! function, inside `thread_local!` — whose type is not plain data is listed
//! in `PROCESS_STATE` with what kind of state it is and why it may be
//! process-wide. A new one fails until it is moved onto the pool or listed; a
//! listed one that is gone fails until its entry is removed. `static mut` is
//! not allowed at all.
//!
//! Plain data is a number, `bool`, `char`, `&str`, and slices, arrays and
//! tuples of plain data. Test-support packages under `tests/` are not scanned.

use std::sync::OnceLock;

use guards::allow::{self, Finding};
use guards::scan::{self, Static};
use guards::{par_flat_map, ws, View};

#[derive(Clone, Copy, Debug)]
enum Kind {
    /// Set once and never changed: compiled patterns, tables, a stateless default.
    Constant,
    /// Hands out process-unique numbers.
    IdCounter,
    /// Remembers that something happened once in this process.
    OnceFlag,
    /// Something the process has exactly one of: a resource the operating
    /// system gives it, or a library's process-global registration.
    OsResource,
    /// Derived from the process environment, which every pool shares anyway.
    ProcessEnv,
    /// A cache or pool keyed by everything it depends on.
    SharedCache,
    /// Shared today and should not be; says what removes it. No entry needs
    /// it right now; the next one that does is listed under it.
    #[allow(dead_code)]
    NotYet(&'static str),
}

use Kind::*;

const PROCESS_STATE: &[(&str, &str, Kind, &str)] = &[
    ("crates/model/src/tokens.rs", "C", Constant, "the default counter: a stateless estimator"),
    ("crates/scene/src/code.rs", "SECTION_CACHE", SharedCache, "prompt sections keyed by a fingerprint of their inputs"),
    ("crates/core/src/memory.rs", "PATTERNS", Constant, "compiled regexes"),
    ("crates/core/src/dir_lock.rs", "LOCKS", OsResource, "serialises the holders of a settings or memory directory within this process, which every session of every pool may share; flock covers other processes"),
    ("crates/tools/src/bash/classify/mod.rs", "SUFFIXES", Constant, "a constant table"),
    ("crates/platform/src/network.rs", "PROXIED", SharedCache, "an HTTP client; it reads the process environment's proxy, which every pool shares anyway"),
    ("crates/platform/src/network.rs", "DIRECT", SharedCache, "an HTTP client with no per-pool configuration"),
    ("crates/platform/src/network.rs", "PUBLIC_ONLY", SharedCache, "an HTTP client whose resolver refuses internal addresses; the only thing it reads is the process environment's proxy"),
    ("crates/platform/src/sandbox.rs", "AVAILABLE", Constant, "whether this machine can sandbox at all"),
    ("crates/platform/src/sandbox.rs", "SERVED", OsResource, "the sockets and ports this process serves clients on, which no sandboxed child of any pool may reach, and the token files it writes for them, which none may read"),
    ("crates/core/src/interface/exec/env.rs", "NAMED_CREDENTIALS", ProcessEnv, "variables holding a provider key, kept out of every child's environment: the environment children inherit is the process's, which every pool shares"),
    ("crates/platform/src/orphans.rs", "ORPHANS", OsResource, "the process groups this process leads: an OS resource of the process, which an aborting panic hook must reach in full"),
    ("crates/assembly/src/telemetry_otel.rs", "PROCESS_EXPORTER", OsResource, "the OpenTelemetry exporter installs process-wide providers; see assemble::pool"),
    ("crates/history/src/lock.rs", "HELD", OsResource, "the session-log locks this process holds: an flock belongs to an open file, so every pool writing one state directory must share the one open file per log"),
    ("crates/ptc/src/engine.rs", "ORIGIN", Constant, "the epoch of a monotonic clock, fixed on first read"),
    ("crates/core/src/settings.rs", "KNOWN", Constant, "the settings keys, derived once from the schema"),
    ("crates/core/src/public_suffix.rs", "RULES", Constant, "the public suffix list compiled into the binary, parsed once"),
    ("crates/core/src/engine_exec.rs", "WARNED", OnceFlag, "says once per process that the engine's subprocesses run unconfined"),
    ("crates/tools/src/bash/sandbox.rs", "ONCE", OnceFlag, "sweeps stale session temp directories once per process"),
    ("crates/platform/src/network.rs", "ONCE", OnceFlag, "installs the process-wide TLS provider reqwest needs"),
    ("crates/assembly/src/telemetry_otel.rs", "TOLD", OnceFlag, "says once that a later OpenTelemetry endpoint is ignored"),
    ("crates/core/src/interface/tool.rs", "NEXT", IdCounter, "registration ids, unique across every registry in the process"),
    ("crates/history/src/path.rs", "NEXT", IdCounter, "temporary sibling names, distinct for every writer in the process"),
    ("crates/platform/src/sandbox.rs", "NEXT_FENCE", IdCounter, "the keys of SERVED's entries"),
    ("crates/plugin/src/state.rs", "SEQ", IdCounter, "temporary file names for saving the enable state"),
    ("crates/core/src/interface/catalog.rs", "POINTS", Constant, "the table of extension points"),
    ("crates/runtime/src/turn/user_turn.rs", "EMPTY", Constant, "an empty set to borrow before the frozen context exists"),
    ("crates/skills/src/tool.rs", "FENCED", Constant, "a compiled regex"),
    ("crates/skills/src/tool.rs", "INLINE", Constant, "a compiled regex"),
];

fn scanned() -> &'static [Static] {
    static FOUND: OnceLock<Vec<Static>> = OnceLock::new();
    FOUND.get_or_init(|| {
        let files = ws()
            .production()
            .filter(|f| !ws().package(&f.package).dir.starts_with("tests/"));
        par_flat_map(files, |f| scan::statics(f, View::Prod))
    })
}

fn key(s: &Static) -> String {
    format!("{} {}", s.hit.file, s.name)
}

fn is_plain(ty: &syn::Type) -> bool {
    match ty {
        syn::Type::Path(p) => {
            p.qself.is_none()
                && p.path.get_ident().is_some_and(|id| {
                    matches!(
                        id.to_string().as_str(),
                        "i8" | "i16"
                            | "i32"
                            | "i64"
                            | "i128"
                            | "isize"
                            | "u8"
                            | "u16"
                            | "u32"
                            | "u64"
                            | "u128"
                            | "usize"
                            | "f32"
                            | "f64"
                            | "bool"
                            | "char"
                    )
                })
        }
        syn::Type::Reference(r) => {
            r.mutability.is_none()
                && r.lifetime.as_ref().is_none_or(|l| l.ident == "static")
                && match &*r.elem {
                    syn::Type::Path(p) => p.path.is_ident("str"),
                    syn::Type::Slice(s) => is_plain(&s.elem),
                    _ => false,
                }
        }
        syn::Type::Array(a) => is_plain(&a.elem),
        syn::Type::Tuple(t) => t.elems.iter().all(is_plain),
        syn::Type::Paren(p) => is_plain(&p.elem),
        _ => false,
    }
}

fn plain(s: &Static) -> bool {
    syn::parse_str::<syn::Type>(&s.ty_tokens)
        .map(|ty| is_plain(&ty))
        .unwrap_or(false)
}

fn idents(s: &Static) -> Vec<String> {
    s.ty_tokens
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

#[test]
fn every_process_wide_static_is_listed_with_its_reason() {
    let found: Vec<Finding> = scanned()
        .iter()
        .filter(|s| !plain(s))
        .map(|s| Finding {
            key: key(s),
            hit: s.hit.clone(),
        })
        .collect();
    let keys: Vec<String> = PROCESS_STATE
        .iter()
        .map(|(file, name, _, _)| format!("{file} {name}"))
        .collect();
    let allowed: Vec<(&str, &str)> = keys
        .iter()
        .zip(PROCESS_STATE)
        .map(|(k, (_, _, _, why))| (k.as_str(), *why))
        .collect();
    allow::check(
        "process_state::PROCESS_STATE",
        found,
        &allowed,
        "a static holding anything but plain data is shared by every pool in the process: \
         move it onto the pool, or list it as `(file, name, Kind, reason)`",
    );
}

#[test]
fn no_static_is_mutable() {
    let found: Vec<String> = scanned()
        .iter()
        .filter(|s| s.is_mut)
        .map(|s| s.hit.to_string())
        .collect();
    assert!(
        found.is_empty(),
        "`static mut` is shared by every pool and unsynchronised; there is no list for it:\n  {}",
        found.join("\n  ")
    );
}

#[test]
fn each_listed_static_is_the_kind_it_says() {
    const MUTABLE: &[&str] = &[
        "Mutex",
        "RwLock",
        "RefCell",
        "Cell",
        "UnsafeCell",
        "Semaphore",
        "Notify",
        "Condvar",
        "mpsc",
        "watch",
        "broadcast",
    ];
    let mut wrong = Vec::new();
    for (file, name, kind, _) in PROCESS_STATE {
        for s in scanned()
            .iter()
            .filter(|s| s.hit.file == *file && s.name == *name)
        {
            let ids = idents(s);
            let last = ids.last().map(String::as_str).unwrap_or("");
            let bad = match kind {
                Constant => ids
                    .iter()
                    .find(|id| MUTABLE.contains(&id.as_str()) || id.starts_with("Atomic"))
                    .map(|id| format!("a Constant holds `{id}`")),
                IdCounter => (!matches!(last, "AtomicU32" | "AtomicU64" | "AtomicUsize"))
                    .then(|| "an IdCounter is an AtomicU32, AtomicU64 or AtomicUsize".to_string()),
                OnceFlag => (!matches!(last, "Once" | "AtomicBool"))
                    .then(|| "a OnceFlag is a Once or an AtomicBool".to_string()),
                NotYet(removed_by) => removed_by
                    .trim()
                    .is_empty()
                    .then(|| "a NotYet says what removes it".to_string()),
                OsResource | ProcessEnv | SharedCache => None,
            };
            if let Some(why) = bad {
                wrong.push(format!(
                    "{}\n      listed as {kind:?}, but {why}: `{}`",
                    s.hit, s.ty_tokens
                ));
            }
        }
    }
    assert!(
        wrong.is_empty(),
        "PROCESS_STATE in tests/guards/tests/process_state.rs names the wrong kind:\n  {}",
        wrong.join("\n  ")
    );
}

#[test]
fn the_scan_sees_the_statics_it_must() {
    let names: Vec<&str> = scanned().iter().map(|s| s.name.as_str()).collect();
    for expected in ["WARNED", "NEXT_FENCE", "ORIGIN"] {
        assert!(
            names.contains(&expected),
            "the scan misses `{expected}`: {names:?}"
        );
    }
    allow::non_vacuous("process_state::statics", scanned().len(), 25);
}
