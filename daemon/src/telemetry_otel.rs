//! OpenTelemetry OTLP export — optional feature-gated module.
//!
//! This is the daemon's own `TelemetryRecorder`: the `telemetry` crate never
//! talks to the network itself, so anything that actually sends events
//! somewhere (here, an OTLP collector) lives on the caller's side of that
//! trait boundary. See `session_pool::otel_sink` for where this gets
//! constructed and wired into a session's recorder list.
//!
//! When the `otel` feature is enabled, this module provides an [`OtelExporter`]
//! that initialises an OTLP pipeline (trace + metrics) against the configured
//! endpoint. Protocol auto-detection: port 4317 → gRPC, port 4318 → HTTP/protobuf.
//!
//! # Metrics & spans exported
//!
//! - Turn duration histogram (`agix.turn.duration`)
//! - Tool execution counter (`agix.tool.executions`)
//! - API call latency histogram (`agix.api.latency`)
//! - Token usage histogram (`agix.token.usage`)

use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry_otlp::WithExportConfig;
use url::Url;

// ── Protocol detection ──────────────────────────────────────────────────────

/// OTLP transport protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OtelProtocol {
    /// gRPC (default port 4317).
    Grpc,
    /// HTTP + Protobuf (default port 4318).
    HttpProtobuf,
}

/// Detect transport protocol from the endpoint URL.
///
/// Rules:
/// - Port 4317 → `Grpc`
/// - Port 4318 → `HttpProtobuf`
/// - Fallback → `HttpProtobuf` (safe default for most collector deployments)
pub fn detect_protocol(endpoint: &str) -> OtelProtocol {
    let parsed = Url::parse(endpoint).ok();
    match parsed.and_then(|u| u.port()) {
        Some(4317) => OtelProtocol::Grpc,
        Some(4318) => OtelProtocol::HttpProtobuf,
        _ => OtelProtocol::HttpProtobuf,
    }
}

// ── Error type ──────────────────────────────────────────────────────────────

/// Errors that can occur during OTLP exporter initialisation.
#[derive(Debug, thiserror::Error)]
pub enum OtelError {
    #[error("invalid endpoint URL: {0}")]
    InvalidEndpoint(String),
    #[error("OTLP exporter initialisation failed: {0}")]
    InitFailed(String),
}

// ── Exporter ────────────────────────────────────────────────────────────────

/// Holds the OTLP meter- and tracer-provider so they are kept alive for the
/// lifetime of the application. Dropping the exporter triggers a graceful
/// shutdown of the pipeline.
#[must_use]
pub struct OtelExporter {
    meter_provider: Option<opentelemetry_sdk::metrics::SdkMeterProvider>,
    tracer_provider: Option<opentelemetry_sdk::trace::TracerProvider>,
}

impl OtelExporter {
    /// Shut down the exporter and flush any remaining data.
    pub fn shutdown(&self) {
        if let Some(ref mp) = self.meter_provider {
            if let Err(e) = mp.shutdown() {
                tracing::warn!(error = %e, "otel meter provider shutdown failed");
            }
        }
        if let Some(ref tp) = self.tracer_provider {
            if let Err(e) = tp.shutdown() {
                tracing::warn!(error = %e, "otel tracer provider shutdown failed");
            }
        }
    }
}

impl Drop for OtelExporter {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Route the telemetry event stream into the OTLP instruments.
///
/// Only the variants with a natural metric counterpart are mapped; the rest
/// are already captured in full by the JSONL recorder, and inventing
/// instruments for them here would produce cardinality without meaning.
impl telemetry::TelemetryRecorder for OtelExporter {
    fn record(
        &self,
        event: telemetry::TelemetryEvent,
    ) -> Result<(), telemetry::TelemetryHandleError> {
        use telemetry::EventPayload;
        match event.payload {
            EventPayload::TurnComplete(p) => {
                record_turn_duration(p.turn_duration_ms as f64);
            }
            EventPayload::ToolExecution(p) => {
                record_tool_execution(
                    &p.tool_name,
                    matches!(p.outcome, telemetry::ToolOutcome::Succeeded),
                );
            }
            EventPayload::ApiRequest(p) => {
                record_api_latency(p.latency_ms as f64, &p.model);
                record_token_usage(
                    p.input_tokens,
                    p.output_tokens,
                    p.cache_creation_tokens,
                    p.cache_read_tokens,
                    &p.model,
                );
            }
            _ => {}
        }
        Ok(())
    }

