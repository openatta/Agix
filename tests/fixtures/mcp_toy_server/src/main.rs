//! 测试用的 MCP 服务 —— `tests/fixtures/template_project` 的 `mcp_servers.demo`
//! 与 `tests/live` 的 MCP 用例连的就是它。不是产品代码，只服务于测试基础设施。
//!
//! 命令行：
//!
//! ```text
//! mcp-toy-server [--transport stdio|http|sse] [--port N] [--port-file PATH]
//!                [--calls PATH] [--name NAME] [--resources DIR] [--mute]
//! ```
//!
//! - `http` / `sse` 在 127.0.0.1 上起 streamable HTTP，绑定后先在 stdout 第一行打出
//!   `http://127.0.0.1:<port>/mcp`（给了 `--port-file` 也写进那个文件）。rmcp 3 已经
//!   没有旧式 SSE 服务端，Agix 的 `type: "sse"` 客户端也是走 streamable HTTP，
//!   所以 `sse` 起的是同一个服务。
//! - `--calls`（或环境变量 `TOY_MCP_CALLS`）：每次工具调用往这个文件追加一行
//!   `<name>\t<tool>`，`slow` 被取消时另加一行 `<name>\tslow:cancelled`——测试据此证明
//!   调用真的到了这个进程。
//! - `--resources DIR`：目录里每个文件成为一个资源 `toy://files/<文件名>`，测试把
//!   运行时生成的令牌写进去，就能证明模型读到的是资源本身。
//! - `--mute`（只对 stdio）：读 stdin 但从不应答，握手永远完成不了。收到第一行时往
//!   `--calls` 记一行 `<name>\tmuted`，测试据此知道连接已经卡在它身上。
//!
//! 工具：`ping`、`echo`、`slow`（可取消）、`crash`（进程直接退出）、`big`（指定字节数）、
//! `elicit`（结果里带一个 `elicitation://` URL，Agix 据此触发 Elicitation hook）。
//! 资源：`toy://notes/readme.md`、`toy://unicode`、`toy://big`，以及 `--resources` 的。
//! 提示词：`greet`（`name` 必填、`style` 可选）、`mark`（无参数）。

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    GetPromptRequestParams, GetPromptResponse, GetPromptResult, ListPromptsResult,
    ListResourcesResult, PaginatedRequestParams, Prompt, PromptArgument, PromptMessage,
    ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult, Resource,
    ResourceContents, Role, ServerCapabilities, ServerConfig,
};
use rmcp::service::RequestContext;
use rmcp::{tool, tool_handler, tool_router, ErrorData, RoleServer, ServerHandler, ServiceExt};
use tokio_util::sync::CancellationToken;

const BIG_RESOURCE_BYTES: usize = 200_000;
const BIG_RESOURCE_TAIL: &str = "TOY-BIG-RESOURCE-END";
const UNICODE_TEXT: &str =
    "统一码 ✓ — emoji 🦀🎉 — RTL مرحبا بالعالم — combining e\u{301} — zero-width a\u{200b}b";

struct Options {
    transport: String,
    port: u16,
    port_file: Option<PathBuf>,
    calls: Option<PathBuf>,
    name: String,
    resources: Option<PathBuf>,
    mute: bool,
}

fn parse_args() -> anyhow::Result<Options> {
    let mut opts = Options {
        transport: "stdio".into(),
        port: 0,
        port_file: None,
        calls: std::env::var_os("TOY_MCP_CALLS").map(PathBuf::from),
        name: "toy".into(),
        resources: None,
        mute: false,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || {
            args.next()
                .ok_or_else(|| anyhow::anyhow!("{arg} needs a value"))
        };
        match arg.as_str() {
            "--transport" => opts.transport = value()?,
            "--port" => opts.port = value()?.parse()?,
            "--port-file" => opts.port_file = Some(value()?.into()),
            "--calls" => opts.calls = Some(value()?.into()),
            "--name" => opts.name = value()?,
            "--resources" => opts.resources = Some(value()?.into()),
            "--mute" => opts.mute = true,
            other => anyhow::bail!("unknown argument {other}"),
        }
    }
    anyhow::ensure!(
        matches!(opts.transport.as_str(), "stdio" | "http" | "sse"),
        "--transport is stdio, http or sse, not {}",
        opts.transport
    );
    Ok(opts)
}

