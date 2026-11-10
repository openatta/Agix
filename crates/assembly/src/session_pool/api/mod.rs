//! 会话池对外的领域类型：调用方（server、嵌入方）只经由这些类型和池打交道，
//! 不接触 JSON-RPC 线格式。

mod error;

pub use error::{Busy, EngineFailure, PoolError};
