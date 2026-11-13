//! `agix.session/spec` 条目的载荷格式：0.3.11 写出的日志要能读回来，且原样写回得到同一份 JSON。

use super::*;

const SPEC_0_3_11: &str = include_str!("fixtures/spec_0.3.11.json");

#[test]
fn a_spec_entry_written_by_0_3_11_reads_back_and_writes_the_same_json() {
    let written: serde_json::Value = serde_json::from_str(SPEC_0_3_11).unwrap();
    let spec: RecordedSpec = serde_json::from_value(written.clone()).unwrap();

    let options = spec.options.as_ref().expect("options");
    assert_eq!(options.model.as_deref(), Some("m-options"));
    assert_eq!(
        options.clock.as_ref().unwrap().start,
        "2026-10-28T09:00:00Z"
    );
    assert_eq!(options.recorder.as_ref().unwrap().mode, "record");
    assert_eq!(options.telemetry.as_ref().unwrap().output, "/tmp/t.jsonl");
    assert_eq!(options.permission_rules.len(), 1);
    assert_eq!(options.prompt_types, ["question"]);
    assert_eq!(spec.provider.as_deref(), Some("p-spec"));

    assert_eq!(serde_json::to_value(&spec).unwrap(), written);
}

#[test]
fn an_empty_spec_entry_reads_as_no_spec() {
    let spec: RecordedSpec = serde_json::from_value(serde_json::json!({})).unwrap();
    assert!(spec.options.is_none() && spec.prompt.is_none());
    assert_eq!(serde_json::to_value(&spec).unwrap(), serde_json::json!({}));
}
