//! 守护进程级的事件出口 `DaemonEvents`：`daemon.subscribeEvents` 的广播，加上池级
//! 遥测（`pool.jsonl`）。克隆很便宜，要发事件的协作者各持一份。不持锁。

use crate::session_pool::api::DaemonEvent;
use crate::session_pool::pool_telemetry::otel_sink;
use tokio::sync::broadcast;
use tracing::warn;

#[derive(Clone)]
pub(in crate::session_pool) struct DaemonEvents {
    /// `send` returning `Err` only means nobody listens right now.
    tx: broadcast::Sender<DaemonEvent>,
    /// For what happens before any session's own handle exists — a resume
    /// refused, or answered by a session already live — which has nowhere
    /// else to record to. Every per-session event goes through that
    /// session's handle.
    telemetry: telemetry::TelemetryHandle,
}

impl DaemonEvents {
    pub(in crate::session_pool) fn new(settings: &base::settings::LoadedSettings) -> Self {
        let (tx, _) = broadcast::channel(1024);
        Self {
            tx,
            telemetry: build_pool_telemetry(settings),
        }
    }

    pub(in crate::session_pool) fn subscribe(&self) -> broadcast::Receiver<DaemonEvent> {
        self.tx.subscribe()
    }

    pub(in crate::session_pool) fn emit(&self, event: DaemonEvent) {
        if let Some(recorded) = event.telemetry() {
            let _ = self.telemetry.record(recorded);
        }
        let _ = self.tx.send(event);
    }

    pub(in crate::session_pool) fn record(&self, event: telemetry::TelemetryEvent) {
        let _ = self.telemetry.record(event);
    }

    pub(in crate::session_pool) fn record_startup(&self, timing: telemetry::StartupTimingPayload) {
        self.record(telemetry::TelemetryEvent::startup_timing(
            "", 0, None, timing,
        ));
    }
}

/// The pool-level telemetry handle: the per-session pipeline's shape
/// (`FileRecorder` plus the OTel sink when one is configured), writing to one
/// shared `<local_data_dir>/telemetry/pool.jsonl`. A no-op when telemetry is
/// off or `local_data_dir` is not an absolute path.
pub(in crate::session_pool) fn build_pool_telemetry(
    settings: &base::settings::LoadedSettings,
) -> telemetry::TelemetryHandle {
    #[cfg(feature = "otel")]
    for name in OTLP_HEADER_VARIABLES {
        base::interface::exec::env::hold_back_credential(name);
    }
    if !settings.values.telemetry_enabled {
        return telemetry::TelemetryHandle::noop();
    }
    let dir = &settings.paths.local_data_dir;
    if !dir.is_absolute() {
        // Same reasoning as `create()`'s per-session path: recording
        // nothing is better than scattering a `telemetry/` directory into
        // whatever the process's working directory happens to be.
        return telemetry::TelemetryHandle::noop();
    }
    let path = dir.join("telemetry").join("pool.jsonl");
    let rec = match telemetry::file_recorder::FileRecorder::new(&path) {
        Ok(rec) => {
            base::paths::ensure_local_data_gitignore(dir);
            rec
        }
        Err(e) => {
            warn!(path = %path.display(), error = %e, "could not open pool-level telemetry file; pool-level events record nothing");
            return telemetry::TelemetryHandle::noop();
        }
    };
    let mut recorders: Vec<std::sync::Arc<dyn telemetry::TelemetryRecorder>> =
        vec![std::sync::Arc::new(rec)];
    if let Some(otel) = otel_sink(settings.values.telemetry_url.as_deref()) {
        recorders.push(otel);
    }
    let config = telemetry::TelemetryConfig {
        enabled: true,
        mode: telemetry::TelemetryMode::Enabled,
        default_event_enabled: true,
        ..Default::default()
    };
    match telemetry::spawn(config, recorders) {
        Ok((handle, consumer)) => {
            tokio::spawn(consumer);
            handle
        }
        Err(e) => {
            warn!(error = %e, "failed to start pool-level telemetry pipeline");
            telemetry::TelemetryHandle::noop()
        }
    }
}

/// What the OTLP exporter reads its request headers from — where a
/// collector's API key goes — so a build that exports holds them back from
/// every child process, whether or not this pool exports.
#[cfg(feature = "otel")]
const OTLP_HEADER_VARIABLES: &[&str] = &[
    "OTEL_EXPORTER_OTLP_HEADERS",
    "OTEL_EXPORTER_OTLP_TRACES_HEADERS",
    "OTEL_EXPORTER_OTLP_METRICS_HEADERS",
    "OTEL_EXPORTER_OTLP_LOGS_HEADERS",
];
