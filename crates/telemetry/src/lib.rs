//! Telemetry — structured event recording with pluggable, callback-based export.
//!
//! This crate never talks to the network itself: it only defines the event
//! schema, applies per-event-kind filtering and redaction, and hands
//! surviving events to whatever [`TelemetryRecorder`] callbacks the caller
//! passes to [`spawn`]. Where events actually end up (a file, an HTTP
//! endpoint, an OTLP collector, …) is entirely the caller's decision — see
//! `daemon::telemetry_otel` for the OTLP recorder this workspace ships.
//!
//! Default: noop (zero overhead) when disabled or given no recorders.
//! Events are dropped silently if the channel is full — telemetry never
//! blocks the agent.

pub mod config;
pub mod events;
pub mod file_recorder;
pub mod handle;
pub mod model;
pub mod perf;
pub mod recorder;
pub mod redact;
pub mod spawn;

pub use config::{TelemetryConfig, TelemetryMode};
pub use events::{
    AgentCompletedPayload, AgentSpawnedPayload, ApiErrorPayload, ApiRequestPayload,
    BudgetEnforcedAction, BudgetEnforcedPayload, CompactActionPayload, EventPayload,
    McpConnectionErrorPayload, McpServerConnectedPayload, PermissionDecisionOutcome,
    PermissionDecisionPayload, ResumeActionPayload, ResumeOutcome, SessionEndPayload,
    SessionStartPayload, SlashCommandUsedPayload, StartupTimingPayload, TeamStageCompletePayload,
    TelemetryEvent, ToolCancelledPayload, ToolExecutionPayload, ToolOutcome, ToolStartPayload,
    TurnCompletePayload, TurnStartPayload, UserPromptSubmitPayload,
};
pub use file_recorder::FileRecorder;
pub use handle::{
    EventFilter, NoopHandle, TelemetryHandle, TelemetryHandleError, TelemetryRecorder,
};
pub use redact::RedactionPolicy;
pub use spawn::{spawn, SpawnError, TelemetryConsumer};
