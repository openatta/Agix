//! 会话池对外的领域类型：调用方（server、嵌入方）只经由这些类型和池打交道，
//! 不接触 JSON-RPC 线格式。

mod commands;
mod daemon;
mod error;
mod events;
mod turn;

pub use commands::{CommandInfo, CommandKind, CommandSource, NewSchedule};
pub use daemon::{CloseReason, DaemonEvent, EvictReason, ImportSource};
pub use error::{Busy, BusyReason, EngineFailure, PoolError};
pub use events::{
    emitted_outside_turns, AskKind, PendingAsk, SessionEvent, SessionFrame, SessionWatcher,
    Subscribed, WatcherId,
};
pub use turn::{Initiator, TurnOutcome, TurnRequest};
