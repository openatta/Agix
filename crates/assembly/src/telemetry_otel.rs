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
//! endpoint. Protocol auto-detection: port 4317 → gRPC, anything else →
//! HTTP/protobuf, sent with the workspace's `reqwest` to `<endpoint>/v1/traces`
//! and `<endpoint>/v1/metrics`.
//!
//! # Metrics & spans exported
//!
//! - Turn duration histogram (`agix.turn.duration`)
//! - Tool execution counter (`agix.tool.executions`)
//! - API call latency histogram (`agix.api.latency`)
//! - Token usage histogram (`agix.token.usage`)

use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry_otlp::{WithExportConfig, WithHttpConfig};
use url::Url;

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

/// The OTLP/HTTP exporter's client: `opentelemetry-otlp` ships none unless one
/// of its own reqwest features is on, and builds nothing without it.
#[derive(Debug, Clone)]
struct OtlpHttpClient(reqwest::Client);

#[async_trait::async_trait]
impl opentelemetry_http::HttpClient for OtlpHttpClient {
    async fn send(
        &self,
        request: opentelemetry_http::Request<Vec<u8>>,
    ) -> Result<
        opentelemetry_http::Response<opentelemetry_http::Bytes>,
        opentelemetry_http::HttpError,
    > {
        let response = self
            .0
            .execute(reqwest::Request::try_from(request)?)
            .await?
            .error_for_status()?;
        let status = response.status();
        let headers = response.headers().clone();
        let mut out = opentelemetry_http::Response::builder()
            .status(status)
            .body(response.bytes().await?)?;
        *out.headers_mut() = headers;
        Ok(out)
    }
}

/// Where one signal goes on an OTLP/HTTP collector: the configured endpoint is
/// the collector's base, as `OTEL_EXPORTER_OTLP_ENDPOINT` is.
fn http_signal_endpoint(endpoint: &str, signal: &str) -> String {
    format!("{}/v1/{signal}", endpoint.trim_end_matches('/'))
}

/// A collector on this machine is reached directly: reqwest would otherwise
/// send loopback traffic through an ambient `HTTP(S)_PROXY` too.
fn is_loopback(endpoint: &str) -> bool {
    match Url::parse(endpoint).ok().as_ref().and_then(Url::host) {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        None => false,
    }
}

/// Errors that can occur during OTLP exporter initialisation.
#[derive(Debug, thiserror::Error)]
pub enum OtelError {
    #[error("invalid endpoint URL: {0}")]
    InvalidEndpoint(String),
    #[error("OTLP exporter initialisation failed: {0}")]
    InitFailed(String),
}

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

