//! `PersistentMembers`: the team members a session keeps across messages
//! (the `Agent` tool's `team_name` + `name`), keyed by team and name.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU64};
use std::sync::{Arc, RwLock};

use tokio_util::sync::CancellationToken;

/// A live member. Its agent and channels live inside its loop; `cancel` is
/// enough to stop it.
pub(crate) struct PersistentMember {
    /// For logs only: routing is by the `(team, member)` key, and a name
    /// reused after a stop gets a new id.
    pub(crate) id: String,
    pub(crate) message_tx: tokio::sync::mpsc::UnboundedSender<PersistentMessage>,
    pub(crate) cancel: CancellationToken,
    /// Epoch seconds since the member last went idle, or `i64::MAX` while it
    /// runs a message: a running member is never reclaimed for idling.
    pub(crate) idle_since: Arc<AtomicI64>,
    /// Strictly increasing per idle transition (`u64::MAX` while running), so
    /// "oldest idle" never ties the way second-resolution `idle_since` can.
    pub(crate) idle_seq: Arc<AtomicU64>,
}

pub(crate) struct PersistentMessage {
    pub(crate) prompt: String,
    pub(crate) task: Arc<base::context::RunningTask>,
}

pub(crate) struct PersistentMembers {
    /// Shared with `TeamCreate`/`TeamList`/`TeamDelete`; `None` when team
    /// coordination is off for this session.
    pub(crate) registry: Option<Arc<team::registry::TeamRegistry>>,
    pub(crate) live: Arc<RwLock<HashMap<(String, String), PersistentMember>>>,
    /// Source of `PersistentMember::idle_seq`: one counter for every member
    /// of the session, so "oldest idle" has a single ordering.
    pub(crate) idle_seq_counter: Arc<AtomicU64>,
    /// Serializes find-or-create: two concurrent calls for one new key would
    /// otherwise both spawn, and the second insert would orphan the first
    /// member's loop.
    pub(crate) spawn_lock: Arc<tokio::sync::Mutex<()>>,
}

impl PersistentMembers {
    pub(crate) fn new(registry: Option<Arc<team::registry::TeamRegistry>>) -> Self {
        Self {
            registry,
            live: Default::default(),
            idle_seq_counter: Default::default(),
            spawn_lock: Default::default(),
        }
    }
}