struct State {
    name: String,
    calls: Option<PathBuf>,
    resources: Option<PathBuf>,
    elicitations: AtomicUsize,
}

impl State {
    fn record(&self, what: &str) {
        let Some(path) = &self.calls else {
            return;
        };
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(f, "{}\t{what}", self.name);
        }
    }

    fn file_resources(&self) -> Vec<(String, PathBuf)> {
        let Some(dir) = &self.resources else {
            return Vec::new();
        };
        let mut out: Vec<(String, PathBuf)> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().is_file())
            .map(|e| {
                (
                    format!("toy://files/{}", e.file_name().to_string_lossy()),
                    e.path(),
                )
            })
            .collect();
        out.sort();
        out
    }
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct PingRequest {
    #[schemars(description = "message to echo back")]
    message: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct EchoRequest {
    #[schemars(description = "text returned exactly as given")]
    text: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct SlowRequest {
    #[schemars(description = "milliseconds to wait before answering")]
    ms: u64,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct BigRequest {
    #[schemars(description = "how many bytes of text to return")]
    bytes: usize,
    #[schemars(description = "text the output ends with (default BIG-END)")]
    tail: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ElicitRequest {
    #[schemars(description = "what the user is asked to confirm")]
    reason: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct Empty {}

// `#[tool_router]`/`#[tool_handler]` build `Self::tool_router()` per dispatch
// rather than reading a stored router, so the only field is the shared state.
#[derive(Clone)]
struct ToyServer {
    state: Arc<State>,
}

#[tool_router]
impl ToyServer {
    #[tool(description = "Echo the given message back, prefixed with 'toy: '.")]
    fn ping(&self, Parameters(PingRequest { message }): Parameters<PingRequest>) -> String {
        self.state.record("ping");
        format!("toy: {message}")
    }

    #[tool(description = "Return the given text unchanged.")]
    fn echo(&self, Parameters(EchoRequest { text }): Parameters<EchoRequest>) -> String {
        self.state.record("echo");
        text
    }

    #[tool(description = "Wait the given number of milliseconds, then answer. Cancellable.")]
    async fn slow(
        &self,
        Parameters(SlowRequest { ms }): Parameters<SlowRequest>,
        cancel: CancellationToken,
    ) -> String {
        self.state.record("slow");
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_millis(ms)) => {
                format!("slow: done after {ms}ms")
            }
            _ = cancel.cancelled() => {
                self.state.record("slow:cancelled");
                "slow: cancelled".to_string()
            }
        }
    }

    #[tool(description = "Exit the server process immediately, without answering.")]
    fn crash(&self, Parameters(Empty {}): Parameters<Empty>) -> String {
        self.state.record("crash");
        std::process::exit(3)
    }

    #[tool(description = "Return exactly `bytes` bytes of text ending with `tail`.")]
    fn big(&self, Parameters(BigRequest { bytes, tail }): Parameters<BigRequest>) -> String {
        self.state.record("big");
        sized_text(bytes, tail.as_deref().unwrap_or("BIG-END"))
    }

    #[tool(description = "Ask the user to confirm something before the work can continue.")]
    fn elicit(&self, Parameters(ElicitRequest { reason }): Parameters<ElicitRequest>) -> String {
        self.state.record("elicit");
        let n = self.state.elicitations.fetch_add(1, Ordering::SeqCst) + 1;
        format!(
            "Confirmation required ({}). Open elicitation://toy/confirm?call={n} to continue",
            reason.unwrap_or_else(|| "no reason given".into())
        )
    }
}

#[tool_handler]
impl ServerHandler for ToyServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .enable_prompts()
                .build(),
        )
        .with_instructions("Test-only MCP server: tools, resources and prompts for the harness.")
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        let mut resources = vec![
            Resource::new("toy://notes/readme.md", "readme")
                .with_description("A short note")
                .with_mime_type("text/markdown"),
            Resource::new("toy://unicode", "unicode")
                .with_description("Non-ASCII text: CJK, emoji, right-to-left, combining marks")
                .with_mime_type("text/plain"),
            Resource::new("toy://big", "big")
                .with_description(format!("{BIG_RESOURCE_BYTES} bytes of text"))
                .with_mime_type("text/plain")
                .with_size(BIG_RESOURCE_BYTES as u64),
        ];
        for (uri, path) in self.state.file_resources() {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            resources.push(Resource::new(uri, name).with_mime_type("text/plain"));
        }
        Ok(ListResourcesResult::with_all_items(resources))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        let uri = request.uri;
        let text = match uri.as_str() {
            "toy://notes/readme.md" => "# Toy notes\n\nTOY-NOTES-MARK\n".to_string(),
            "toy://unicode" => UNICODE_TEXT.to_string(),
            "toy://big" => sized_text(BIG_RESOURCE_BYTES, BIG_RESOURCE_TAIL),
            other => match self
                .state
                .file_resources()
                .into_iter()
                .find(|(u, _)| u == other)
            {
                Some((_, path)) => std::fs::read_to_string(path)
                    .map_err(|e| ErrorData::internal_error(e.to_string(), None))?,
                None => {
                    return Err(ErrorData::resource_not_found(
                        format!("no resource {other}"),
                        None,
                    ))
                }
            },
        };
        Ok(ReadResourceResult::new(vec![ResourceContents::text(text, uri)]).into())
    }

    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, ErrorData> {
        Ok(ListPromptsResult::with_all_items(vec![
            Prompt::new(
                "greet",
                Some("Greet someone by name"),
                Some(vec![
                    PromptArgument::new("name")
                        .with_description("who to greet")
                        .with_required(true),
                    PromptArgument::new("style")
                        .with_description("how to greet them")
                        .with_required(false),
                ]),
            ),
            Prompt::new("mark", Some("A fixed instruction"), None),
        ]))
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetPromptResponse, ErrorData> {
        let arg = |key: &str| {
            request
                .arguments
                .as_ref()
                .and_then(|a| a.get(key))
                .and_then(|v| v.as_str())
                .map(str::to_string)
        };
        let text = match request.name.as_str() {
            "greet" => {
                let name = arg("name").ok_or_else(|| {
                    ErrorData::invalid_params("greet needs the `name` argument", None)
                })?;
                match arg("style") {
                    Some(style) => format!("Greet {name} in a {style} style."),
                    None => format!("Greet {name}."),
                }
            }
            "mark" => "Reply with exactly TOY-PROMPT-MARK.".to_string(),
            other => {
                return Err(ErrorData::invalid_params(
                    format!("no prompt {other}"),
                    None,
                ))
            }
        };
        Ok(GetPromptResult::new(vec![PromptMessage::new_text(Role::User, text)]).into())
    }
}

/// ASCII text of exactly `bytes` bytes (at least the tail) ending with `tail`.
fn sized_text(bytes: usize, tail: &str) -> String {
    let fill = bytes.saturating_sub(tail.len());
    let mut out: String = "0123456789abcdef\n".chars().cycle().take(fill).collect();
    out.push_str(tail);
    out
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let opts = parse_args()?;
    let state = Arc::new(State {
        name: opts.name.clone(),
        calls: opts.calls.clone(),
        resources: opts.resources.clone(),
        elicitations: AtomicUsize::new(0),
    });
    if opts.mute {
        return stay_mute(&state).await;
    }
    if opts.transport == "stdio" {
        let service = ToyServer { state }.serve(rmcp::transport::stdio()).await?;
        service.waiting().await?;
        return Ok(());
    }
    serve_http(opts, state).await
}

async fn stay_mute(state: &State) -> anyhow::Result<()> {
    use tokio::io::AsyncBufReadExt as _;
    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    let mut told = false;
    while lines.next_line().await?.is_some() {
        if !told {
            state.record("muted");
            told = true;
        }
    }
    Ok(())
}

async fn serve_http(opts: Options, state: Arc<State>) -> anyhow::Result<()> {
    use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
    use rmcp::transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService,
    };

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", opts.port)).await?;
    let url = format!("http://127.0.0.1:{}/mcp", listener.local_addr()?.port());
    if let Some(path) = &opts.port_file {
        std::fs::write(path, &url)?;
    }
    println!("{url}");
    std::io::stdout().flush()?;

    let service = StreamableHttpService::new(
        move || {
            Ok(ToyServer {
                state: state.clone(),
            })
        },
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );
    loop {
        let (sock, _) = listener.accept().await?;
        let service = hyper_util::service::TowerToHyperService::new(service.clone());
        tokio::spawn(async move {
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(hyper_util::rt::TokioIo::new(sock), service)
                .await;
        });
    }
}
