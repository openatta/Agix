//! 池级遥测、录制目录、OTel sink 与会话时钟。

use super::*;

/// Build the pool-level telemetry handle — see `SessionPool::pool_telemetry`'s
/// doc comment for what this is for. Same shape as the per-session pipeline
/// `create()` builds (`FileRecorder` + optional OTel sink via
/// `telemetry::spawn`), just writing to one shared `pool.jsonl` instead of
/// per-session files, since these events happen before any session exists.
pub(super) fn build_pool_telemetry(
    settings: &base::settings::LoadedSettings,
) -> telemetry::TelemetryHandle {
    #[cfg(feature = "otel")]
    for name in OTLP_HEADER_VARIABLES {
        base::interface::exec::env::hold_back_credential(name);
    }
    if !settings.telemetry_enabled {
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
    let rec = match FileRecorder::new(&path) {
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
    if let Some(otel) = otel_sink(settings.telemetry_url.as_deref()) {
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

/// Where recordings go: what the caller asked for, else what this deployment
/// configured, else the conventional user-level directory.
///
/// The per-call override stays because a test harness points recordings at a
/// fixture directory; a host that simply wants recording on should not have to
/// invent a path, so `settings.recorder` is consulted next.
pub(super) fn recordings_root(
    settings: &base::settings::LoadedSettings,
    requested: Option<&str>,
) -> PathBuf {
    if let Some(dir) = requested {
        return PathBuf::from(dir);
    }
    if let Some(recorder) = &settings.recorder {
        return recorder.root.clone();
    }
    base::paths::ConfigPaths::from_settings(&settings.paths).global_recordings_dir()
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

/// A `TelemetryRecorder` that forwards turn/tool events to OpenTelemetry, or
/// `None` when no OTLP endpoint is configured.
///
/// Every session gets the same process-wide exporter
/// (`crate::telemetry_otel::process_exporter`); a session's pipeline ending
/// flushes it, and only the process's own shutdown stops it.
///
/// The `telemetry` crate itself never talks to the network — this recorder
/// is how the daemon opts in to sending data anywhere. Compiled out unless
/// the daemon's own `otel` feature is on — the OTLP SDK is a heavy
/// dependency tree and the JSONL recorder is what makes a session debuggable
/// by default. Build the daemon with `--features otel` to get this.
#[cfg(feature = "otel")]
pub(super) fn otel_sink(
    endpoint: Option<&str>,
) -> Option<std::sync::Arc<dyn telemetry::TelemetryRecorder>> {
    let endpoint = endpoint
        .map(str::to_string)
        .or_else(|| std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").ok())
        .filter(|e| !e.trim().is_empty())?;
    crate::telemetry_otel::process_exporter(&endpoint)
        .map(|e| e as std::sync::Arc<dyn telemetry::TelemetryRecorder>)
}

/// No-op stand-in when the `otel` feature is off.
#[cfg(not(feature = "otel"))]
pub(super) fn otel_sink(
    endpoint: Option<&str>,
) -> Option<std::sync::Arc<dyn telemetry::TelemetryRecorder>> {
    if endpoint.is_some_and(|e| !e.trim().is_empty()) {
        warn!(
            "an OTLP endpoint is configured but this build has no OpenTelemetry support; \
             rebuild with `--features otel`"
        );
    }
    None
}

/// The clock a session's kept time comes from, when it is not the machine's: the start the
/// host asked for, else — replaying — the moment the recording began, so the date in the
/// prompt is the one the recording's prompt carried.
pub(super) fn session_clock(
    options: Option<&SessionOptions>,
    recorder: Option<&RecorderConfig>,
    session_id: &str,
) -> Option<Arc<dyn base::interface::environment::Environment>> {
    use base::interface::environment::ShiftedEnvironment;
    if let Some(clock) = options.and_then(|o| o.clock.as_ref()) {
        match time::OffsetDateTime::parse(
            &clock.start,
            &time::format_description::well_known::Rfc3339,
        ) {
            Ok(start) => return Some(Arc::new(ShiftedEnvironment::starting_at(start))),
            Err(e) => {
                tracing::warn!(start = %clock.start, error = %e, "clock.start is not RFC 3339; using the machine's clock")
            }
        }
    }
    let recorder = recorder.filter(|r| r.mode == RecorderMode::Replay)?;
    let name = recorder.name.as_deref().unwrap_or(session_id);
    let started = telemetry::recorder::recorded_at(&recorder.root, name)?;
    ShiftedEnvironment::starting_at_ms(started)
        .map(|e| Arc::new(e) as Arc<dyn base::interface::environment::Environment>)
}
