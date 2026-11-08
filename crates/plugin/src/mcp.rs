//! Turning a package's `[[mcp]]` declarations into MCP server configs.
//!
//! Pure data: reading a config file, or naming the bridge process that speaks
//! MCP on a DSH payload's behalf.

use crate::manifest::{McpKind, McpPayload, Plugin};
use std::path::PathBuf;

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
            McpKind::Dsh => dsh_bridge_config(plugin, server),
        };
        if let Some(config) = config {
            out.push((key, config));
        }
    }
    out
}

fn native_config(plugin: &Plugin, server: &McpPayload) -> Option<serde_json::Value> {
    let path = plugin.path(server.config.as_ref()?);
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

/// A DSH payload as a stdio MCP server: `agix-dsh-bridge` loading the
/// plugin's entry module.
fn dsh_bridge_config(plugin: &Plugin, server: &McpPayload) -> Option<serde_json::Value> {
    let entry = plugin.path(server.entry.as_ref()?);
    if !entry.exists() {
        tracing::warn!(
            plugin = %plugin.name(),
            entry = %entry.display(),
            "DSH plugin entry module does not exist, skipping"
        );
        return None;
    }
    let bridge = bridge_entry_path()?;
    // Only the variables the manifest named are passed through.
    let env: serde_json::Map<String, serde_json::Value> = server
        .env
        .iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| (k.clone(), v.into())))
        .collect();

    Some(serde_json::json!({
        "type": "stdio",
        "command": "node",
        "args": [bridge.to_string_lossy(), entry.to_string_lossy()],
        "env": env,
    }))
}

/// Path of the bridge's entry module inside a checkout or an install.
const BRIDGE_REL: &str = "bridges/agix-dsh-bridge/src/main.js";

/// Where the bridge's entry point lives, or `None` when it cannot be found.
///
/// Deliberately not `CARGO_MANIFEST_DIR`: that is a compile-time constant,
/// so a shipped binary would carry the build machine's absolute path and
/// point at a directory that exists on no user's computer.
///
/// Instead, `AGIX_DSH_BRIDGE` wins if it points at a file, and otherwise the
/// search walks up from the running executable. Every candidate is checked
/// for existence: handing `node` a script that is not there produces a
/// failure message about node, from a process we did not start, with nothing
/// connecting it back to the plugin that caused it.
fn locate_bridge(override_path: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(p) = override_path {
        if p.is_file() {
            return Some(p);
        }
        tracing::warn!(
            path = %p.display(),
            "AGIX_DSH_BRIDGE does not point at a file; falling back to the search"
        );
    }
    crate::locate::search_near_executable(&[BRIDGE_REL, "agix-dsh-bridge/src/main.js"])
}

fn bridge_entry_path() -> Option<PathBuf> {
    locate_bridge(std::env::var("AGIX_DSH_BRIDGE").ok().map(Into::into))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bridge is found relative to the running executable, never from a
    /// compile-time path: a shipped binary carrying the build machine's
    /// directory would point at somewhere that exists on no user's computer.
    ///
    /// Takes the override as an argument rather than setting the process
    /// environment, because tests share one process and an env var set here
    /// would leak into whatever runs beside it.
    #[test]
    fn an_existing_override_is_used_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let bridge = dir.path().join("main.js");
        std::fs::write(&bridge, "// bridge").unwrap();
        assert_eq!(
            locate_bridge(Some(bridge.clone())).as_deref(),
            Some(bridge.as_path())
        );
    }

    #[test]
    fn an_override_pointing_nowhere_is_not_returned() {
        let dir = tempfile::tempdir().unwrap();
        let absent = dir.path().join("absent.js");
        assert_ne!(
            locate_bridge(Some(absent.clone())).as_deref(),
            Some(absent.as_path()),
            "a path that does not exist must never be handed to node"
        );
    }

    /// The walk has to reach the repo copy from wherever the test binary
    /// lives — which is `target/debug/deps/`, a different depth from the
    /// `target/debug/` a normal build produces.
    #[test]
    fn the_search_finds_the_repo_copy_from_a_test_binary() {
        let found = locate_bridge(None).expect("the in-repo bridge should be reachable");
        assert!(found.is_file());
        assert!(found.ends_with("agix-dsh-bridge/src/main.js"), "{found:?}");
    }

    const HEAD: &str = "[plugin]\nname = \"p\"\nversion = \"1.0.0\"\napi_version = \"0.1\"\n";

    fn load(root: &std::path::Path, body: &str) -> Plugin {
        std::fs::write(root.join("plugin.toml"), format!("{HEAD}{body}")).unwrap();
        Plugin::load(root, &root.join("plugin.toml")).unwrap()
    }

    #[test]
    fn a_dsh_payload_becomes_a_bridge_invocation() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("dist")).unwrap();
        std::fs::write(
            dir.path().join("dist/index.js"),
            "export function apply() {}",
        )
        .unwrap();
        std::env::set_var("AGIX_TEST_DSH_TOKEN", "secret");
        let p = load(
            dir.path(),
            r#"
[[mcp]]
name = "pr-helper"
kind = "dsh"
entry = "dist/index.js"
env = ["AGIX_TEST_DSH_TOKEN", "AGIX_TEST_UNSET"]
"#,
        );

        let servers = servers_for(&p);
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].0, "p-mcp-pr-helper");

        let cfg = &servers[0].1;
        assert_eq!(cfg["type"], "stdio");
        assert_eq!(cfg["command"], "node");
        let args = cfg["args"].as_array().unwrap();
        assert!(args[0].as_str().unwrap().ends_with("main.js"), "{args:?}");
        assert!(
            args[1].as_str().unwrap().ends_with("dist/index.js"),
            "{args:?}"
        );
        assert_eq!(cfg["env"]["AGIX_TEST_DSH_TOKEN"], "secret");
        assert!(cfg["env"].get("AGIX_TEST_UNSET").is_none());
    }

    #[test]
    fn a_dsh_payload_with_no_entry_module_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let p = load(
            dir.path(),
            "\n[[mcp]]\nname = \"gone\"\nkind = \"dsh\"\nentry = \"dist/missing.js\"\n",
        );
        assert!(servers_for(&p).is_empty());
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
