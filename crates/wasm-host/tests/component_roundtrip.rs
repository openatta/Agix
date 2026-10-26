//! End-to-end against a real component.
//!
//! Everything else in this crate tests a piece in isolation. This builds the
//! fixture in `tests/fixtures/wasm_echo_plugin`, links it, and provokes it —
//! because the claims that matter (a spinning plugin is survivable, an
//! undeclared host is unreachable, state does not leak between calls) are
//! claims about the whole path, and each is exactly the kind of thing that
//! passes in a unit test and fails against a real guest.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use wasm_host::capabilities::ResolvedCapabilities;
use wasm_host::{CallFailure, PluginInstance, WasmEngine};

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/wasm_echo_plugin")
        .canonicalize()
        .expect("the fixture directory should exist")
}

/// Build the fixture on demand. A missing `wasm32-wasip2` target is the one
/// likely failure, so it gets its own message rather than a wall of cargo
/// output.
fn component_path() -> PathBuf {
    let dir = fixture_dir();
    let out = dir.join("target/wasm32-wasip2/release/wasm_echo_plugin.wasm");
    if out.exists() {
        return out;
    }
    let status = std::process::Command::new("cargo")
        .args(["build", "--release", "--target", "wasm32-wasip2"])
        .current_dir(&dir)
        .status()
        .expect("cargo should be runnable");
    assert!(
        status.success(),
        "could not build the fixture component. If the target is missing: \
         rustup target add wasm32-wasip2"
    );
    out
}

fn caps(net: Vec<String>, timeout_ms: u64) -> Arc<ResolvedCapabilities> {
    let c = plugin::manifest::Capabilities {
        net,
        timeout_ms,
        ..Default::default()
    };
    Arc::new(
        wasm_host::capabilities::resolve(&c, &std::env::temp_dir(), &std::env::temp_dir()).unwrap(),
    )
}

