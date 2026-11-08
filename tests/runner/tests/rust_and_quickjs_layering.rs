//! Where a Rust plugin and a QuickJS script share a point, Rust runs first in
//! a chain and innermost around a tool call, and a local script takes a name
//! Rust also registers — whichever of the two the builder was handed first.

use base::interface::prompt_registry::{InMemoryPromptRegistry, PromptRegistry, RegisteredBlock};
use base::interface::tool::{
    PermissionDecision, ProgressSender, PromptContext, Tool, ToolContext, ToolResult,
};
use base::interface::tool_middleware::{
    NextDispatch, ToolCall, ToolExec, ToolMiddleware, ToolOutcome,
};
use base::interface::tool_result::{ToolResultDraft, ToolResultTransformer};
use runtime::plugin::{Plugin, PluginSession, Registrar};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use test_runner::script_session::{drive, RustOrder, Session};
use test_runner::scripted_model::Reply;

const ORDERS: [RustOrder; 2] = [RustOrder::BeforeScripts, RustOrder::AfterScripts];

fn fixtures() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("tests/")
        .join("fixtures")
        .join("scripts")
}

fn calls_the_echo_tool() -> Vec<Reply> {
    vec![
        Reply::Tool {
            id: "call-1",
            name: "ScriptEcho",
            input: serde_json::json!({"say": "anything"}),
        },
        Reply::Text("done"),
    ]
}

struct Wraps;

impl ToolResultTransformer for Wraps {
    fn transform(&self, _call: &ToolCall, draft: &mut ToolResultDraft) {
        draft.text = format!("RUST({})", draft.text);
    }
}

struct Counts(Arc<AtomicUsize>);

#[async_trait::async_trait]
impl ToolMiddleware for Counts {
    async fn around(
        &self,
        call: &ToolCall,
        exec: &mut ToolExec,
        next: NextDispatch<'_>,
    ) -> ToolOutcome {
        self.0.fetch_add(1, Ordering::SeqCst);
        next.run(call, exec).await
    }
}

struct Contributes {
    counted: Arc<AtomicUsize>,
}

impl Plugin for Contributes {
    fn name(&self) -> &str {
        "contributes"
    }