fn flush(
    meter_provider: Option<&opentelemetry_sdk::metrics::SdkMeterProvider>,
    tracer_provider: Option<&opentelemetry_sdk::trace::TracerProvider>,
) {
    if let Some(mp) = meter_provider {
        if let Err(e) = mp.force_flush() {
            tracing::warn!(error = %e, "otel meter provider flush failed");
        }
    }
    if let Some(tp) = tracer_provider {
        for e in tp.force_flush().into_iter().filter_map(Result::err) {
            tracing::warn!(error = %e, "otel tracer provider flush failed");
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

    /// Flushes and nothing more: one exporter serves every session's pipeline
    /// (see [`process_exporter`]), and this is called each time one of those
    /// pipelines ends. The SDK's flush blocks on its own Tokio tasks, hence the
    /// blocking thread.
    fn shutdown(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        let meter_provider = self.meter_provider.clone();
        let tracer_provider = self.tracer_provider.clone();
        Box::pin(async move {
            let flushed = tokio::task::spawn_blocking(move || {
                flush(meter_provider.as_ref(), tracer_provider.as_ref())
            })
            .await;
            if let Err(e) = flushed {
                tracing::warn!(error = %e, "otel flush did not finish");
            }
        })
    }
}

type ProcessExporter = (String, Option<std::sync::Arc<OtelExporter>>);

static PROCESS_EXPORTER: std::sync::OnceLock<ProcessExporter> = std::sync::OnceLock::new();

/// The exporter this process sends to `endpoint`, started on first use.
///
/// `start_otel` installs *global* meter and tracer providers, so it runs at
/// most once per process however many sessions and pools ask; a later
/// `endpoint` that differs from the first is ignored (and said so once). The
/// exporter lives until [`shutdown_process_exporter`].
pub fn process_exporter(endpoint: &str) -> Option<std::sync::Arc<OtelExporter>> {
    static TOLD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let (started_on, exporter) = PROCESS_EXPORTER.get_or_init(|| {
        let exporter = match start_otel(endpoint) {
            Ok(e) => {
                tracing::info!(%endpoint, "OpenTelemetry exporter started");
                Some(std::sync::Arc::new(e))
            }
            Err(e) => {
                tracing::warn!(%endpoint, error = %e, "could not start the OpenTelemetry exporter; \
                               telemetry still records to file");
                None
            }
        };
        (endpoint.to_string(), exporter)
    });
    if started_on != endpoint && !TOLD.swap(true, std::sync::atomic::Ordering::Relaxed) {
        tracing::warn!(
            used = %started_on,
            ignored = %endpoint,
            "the OpenTelemetry exporter is process-wide and was started on another endpoint; \
             this pool's endpoint is ignored"
        );
    }
    exporter.clone()
}

/// Flush and stop the process's exporter, if one was started. For the end of
/// the process: nothing is exported after it.
pub fn shutdown_process_exporter() {
    if let Some((_, Some(exporter))) = PROCESS_EXPORTER.get() {
        exporter.shutdown();
    }
}

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
    base::interface::exec::local::install_tls_provider();
    let mut client = reqwest::Client::builder();
    if is_loopback(endpoint) {
        client = client.no_proxy();
    }
    let http_client = OtlpHttpClient(
        client
            .build()
            .map_err(|e| OtelError::InitFailed(e.to_string()))?,
    );

    let resource = opentelemetry_sdk::Resource::new(vec![KeyValue::new("service.name", "agix")]);

    let trace_exporter = match protocol {
        OtelProtocol::Grpc => opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .with_endpoint(endpoint)
            .with_timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| OtelError::InitFailed(e.to_string()))?,
        OtelProtocol::HttpProtobuf => opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_http_client(http_client.clone())
            .with_endpoint(http_signal_endpoint(endpoint, "traces"))
            .with_timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| OtelError::InitFailed(e.to_string()))?,
    };

    let tracer_provider = opentelemetry_sdk::trace::TracerProvider::builder()
        .with_batch_exporter(trace_exporter, opentelemetry_sdk::runtime::Tokio)
        .with_resource(resource.clone())
        .build();

    opentelemetry::global::set_tracer_provider(tracer_provider.clone());

    let metric_exporter = match protocol {
        OtelProtocol::Grpc => opentelemetry_otlp::MetricExporter::builder()
            .with_tonic()
            .with_endpoint(endpoint)
            .with_timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| OtelError::InitFailed(e.to_string()))?,
        OtelProtocol::HttpProtobuf => opentelemetry_otlp::MetricExporter::builder()
            .with_http()
            .with_http_client(http_client)
            .with_endpoint(http_signal_endpoint(endpoint, "metrics"))
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

    /// A collector that answers every request with 200 and keeps the path
    /// each one was sent to.
    async fn http_collector() -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let paths = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = paths.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let seen = seen.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    let head_end = loop {
                        if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            break at + 4;
                        }
                        match sock.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    };
                    let head = String::from_utf8_lossy(&buf[..head_end]).to_lowercase();
                    let length: usize = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse().ok())
                        .unwrap_or(0);
                    while buf.len() < head_end + length {
                        match sock.read(&mut chunk).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    }
                    let path = head.split_whitespace().nth(1).unwrap_or("").to_string();
                    seen.lock().unwrap().push(path);
                    let _ = sock
                        .write_all(
                            b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                        )
                        .await;
                });
            }
        });
        (endpoint, paths)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn spans_and_metrics_reach_an_http_collector() {
        use opentelemetry::metrics::MeterProvider as _;
        use opentelemetry::trace::{Span as _, Tracer as _, TracerProvider as _};

        let (endpoint, paths) = http_collector().await;
        let exporter = start_otel(&endpoint).expect("an HTTP exporter starts");
        let tracer_provider = exporter.tracer_provider.clone().expect("a tracer");
        let meter_provider = exporter.meter_provider.clone().expect("a meter");
        tracer_provider.tracer("agix-test").start("a-span").end();
        meter_provider
            .meter("agix-test")
            .u64_counter("agix.test.count")
            .build()
            .add(1, &[]);
        tokio::task::spawn_blocking(move || flush(Some(&meter_provider), Some(&tracer_provider)))
            .await
            .unwrap();

        let paths = paths.lock().unwrap().clone();
        assert!(paths.iter().any(|p| p == "/v1/traces"), "{paths:?}");
        assert!(paths.iter().any(|p| p == "/v1/metrics"), "{paths:?}");
    }

    #[test]
    fn a_signal_goes_under_the_collector_base() {
        assert_eq!(
            http_signal_endpoint("http://c:4318/", "traces"),
            "http://c:4318/v1/traces"
        );
        assert_eq!(
            http_signal_endpoint("https://c.example/otlp", "metrics"),
            "https://c.example/otlp/v1/metrics"
        );
    }

    #[test]
    fn start_otel_returns_error_on_empty_endpoint() {
        let result = start_otel("");
        assert!(result.is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_session_pipeline_ending_leaves_the_shared_exporter_running() {
        use opentelemetry::trace::{Span as _, Tracer as _, TracerProvider as _};

        let exporter = std::sync::Arc::new(start_otel("http://127.0.0.1:4317").unwrap());
        let config = telemetry::TelemetryConfig {
            enabled: true,
            mode: telemetry::TelemetryMode::Enabled,
            default_event_enabled: true,
            ..Default::default()
        };
        for _session in 0..2 {
            let recorders: Vec<std::sync::Arc<dyn telemetry::TelemetryRecorder>> =
                vec![exporter.clone()];
            let (handle, consumer) = telemetry::spawn(config.clone(), recorders).unwrap();
            drop(handle);
            consumer.await;
        }

        let tracer = exporter
            .tracer_provider
            .as_ref()
            .expect("started with a tracer")
            .tracer("agix-test");
        assert!(
            tracer.start("after-two-sessions").is_recording(),
            "a session ending shut the process-wide tracer provider down"
        );
    }
}
