//! 各命名空间方法的 params 与线格式包装，一个命名空间一个文件。领域结果类型在
//! `session_pool::api`，这里只放线上多出来的那一层（`ping` 的 `pong`、
//! `ignored_options` 之类）。

pub mod config;
pub mod daemon;
pub mod goal;
pub mod history;
pub mod import;
pub mod mcp;
pub mod plugin;
pub mod project;
pub mod scene;
pub mod schedule;
pub mod session;

pub use config::ConfigTarget;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Params that name one session and nothing else.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct SessionRef {
    pub session_id: String,
}

/// `result` with what the call's `options` left out, as `ignored_options`;
/// absent when nothing was.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(rename = "{T}WithIgnored")]
pub struct WithIgnored<T> {
    #[serde(flatten)]
    pub result: T,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ignored_options: Vec<super::IgnoredOption>,
}

/// A field where an absent key, `null` and a value mean three things.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Patch<T> {
    #[default]
    Keep,
    Clear,
    Set(T),
}

impl<T> Patch<T> {
    pub fn is_keep(&self) -> bool {
        matches!(self, Self::Keep)
    }

    /// For `#[serde(default, deserialize_with = "Patch::present")]`: serde
    /// calls it only when the key is there, so `Keep` is the field's default.
    pub fn present<'de, D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
        T: serde::Deserialize<'de>,
    {
        Ok(match Option::<T>::deserialize(deserializer)? {
            None => Self::Clear,
            Some(value) => Self::Set(value),
        })
    }
}
