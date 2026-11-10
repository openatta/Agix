//! Spawn telemetry pipeline — returns a handle and a background consumer future.
//!
//! This crate never talks to the network itself. The caller supplies one or
//! more [`TelemetryRecorder`] implementations (file, HTTP, OTLP, …); `spawn`
//! wires up the channel, per-event-kind filtering (`config.event_toggles`),
//! and redaction, and the consumer future fans each event out to every
//! recorder. An empty recorder list yields a noop handle — same as
//! `enabled == false`.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use crate::config::{TelemetryConfig, TelemetryMode};
use crate::events::TelemetryEvent;
use crate::handle::{EventFilter, TelemetryHandle, TelemetryRecorder};
use crate::redact::RedactionPolicy;

/// Concrete consumer future (not type-erased, so channel rx lifetime is clear).
pub struct TelemetryConsumer {
    inner: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
}

impl TelemetryConsumer {
    pub(crate) fn new(fut: impl Future<Output = ()> + Send + 'static) -> Self {
        Self {
            inner: Some(Box::pin(fut)),
        }
    }
    pub(crate) fn disabled() -> Self {
        Self { inner: None }
    }
}

impl Future for TelemetryConsumer {
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        match &mut self.get_mut().inner {
            Some(fut) => fut.as_mut().poll(cx),
            None => Poll::Ready(()),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SpawnError {
    #[error("invalid telemetry configuration: {0}")]
    InvalidConfig(String),
}

/// Create a telemetry pipeline. Returns `(handle, consumer)`:
/// - `handle` records events (cloneable, non-blocking); events whose kind is
///   disabled via `config.event_toggles`/`default_event_enabled` never enter
///   the channel.
/// - `consumer` drives the pipeline in the background (`tokio::spawn` or
///   `await`): it redacts each event once, then forwards it to every
///   recorder in `recorders`, in order.
///
/// `recorders` is how the caller decides what happens to telemetry data —
/// this crate never constructs a network client on its own.
pub fn spawn(
    config: TelemetryConfig,
    recorders: Vec<Arc<dyn TelemetryRecorder>>,
) -> Result<(TelemetryHandle, TelemetryConsumer), SpawnError> {
    if !config.enabled || matches!(config.mode, TelemetryMode::Disabled) || recorders.is_empty() {
        return Ok((TelemetryHandle::noop(), TelemetryConsumer::disabled()));
    }

    let queue_size = config.queue_size();
    if queue_size == 0 {
        return Ok((TelemetryHandle::noop(), TelemetryConsumer::disabled()));
    }

    let policy = RedactionPolicy::all();
    let filter = Arc::new(EventFilter::from_config(&config));

    let (tx, rx) = tokio::sync::mpsc::channel(queue_size);
    let handle = TelemetryHandle::new_with_filter(tx, filter);
    let consumer = TelemetryConsumer::new(fan_out(rx, recorders, policy));
    Ok((handle, consumer))
}

/// How often a recorder that keeps failing is reported.
const WARN_EVERY: std::time::Duration = std::time::Duration::from_secs(60);

/// Drain `rx`, redact each event once, and forward it to every recorder.
/// Returns when the channel is closed (all handles dropped), after giving
/// every recorder a chance to flush via `shutdown()`.
async fn fan_out(
    mut rx: tokio::sync::mpsc::Receiver<TelemetryEvent>,
    recorders: Vec<Arc<dyn TelemetryRecorder>>,
    policy: RedactionPolicy,
) {
    let mut last_warned: Option<std::time::Instant> = None;
    let mut failed_since_warning = 0u64;
    while let Some(event) = rx.recv().await {
        let event = event.redact(&policy);
        for recorder in &recorders {
            let Err(error) = recorder.record(event.clone()) else {
                continue;
            };
            failed_since_warning += 1;
            if last_warned.map_or(true, |at| at.elapsed() >= WARN_EVERY) {
                tracing::warn!(%error, failed = failed_since_warning, "a telemetry recorder is failing; events are being lost");
                last_warned = Some(std::time::Instant::now());
                failed_since_warning = 0;
            }
        }
    }
    for recorder in &recorders {
        recorder.shutdown().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::TurnStartPayload;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    fn dummy_event(kind_marker: bool) -> TelemetryEvent {
        TelemetryEvent::turn_start(
            "sess",
            1,
            None,
            TurnStartPayload {
                resumed: kind_marker,
                is_retry: false,
            },
        )
    }

    #[derive(Default)]
    struct CountingRecorder {
        count: AtomicUsize,
        last: Mutex<Option<TelemetryEvent>>,
    }

    impl TelemetryRecorder for CountingRecorder {
        fn record(&self, event: TelemetryEvent) -> Result<(), crate::handle::TelemetryHandleError> {
            self.count.fetch_add(1, Ordering::SeqCst);
            *self.last.lock().unwrap() = Some(event);
            Ok(())
        }
        fn shutdown(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
            Box::pin(std::future::ready(()))
        }
    }

    #[tokio::test]
    async fn disabled_config_yields_noop_handle() {
        let config = TelemetryConfig::disabled();
        let (handle, consumer) =
            spawn(config, vec![Arc::new(CountingRecorder::default())]).unwrap();
        assert!(handle.record(dummy_event(false)).is_ok());
        consumer.await; // resolves immediately for the disabled/noop path
    }

    #[tokio::test]
    async fn empty_recorders_yields_noop_handle() {
        let config = TelemetryConfig {
            enabled: true,
            ..Default::default()
        };
        let (handle, consumer) = spawn(config, vec![]).unwrap();
        assert!(handle.record(dummy_event(false)).is_ok());
        consumer.await;
    }

    #[tokio::test]
    async fn events_fan_out_to_every_recorder() {
        let config = TelemetryConfig {
            enabled: true,
            ..Default::default()
        };
        let a = Arc::new(CountingRecorder::default());
        let b = Arc::new(CountingRecorder::default());
        let (handle, consumer) = spawn(
            config,
            vec![
                a.clone() as Arc<dyn TelemetryRecorder>,
                b.clone() as Arc<dyn TelemetryRecorder>,
            ],
        )
        .unwrap();

        handle.record(dummy_event(false)).unwrap();
        drop(handle); // closes the channel so the consumer task can finish
        consumer.await;

        assert_eq!(a.count.load(Ordering::SeqCst), 1);
        assert_eq!(b.count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn disabled_event_kind_never_reaches_recorders() {
        let mut config = TelemetryConfig {
            enabled: true,
            ..Default::default()
        };
        config.event_toggles.insert("turn_start".to_string(), false);
        let recorder = Arc::new(CountingRecorder::default());
        let (handle, consumer) =
            spawn(config, vec![recorder.clone() as Arc<dyn TelemetryRecorder>]).unwrap();

        handle.record(dummy_event(false)).unwrap();
        drop(handle);
        consumer.await;

        assert_eq!(recorder.count.load(Ordering::SeqCst), 0);
    }
}