    fn register(&self, _session: &PluginSession<'_>, r: &mut Registrar) {
        r.tool_result(Arc::new(Wraps))
            .tool_middleware(Arc::new(Counts(self.counted.clone())))
            .prompt_block(RegisteredBlock::system(
                "team.conventions",
                250,
                "RUST-BLOCK: the Rust plugin's conventions.",
            ));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_quickjs_result_policy_reads_what_the_rust_one_left() {
    for order in ORDERS {
        let tmp = tempfile::tempdir().expect("tempdir");
        let counted = Arc::new(AtomicUsize::new(0));
        let session = Session::new(fixtures(), &["echo something"], calls_the_echo_tool())
            .scene(Arc::new(scene::code::CodeScene::default()))
            .tool(Arc::new(ScriptEcho))
            .bind("tool_result.js", "tool.result", "onResult")
            .rust(order, move |b| b.plugin(Arc::new(Contributes { counted })));
        let ran = drive(tmp.path(), session).await;
        assert!(
            ran.holds("SCRIPT-TRACE-RESULT(ScriptEcho) RUST(echo: anything)"),
            "{order:?}: the QuickJS policy did not run after the Rust one: {:?}",
            ran.requests.last()
        );
    }
}

/// The script refuses the call; a Rust ring inside it never sees it. With a
/// script that lets the call through, the same ring does.
#[tokio::test(flavor = "multi_thread")]
async fn a_quickjs_ring_sits_outside_the_rust_ones() {
    for order in ORDERS {
        for (script, reaches_rust) in [("tool_around_deny.js", 0), ("tool_around_observe.js", 1)] {
            let tmp = tempfile::tempdir().expect("tempdir");
            let counted = Arc::new(AtomicUsize::new(0));
            let seen = counted.clone();
            let session = Session::new(fixtures(), &["echo something"], calls_the_echo_tool())
                .scene(Arc::new(scene::code::CodeScene::default()))
                .tool(Arc::new(ScriptEcho))
                .bind(script, "tool.around", "onAround")
                .rust(order, move |b| b.plugin(Arc::new(Contributes { counted })));
            drive(tmp.path(), session).await;
            assert_eq!(
                seen.load(Ordering::SeqCst),
                reaches_rust,
                "{order:?}, {script}: the Rust ring inside the QuickJS one saw the wrong calls"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_local_script_takes_a_block_name_a_rust_plugin_also_registers() {
    for order in ORDERS {
        let tmp = tempfile::tempdir().expect("tempdir");
        let counted = Arc::new(AtomicUsize::new(0));
        let session = Session::new(fixtures(), &["hello"], vec![Reply::Text("done")])
            .bind("prompt_block.js", "prompt.block", "onBlock")
            .rust(order, move |b| b.plugin(Arc::new(Contributes { counted })));
        let ran = drive(tmp.path(), session).await;
        assert!(
            ran.holds("SCRIPT-TRACE-BLOCK"),
            "{order:?}: the script's block is missing"
        );
        assert!(
            !ran.holds("RUST-BLOCK"),
            "{order:?}: both blocks named `team.conventions` reached the model"
        );
    }
}

/// A host that brings its own registry used to replace the one the scripts
/// registered into, and every script contribution vanished without a word.
#[tokio::test(flavor = "multi_thread")]
async fn a_host_registry_keeps_the_scripts_contributions() {
    for order in ORDERS {
        let tmp = tempfile::tempdir().expect("tempdir");
        let session = Session::new(fixtures(), &["hello"], vec![Reply::Text("done")])
            .bind("prompt_block.js", "prompt.block", "onBlock")
            .rust(order, |b| {
                let host = InMemoryPromptRegistry::new();
                host.register_block(RegisteredBlock::system("host.marker", 260, "HOST-BLOCK"));
                b.prompt_registry(host)
            });
        let ran = drive(tmp.path(), session).await;
        assert!(
            ran.holds("HOST-BLOCK"),
            "{order:?}: the host's block is missing"
        );
        assert!(
            ran.holds("SCRIPT-TRACE-BLOCK"),
            "{order:?}: the host's registry erased the script's block"
        );
    }
}

#[derive(Debug)]
struct ScriptEcho;

#[async_trait::async_trait]
impl Tool for ScriptEcho {
    fn name(&self) -> &str {
        "ScriptEcho"
    }
    fn description(&self) -> &str {
        "Echo the `say` argument back."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"say": {"type": "string"}},
            "required": ["say"]
        })
    }
    fn is_read_only(&self, _: &serde_json::Value) -> bool {
        true
    }
    async fn check_permissions(
        &self,
        _: &serde_json::Value,
        _: &ToolContext,
    ) -> PermissionDecision {
        PermissionDecision::allow()
    }
    async fn prompt(&self, _: &PromptContext) -> String {
        self.description().to_string()
    }
    async fn call(
        &self,
        input: serde_json::Value,
        _ctx: ToolContext,
        _p: ProgressSender,
    ) -> Result<ToolResult, base::error::ToolError> {
        let say = input.get("say").and_then(|v| v.as_str()).unwrap_or("");
        Ok(ToolResult::text(format!("echo: {say}")))
    }
}

struct RecordsDepth(Arc<std::sync::Mutex<Vec<u32>>>);

impl Plugin for RecordsDepth {
    fn name(&self) -> &str {
        "records-depth"
    }

    fn register(&self, session: &PluginSession<'_>, r: &mut Registrar) {
        self.0.lock().unwrap().push(session.depth);
        r.tool_result(Arc::new(Wraps)).tool(Arc::new(PluginEcho));
    }
}

#[derive(Debug)]
struct PluginEcho;

#[async_trait::async_trait]
impl Tool for PluginEcho {
    fn name(&self) -> &str {
        "PluginEcho"
    }
    fn description(&self) -> &str {
        "Echo the `say` argument back, from a plugin."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {"say": {"type": "string"}}})
    }
    async fn prompt(&self, _: &PromptContext) -> String {
        self.description().to_string()
    }
    async fn call(
        &self,
        _input: serde_json::Value,
        _ctx: ToolContext,
        _p: ProgressSender,
    ) -> Result<ToolResult, base::error::ToolError> {
        Ok(ToolResult::text("plugin echo"))
    }
}

/// A delegate is built with the session's plugins, and gets the tools they
/// register from them — not as copies of its parent's, which would collide —
/// narrowed by its type's tool list like any other tool.
#[tokio::test(flavor = "multi_thread")]
async fn a_rust_plugin_follows_the_work_into_a_sub_agent() {
    for (subagent_type, offered) in [("general-purpose", true), ("explore", false)] {
        let tmp = tempfile::tempdir().expect("tempdir");
        let depths = Arc::new(std::sync::Mutex::new(Vec::new()));
        let plugin = Arc::new(RecordsDepth(depths.clone()));
        let replies = vec![
            Reply::Tool {
                id: "spawn-1",
                name: "Agent",
                input: serde_json::json!({
                    "prompt": "DELEGATED-TASK: echo something",
                    "subagent_type": subagent_type,
                }),
            },
            Reply::Tool {
                id: "call-1",
                name: "ScriptEcho",
                input: serde_json::json!({"say": "from the delegate"}),
            },
            Reply::Text("the delegate echoed"),
            Reply::Text("the delegate reported back"),
            Reply::Text("(background)"),
        ];
        let session = Session::new(fixtures(), &["delegate an echo"], replies)
            .scene(Arc::new(scene::code::CodeScene::default()))
            .tool(Arc::new(ScriptEcho))
            .rust(RustOrder::AfterScripts, move |b| b.plugin(plugin));
        let ran = drive(tmp.path(), session).await;

        assert_eq!(*depths.lock().unwrap(), vec![0, 1], "{subagent_type}");
        let delegate = ran
            .requests
            .iter()
            .position(|r| r.contains("DELEGATED-TASK") && !r.contains("delegate an echo"))
            .unwrap_or_else(|| panic!("{subagent_type}: the delegate never called the model"));
        let tools = &ran.tools[delegate];
        assert_eq!(
            tools.iter().any(|t| t == "PluginEcho"),
            offered,
            "{subagent_type}: {tools:?}"
        );
        if offered {
            assert!(
                ran.holds("RUST(echo: from the delegate)"),
                "the delegate's tool result did not go through the Rust plugin"
            );
        }
    }
}