    fn shutdown(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        // `OtelExporter::shutdown` is synchronous (it drives the SDK's own
        // flush), so there is nothing to await — but the trait is async
        // because other recorders do need to await a drain.
        OtelExporter::shutdown(self);
        Box::pin(std::future::ready(()))
    }
}

// ── Initialisation ──────────────────────────────────────────────────────────

/// Initialise an OTLP exporter against `endpoint`.
///
/// Protocol is auto-detected from the endpoint URL (see [`detect_protocol`]).
/// Returns an error if the endpoint is empty.
///
/// # Metrics created
///
/// The global meter provider is configured with instruments for:
///
/// | Instrument        | Name                   | Type        | Unit    |
/// |-------------------|------------------------|-------------|---------|
/// | Turn duration     | `agix.turn.duration`   | Histogram   | ms      |
/// | Tool executions   | `agix.tool.executions` | Counter     | 1       |
/// | API call latency  | `agix.api.latency`     | Histogram   | ms      |
/// | Token usage       | `agix.token.usage`     | Histogram   | 1       |
pub fn start_otel(endpoint: &str) -> Result<OtelExporter, OtelError> {
    if endpoint.is_empty() {
        return Err(OtelError::InvalidEndpoint("endpoint is empty".into()));
    }

    let protocol = detect_protocol(endpoint);

    let resource = opentelemetry_sdk::Resource::new(vec![KeyValue::new("service.name", "agix")]);

    // ── Trace exporter ──────────────────────────────────────────────────
    let trace_exporter = match protocol {
        OtelProtocol::Grpc => opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .with_endpoint(endpoint)
            .with_timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| OtelError::InitFailed(e.to_string()))?,
        OtelProtocol::HttpProtobuf => opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_endpoint(endpoint)
            .with_timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| OtelError::InitFailed(e.to_string()))?,
    };

    let tracer_provider = opentelemetry_sdk::trace::TracerProvider::builder()
        .with_batch_exporter(trace_exporter, opentelemetry_sdk::runtime::Tokio)
        .with_resource(resource.clone())
        .build();

    opentelemetry::global::set_tracer_provider(tracer_provider.clone());

    // ── Metric exporter ─────────────────────────────────────────────────
    let metric_exporter = match protocol {
        OtelProtocol::Grpc => opentelemetry_otlp::MetricExporter::builder()
            .with_tonic()
            .with_endpoint(endpoint)
            .with_timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| OtelError::InitFailed(e.to_string()))?,
        OtelProtocol::HttpProtobuf => opentelemetry_otlp::MetricExporter::builder()
            .with_http()
            .with_endpoint(endpoint)
            .with_timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| OtelError::InitFailed(e.to_string()))?,
    };

    let meter_provider = opentelemetry_sdk::metrics::SdkMeterProvider::builder()
        .with_reader(
            opentelemetry_sdk::metrics::PeriodicReader::builder(
                metric_exporter,
                opentelemetry_sdk::runtime::Tokio,
            )
            .with_interval(Duration::from_secs(60))
            .build(),
        )
        .with_resource(resource)
        .build();

    opentelemetry::global::set_meter_provider(meter_provider.clone());

    // Pre-register standard instruments via the global meter.
    let meter = opentelemetry::global::meter("agix");

    let _turn_duration = meter
        .f64_histogram("agix.turn.duration")
        .with_description("Turn execution duration")
        .with_unit("ms")
        .build();

    let _tool_executions = meter
        .u64_counter("agix.tool.executions")
        .with_description("Number of tool executions")
        .with_unit("1")
        .build();

    let _api_latency = meter
        .f64_histogram("agix.api.latency")
        .with_description("API call latency")
        .with_unit("ms")
        .build();

    let _token_usage = meter
        .u64_histogram("agix.token.usage")
        .with_description("Token usage distribution")
        .with_unit("1")
        .build();

    Ok(OtelExporter {
        meter_provider: Some(meter_provider),
        tracer_provider: Some(tracer_provider),
    })
}

