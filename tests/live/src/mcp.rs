//! The toy MCP server (`tests/fixtures/mcp_toy_server`), as a case uses it:
//! the `mcp_servers` entry that starts it over stdio, or a running copy over
//! streamable HTTP and the entry that points at it.
//!
//! The binary is built on first use and asked of cargo rather than guessed
//! at, so a `CARGO_TARGET_DIR` or another profile finds the same one.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::AsyncBufReadExt;

pub struct McpToy;

impl McpToy {
    pub fn binary() -> anyhow::Result<PathBuf> {
        static BUILT: OnceLock<Result<PathBuf, String>> = OnceLock::new();
        BUILT
            .get_or_init(|| build().map_err(|e| format!("{e:#}")))
            .clone()
            .map_err(anyhow::Error::msg)
    }

    /// An `mcp_servers.<name>` value that starts the server over stdio with
    /// `args` (`--calls`, `--name`, `--resources`).
    pub fn stdio_config(args: &[&str]) -> anyhow::Result<Value> {
        Ok(json!({
            "type": "stdio",
            "command": Self::binary()?.to_string_lossy(),
            "args": args,
        }))
    }

    /// The server listening on 127.0.0.1 over streamable HTTP; `args` as for
    /// [`stdio_config`](Self::stdio_config). It stops when the handle drops.
    pub async fn spawn_http(args: &[&str]) -> anyhow::Result<McpToyHttp> {
        let mut child = tokio::process::Command::new(Self::binary()?)
            .args(["--transport", "http"])
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("toy server has no stdout"))?;
        let mut first = String::new();
        tokio::time::timeout(
            Duration::from_secs(10),
            tokio::io::BufReader::new(stdout).read_line(&mut first),
        )
        .await
        .map_err(|_| anyhow::anyhow!("toy server printed no URL within 10s"))??;
        let url = first.trim().to_string();
        anyhow::ensure!(
            url.starts_with("http://"),
            "toy server's first line is not its URL: {url:?}"
        );
        Ok(McpToyHttp { url, child })
    }

    /// The lines of a `--calls` file: `(server name, tool)` per call, plus
    /// `slow:cancelled` for a `slow` that was cancelled.
    pub fn calls(path: &Path) -> Vec<(String, String)> {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.split_once('\t'))
            .map(|(name, tool)| (name.to_string(), tool.to_string()))
            .collect()
    }
}

pub struct McpToyHttp {
    pub url: String,
    child: tokio::process::Child,
}

impl McpToyHttp {
    /// An `mcp_servers.<name>` value for this server: `type` is
    /// `streamable_http` or `sse`.
    pub fn config(&self, kind: &str) -> Value {
        json!({ "type": kind, "url": self.url })
    }

    pub async fn stop(mut self) {
        let _ = self.child.kill().await;
    }
}

fn build() -> anyhow::Result<PathBuf> {
    let out = std::process::Command::new(env!("CARGO"))
        .args([
            "build",
            "-p",
            "mcp-toy-server",
            "--message-format=json-render-diagnostics",
        ])
        .stderr(Stdio::inherit())
        .output()?;
    anyhow::ensure!(out.status.success(), "cargo build -p mcp-toy-server failed");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|v| v["reason"] == "compiler-artifact" && v["target"]["name"] == "mcp-toy-server")
        .and_then(|v| v["executable"].as_str().map(PathBuf::from))
        .ok_or_else(|| anyhow::anyhow!("cargo reported no executable for mcp-toy-server"))
}
