//! The session pool does not know the wire (D4 G1, G2a).
//!
//! `session_pool` is a domain layer: it takes and returns the types in
//! `session_pool::api`, and `rpc` and `server` translate to and from JSON-RPC.
//! Rust cannot make a sibling module invisible while keeping it public for the
//! daemon's tests, so the direction is held by reading the source.

use std::collections::BTreeMap;

use guards::allow::{self, Finding};
use guards::{modgraph, par_flat_map, ratchet, scan, ws, SourceFile, View};

const POOL_DIR: &str = "crates/assembly/src/session_pool/";
const WIRE_MODULES: &[&str] = &["rpc", "server"];
const WIRE_IDENTS: &[&str] = &[
    "RpcResponse",
    "StreamFrame",
    "codes",
    "Client",
    "ConnectionId",
    "FrameSink",
];
const JSON_DATA: &str = "ratchet/layering.txt";

fn pool_files() -> impl Iterator<Item = &'static SourceFile> {
    ws().files()
        .filter(|f| f.package == "assembly" && f.rel.starts_with(POOL_DIR))
}

/// `Initiator::Client` is a variant of the pool's own enum, not the wire's `Client`.
fn is_type_name(segment: &str) -> bool {
    segment.starts_with(|c: char| c.is_ascii_uppercase())
}

fn wire_references(f: &SourceFile) -> Vec<Finding> {
    let mut found = Vec::new();
    for (path, hit) in modgraph::module_refs(f, View::Full) {
        if path
            .first()
            .is_some_and(|m| WIRE_MODULES.contains(&m.as_str()))
        {
            found.push(Finding {
                key: format!("{} -> {}", f.rel, path.join("::")),
                hit,
            });
        }
    }
    for (segments, hit) in scan::paths(f, View::Full) {
        let named = segments.iter().enumerate().find(|(i, s)| {
            WIRE_IDENTS.contains(&s.as_str()) && !(*i > 0 && is_type_name(&segments[i - 1]))
        });
        if let Some((_, ident)) = named {
            found.push(Finding {
                key: format!("{} -> {ident}", f.rel),
                hit,
            });
        }
    }
    found
}

#[test]
fn the_session_pool_does_not_reach_into_rpc_or_server() {
    allow::non_vacuous("layering::session_pool", pool_files().count(), 30);
    let found = par_flat_map(pool_files(), wire_references);
    allow::check(
        "layering::NO_WIRE_IN_POOL",
        found,
        &[],
        "move the wire-side work into `rpc/` or `server/` and give the pool a type in `session_pool/api`",
    );
}

#[test]
fn json_values_in_the_pool_are_only_where_settings_are_edited() {
    let measured: BTreeMap<String, usize> = par_flat_map(
        pool_files().filter(|f| !f.test_only),
        |f| match scan::macro_calls(f, View::Prod, "json").len() {
            0 => Vec::new(),
            n => vec![(format!("json! {}", f.rel), n)],
        },
    )
    .into_iter()
    .collect();
    allow::non_vacuous("layering::pool_json", measured.len(), 1);
    ratchet::check(JSON_DATA, measured, 0);
}
