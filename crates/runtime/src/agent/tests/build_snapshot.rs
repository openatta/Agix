//! 冻结 `Builder::build` 装出来的会话：registry 的工具名（按注册顺序）、`EngineConfig`
//! 的路径风格与延迟工具、`SessionState` 的 env，以及一个探针模块在 `on_session` 时
//! 看到的东西；再加几条拒绝构建的错误消息。拆 `build` 之前的特征化测试，要改时用
//! `UPDATE_GOLDEN=1 cargo test -p runtime --lib agent::tests::build_snapshot` 重新生成。
//!
//! 工具表依赖 `web`、`structured-output`、`ptc` 三个默认 feature，精简构建下不跑。

#![cfg(all(feature = "web", feature = "structured-output", feature = "ptc"))]

use super::*;
use crate::module::{Module, ModuleError, SessionContext};
use std::fmt::Write as _;
use std::path::Path;
use std::sync::Mutex;

const GOLDEN: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/golden/build_snapshot.txt"
);

struct Named(&'static str);

#[async_trait::async_trait]
impl base::tool::Tool for Named {
    fn name(&self) -> &str {
        self.0
    }
    fn spec(&self) -> base::tool::ToolSpec {
        base::tool::ToolSpec::new()
    }
    fn description(&self) -> &str {
        "named (test tool)"
    }
    fn input_schema(&self, _: base::tool::PathStyle) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn call(
        &self,
        _: serde_json::Value,
        _: base::tool::ToolContext,
        _: base::tool::ProgressSender,
    ) -> Result<base::tool::ToolResult, base::error::ToolError> {
        Ok(base::tool::ToolResult::text("done"))
    }
}

struct Probe {
    seen: Arc<Mutex<Vec<String>>>,
}

impl Module for Probe {
    fn id(&self) -> &'static str {
        "probe"
    }

    fn on_session(&self, cx: &mut SessionContext<'_>) -> Result<(), ModuleError> {
        let agent_types: Vec<&str> = cx
            .plugin_agent_types
            .iter()
            .map(|t| t.name.as_str())
            .collect();
        self.seen.lock().unwrap().push(format!(
            "tools={:?} agent_types={agent_types:?} depth={}",
            cx.tools.names(),
            cx.depth
        ));
        Ok(())
    }
}

struct NeedsMissing;

impl Module for NeedsMissing {
    fn id(&self) -> &'static str {
        "needs-missing"
    }

    fn requires(&self) -> &'static [&'static str] {
        &["never-assembled"]
    }

    fn on_session(&self, _: &mut SessionContext<'_>) -> Result<(), ModuleError> {
        Ok(())
    }
}

struct Contributes {
    tool: &'static str,
    block: bool,
}

impl crate::plugin::Plugin for Contributes {
    fn name(&self) -> &str {
        "snapshot-plugin"
    }

    fn register(&self, _: &crate::plugin::PluginSession<'_>, r: &mut crate::plugin::Registrar) {
        r.tool(Arc::new(Named(self.tool)));
        if self.block {
            r.prompt_block(base::interface::prompt_registry::RegisteredBlock::system(
                "snapshot_block",
                5,
                "from the plugin",
            ));
            r.agent_type(base::agent_type::AgentTypeDefinition {
                name: "snapshot-type".into(),
                ..Default::default()
            });
        }
    }
}

fn scene_deferring(id: &str, deferred: &[&str]) -> SceneSpec {
    let mut s = bare_scene(id);
    s.interactive = true;
    s.tools.deferred = deferred.iter().map(|n| n.to_string()).collect();
    s
}

fn template_of(tools: Vec<Arc<dyn base::tool::Tool>>) -> Arc<InMemoryToolRegistry> {
    let template = Arc::new(InMemoryToolRegistry::new());
    for t in tools {
        template.register(t);
    }
    template
}

fn builder_for(scene: SceneSpec, template: Arc<InMemoryToolRegistry>) -> Builder {
    Builder::new()
        .scene(Arc::new(scene))
        .model(Arc::new(DummyModel) as Arc<dyn Model>)
        .settings(Arc::new(test_settings()))
        .tools(template)
        .skip_warmup(true)
}

fn describe(agent: &Agent, seen: &Mutex<Vec<String>>) -> String {
    let mut out = String::new();
    writeln!(out, "tools: {:?}", agent.tooling.tools.names()).unwrap();
    writeln!(out, "path_style: {:?}", agent.config.path_style).unwrap();
    let mut deferred: Vec<&String> = agent.config.deferred_tools.iter().collect();
    deferred.sort();
    writeln!(out, "deferred_tools: {deferred:?}").unwrap();
    let env = agent.session_state.env();
    writeln!(out, "env.cwd: {}", env.cwd.display()).unwrap();
    writeln!(
        out,
        "env.additional_writable_dirs: {:?}",
        env.additional_writable_dirs
    )
    .unwrap();
    writeln!(out, "env.read_directories: {:?}", env.read_directories).unwrap();
    writeln!(
        out,
        "env.flags: read_anywhere={} unattended={} base_mode={:?}",
        env.read_anywhere, env.unattended, env.base_mode
    )
    .unwrap();
    let blocks = agent.prompt.registry().blocks();
    let names: Vec<&str> = blocks.iter().map(|b| b.name.as_str()).collect();
    writeln!(out, "prompt_registry_blocks: {names:?}").unwrap();
    for line in seen.lock().unwrap().iter() {
        writeln!(out, "probe: {line}").unwrap();
    }
    out
}

fn build_described(builder: Builder, seen: &Arc<Mutex<Vec<String>>>) -> String {
    let (agent, _rx, _tx) = builder
        .module(Arc::new(Probe { seen: seen.clone() }))
        .build()
        .expect("build should succeed");
    describe(&agent, seen)
}

