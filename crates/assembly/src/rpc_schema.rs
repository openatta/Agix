//! Machine-readable description of the JSON-RPC interface: `docs/schemas/rpc.schema.json`.
//!
//! The method, event-kind and error-code tables here are the schema's
//! skeleton; `daemon/tests/protocol_doc_matches_dispatch.rs` holds them equal
//! to the dispatch table, the protocol doc and `rpc::codes`. Params and
//! results are typed only where a method already parses into a struct; the
//! rest is marked untyped rather than guessed. Client code generation is the
//! consumer's job; this file is the input.
//!
//! Regenerate: `cargo test -p assembly write_rpc_schema -- --ignored`.

use schemars::SchemaGenerator;
use serde_json::{json, Map, Value};

use crate::rpc::{RpcError, RpcRequest, RpcResponse, StartAgentParams};

const METHODS: &[&str] = &[
    "commands.list",
    "config.get",
    "config.getProvider",
    "config.reload",
    "config.setProvider",
    "daemon.doctor",
    "daemon.ping",
    "daemon.shutdown",
    "daemon.status",
    "daemon.subscribeEvents",
    "import.list",
    "import.run",
    "mcp.addServer",
    "mcp.auth.cancel",
    "mcp.auth.complete",
    "mcp.auth.logout",
    "mcp.auth.setIdentity",
    "mcp.auth.start",
    "mcp.status",
    "mcp.updateServer",
    "plugin.disable",
    "plugin.enable",
    "plugin.install",
    "plugin.list",
    "plugin.reload",
    "plugin.uninstall",
    "project.status",
    "project.trust",
    "scene.activate",
    "scene.deactivate",
    "scene.describe",
    "scene.list",
    "scene.register",
    "scene.reload",
    "scene.unregister",
    "schedule.create",
    "schedule.delete",
    "schedule.list",
    "session.appendEntry",
    "session.clear",
    "session.close",
    "session.compact",
    "session.create",
    "session.delete",
    "session.dropMessages",
    "session.editMessage",
    "session.entries",
    "session.export",
    "session.fork",
    "session.get",
    "session.goal",
    "session.history",
    "session.inspectContext",
    "session.interrupt",
    "session.list",
    "session.messageTask",
    "session.rename",
    "session.respondToPrompt",
    "session.resume",
    "session.run_turn",
    "session.setGoal",
    "session.setModel",
    "session.setPrompt",
    "session.startAgent",
    "session.stopTask",
    "session.subscribe",
    "session.taskOutput",
    "session.tasks",
    "session.unsubscribe",
    "session.updateGoal",
];

const SESSION_EVENT_KINDS: &[&str] = &[
    "turn_start",
    "step_start",
    "step_end",
    "text_delta",
    "thinking_delta",
    "tool_use_start",
    "tool_args_delta",
    "tool_use",
    "tool_progress",
    "tool_result",
    "prompt",
    "subagent_progress",
    "team_progress",
    "skills_changed",
    "notification",
    "compact",
    "user_message_delivered",
    "user_message_refused",
    "user_messages_returned",
    "turn_complete",
    "error",
    "task_started",
    "task_updated",
    "task_progress",
    "task_finished",
    "goal_changed",
    "extension_entry",
    "subscription_dropped",
];

const DAEMON_EVENT_KINDS: &[&str] = &[
    "mcp_connected",
    "mcp_connect_failed",
    "mcp_auth_required",
    "mcp_auth_completed",
    "mcp_auth_failed",
    "import_detected",
    "session_created",
    "session_resumed",
    "session_evicted",
    "session_closed",
    "session_deleted",
    "turn_started",
    "turn_completed",
    "events_lagged",
    "project_trust_changed",
];

const ERROR_CODES: &[(i32, &str)] = &[
    (-32700, "PARSE_ERROR"),
    (-32600, "INVALID_REQUEST"),
    (-32601, "METHOD_NOT_FOUND"),
    (-32602, "INVALID_PARAMS"),
    (-32603, "INTERNAL_ERROR"),
    (-32000, "SESSION_NOT_FOUND"),
    (-32002, "ENGINE_ERROR"),
    (-32003, "UNAUTHORIZED"),
    (-32006, "SCENE_MISMATCH"),
    (-32010, "PROJECT_NOT_FOUND"),
    (-32004, "SCENE_NOT_FOUND"),
    (-32009, "SCENE_HAS_ACTIVE_SESSIONS"),
    (-32014, "SIDECHAIN_TERMINAL"),
    (-32016, "PLUGINS_DISABLED"),
    (-32017, "TOO_MANY_IN_FLIGHT"),
    (-32015, "SESSION_BUSY"),
    (-32012, "PROJECT_REQUIRED"),
    (-32018, "TASK_NOT_FOUND"),
    (-32019, "TASK_LIMIT"),
    (-32020, "MCP_SERVER_DENIED"),
    (-32021, "MCP_AUTH_FAILED"),
    (-32022, "PROJECT_UNTRUSTED"),
];

