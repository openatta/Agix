//! The protocol doc and the code describe the same set of methods, and the
//! same set of stream events.
//!
//! `docs/daemon_rpc_protocol.md` is what someone writes a client against.
//! When it drifts, both directions hurt and neither is visible from the
//! code: a documented method that does not exist fails at runtime with
//! METHOD_NOT_FOUND, and an implemented method nobody documented is a
//! capability clients never learn about.
//!
//! Both had happened. At the time this test was written the doc described
//! fifteen methods with no dispatch arm — `daemon.ping` among them, the one
//! it recommends for keepalive — and eleven methods were implemented with
//! no entry in the doc.
//!
//! The same drift is possible one level down, in the events: §7's table is a
//! whitelist of `session.event` kinds, and the whitelist in the code is the
//! `match` in `run_turn`'s forwarding loop. Nothing compared them, so an
//! event could be documented and never sent — a client waiting forever for a
//! frame that does not exist — or sent and never documented. The `compact`
//! kind was added with nothing checking either direction.
//!
//! Source-text scanning: match arms out of `dispatch`, `#### \`name\``
//! headings out of §6, `AgentEvent` arms out of the forwarding loop, table
//! rows out of §7. It cannot check that a documented *shape* is right,
//! only that both sides agree on what exists. That is the failure that kept
//! happening.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("daemon crate has a parent (repo root)")
        .to_path_buf()
}

/// Method names from `dispatch`'s match arms.
///
/// Reads the arms rather than a hand-kept list, so a new method is covered
/// by this test the moment it is dispatchable — a list would need the same
/// discipline the test exists to enforce.
fn dispatched_methods(server_rs: &str) -> Vec<String> {
    let body = server_rs
        .split_once("match req.method.as_str() {")
        .expect("dispatch still matches on req.method")
        .1;

    let mut out = Vec::new();
    for line in body.lines() {
        let line = line.trim();
        // `"a.b" => ...` and `"a.b" | "c.d" => ...`, with or without a guard.
        if !line.starts_with('"') || !line.contains("=>") && !line.contains('|') {
            continue;
        }
        let arm = line.split("=>").next().unwrap_or(line);
        for part in arm.split('|') {
            let part = part.trim();
            let Some(name) = part
                .strip_prefix('"')
                .and_then(|rest| rest.split('"').next())
            else {
                continue;
            };
            // A method name, not some other string literal in the arm.
            if name.contains('.') && !out.contains(&name.to_string()) {
                out.push(name.to_string());
            }
        }
        // The wildcard arm ends the table.
        if line.starts_with("_ =>") {
            break;
        }
    }
    out.sort();
    out
}

