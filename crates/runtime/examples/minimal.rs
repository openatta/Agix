//! The smallest agent the current crates can assemble: one Anthropic model,
//! the code scene, six basic tools, no MCP/skills/plugins/daemon. Reads one
//! prompt from argv, runs one turn, prints the text and the turn's usage.
//!
//! It exists as the size and composition baseline for the minimal build:
//! whatever this binary links is what the kernel cannot yet shed.
//!
//! Reads the same environment as `agixd`: `ANTHROPIC_AUTH_TOKEN` or
//! `ANTHROPIC_API_KEY`, optionally `ANTHROPIC_BASE_URL` for an
//! Anthropic-compatible endpoint, and `ANTHROPIC_MODEL`.
//!
//! ```sh
//! ANTHROPIC_API_KEY=... cargo run --release -p runtime --example minimal -- "list the files here"
//! ```

use std::sync::Arc;

use base::event::AgentEvent;
use base::interface::scene::Scene;
use base::interface::settings::*;
use base::tool::{InMemoryToolRegistry, Tool};
use model::adapter::AnthropicModel;
use model::client::{AuthMode, HttpAnthropicClient};
use tokio_util::sync::CancellationToken;

#[allow(deprecated)]
fn settings(
    model_name: String,
    data_dir: std::path::PathBuf,
    project: std::path::PathBuf,
) -> Settings {
    Settings {
        model: ModelSettings {
            api_type: base::provider::ApiType::Anthropic,
            base_url: String::new(),
            auth_token: String::new(),
            model_name,
            max_tokens: 8192,
            thinking_mode: ThinkingMode::Auto,
            effort: None,
            fallback_model: None,
        },
        paths: PathSettings {
            user_data_dir: data_dir.clone(),
            global_data_dir: data_dir,
            // The project is the directory this lives in.
            local_data_dir: project.join(".agix"),
            scope: "code".into(),
        },
        execution: ExecutionSettings::default(),
        compaction: CompactionConfig::default(),
        sandbox: SandboxConfig::default(),
        plugins: Default::default(),
        instruction_file: None,
        prompt_append: None,
        prompt_override: None,
        recorder: None,
        telemetry_url: None,
        session_dir: None,
        memory_enabled: false,
        disable_skill_shell_execution: true,
        path_style: Default::default(),
        permission_mode: PermissionMode::default(),
        permission_rules: Vec::new(),
        local_permission_rules: Vec::new(),
        allow_client_permission_override: false,
        telemetry_enabled: false,
        hooks_config: None,
        project_trust: Default::default(),
        project_layer: Default::default(),
        ignored_values: Vec::new(),
        managed: None,
        mcp_servers: Default::default(),
        mcp_server_dirs: Default::default(),
        providers: Default::default(),
        default_provider: None,
        task_models: Default::default(),
        language: None,
        scripts: Vec::new(),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let prompt = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
    anyhow::ensure!(!prompt.is_empty(), "usage: minimal <prompt>");
    let api_key = std::env::var("ANTHROPIC_AUTH_TOKEN")
        .or_else(|_| std::env::var("ANTHROPIC_API_KEY"))
        .map_err(|_| anyhow::anyhow!("set ANTHROPIC_AUTH_TOKEN or ANTHROPIC_API_KEY"))?;
    let model_name =
        std::env::var("ANTHROPIC_MODEL").unwrap_or_else(|_| "claude-sonnet-5-5".to_string());
    let data_dir = std::env::temp_dir().join("agix-minimal");
    std::fs::create_dir_all(&data_dir)?;

    let auth = AuthMode::ApiKey(api_key);
    let client = Arc::new(match std::env::var("ANTHROPIC_BASE_URL") {
        // A trailing slash, so joining the endpoint path appends to it.
        Ok(base) => HttpAnthropicClient::with_base(
            auth,
            url::Url::parse(&format!("{}/", base.trim_end_matches('/')))?,
        )?,
        Err(_) => HttpAnthropicClient::new(auth)?,
    });
    let model = Arc::new(AnthropicModel::new(client));

    let registry = Arc::new(InMemoryToolRegistry::new());
    let basic: Vec<Arc<dyn Tool>> = vec![
        Arc::new(tools::file_read::FileReadTool),
        Arc::new(tools::file_write::FileWriteTool),
        Arc::new(tools::file_edit::FileEditTool),
        Arc::new(tools::glob::GlobTool),
        Arc::new(tools::grep::GrepTool),
        Arc::new(tools::bash::BashTool),
    ];
    for t in basic {
        registry.register(t);
    }

    let (mut agent, mut events, _input) = runtime::agent::Builder::new()
        .scene(Arc::new(scene::code::CodeScene::default()) as Arc<dyn Scene>)
        .model(model)
        .settings(Arc::new(settings(
            model_name,
            data_dir,
            std::env::current_dir()?,
        )))
        .tools(registry)
        .skip_warmup(true)
        .build()?;

    let printer = tokio::spawn(async move {
        while let Some(ev) = events.recv().await {
            match ev {
                AgentEvent::TextDelta { text, .. } => print!("{text}"),
                AgentEvent::ToolUse { name, .. } => eprintln!("\n[tool] {name}"),
                AgentEvent::TurnComplete {
                    api_calls,
                    tool_calls,
                    usage,
                    ..
                } => eprintln!(
                    "\n[turn] api_calls={api_calls} tool_calls={tool_calls} usage={usage:?}"
                ),
                _ => {}
            }
        }
    });

    agent
        .run_turn(prompt, "turn-1".into(), CancellationToken::new())
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    drop(agent);
    let _ = printer.await;
    Ok(())
}