fn instance(caps: Arc<ResolvedCapabilities>) -> PluginInstance {
    let engine = WasmEngine::new().unwrap();
    let dir = fixture_dir();
    let component = engine.load(&component_path(), &dir).unwrap();
    PluginInstance::link(&engine, &component, "echo-plugin".into(), caps).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_real_component_reports_the_tools_it_exports() {
    let inst = instance(caps(vec![], 5_000));
    let tools = inst.list_tools(&CancellationToken::new()).await.unwrap();

    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    assert!(names.contains(&"echo"), "{names:?}");
    assert!(names.contains(&"spin"), "{names:?}");

    let echo = tools.iter().find(|t| t.name == "echo").unwrap();
    assert!(
        echo.read_only,
        "the component's own declaration comes through"
    );
    assert!(
        echo.doc.as_ref().is_some_and(|d| d.contains("verbatim")),
        "the long doc is what ToolSearch fetches on demand"
    );
    assert!(echo.input_schema.contains("\"text\""));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tool_call_round_trips_content_and_structured_output() {
    let inst = instance(caps(vec![], 5_000));
    let out = inst
        .call_tool(
            "echo",
            r#"{"text":"hello"}"#,
            "call-1",
            None,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(out.content, "hello");
    assert!(!out.is_error);
    assert_eq!(
        out.structured.as_deref(),
        Some(r#"{"echoed":"hello"}"#),
        "a plugin tool must be able to return both a rendering and a value"
    );
}

/// The claim the whole execution model rests on: a plugin that never returns
/// costs one call, not the process.
#[tokio::test(flavor = "multi_thread")]
async fn a_spinning_plugin_is_stopped_at_its_declared_timeout() {
    let inst = instance(caps(vec![], 150));
    let started = std::time::Instant::now();

    let err = inst
        .call_tool("spin", "{}", "call-spin", None, &CancellationToken::new())
        .await
        .unwrap_err();

    assert_eq!(err, CallFailure::TimedOut);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the deadline must actually interrupt, not merely be reported after the fact"
    );

    // And the engine is still usable afterwards.
    let out = inst
        .call_tool(
            "echo",
            r#"{"text":"still here"}"#,
            "call-after",
            None,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(out.content, "still here");
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_a_running_call_returns_promptly() {
    let inst = instance(caps(vec![], 60_000));
    let cancel = CancellationToken::new();
    let token = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        token.cancel();
    });

    let started = std::time::Instant::now();
    let err = inst
        .call_tool("spin", "{}", "call-cancel", None, &cancel)
        .await
        .unwrap_err();

    assert_eq!(err, CallFailure::Cancelled);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "cancellation must not wait out the plugin's timeout"
    );
}

/// A trap is a tool error, not a process problem — the store is discarded and
/// the next call proceeds.
#[tokio::test(flavor = "multi_thread")]
async fn a_trapping_tool_is_contained() {
    let inst = instance(caps(vec![], 5_000));
    let err = inst
        .call_tool(
            "explode",
            "{}",
            "call-boom",
            None,
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, CallFailure::Faulted(_)), "{err:?}");

    let out = inst
        .call_tool(
            "echo",
            r#"{"text":"survived"}"#,
            "call-next",
            None,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(out.content, "survived");
}

/// The capability list is enforced against the running guest, not just
/// against the manifest parser.
#[tokio::test(flavor = "multi_thread")]
async fn an_undeclared_host_is_unreachable_from_inside_the_component() {
    let inst = instance(caps(vec![], 5_000));
    let out = inst
        .call_tool(
            "fetch",
            r#"{"url":"https://example.com/"}"#,
            "call-fetch",
            None,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

    assert!(out.is_error, "an undeclared host must not be fetchable");
    assert!(
        out.content.contains("net"),
        "the guest should learn which capability would have allowed it: {}",
        out.content
    );
}

/// Instance-per-call means no linear memory survives a call. Anything that
/// must persist goes through the host's key-value namespace — which is how
/// the host stays able to see and clear it.
#[tokio::test(flavor = "multi_thread")]
async fn state_survives_only_through_the_host_namespace() {
    let inst = instance(caps(vec![], 5_000));
    let cancel = CancellationToken::new();

    let first = inst
        .call_tool("remember", r#"{"value":"one"}"#, "c1", None, &cancel)
        .await
        .unwrap();
    assert_eq!(
        first.content, "(none)",
        "the first call has nothing stored yet"
    );

    let second = inst
        .call_tool("remember", r#"{"value":"two"}"#, "c2", None, &cancel)
        .await
        .unwrap();
    assert_eq!(
        second.content, "one",
        "the previous call's value must come back through the host, not through memory"
    );

    inst.kv().clear();
    let third = inst
        .call_tool("remember", r#"{"value":"three"}"#, "c3", None, &cancel)
        .await
        .unwrap();
    assert_eq!(
        third.content, "(none)",
        "clearing the namespace is what unloading a plugin must be able to do"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn init_reports_a_configuration_the_plugin_refuses() {
    let inst = instance(caps(vec![], 5_000));
    let cancel = CancellationToken::new();

    inst.init(r#"{"ok":true}"#, &cancel).await.unwrap();

    let err = inst.init(r#"{"fail":true}"#, &cancel).await.unwrap_err();
    match err {
        CallFailure::Faulted(msg) => assert!(msg.contains("configuration"), "{msg}"),
        other => panic!("expected the plugin's own rejection, got {other:?}"),
    }
}

// ── The adapter: the same component, seen as an ordinary engine tool ──

mod as_a_tool {
    use super::*;
    use base::tool::{Tool, ToolContext, ToolResultContent};
    use wasm_host::WasmToolAdapter;

    async fn adapter_for(tool: &str, timeout_ms: u64) -> WasmToolAdapter {
        let inst = Arc::new(instance(caps(vec![], timeout_ms)));
        let defs = inst.list_tools(&CancellationToken::new()).await.unwrap();
        let def = defs.iter().find(|d| d.name == tool).expect("fixture tool");
        WasmToolAdapter::new(inst.clone(), "echo-plugin", def)
    }

    fn ctx() -> ToolContext {
        ToolContext::for_test(std::env::temp_dir())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_adapter_presents_the_component_as_a_named_deferred_tool() {
        let t = adapter_for("echo", 5_000).await;

        assert_eq!(t.name(), "plugin__echo-plugin__echo");
        assert!(
            t.is_deferred(),
            "a third-party schema is not worth a per-call token cost"
        );
        assert!(
            t.is_dynamic(),
            "list-tools is the authority and it can change"
        );
        assert_eq!(t.input_schema()["properties"]["text"]["type"], "string");
        assert_eq!(
            t.short_description().as_deref(),
            Some("Echo the `text` argument back"),
            "the one-liner is the component's own, and it is what ships every turn"
        );
    }

    /// `description` is what ships every turn; the long guide is only
    /// reachable through the on-demand path.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_long_guide_is_what_tool_search_would_fetch() {
        let t = adapter_for("echo", 5_000).await;
        let detailed = t
            .detailed_prompt(&base::tool::PromptContext::default())
            .await
            .expect("a documented tool has something to fetch");
        assert!(detailed.contains("verbatim"), "{detailed}");
        assert_ne!(detailed, t.description());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_call_through_the_adapter_returns_both_renderings() {
        let t = adapter_for("echo", 5_000).await;
        let r = t
            .call(
                serde_json::json!({"text": "through the adapter"}),
                ctx(),
                base::tool::ProgressSender::noop("t"),
            )
            .await
            .unwrap();

        assert!(!r.is_error);
        match &r.content {
            ToolResultContent::Text(s) => assert_eq!(s, "through the adapter"),
            other => panic!("expected text, got {other:?}"),
        }
        assert_eq!(
            r.structured_content.as_ref().unwrap()["echoed"],
            "through the adapter",
            "structured output must survive the trip into the engine's own type"
        );
    }

    /// A plugin's own tool failing is not the turn failing. The model sees an
    /// error result and can choose something else.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_timeout_becomes_an_error_result_not_a_turn_failure() {
        let t = adapter_for("spin", 150).await;
        let r = t
            .call(
                serde_json::json!({}),
                ctx(),
                base::tool::ProgressSender::noop("t"),
            )
            .await
            .expect("a plugin timing out must not fail the turn");
        assert!(r.is_error);
        match &r.content {
            ToolResultContent::Text(s) => assert!(s.contains("timeout"), "{s}"),
            other => panic!("expected text, got {other:?}"),
        }
    }

    /// Cancellation is the engine's own vocabulary, so it has to arrive as
    /// the engine's error rather than as a result the model would read as an
    /// answer.
    #[tokio::test(flavor = "multi_thread")]
    async fn cancellation_surfaces_as_the_engines_own_error() {
        let t = adapter_for("spin", 60_000).await;
        let c = ctx();
        let token = c.cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            token.cancel();
        });

        let err = t
            .call(
                serde_json::json!({}),
                c,
                base::tool::ProgressSender::noop("t"),
            )
            .await
            .expect_err("a cancelled call is not a result");
        assert!(matches!(err, base::error::ToolError::Cancelled), "{err:?}");
    }

    /// There is no per-tool validation hook — `Tool::validate_input` has no
    /// production caller — so the adapter only checks the shape the ABI
    /// requires, without reaching into the component.
    #[tokio::test(flavor = "multi_thread")]
    async fn validation_checks_the_shape_without_calling_the_component() {
        let t = adapter_for("echo", 5_000).await;
        assert!(t
            .validate_input(&serde_json::json!("a string"), &ctx())
            .await
            .is_err());
        assert!(t
            .validate_input(&serde_json::json!({}), &ctx())
            .await
            .is_ok());
    }

    /// A plugin does not get to vouch for itself; the user's rules decide.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_adapter_never_grants_its_own_permission() {
        let t = adapter_for("echo", 5_000).await;
        let d = t.check_permissions(&serde_json::json!({}), &ctx()).await;
        assert!(
            matches!(d, base::tool::PermissionDecision::Ask { .. }),
            "{d:?}"
        );
    }
}

// ── The event surface ──

mod as_an_event_subscriber {
    use super::*;
    use wasm_host::bindings::agix::plugin::types::HookDecision;

    #[tokio::test(flavor = "multi_thread")]
    async fn a_component_can_refuse_an_event_with_a_reason() {
        let inst = instance(caps(vec![], 5_000));
        let decision = inst
            .on_event(
                "PreToolUse",
                r#"{"tool_name":"Bash"}"#,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        match decision {
            HookDecision::Block(reason) => assert!(reason.contains("blocks"), "{reason}"),
            other => panic!("the fixture blocks PreToolUse, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_component_can_add_context_without_deciding_anything() {
        let inst = instance(caps(vec![], 5_000));
        let decision = inst
            .on_event("SessionStart", "{}", &CancellationToken::new())
            .await
            .unwrap();
        assert!(
            matches!(decision, HookDecision::AddContext(_)),
            "{decision:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_event_the_component_does_not_care_about_proceeds() {
        let inst = instance(caps(vec![], 5_000));
        let decision = inst
            .on_event("PostToolUse", "{}", &CancellationToken::new())
            .await
            .unwrap();
        assert!(matches!(decision, HookDecision::Proceed), "{decision:?}");
    }

    /// An event handler gets the same store treatment as a tool call, so a
    /// plugin cannot hold a turn hostage from inside a hook either.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_event_handler_is_bounded_by_the_same_deadline() {
        let inst = instance(caps(vec![], 60_000));
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = inst
            .on_event("PreToolUse", "{}", &cancel)
            .await
            .unwrap_err();
        assert_eq!(err, CallFailure::Cancelled);
    }
}

// ── Health: when to stop asking ──

mod health {
    use super::*;
    use base::tool::{Tool, ToolContext, ToolResultContent};
    use wasm_host::{health::FAULT_LIMIT, WasmToolAdapter};

    async fn adapter_for(inst: &Arc<wasm_host::PluginInstance>, tool: &str) -> WasmToolAdapter {
        let defs = inst.list_tools(&CancellationToken::new()).await.unwrap();
        let def = defs.iter().find(|d| d.name == tool).expect("fixture tool");
        WasmToolAdapter::new(inst.clone(), "echo-plugin", def)
    }

    /// Per-call isolation makes it *safe* to keep calling a broken plugin,
    /// not useful. After enough consecutive traps the adapter stops asking
    /// and says why, instead of handing the model the same failure forever.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_component_that_keeps_trapping_is_set_aside() {
        let inst = Arc::new(instance(caps(vec![], 5_000)));
        let boom = adapter_for(&inst, "explode").await;
        let echo = adapter_for(&inst, "echo").await;
        let ctx = || ToolContext::for_test(std::env::temp_dir());

        for _ in 0..FAULT_LIMIT {
            let r = boom
                .call(
                    serde_json::json!({}),
                    ctx(),
                    base::tool::ProgressSender::noop("t"),
                )
                .await
                .unwrap();
            assert!(r.is_error);
        }
        assert!(inst.health().is_broken());

        // Now even a tool that works is refused, because the plugin is what
        // was disabled — not the individual tool.
        let r = echo
            .call(
                serde_json::json!({"text": "hello"}),
                ctx(),
                base::tool::ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        assert!(r.is_error);
        match &r.content {
            ToolResultContent::Text(s) => {
                assert!(s.contains("disabled"), "{s}");
                assert!(
                    s.contains("echo-plugin"),
                    "the user needs to know which one: {s}"
                );
            }
            other => panic!("expected text, got {other:?}"),
        }
    }

    /// A plugin that fails once and then works is being used at its edges.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_recovered_component_is_not_penalised() {
        let inst = Arc::new(instance(caps(vec![], 5_000)));
        let boom = adapter_for(&inst, "explode").await;
        let echo = adapter_for(&inst, "echo").await;
        let ctx = || ToolContext::for_test(std::env::temp_dir());

        for _ in 0..FAULT_LIMIT * 2 {
            let _ = boom
                .call(
                    serde_json::json!({}),
                    ctx(),
                    base::tool::ProgressSender::noop("t"),
                )
                .await;
            let ok = echo
                .call(
                    serde_json::json!({"text": "fine"}),
                    ctx(),
                    base::tool::ProgressSender::noop("t"),
                )
                .await
                .unwrap();
            assert!(!ok.is_error, "the working tool must keep working");
        }
        assert!(!inst.health().is_broken());
    }

    /// A slow plugin is not a broken one; disabling it for being used as
    /// intended would be the worst possible reading of a timeout.
    #[tokio::test(flavor = "multi_thread")]
    async fn timing_out_repeatedly_does_not_disable_a_plugin() {
        let inst = Arc::new(instance(caps(vec![], 100)));
        let cancel = CancellationToken::new();
        for _ in 0..FAULT_LIMIT * 2 {
            let err = inst
                .call_tool("spin", "{}", "c", None, &cancel)
                .await
                .unwrap_err();
            assert_eq!(err, CallFailure::TimedOut);
        }
        assert!(
            !inst.health().is_broken(),
            "a plugin doing something slow has not misbehaved"
        );
    }
}