/// Method names documented as `#### \`name\`` headings.
///
/// A heading may name a pair that is documented together
/// (``#### `config.getProvider` / `config.setProvider` ``), so every
/// backticked name on the line counts.
fn documented_methods(doc: &str) -> Vec<String> {
    let mut out: Vec<String> = doc
        .lines()
        .filter(|line| line.starts_with("#### "))
        .flat_map(|line| {
            line.split('`')
                .skip(1)
                .step_by(2)
                .map(str::trim)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .filter(|name| name.contains('.'))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Methods the daemon answers that the doc does not describe.
///
/// `daemon.auth` is the one exception: it is the handshake, specified in
/// §5.1 as part of connecting rather than as a callable method, so it has no
/// §6 heading by design.
const UNDOCUMENTED_BY_DESIGN: &[&str] = &["daemon.auth"];

#[test]
fn every_dispatched_method_is_documented() {
    let root = repo_root();
    let server = std::fs::read_to_string(root.join("daemon/src/server.rs")).unwrap();
    let doc = std::fs::read_to_string(root.join("docs/daemon_rpc_protocol.md")).unwrap();

    let dispatched = dispatched_methods(&server);
    assert!(
        dispatched.len() > 10,
        "only found {dispatched:?} — the dispatch scan is broken, not the doc"
    );
    let documented = documented_methods(&doc);

    let missing: Vec<&String> = dispatched
        .iter()
        .filter(|m| !documented.contains(m) && !UNDOCUMENTED_BY_DESIGN.contains(&m.as_str()))
        .collect();

    assert!(
        missing.is_empty(),
        "these methods are dispatched but not in docs/daemon_rpc_protocol.md §6: {missing:?}\n\
         A capability clients never learn about is not much better than one that does not exist."
    );
}

#[test]
fn every_documented_method_is_dispatched() {
    let root = repo_root();
    let server = std::fs::read_to_string(root.join("daemon/src/server.rs")).unwrap();
    let doc = std::fs::read_to_string(root.join("docs/daemon_rpc_protocol.md")).unwrap();

    let dispatched = dispatched_methods(&server);
    let documented = documented_methods(&doc);
    assert!(
        documented.len() > 10,
        "only found {documented:?} — the doc scan is broken, not the code"
    );

    let phantom: Vec<&String> = documented
        .iter()
        .filter(|m| !dispatched.contains(m))
        .collect();

    assert!(
        phantom.is_empty(),
        "these methods are documented but have no dispatch arm: {phantom:?}\n\
         A client written against the doc gets METHOD_NOT_FOUND. Implement them, or move them \
         out of §6 into a clearly-marked not-implemented list."
    );
}

/// Error codes the doc lists must exist in `rpc::codes`, for the same reason:
/// a client cannot branch on a constant nothing ever returns.
#[test]
fn every_documented_error_code_exists() {
    let root = repo_root();
    let rpc = std::fs::read_to_string(root.join("daemon/src/rpc.rs")).unwrap();
    let doc = std::fs::read_to_string(root.join("docs/daemon_rpc_protocol.md")).unwrap();

    let mut missing = Vec::new();
    for line in doc.lines() {
        // The error table rows: `| -32015 | SESSION_BUSY | … |`
        let mut cells = line.split('|').map(str::trim);
        let (Some(""), Some(code), Some(name)) = (cells.next(), cells.next(), cells.next()) else {
            continue;
        };
        if !code.starts_with("-32") || !name.chars().all(|c| c.is_ascii_uppercase() || c == '_') {
            continue;
        }
        if name.is_empty() {
            continue;
        }
        let declaration = format!("{name}: i32 = {code};");
        if !rpc.contains(&declaration) {
            missing.push(format!("{name} ({code})"));
        }
    }

    assert!(
        missing.is_empty(),
        "these error codes are documented but not declared in daemon/src/rpc.rs: {missing:?}"
    );
}

/// Every method the daemon answers is invoked by some test.
///
/// The two tests above keep the document and the dispatch table agreeing
/// about what *exists*. Neither notices a method that exists, is documented,
/// and has nothing driving it — and `plugin.reload` was in exactly that
/// state: its only two call sites sat behind mutually exclusive `#[cfg]`s, so
/// no single build compiled either one in, and CI built both configurations
/// without running either.
///
/// A method counts as driven when a test names it as a JSON-RPC method
/// string, or calls the `rpc-client` wrapper that sends it — **in a file that
/// stands up a daemon**. That last part is the floor: without it the check
/// is satisfied by any file that merely mentions a name, and several files in
/// this corpus scan source text for a living. Naming a method in a source
/// scan is not sending it.
///
/// `daemon.auth` is checked here even though it never reaches `dispatch` —
/// it is the first message on every network connection, so a client that
/// cannot send it cannot send anything else either.
#[test]
fn every_method_a_client_can_send_is_driven_by_a_test() {
    let root = repo_root();
    let mut required =
        dispatched_methods(&std::fs::read_to_string(root.join("daemon/src/server.rs")).unwrap());
    required.push("daemon.auth".to_string());

    let wrappers = client_wrappers(&root);
    let corpus: Vec<(String, String)> = test_sources(&root)
        .into_iter()
        .filter(|(_, src)| starts_a_daemon(src))
        .collect();
    assert!(
        corpus.len() > 5,
        "the daemon-starting corpus is almost empty — the marker list is stale, not the tests"
    );

    let undriven: Vec<&String> = required
        .iter()
        .filter(|method| {
            let literal = format!("\"{method}\"");
            !corpus.iter().any(|(_, src)| {
                src.contains(&literal)
                    || wrappers
                        .iter()
                        .any(|(f, m)| m == *method && src.contains(&format!(".{f}(")))
            })
        })
        .collect();

    assert!(
        undriven.is_empty(),
        "these methods are dispatched but no test sends them: {undriven:?}\n\
         A method whose only proof is that it compiles is a method nobody has run."
    );
}

/// The `kind` each `AgentEvent` arm of `run_turn`'s forwarding loop emits.
///
/// Bounded to the loop rather than scanned over the file: `daemon.event`
/// frames carry a `kind` of their own and are not part of this whitelist, and
/// a scan that could not tell them apart would report the wrong drift.
fn forwarded_event_kinds(pool_rs: &str) -> Vec<String> {
    const ARM: &str = "Some(AgentEvent::";
    let start = pool_rs
        .find(ARM)
        .expect("run_turn still forwards `AgentEvent` arms");
    let loop_body = &pool_rs[start..];
    // The wildcard that ends the forwarding match — everything past it
    // belongs to some other part of the file.
    let end = loop_body
        .find("_ => continue,")
        .expect("the forwarding match no longer ends in a wildcard");
    let loop_body = &loop_body[..end];

    let mut out: Vec<String> = loop_body
        .split(ARM)
        .skip(1)
        .filter_map(|arm| {
            let at = arm.find("\"kind\"")?;
            let rest = arm[at + "\"kind\"".len()..].trim_start();
            let rest = rest.strip_prefix(':')?.trim_start();
            let value = rest.strip_prefix('"')?;
            Some(value.split('"').next()?.to_string())
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

/// `kind`s documented in §7's table — the first cell of each row, backticked.
fn documented_event_kinds(doc: &str) -> Vec<String> {
    let section = doc
        .split("## 7. 流式帧")
        .nth(1)
        .expect("the doc still has a §7");
    let table = section
        .split("\n\n")
        .find(|block| block.trim_start().starts_with("| `kind`"))
        .expect("§7 still opens with the kind table");

    let mut out: Vec<String> = table
        .lines()
        .filter(|line| line.starts_with("| `"))
        .filter_map(|line| line.split('`').nth(1).map(str::to_string))
        .filter(|kind| kind != "kind")
        .collect();
    out.sort();
    out.dedup();
    out
}

/// The `kind` of each `session.event` frame sent outside a turn — the ones
/// `StreamFrame`'s own constructors build (`subscription_dropped`, …) rather
/// than `run_turn`'s forwarding loop.
fn out_of_turn_event_kinds(rpc_rs: &str) -> Vec<String> {
    let start = rpc_rs
        .find("impl StreamFrame {")
        .expect("rpc.rs still has `impl StreamFrame`");
    let block = &rpc_rs[start..];
    let block = &block[..block.find("\n}\n").unwrap_or(block.len())];
    let mut out: Vec<String> = block
        .split("\"kind\":")
        .skip(1)
        .filter_map(|rest| {
            let value = rest.trim_start().strip_prefix('"')?;
            Some(value.split('"').next()?.to_string())
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

/// §7's table and the frames the daemon sends are the same whitelist.
///
/// Both directions fail differently. A documented kind with no arm is a
/// client waiting for a frame the engine will never send; an arm with no row
/// is a capability nobody knows to handle — and since §7 tells clients to
/// ignore kinds they do not know, one that every well-behaved client drops.
#[test]
fn every_stream_event_is_documented_and_every_documented_one_is_sent() {
    let root = repo_root();
    let pool = std::fs::read_to_string(root.join("daemon/src/session_pool.rs")).unwrap();
    let rpc = std::fs::read_to_string(root.join("daemon/src/rpc.rs")).unwrap();
    let doc = std::fs::read_to_string(root.join("docs/daemon_rpc_protocol.md")).unwrap();

    let mut sent = forwarded_event_kinds(&pool);
    sent.extend(out_of_turn_event_kinds(&rpc));
    sent.sort();
    sent.dedup();
    let documented = documented_event_kinds(&doc);

    assert!(
        sent.len() > 5,
        "only found {sent:?} — the forwarding scan is broken, not the doc"
    );
    let undocumented: Vec<&String> = sent.iter().filter(|k| !documented.contains(k)).collect();
    let never_sent: Vec<&String> = documented.iter().filter(|k| !sent.contains(k)).collect();
    assert!(
        undocumented.is_empty() && never_sent.is_empty(),
        "§7's table and `run_turn`'s forwarding match disagree.\n\
         sent but undocumented: {undocumented:?}\n\
         documented but never sent: {never_sent:?}"
    );
}

/// Whether this test file talks to a running daemon at all.
///
/// The check runs in the sound direction only: a file containing none of
/// these — the assembly, a raw socket, the typed client, the scenario harness
/// — cannot have sent anything, whatever names it contains. A file that
/// matches one still might not, and that is fine; the floor is meant to
/// exclude `rpc_smoke.rs`, whose own header says "pure data tests; no real
/// socket needed" and which was standing in as the evidence that
/// `session.create` gets exercised.
fn starts_a_daemon(src: &str) -> bool {
    const MARKERS: &[&str] = &[
        "assemble::pool",
        "DaemonRpcClient::connect",
        "daemon_harness::",
        "serve_unix",
    ];
    MARKERS.iter().any(|marker| src.contains(marker))
}

/// `rpc-client`'s typed wrappers, paired with the method each one sends, so a
/// test that calls `client.session_get(..)` counts as driving `session.get`.
fn client_wrappers(root: &Path) -> Vec<(String, String)> {
    let src = std::fs::read_to_string(root.join("tests/rpc_client/src/lib.rs"))
        .expect("the typed client exists");
    let mut out = Vec::new();
    // Split rather than slice a window: each chunk already ends where the
    // next wrapper begins, and this file has multi-byte comments that a
    // fixed byte length lands in the middle of.
    for chunk in src.split("pub async fn ").skip(1) {
        let Some(name_end) = chunk.find(['(', '<', ' ']) else {
            continue;
        };
        let name = &chunk[..name_end];
        // The first method string in the body is the one this wrapper sends.
        if let Some(method) = chunk
            .split('"')
            .skip(1)
            .step_by(2)
            .find(|s| s.contains('.') && !s.contains(' ') && !s.contains('/'))
        {
            out.push((name.to_string(), method.to_string()));
        }
    }
    out
}

/// Every test file that can drive a daemon.
///
/// This file is excluded: it names methods to reason about them and sends
/// none of them, which would make it evidence for itself.
fn test_sources(root: &Path) -> Vec<(String, String)> {
    let dirs = [
        "daemon/tests",
        "tests/daemon_harness/tests",
        "tests/rpc_client/tests",
    ];
    let mut out = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(root.join(dir)) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "rs")
                && path
                    .file_name()
                    .is_some_and(|n| n != "protocol_doc_matches_dispatch.rs")
            {
                if let Ok(src) = std::fs::read_to_string(&path) {
                    out.push((path.display().to_string(), src));
                }
            }
        }
    }
    assert!(out.len() > 5, "the test corpus scan found almost nothing");
    out
}
