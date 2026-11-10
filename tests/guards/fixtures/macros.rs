use std::process::Command;

#[derive(serde::Serialize)]
pub struct Wire {
    #[serde(with = "crate::codec::base64")]
    pub bytes: Vec<u8>,
}

#[cfg(feature = "mcp")]
pub fn with_mcp() {}

#[cfg_attr(feature = "mcp", allow(dead_code))]
pub fn maybe() -> bool {
    cfg!(feature = "mcp")
}

pub fn in_macros() {
    let _ = vec![telemetry::TelemetryEvent::started(1)];
    let _ = serde_json::json!({ "k": telemetry::TelemetryEvent::finished(2) });
}

pub fn spawn() {
    let _ = Command::new("true");
}
