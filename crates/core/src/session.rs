//! Session / Agent 上下文身份类型。
//!
//! `SessionId` 是 `Id` 的语义包装；后续 jsonl 文件名、事件 envelope 都引用它。

use crate::id::Id;
use serde::{Deserialize, Serialize};
use std::fmt;

/// 一次会话的稳定 id。BASE58(UUID v4)；与 jsonl 文件名相同。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(pub Id);

impl SessionId {
    /// Generate a new random session id.
    pub fn new() -> Self {
        Self(Id::new())
    }
    /// Parse a BASE58 session id string; returns `Err` if not 16 bytes after decode.
    pub fn parse(s: &str) -> Result<Self, crate::id::IdError> {
        Id::parse(s).map(Self)
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// 子 agent 上下文。主线程 ToolContext 的 `agent` 字段在主 agent 时是 None。
#[derive(Debug, Clone)]
pub struct AgentContext {
    pub agent_id: Id,
    pub agent_type: String,
    pub parent_session: SessionId,
    pub depth: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_roundtrip() {
        let a = SessionId::new();
        let s = a.to_string();
        let b = SessionId::parse(&s).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn session_id_serde() {
        let a = SessionId::new();
        let s = serde_json::to_string(&a).unwrap();
        let b: SessionId = serde_json::from_str(&s).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn session_id_parse_invalid_empty() {
        assert!(SessionId::parse("").is_err());
    }

    #[test]
    fn session_id_parse_invalid_characters() {
        // BASE58 alphabet excludes 0, O, I, l
        assert!(SessionId::parse("0OIl").is_err());
    }

    #[test]
    fn session_id_parse_too_short() {
        // A valid BASE58 UUID v4 is 22 chars; shorter strings should fail
        assert!(SessionId::parse("abc").is_err());
    }

    #[test]
    fn session_id_display_format() {
        let a = SessionId::new();
        let s = format!("{a}");
        assert!(!s.is_empty());
        // BASE58 of a 16-byte UUID is 21 or 22 characters: leading zero bytes
        // encode to fewer digits, which happens for roughly 1 in 256 random
        // UUIDs. Asserting exactly 22 made this test fail ~0.4% of runs.
        assert!(
            (21..=22).contains(&s.len()),
            "expected a 21- or 22-char BASE58 id, got {} chars: {s}",
            s.len()
        );
    }
}
