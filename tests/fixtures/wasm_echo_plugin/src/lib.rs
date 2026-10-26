//! The smallest plugin that exercises every part of the host contract.
//!
//! Its tools exist to be provoked: one echoes, one spins forever, one traps,
//! one asks for a URL it may or may not be allowed to fetch. The host's tests
//! use it to check that each of those ends the way the design says it should.

wit_bindgen::generate!({
    path: "../../../crates/wasm-host/wit",
    world: "plugin",
});

use exports::agix::plugin::tools::{Guest as ToolsGuest, ToolDef, ToolOutput};
use exports::agix::plugin::events::{Guest as EventsGuest, HookDecision};

struct Component;

impl ToolsGuest for Component {
    fn list_tools() -> Vec<ToolDef> {
        vec![
            ToolDef {
                name: "echo".into(),
                description: "Echo the `text` argument back".into(),
                doc: Some("Returns `text` verbatim, plus a structured copy.".into()),
                input_schema: r#"{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}"#.into(),
                read_only: true,
                concurrency_safe: true,
            },
            ToolDef {
                name: "spin".into(),
                description: "Loop forever".into(),
                doc: None,
                input_schema: r#"{"type":"object"}"#.into(),
                read_only: true,
                concurrency_safe: true,
            },
            ToolDef {
                name: "explode".into(),
                description: "Trap immediately".into(),
                doc: None,
                input_schema: r#"{"type":"object"}"#.into(),
                read_only: true,
                concurrency_safe: true,
            },
            ToolDef {
                name: "fetch".into(),
                description: "Fetch a URL through the host".into(),
                doc: None,
                input_schema: r#"{"type":"object","properties":{"url":{"type":"string"}}}"#.into(),
                read_only: true,
                concurrency_safe: false,
            },
            ToolDef {
                name: "remember".into(),
                description: "Store and read back a value across calls".into(),
                doc: None,
                input_schema: r#"{"type":"object","properties":{"value":{"type":"string"}}}"#.into(),
                read_only: false,
                concurrency_safe: false,
            },
        ]
    }

    fn call_tool(name: String, input_json: String, call_id: String) -> ToolOutput {
        match name.as_str() {
            "echo" => {
                let text = field(&input_json, "text").unwrap_or_default();
                agix::plugin::host::progress(&call_id, "echoing");
                ToolOutput {
                    content: text.clone(),
                    structured: Some(format!(r#"{{"echoed":"{text}"}}"#)),
                    is_error: false,
                }
            }
            "spin" => {
                let mut n: u64 = 0;
                loop {
                    n = n.wrapping_add(1);
                    std::hint::black_box(n);
                }
            }
            "explode" => unreachable!("this tool exists to trap"),
            "fetch" => {
                let url = field(&input_json, "url").unwrap_or_default();
                match agix::plugin::host::http_request("GET", &url, &[], None) {
                    Ok(bytes) => ToolOutput {
                        content: format!("{} bytes", bytes.len()),
                        structured: None,
                        is_error: false,
                    },
                    Err(e) => ToolOutput {
                        content: e,
                        structured: None,
                        is_error: true,
                    },
                }
            }
            "remember" => {
                let previous = agix::plugin::host::kv_get("last")
                    .map(|b| String::from_utf8_lossy(&b).into_owned())
                    .unwrap_or_else(|| "(none)".into());
                let value = field(&input_json, "value").unwrap_or_default();
                agix::plugin::host::kv_set("last", value.as_bytes());
                ToolOutput {
                    content: previous,
                    structured: None,
                    is_error: false,
                }
            }
            other => ToolOutput {
                content: format!("no such tool: {other}"),
                structured: None,
                is_error: true,
            },
        }
    }

}

impl EventsGuest for Component {
    fn on_event(event: String, _payload_json: String) -> HookDecision {
        match event.as_str() {
            "PreToolUse" => HookDecision::Block("the demo plugin blocks everything".into()),
            "SessionStart" => HookDecision::AddContext("demo plugin is active".into()),
            _ => HookDecision::Proceed,
        }
    }
}

impl Guest for Component {
    fn init(config_json: String) -> Result<(), String> {
        if config_json.contains("\"fail\":true") {
            return Err("configuration asked me to fail".into());
        }
        Ok(())
    }
}

/// Pull a top-level string field out without pulling in a JSON parser — this
/// fixture is meant to stay small enough to compile quickly.
fn field(json: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let start = json.find(&needle)? + needle.len();
    let rest = json[start..].trim_start().strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

export!(Component);
