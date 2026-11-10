//! The protocol doc, the method table and the committed schema describe the
//! same methods, events and error codes.
//!
//! `docs/daemon_rpc_protocol.md` is what someone writes a client against.
//! When it drifts, both directions hurt and neither is visible from the
//! code: a documented method that does not exist fails at runtime with
//! METHOD_NOT_FOUND, and an implemented method nobody documented is a
//! capability clients never learn about. The same holds for §7's and §8's
//! event kinds and §9's error codes.
//!
//! The code side is read at run time — the method table
//! (`assembly::server::methods`), the event types' schema and
//! `ErrorCode::ALL` — and the doc side from its `#### \`name\`` headings and
//! tables. It cannot check that a documented *shape* is right, only that both
//! sides agree on what exists.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("daemon crate has a parent (repo root)")
        .to_path_buf()
}

/// Every method in the dispatch table.
fn dispatched_methods() -> Vec<String> {
    sorted(
        assembly::server::methods()
            .map(|m| m.name.to_string())
            .collect(),
    )
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
    let doc = std::fs::read_to_string(repo_root().join("docs/daemon_rpc_protocol.md")).unwrap();

    let dispatched = dispatched_methods();
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
    let doc = std::fs::read_to_string(repo_root().join("docs/daemon_rpc_protocol.md")).unwrap();

    let dispatched = dispatched_methods();
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

/// §9's error table and `ErrorCode::ALL` are the same set of `(code, name)`
/// pairs: a client cannot branch on a constant nothing ever returns, nor
/// handle one nobody told it about. `PARSE_ERROR` is described under the
/// table rather than in it, as `` `PARSE_ERROR (-32700)` ``.
fn documented_error_codes(doc: &str) -> Vec<String> {
    let section = doc
        .split("## 9. 错误码")
        .nth(1)
        .and_then(|s| s.split("### 9.1").next())
        .expect("the doc still has §9");
    let mut out: Vec<String> = section
        .split('`')
        .skip(1)
        .step_by(2)
        .filter_map(|span| {
            let (name, code) = span.split_once(" (")?;
            let code = code.strip_suffix(')')?;
            (code.starts_with("-32") && name.chars().all(|c| c.is_ascii_uppercase() || c == '_'))
                .then(|| format!("{code} {name}"))
        })
        .collect();
    for line in section.lines() {
        // The error table rows: `| -32015 | SESSION_BUSY | … |`
        let mut cells = line.split('|').map(str::trim);
        let (Some(""), Some(code), Some(name)) = (cells.next(), cells.next(), cells.next()) else {
            continue;
        };
        if !code.starts_with("-32")
            || name.is_empty()
            || !name.chars().all(|c| c.is_ascii_uppercase() || c == '_')
        {
            continue;
        }
        out.push(format!("{code} {name}"));
    }
    sorted(out)
}

#[test]
fn the_documented_error_codes_are_the_declared_ones() {
    let doc = std::fs::read_to_string(repo_root().join("docs/daemon_rpc_protocol.md")).unwrap();
    let declared = sorted(
        daemon::rpc::ErrorCode::ALL
            .iter()
            .map(|c| format!("{} {}", c.code(), c.name()))
            .collect(),
    );
    assert_eq!(documented_error_codes(&doc), declared);
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
    let mut required = dispatched_methods();
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

/// §7's table and the frames the daemon sends are the same whitelist.
///
/// Both directions fail differently. A documented kind with no arm is a
/// client waiting for a frame the engine will never send; an arm with no row
/// is a capability nobody knows to handle — and since §7 tells clients to
/// ignore kinds they do not know, one that every well-behaved client drops.
#[test]
fn every_stream_event_is_documented_and_every_documented_one_is_sent() {
    let doc = std::fs::read_to_string(repo_root().join("docs/daemon_rpc_protocol.md")).unwrap();

    let mut sent: Vec<String> = daemon::rpc::events::SessionEventPayload::KINDS
        .iter()
        .map(|k| k.to_string())
        .collect();
    sent.sort();
    let documented = documented_event_kinds(&doc);

    let undocumented: Vec<&String> = sent.iter().filter(|k| !documented.contains(k)).collect();
    let never_sent: Vec<&String> = documented.iter().filter(|k| !sent.contains(k)).collect();
    assert!(
        undocumented.is_empty() && never_sent.is_empty(),
        "§7's table and the `session.event` projection disagree.\n\
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
        "live::",
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
    let dirs = ["daemon/tests", "tests/live/tests", "tests/rpc_client/tests"];
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

/// Capability names from §6.1's table: the rows between its `| 能力 |` header
/// and the first line that is not a row.
fn documented_capabilities(doc: &str) -> Vec<String> {
    let table = doc
        .split_once("| 能力 | 意思 |")
        .expect("§6.1 still has the capability table")
        .1;
    table
        .lines()
        .skip(2)
        .take_while(|line| line.starts_with('|'))
        .filter_map(|line| line.split('`').nth(1).map(str::to_string))
        .collect()
}

#[test]
fn the_capability_table_lists_what_the_daemon_reports() {
    let doc = std::fs::read_to_string(repo_root().join("docs/daemon_rpc_protocol.md")).unwrap();
    let mut documented = documented_capabilities(&doc);
    let mut reported: Vec<String> = daemon::discovery::capabilities()
        .into_iter()
        .map(str::to_string)
        .collect();
    documented.sort();
    reported.sort();
    assert_eq!(documented, reported);
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v.dedup();
    v
}

fn schema_names(path: &[&str]) -> Vec<String> {
    let text = std::fs::read_to_string(repo_root().join("docs/schemas/rpc.schema.json")).unwrap();
    let root: serde_json::Value = serde_json::from_str(&text).unwrap();
    let mut node = &root;
    for key in path {
        node = &node[*key];
    }
    sorted(node.as_object().unwrap().keys().cloned().collect())
}

/// `docs/schemas/rpc.schema.json` lists the methods `dispatch` answers.
/// `daemon.auth` is the connection handshake and is in none of the three.
#[test]
fn the_rpc_schema_lists_the_dispatched_methods_and_the_documented_ones() {
    let doc = std::fs::read_to_string(repo_root().join("docs/daemon_rpc_protocol.md")).unwrap();
    let documented: Vec<String> = documented_methods(&doc)
        .into_iter()
        .filter(|m| !UNDOCUMENTED_BY_DESIGN.contains(&m.as_str()))
        .collect();

    assert_eq!(
        schema_names(&["methods"]),
        dispatched_methods(),
        "committed schema is out of date"
    );
    assert_eq!(dispatched_methods(), documented, "dispatch vs protocol doc");
}

fn documented_daemon_event_kinds(doc: &str) -> Vec<String> {
    let section = doc
        .split("## 8. 异步通知")
        .nth(1)
        .and_then(|s| s.split("## 9. 错误码").next())
        .expect("the doc still has §8");
    let table = section
        .split("\n\n")
        .find(|block| block.trim_start().starts_with("| `kind`"))
        .expect("§8 still opens with the kind table");
    sorted(
        table
            .lines()
            .filter(|line| line.starts_with("| `"))
            .filter_map(|line| line.split('`').nth(1).map(str::to_string))
            .filter(|kind| kind != "kind")
            .collect(),
    )
}

#[test]
fn the_rpc_schema_lists_the_documented_event_kinds() {
    let doc = std::fs::read_to_string(repo_root().join("docs/daemon_rpc_protocol.md")).unwrap();
    let to_vec = |kinds: &[&str]| sorted(kinds.iter().map(|k| k.to_string()).collect());

    assert_eq!(
        schema_names(&["notifications", "session.event", "kinds"]),
        to_vec(assembly::server::schema::session_event_kinds())
    );
    assert_eq!(
        to_vec(assembly::server::schema::session_event_kinds()),
        documented_event_kinds(&doc)
    );
    assert_eq!(
        schema_names(&["notifications", "daemon.event", "kinds"]),
        sorted(assembly::server::schema::daemon_event_kinds())
    );
    assert_eq!(
        sorted(assembly::server::schema::daemon_event_kinds()),
        documented_daemon_event_kinds(&doc)
    );
}

#[test]
fn the_rpc_schema_error_codes_are_the_declared_ones() {
    let text = std::fs::read_to_string(repo_root().join("docs/schemas/rpc.schema.json")).unwrap();
    let schema: serde_json::Value = serde_json::from_str(&text).unwrap();
    let in_schema = sorted(
        schema["errors"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| format!("{} {}", e["code"], e["name"].as_str().unwrap()))
            .collect(),
    );
    let declared = sorted(
        daemon::rpc::ErrorCode::ALL
            .iter()
            .map(|c| format!("{} {}", c.code(), c.name()))
            .collect(),
    );
    assert_eq!(in_schema, declared);
}

/// A method of an optional module is listed whether or not the module is
/// compiled in, and says which module it is.
#[test]
fn every_optional_module_s_methods_are_listed_with_their_module() {
    let modules: Vec<(&str, Option<&str>)> = assembly::server::methods()
        .map(|m| (m.name, m.module))
        .collect();
    for (name, module) in &modules {
        let expected = if name.starts_with("mcp.") {
            Some("mcp")
        } else if *name == "session.startAgent" {
            Some("agents")
        } else {
            None
        };
        assert_eq!(*module, expected, "{name}");
    }
    let text = std::fs::read_to_string(repo_root().join("docs/schemas/rpc.schema.json")).unwrap();
    let schema: serde_json::Value = serde_json::from_str(&text).unwrap();
    for (name, module) in modules {
        assert_eq!(schema["methods"][name]["module"].as_str(), module, "{name}");
    }
}
