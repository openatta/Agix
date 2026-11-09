//! Turning a package's `[[mcp]]` declarations into MCP server configs.

use crate::manifest::{McpKind, McpPayload, Plugin};

/// Every MCP server this package will start, keyed as `<plugin>-mcp-<name>`.
///
/// A declaration whose config is missing or unreadable is skipped with a
/// warning rather than failing the rest: one broken server should not take
/// out the package's other contributions.
pub fn servers_for(plugin: &Plugin) -> Vec<(String, serde_json::Value)> {
    let mut out = Vec::new();
    for server in &plugin.manifest.mcp {
        let key = format!("{}-mcp-{}", plugin.name(), server.name);
        let config = match server.kind {
            McpKind::Native => native_config(plugin, server),
        };
        if let Some(config) = config {
            out.push((key, config));
        }
    }
    out
}

fn native_config(plugin: &Plugin, server: &McpPayload) -> Option<serde_json::Value> {
    let rel = server.config.as_ref()?;
    let path = match plugin.path(rel) {
        Ok(path) => path,
        Err(e) => {
            tracing::warn!(
                plugin = %plugin.name(),
                error = %e,
                "plugin MCP server config cannot be used, skipping"
            );
            return None;
        }
    };
    match std::fs::read_to_string(&path) {
        Ok(raw) => match serde_json::from_str::<serde_json::Value>(&raw) {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!(
                    plugin = %plugin.name(),
                    path = %path.display(),
                    error = %e,
                    "plugin MCP server config is not valid JSON, skipping"
                );
                None
            }
        },
        Err(e) => {
            tracing::warn!(
                plugin = %plugin.name(),
                path = %path.display(),
                error = %e,
                "failed to read plugin MCP server config, skipping"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEAD: &str = "[plugin]\nname = \"p\"\nversion = \"1.0.0\"\napi_version = \"0.1\"\n";

    fn load(root: &std::path::Path, body: &str) -> Plugin {
        std::fs::write(root.join("plugin.toml"), format!("{HEAD}{body}")).unwrap();
        Plugin::load(root, &root.join("plugin.toml")).unwrap()
    }

    #[test]
    fn native_mcp_payloads_are_namespaced() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("mcp")).unwrap();
        std::fs::write(
            dir.path().join("mcp/server.json"),
            r#"{"command":"echo","args":[]}"#,
        )
        .unwrap();
        let p = load(
            dir.path(),
            "\n[[mcp]]\nname = \"github\"\nkind = \"native\"\nconfig = \"mcp/server.json\"\n",
        );
        let servers = servers_for(&p);
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].0, "p-mcp-github");
        assert_eq!(servers[0].1["command"], "echo");
    }
}
