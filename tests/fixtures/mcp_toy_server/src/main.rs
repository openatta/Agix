//! 最小可用的 stdio MCP 测试服务 —— 只用来让 `tests/fixtures/template_project`
//! 的 `mcp_servers.demo` 真的能连上、真的有一个工具可调，而不是一个预期失败的占位。
//! 不是产品代码，只服务于测试基础设施。

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerConfig};
use rmcp::transport::stdio;
use rmcp::{tool, tool_handler, tool_router, ServerHandler, ServiceExt};

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct PingRequest {
    #[schemars(description = "message to echo back")]
    message: String,
}

// `#[tool_router]`/`#[tool_handler]` generate a fresh `Self::tool_router()` per
// dispatch (see rmcp-macros' tool_handler.rs) rather than reading a stored
// field, so the struct itself carries no state — it only exists to hang the
// `#[tool]`-annotated methods and the `ServerHandler` impl off of.
#[derive(Clone, Default)]
struct ToyServer;

#[tool_router]
impl ToyServer {
    #[tool(description = "Echo the given message back, prefixed with 'toy: '.")]
    fn ping(&self, Parameters(PingRequest { message }): Parameters<PingRequest>) -> String {
        format!("toy: {message}")
    }
}

#[tool_handler]
impl ServerHandler for ToyServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("Test-only MCP server: one 'ping' tool.")
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let service = ToyServer.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
