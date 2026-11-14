//! Where a session's telemetry goes and how long its parts took.

use std::sync::Arc;
use telemetry::perf::PerfCollector;
use telemetry::{TelemetryEvent, TelemetryHandle};

pub(crate) struct Telemetry {
    handle: TelemetryHandle,
    perf: Arc<PerfCollector>,
}

impl Telemetry {
    pub(crate) fn new(handle: TelemetryHandle) -> Self {
        Self {
            handle,
            perf: Arc::new(PerfCollector::new()),
        }
    }

    pub(crate) fn record(&self, event: TelemetryEvent) {
        let _ = self.handle.record(event);
    }

    pub(crate) fn handle(&self) -> &TelemetryHandle {
        &self.handle
    }

    pub(crate) fn perf(&self) -> &PerfCollector {
        &self.perf
    }

    pub(crate) fn timer(&self, category: &str, name: &str) -> impl FnOnce() -> f64 {
        self.perf.start_timer(category, name)
    }
}