const UNTYPED: &str = "未类型化:字段见 docs/daemon_rpc_protocol.md 对应小节";

fn typed_params(method: &str, generator: &mut SchemaGenerator) -> Option<Value> {
    match method {
        "session.startAgent" => Some(schema_value(generator.subschema_for::<StartAgentParams>())),
        _ => None,
    }
}

fn schema_value(schema: schemars::Schema) -> Value {
    serde_json::to_value(schema).unwrap_or(Value::Null)
}

fn untyped() -> Value {
    json!({ "description": UNTYPED })
}

fn kinds_schema(kinds: &[&str], section: &str) -> Value {
    let mut map = Map::new();
    for kind in kinds {
        map.insert(
            (*kind).to_string(),
            json!({
                "type": "object",
                "properties": { "kind": { "const": kind } },
                "required": ["kind"],
                "description": format!("{UNTYPED}(协议文档 {section} 的 kind 表)"),
            }),
        );
    }
    Value::Object(map)
}

pub fn rpc_json_schema() -> Value {
    let mut generator = SchemaGenerator::default();
    let request = schema_value(generator.subschema_for::<RpcRequest>());
    let response = schema_value(generator.subschema_for::<RpcResponse>());
    let error = schema_value(generator.subschema_for::<RpcError>());

    let mut methods = Map::new();
    for method in METHODS {
        let (params, params_typed) = match typed_params(method, &mut generator) {
            Some(schema) => (schema, true),
            None => (untyped(), false),
        };
        methods.insert(
            (*method).to_string(),
            json!({
                "params": params,
                "result": untyped(),
                "params_typed": params_typed,
                "result_typed": false,
            }),
        );
    }

    let errors: Vec<Value> = ERROR_CODES
        .iter()
        .map(|(code, name)| json!({ "code": code, "name": name }))
        .collect();

    let mut root = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "Agix daemon JSON-RPC",
        "description": "daemon 的 JSON-RPC 2.0 接口。语义见 docs/daemon_rpc_protocol.md;params/result 标着\"未类型化\"的,服务端手拼 JSON,这里不做约束。daemon.auth 是连接握手,不在 methods 里(协议文档 §5.1)。",
        "frames": { "request": request, "response": response, "error": error },
        "methods": methods,
        "notifications": {
            "session.event": {
                "description": "params 为 {session_id, turn_id, event:{kind,…}};协议文档 §7",
                "kinds": kinds_schema(SESSION_EVENT_KINDS, "§7"),
            },
            "daemon.event": {
                "description": "params 为 {kind,…};协议文档 §8",
                "kinds": kinds_schema(DAEMON_EVENT_KINDS, "§8"),
            },
        },
        "errors": errors,
    });
    root["$defs"] = Value::Object(generator.take_definitions(true));
    root
}

pub fn method_names() -> &'static [&'static str] {
    METHODS
}

pub fn session_event_kinds() -> &'static [&'static str] {
    SESSION_EVENT_KINDS
}

pub fn daemon_event_kinds() -> &'static [&'static str] {
    DAEMON_EVENT_KINDS
}

pub fn error_codes() -> &'static [(i32, &'static str)] {
    ERROR_CODES
}

#[cfg(test)]
mod tests {
    use super::*;

    fn committed_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/schemas/rpc.schema.json")
    }

    fn rendered() -> String {
        serde_json::to_string_pretty(&rpc_json_schema()).unwrap() + "\n"
    }

    #[test]
    fn rpc_schema_matches_committed_file() {
        let existing = std::fs::read_to_string(committed_path()).unwrap_or_default();
        assert!(
            existing == rendered(),
            "docs/schemas/rpc.schema.json is out of date; run `cargo test -p assembly write_rpc_schema -- --ignored` and commit it"
        );
    }

    #[test]
    #[ignore]
    fn write_rpc_schema() {
        let path = committed_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, rendered()).unwrap();
    }
}
