//! Auto-generate skills from MCP server tools.
//!
//! Each MCP tool becomes a user-invocable skill with the name
//! `mcp__{server}__{tool}` so users can invoke it via `/mcp__{server}__{tool}`
//! slash command. The skill prompt explains how to use the MCP tool and
//! includes its input schema.

use base::frozen::{SkillEntry, SkillSource};
use base::interface::model::ToolDef;
use std::path::PathBuf;

/// Build skill entries from a list of MCP tool definitions.
///
/// Each tool becomes a `SkillEntry` with:
/// - Name: `mcp__{server}__{tool}`
/// - Description: from the tool's description
/// - Source: `Plugin` (MCP servers are treated as plugin capabilities)
/// - Path: `(mcp:{server}:{tool})` — the body is built on demand by
///   [`mcp_skill_body`] from the same tool list
/// - `user_invocable: true` — visible in `/skills` list
pub fn build_skills_from_mcp(server_name: &str, tools: &[ToolDef]) -> Vec<SkillEntry> {
    let mut entries = Vec::with_capacity(tools.len());

    for tool in tools {
        entries.push(SkillEntry {
            name: skill_name(server_name, &tool.name),
            description: tool.description.clone(),
            source: SkillSource::Plugin,
            path: PathBuf::from(format!("(mcp:{server_name}:{})", tool.name)),
            user_invocable: true,
            ..Default::default()
        });
    }

    entries
}

/// The prompt body for one of the skills [`build_skills_from_mcp`] produced
/// from `tools`, or `None` if `name` is not one of them.
pub fn mcp_skill_body(server_name: &str, tools: &[ToolDef], name: &str) -> Option<String> {
    tools
        .iter()
        .find(|tool| skill_name(server_name, &tool.name) == name)
        .map(|tool| build_tool_prompt(server_name, tool))
}

fn skill_name(server_name: &str, tool_name: &str) -> String {
    format!("mcp__{server_name}__{tool_name}")
}

/// Build the prompt body text for a single MCP tool.
fn build_tool_prompt(server_name: &str, tool: &ToolDef) -> String {
    let schema_json = serde_json::to_string_pretty(&tool.input_schema)
        .unwrap_or_else(|_| tool.input_schema.to_string());

    format!(
        r#"# MCP Tool: {server_name} / {tool_name}

{tool_description}

## Usage

This tool is provided by the **{server_name}** MCP server. Invoke it by
calling the MCP server's `{tool_name}` tool with the appropriate arguments.

## Input Schema

```json
{schema_json}
```

## Convention

When using this tool, construct a valid JSON object matching the input schema
above. Pass it as the arguments for the `{tool_name}` tool.
"#,
        server_name = server_name,
        tool_name = tool.name,
        tool_description = tool.description,
        schema_json = schema_json,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_tool(name: &str, desc: &str) -> ToolDef {
        ToolDef {
            name: name.into(),
            description: desc.into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"}
                }
            }),
            source: None,
        }
    }

    #[test]
    fn build_skills_creates_entries() {
        let tools = vec![
            make_tool("search", "Search the web"),
            make_tool("fetch", "Fetch a URL"),
        ];
        let entries = build_skills_from_mcp("web-search", &tools);
        assert_eq!(entries.len(), 2);

        assert_eq!(entries[0].name, "mcp__web-search__search");
        assert_eq!(entries[0].description, "Search the web");
        assert!(entries[0].user_invocable);
        assert_eq!(entries[0].path.to_string_lossy(), "(mcp:web-search:search)");

        assert_eq!(entries[1].name, "mcp__web-search__fetch");
    }

    #[test]
    fn mcp_skill_body_is_built_from_the_tool() {
        let tools = vec![make_tool("search", "Search the web")];
        let body = mcp_skill_body("test-server", &tools, "mcp__test-server__search");
        assert!(body.is_some());
        let body = body.unwrap();
        assert!(body.contains("test-server"));
        assert!(body.contains("search"));
        assert!(body.contains("Search the web"));
        assert!(body.contains("Input Schema"));
    }

    #[test]
    fn mcp_skill_body_returns_none_for_unknown() {
        let tools = vec![make_tool("search", "Search the web")];
        assert!(mcp_skill_body("test-server", &tools, "mcp__unknown__tool").is_none());
        assert!(mcp_skill_body("other-server", &tools, "mcp__test-server__search").is_none());
    }

    #[test]
    fn prompt_body_contains_schema() {
        let schema = json!({
            "type": "object",
            "properties": {
                "url": {"type": "string", "description": "The URL to fetch"}
            },
            "required": ["url"]
        });
        let tool = ToolDef {
            name: "fetch".into(),
            description: "Fetch a URL".into(),
            input_schema: schema,
            source: None,
        };
        let body = build_tool_prompt("fetcher", &tool);
        assert!(body.contains("fetch"));
        assert!(body.contains("\"url\""));
        assert!(body.contains("\"required\""));
    }
}
