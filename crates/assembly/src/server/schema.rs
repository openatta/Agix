//! 机器可读的 JSON-RPC 接口描述 `docs/schemas/rpc.schema.json`，从方法表
//! （[`super::methods`]）、`ErrorCode::ALL` 和两种通知的事件类型生成。
//!
//! Regenerate: `cargo test -p assembly write_rpc_schema -- --ignored`.

use schemars::SchemaGenerator;
use serde_json::{json, Map, Value};

use crate::rpc::{ErrorCode, RpcError, RpcRequest, RpcResponse};

const UNTYPED: &str = "未类型化:字段见 docs/daemon_rpc_protocol.md 对应小节";

fn schema_value(schema: schemars::Schema) -> Value {
    serde_json::to_value(schema).unwrap_or(Value::Null)
}

/// The variants of an enum tagged by `kind`, each under its tag.
fn tagged_variants<T: schemars::JsonSchema>(generator: &mut SchemaGenerator) -> Map<String, Value> {
    let reference = schema_value(generator.subschema_for::<T>());
    let name = reference["$ref"]
        .as_str()
        .and_then(|r| r.rsplit('/').next())
        .unwrap_or_default()
        .to_string();
    let schema = generator
        .definitions()
        .get(&name)
        .cloned()
        .unwrap_or(reference);
    let mut variants = Map::new();
    for variant in schema["oneOf"].as_array().into_iter().flatten() {
        if let Some(kind) = variant["properties"]["kind"]["const"].as_str() {
            variants.insert(kind.to_string(), variant.clone());
        }
    }
    variants
}

fn daemon_event_variants(generator: &mut SchemaGenerator) -> Map<String, Value> {
    let mut kinds = tagged_variants::<crate::session_pool::api::DaemonEvent>(generator);
    kinds.extend(tagged_variants::<crate::rpc::ServerDaemonEvent>(generator));
    kinds
}

fn untyped_kinds(kinds: &[&str], section: &str) -> Value {
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
    for info in super::methods() {
        let mut method = json!({
            "params": schema_value((info.params)(&mut generator)),
            "result": schema_value((info.result)(&mut generator)),
        });
        if let Some(module) = info.module {
            method["module"] = json!(module);
        }
        methods.insert(info.name.to_string(), method);
    }

    let errors: Vec<Value> = ErrorCode::ALL
        .iter()
        .map(|code| json!({ "code": code.code(), "name": code.name() }))
        .collect();

    let mut root = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "Agix daemon JSON-RPC",
        "description": "daemon 的 JSON-RPC 2.0 接口。语义见 docs/daemon_rpc_protocol.md;params/result 标着\"未类型化\"的,这里不做约束。带 module 的方法属于可选模块,构建里没有这个模块时答 METHOD_NOT_FOUND。daemon.auth 是连接握手,不在 methods 里(协议文档 §5.1)。",
        "frames": { "request": request, "response": response, "error": error },
        "methods": methods,
        "notifications": {
            "session.event": {
                "description": "params 为 {session_id, turn_id, event:{kind,…}};协议文档 §7",
                "kinds": untyped_kinds(session_event_kinds(), "§7"),
            },
            "daemon.event": {
                "description": "params 为 {kind,…};协议文档 §8",
                "kinds": daemon_event_variants(&mut generator),
            },
        },
        "errors": errors,
    });
    root["$defs"] = Value::Object(generator.take_definitions(true));
    root
}

pub fn session_event_kinds() -> &'static [&'static str] {
    crate::rpc::events::SessionEventPayload::KINDS
}

pub fn daemon_event_kinds() -> Vec<String> {
    daemon_event_variants(&mut SchemaGenerator::default())
        .into_iter()
        .map(|(kind, _)| kind)
        .collect()
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

    #[test]
    fn every_method_is_listed_once() {
        let mut names: Vec<&str> = super::super::methods().map(|m| m.name).collect();
        let listed = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), listed, "a method is in the table twice");
        assert_eq!(listed, 70);
    }
}