fn refusal(builder: Builder) -> String {
    match builder.build() {
        Ok(_) => "built".into(),
        Err(e) => e.to_string(),
    }
}

#[tokio::test]
async fn build_matches_the_golden_snapshot() {
    let mut out = String::new();
    let mut section = |name: &str, body: String| {
        writeln!(out, "## {name}\n{body}").unwrap();
    };
    let fresh = || Arc::new(Mutex::new(Vec::new()));
    let structured = || -> Arc<dyn base::tool::Tool> {
        Arc::new(tools::structured_output::StructuredOutputTool::default())
    };

    {
        let seen = fresh();
        let mut settings = test_settings();
        settings.values.sandbox.additional_directories = vec!["extra".into()];
        settings.values.sandbox.read_directories = vec!["docs".into()];
        let builder = builder_for(
            scene_deferring("minimal", &["StructuredOutput", "Later"]),
            template_of(vec![Arc::new(Named("Early")), structured()]),
        )
        .settings(Arc::new(settings));
        section("minimal", build_described(builder, &seen));
    }

    {
        let seen = fresh();
        let builder = builder_for(
            scene_deferring("plugin", &[]),
            template_of(vec![Arc::new(Named("Early"))]),
        )
        .plugin(Arc::new(Contributes {
            tool: "PluginTool",
            block: true,
        }));
        section(
            "rust plugin with prompt block",
            build_described(builder, &seen),
        );
    }

    {
        let seen = fresh();
        let builder = builder_for(
            scene_deferring("schema", &["StructuredOutput", "Later"]),
            template_of(vec![Arc::new(Named("Early")), structured()]),
        )
        .output_schema(serde_json::json!({"type": "object"}));
        section("output schema", build_described(builder, &seen));
    }

    {
        let seen = fresh();
        let mut scene = bare_scene("unattended");
        scene.interactive = false;
        let mut template: Vec<Arc<dyn base::tool::Tool>> = vec![Arc::new(Named("Early"))];
        template.extend(
            base::interface::scene::NEEDS_A_USER
                .iter()
                .map(|n| Arc::new(Named(n)) as Arc<dyn base::tool::Tool>),
        );
        let builder = builder_for(scene, template_of(template)).question_prompts(true);
        section("unattended scene", build_described(builder, &seen));
    }

    {
        let seen = fresh();
        let runner = Arc::new(hooks::HookRunner::new(Default::default()));
        let builder = builder_for(
            scene_deferring("delegate", &[]),
            template_of(vec![Arc::new(Named("Early"))]),
        )
        .agent_depth(1)
        .hooks(runner.clone());
        let (agent, _rx, _tx) = builder
            .module(Arc::new(Probe { seen: seen.clone() }))
            .build()
            .expect("build should succeed");
        let mut body = describe(&agent, &seen);
        writeln!(
            body,
            "hooks_are_the_callers: {}",
            std::ptr::eq(agent.hooks(), runner.as_ref())
        )
        .unwrap();
        section("depth 1 with hooks from outside", body);
    }

    {
        let seen = fresh();
        let template = Arc::new(InMemoryToolRegistry::new());
        template.register(Arc::new(Named("Alpha")));
        template.register(Arc::new(tools::tool_search::ToolSearchTool::new(
            template.clone(),
        )));
        template.register(Arc::new(Named("Beta")));
        let builder = builder_for(scene_deferring("search", &["Beta", "ToolSearch"]), template);
        section("template with ToolSearch", build_described(builder, &seen));
    }

    {
        let providers_and_ledger = builder_for(bare_scene("refused"), template_of(vec![]))
            .exec(base::interface::exec::ExecProviders::in_process())
            .process_groups(platform::GroupLedger::new());
        section(
            "refused: exec and process_groups",
            refusal(providers_and_ledger),
        );
    }

    {
        let dir = tempfile::tempdir().unwrap();
        let store = history::store::JsonlHistoryStore::with_roots(
            dir.path(),
            history::path::HistoryRoots::under(dir.path()),
        )
        .await
        .expect("store should build");
        let bad_id = builder_for(bare_scene("refused"), template_of(vec![]))
            .history_store(Arc::new(store))
            .session_id("not-a-session-id");
        section("refused: unparseable session_id", refusal(bad_id));
    }

    {
        let mut scene = bare_scene("refused");
        scene.extra_tools = vec![Arc::new(Named("Taken"))];
        let builder = builder_for(scene, template_of(vec![Arc::new(Named("Taken"))]));
        section("refused: scene tool name taken", refusal(builder));
    }

    {
        let builder = builder_for(
            bare_scene("refused"),
            template_of(vec![Arc::new(Named("Taken"))]),
        )
        .plugin(Arc::new(Contributes {
            tool: "Taken",
            block: false,
        }));
        section("refused: plugin tool name taken", refusal(builder));
    }

    {
        let builder =
            builder_for(bare_scene("refused"), template_of(vec![])).module(Arc::new(NeedsMissing));
        section("refused: module order", refusal(builder));
    }

    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(Path::new(GOLDEN).parent().unwrap()).unwrap();
        std::fs::write(GOLDEN, &out).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(GOLDEN)
        .unwrap_or_else(|e| panic!("{GOLDEN}: {e}; generate it with UPDATE_GOLDEN=1"));
    if out != expected {
        let first = out
            .lines()
            .zip(expected.lines())
            .position(|(a, e)| a != e)
            .unwrap_or_else(|| out.lines().count().min(expected.lines().count()));
        panic!(
            "build snapshot drifted from {GOLDEN} at line {}: {:?} != {:?}",
            first + 1,
            out.lines().nth(first),
            expected.lines().nth(first),
        );
    }
}
