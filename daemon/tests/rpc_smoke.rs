//! The JSON-RPC envelope through the public API. Pure data tests; no real
//! socket needed.

use daemon::rpc::{codes, RpcRequest, RpcResponse};
use serde_json::json;

#[test]
fn rpc_request_deserializes_canonical_form() {
    let raw = r#"{
        "jsonrpc": "2.0",
        "method": "session.create",
        "params": {"cwd": "/tmp"},
        "id": 1
    }"#;
    let req: RpcRequest = serde_json::from_str(raw).unwrap();
    assert_eq!(req.jsonrpc, "2.0");
    assert_eq!(req.method, "session.create");
    assert_eq!(req.id, Some(json!(1)));
    assert_eq!(req.params, json!({"cwd": "/tmp"}));
}

#[test]
fn rpc_request_default_jsonrpc_field() {
    let raw = r#"{"method": "ping", "id": "abc", "params": null}"#;
    let req: RpcRequest = serde_json::from_str(raw).unwrap();
    assert_eq!(req.jsonrpc, "2.0");
    assert_eq!(req.method, "ping");
    assert_eq!(req.id, Some(json!("abc")));
}

#[test]
fn rpc_response_success_serializes_without_error_key() {
    let resp = RpcResponse::ok(json!(1), json!({"session_id": "sess_01"}));
    let v: serde_json::Value = serde_json::to_value(&resp).unwrap();
    assert_eq!(v["jsonrpc"], "2.0");
    assert_eq!(v["result"]["session_id"], "sess_01");
    assert!(v.get("error").is_none(), "error key should be omitted");
}

#[test]
fn rpc_response_error_serializes_without_result_key() {
    let resp = RpcResponse::err(json!("xyz"), codes::METHOD_NOT_FOUND, "Method not found");
    let v: serde_json::Value = serde_json::to_value(&resp).unwrap();
    assert!(v.get("result").is_none(), "result key should be omitted");
    assert_eq!(v["error"]["code"], -32601);
    assert_eq!(v["error"]["message"], "Method not found");
}

#[test]
fn rpc_error_code_constants_match_jsonrpc_spec() {
    assert_eq!(codes::PARSE_ERROR, -32700);
    assert_eq!(codes::INVALID_REQUEST, -32600);
    assert_eq!(codes::METHOD_NOT_FOUND, -32601);
    assert_eq!(codes::INVALID_PARAMS, -32602);
    assert_eq!(codes::INTERNAL_ERROR, -32603);
}
