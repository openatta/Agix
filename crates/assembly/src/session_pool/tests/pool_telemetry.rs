//! `pool_telemetry_tests` 原样搬来的用例。

use super::*;

#[tokio::test]
async fn relative_local_data_dir_yields_a_noop_handle() {
    // `Settings::defaults_for` leaves `local_data_dir` at "." (relative)
    // — the same "don't scatter a telemetry/ dir into the working
    // directory" guard `create()`'s per-session path uses.
    let settings = Settings::defaults_for("claude-sonnet-4-6");
    let handle = build_pool_telemetry(&settings);
    // A noop handle's `record` always succeeds and writes nothing
    // anywhere observable; the real assertion is in the next test,
    // which proves the non-noop path actually persists.
    assert!(handle
        .record(telemetry::TelemetryEvent::resume_action(
            "sess",
            0,
            None,
            telemetry::ResumeActionPayload {
                outcome: telemetry::ResumeOutcome::Failed,
                source: "not_found".into(),
                entry_count: 0,
                projected_message_count: 0,
                compact_boundary_count: 0,
                sidechain_entry_count: 0,
                warning_kind: Some("not_found".into()),
                latency_ms: 0,
            },
        ))
        .is_ok());
}

#[tokio::test]
async fn telemetry_disabled_yields_a_noop_handle_even_with_an_absolute_dir() {
    let dir = tempfile::tempdir().unwrap();
    let mut settings = Settings::defaults_for("claude-sonnet-4-6");
    settings.telemetry_enabled = false;
    settings.paths.local_data_dir = dir.path().to_path_buf();
    let _handle = build_pool_telemetry(&settings);
    assert!(!dir.path().join("telemetry").join("pool.jsonl").exists());
}

#[tokio::test]
async fn absolute_dir_and_telemetry_enabled_persists_events_to_pool_jsonl() {
    let dir = tempfile::tempdir().unwrap();
    let mut settings = Settings::defaults_for("claude-sonnet-4-6");
    settings.telemetry_enabled = true;
    settings.paths.local_data_dir = dir.path().to_path_buf();
    let handle = build_pool_telemetry(&settings);

    handle
        .record(telemetry::TelemetryEvent::resume_action(
            "sess-1",
            0,
            None,
            telemetry::ResumeActionPayload {
                outcome: telemetry::ResumeOutcome::Failed,
                source: "not_found".into(),
                entry_count: 0,
                projected_message_count: 0,
                compact_boundary_count: 0,
                sidechain_entry_count: 0,
                warning_kind: Some("not_found".into()),
                latency_ms: 5,
            },
        ))
        .expect("record should succeed");

    // `FileRecorder` writes synchronously, but the event still has to
    // cross the channel to the fan-out task first — poll briefly rather
    // than assume it has landed the instant `record()` returns.
    let path = dir.path().join("telemetry").join("pool.jsonl");
    let content = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if let Ok(content) = tokio::fs::read_to_string(&path).await {
                if !content.is_empty() {
                    return content;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("pool.jsonl should have received the event");

    assert!(content.contains("resume_action"));
    assert!(content.contains("not_found"));
}