// ── Instrument helpers ──────────────────────────────────────────────────────

/// Record a turn duration via the global meter provider.
///
/// This is a no-op if the global meter provider has not been configured
/// (i.e. `start_otel` was not called).
pub fn record_turn_duration(duration_ms: f64) {
    let meter = opentelemetry::global::meter("agix");
    meter
        .f64_histogram("agix.turn.duration")
        .with_description("Turn execution duration")
        .with_unit("ms")
        .build()
        .record(duration_ms, &[]);
}

/// Record a tool execution via the global meter provider.
///
/// This is a no-op if the global meter provider has not been configured.
pub fn record_tool_execution(tool_name: &str, success: bool) {
    let meter = opentelemetry::global::meter("agix");
    let outcome = if success { "success" } else { "failure" };
    meter
        .u64_counter("agix.tool.executions")
        .with_description("Number of tool executions")
        .with_unit("1")
        .build()
        .add(
            1,
            &[
                KeyValue::new("tool", tool_name.to_string()),
                KeyValue::new("outcome", outcome),
            ],
        );
}

/// Record API call latency via the global meter provider.
///
/// This is a no-op if the global meter provider has not been configured.
pub fn record_api_latency(latency_ms: f64, model: &str) {
    let meter = opentelemetry::global::meter("agix");
    meter
        .f64_histogram("agix.api.latency")
        .with_description("API call latency")
        .with_unit("ms")
        .build()
        .record(latency_ms, &[KeyValue::new("model", model.to_string())]);
}

/// Record token usage via the global meter provider.
///
/// This is a no-op if the global meter provider has not been configured.
/// One call's token counts, one histogram, one `direction` per kind.
///
/// Cache reads and writes are their own directions rather than being folded
/// into `input`: they are priced differently, so a dashboard that added them
/// together could not compute a cost from what it sees.
pub fn record_token_usage(
    input_tokens: u64,
    output_tokens: u64,
    cache_creation_tokens: u64,
    cache_read_tokens: u64,
    model: &str,
) {
    let meter = opentelemetry::global::meter("agix");
    let hist = meter
        .u64_histogram("agix.token.usage")
        .with_description("Token usage distribution")
        .with_unit("1")
        .build();
    hist.record(
        input_tokens,
        &[
            KeyValue::new("direction", "input"),
            KeyValue::new("model", model.to_string()),
        ],
    );
    hist.record(
        output_tokens,
        &[
            KeyValue::new("direction", "output"),
            KeyValue::new("model", model.to_string()),
        ],
    );
    hist.record(
        cache_creation_tokens,
        &[
            KeyValue::new("direction", "cache_creation"),
            KeyValue::new("model", model.to_string()),
        ],
    );
    hist.record(
        cache_read_tokens,
        &[
            KeyValue::new("direction", "cache_read"),
            KeyValue::new("model", model.to_string()),
        ],
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_grpc_from_port_4317() {
        assert_eq!(detect_protocol("http://localhost:4317"), OtelProtocol::Grpc);
    }

    #[test]
    fn detect_http_from_port_4318() {
        assert_eq!(
            detect_protocol("http://localhost:4318"),
            OtelProtocol::HttpProtobuf
        );
    }

    #[test]
    fn detect_defaults_to_http() {
        assert_eq!(
            detect_protocol("http://otel-collector.example.com:55681"),
            OtelProtocol::HttpProtobuf
        );
    }

    #[test]
    fn detect_fallback_on_unparseable_url() {
        assert_eq!(
            detect_protocol("not-a-valid-url"),
            OtelProtocol::HttpProtobuf
        );
    }

    #[test]
    fn detect_handles_https_and_paths() {
        assert_eq!(
            detect_protocol("https://collector.example.com/v1/traces"),
            OtelProtocol::HttpProtobuf
        );
    }

    #[test]
    fn start_otel_returns_error_on_empty_endpoint() {
        let result = start_otel("");
        assert!(result.is_err());
    }
}
