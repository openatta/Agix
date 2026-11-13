//! 会话池对外的领域类型：调用方（server、嵌入方）只经由这些类型和池打交道，
//! 不接触 JSON-RPC 线格式。

mod commands;
mod daemon;
mod error;
mod events;
mod history;
mod mcp;
mod plugin;
mod scene;
mod session;
mod settings;
mod tasks;
mod turn;

pub use commands::{CommandInfo, CommandKind, CommandSource, NewSchedule};
pub use daemon::{CloseReason, DaemonEvent, EvictReason, ImportSource};
pub use error::{Busy, BusyReason, EngineFailure, PoolError};
pub use events::{
    emitted_outside_turns, AskKind, PendingAsk, SessionEvent, SessionFrame, SessionWatcher,
    Subscribed, WatcherId,
};
pub use history::{
    ArchiveFormat, EntryList, EntryView, ExportSummary, ExportedSession, HistoryItem,
    HistoryOrigin, HistoryPage, HistoryUsage, SessionKindName,
};
pub use mcp::{
    McpAuthDone, McpAuthStarted, McpAuthState, McpAuthStatus, McpIdentity, McpReconnected,
    McpRedirect, McpServerRef, McpServerState, McpServerStatus, McpServerWritten,
};
pub use plugin::{
    PluginDisclosure, PluginInfo, PluginInstalled, PluginRemoved, PluginScope, PluginToggled,
    SkillConflict, SkillOrigin, VisibleText,
};
pub use scene::{SceneCapabilities, SceneDescription, SceneReload, SceneSummary, SceneTools};
pub use session::{
    Applies, DeleteReport, ForkReport, ModelChoice, PermissionReport, PromptChoice, Renamed,
    ResumeReport, ResumeStatus, ScriptCall, ScriptCallOutcome, ScriptReport, SessionCreated,
    SessionDetail, TurnState,
};
pub use settings::{
    ConfigTier, ConfigTierName, ImportRun, ProjectStatus, ProjectTrustSet, ProviderConfigView,
    ProviderPatch, ProviderSaved, ProviderView, RoutingStatus, SettingsReloaded, TrustState,
};
pub use tasks::{AgentStarted, ContextReport, MessageDelivery, SessionTasks, TaskOutput};
pub use turn::{Initiator, TurnOutcome, TurnRequest};
