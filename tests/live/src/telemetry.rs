//! Reading the telemetry file a session was told to write
//! (`options.telemetry.output`).
//!
//! Telemetry leaves the engine on a channel and is written by another task,
//! so a turn's last events land some time after the RPC that produced them
//! has already answered. Reading the file the moment a turn returns is a race
//! that only loses under load — which is to say, in CI — so the reader here
//! waits for what the case needs.

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::Value;

/// The events written to `path` so far, re-read until `enough` holds or ten
/// seconds pass; the caller asserts on what it got either way.
pub async fn telemetry_until(path: &Path, enough: impl Fn(&[Value]) -> bool) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let events: Vec<Value> = std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        if enough(&events) || Instant::now() > deadline {
            return events;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Each event's `type`, in order.
pub fn telemetry_kinds(events: &[Value]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|e| e.get("type").and_then(|t| t.as_str()))
        .collect()
}
