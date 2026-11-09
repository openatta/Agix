//! Observers that ship with the engine.
//!
//! [`AppendObserver`] is a seam a host fills. What a host asks about a
//! running session first is how much is in it, and reading the log back to
//! answer would parse it per question, per session, on the same thread that
//! is trying to serve the next turn. The numbers here are a by-product of
//! writes that already happened.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use base::session::SessionId;

use crate::entry::LogEntry;
use crate::store::AppendObserver;

/// A running count of what has been written to each session.
///
/// Counts what *this process* saw land, which is what a live view and a quota
/// both want; it is not a substitute for reading a log that was written by
/// someone else, or before a restart. Register it once and hold the same
/// `Arc` the store holds:
///
/// ```ignore
/// let counts = Arc::new(AppendCounts::new());
/// let store = ObservedHistoryStore::new(inner, vec![counts.clone()]);
/// // …
/// counts.entries(session)
/// ```
#[derive(Default)]
pub struct AppendCounts {
    total: AtomicU64,
    per_session: Mutex<HashMap<SessionId, BTreeMap<&'static str, u64>>>,
}

impl AppendCounts {
    pub fn new() -> Self {
        Self::default()
    }

    /// Everything written to every session this has watched.
    pub fn total(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }

    /// Entries written to one session.
    pub fn entries(&self, session: SessionId) -> u64 {
        self.locked()
            .get(&session)
            .map(|kinds| kinds.values().sum())
            .unwrap_or(0)
    }

    /// Entries of one [`LogEntry::kind`] written to one session.
    pub fn of_kind(&self, session: SessionId, kind: &str) -> u64 {
        self.locked()
            .get(&session)
            .and_then(|kinds| kinds.get(kind))
            .copied()
            .unwrap_or(0)
    }

    /// The whole breakdown for one session, by kind.
    pub fn by_kind(&self, session: SessionId) -> BTreeMap<&'static str, u64> {
        self.locked().get(&session).cloned().unwrap_or_default()
    }

    /// The sessions written to so far.
    pub fn sessions(&self) -> Vec<SessionId> {
        self.locked().keys().copied().collect()
    }

    /// Drop what is remembered about one session.
    ///
    /// A long-lived host sees sessions it will never see again; without this
    /// the map is a slow leak, and a leak is what would stop anyone from
    /// registering this in the first place.
    pub fn forget(&self, session: SessionId) {
        let mut held = self.locked();
        if let Some(kinds) = held.remove(&session) {
            let gone: u64 = kinds.values().sum();
            self.total.fetch_sub(gone, Ordering::Relaxed);
        }
    }

    fn locked(&self) -> std::sync::MutexGuard<'_, HashMap<SessionId, BTreeMap<&'static str, u64>>> {
        // A panic somewhere else must not turn every subsequent append into a
        // panic of its own — an observer is not allowed to cost the log.
        self.per_session
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl AppendObserver for AppendCounts {
    fn observed(&self, session: SessionId, entry: &LogEntry) {
        *self
            .locked()
            .entry(session)
            .or_default()
            .entry(entry.kind())
            .or_insert(0) += 1;
        self.total.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use base::message::ContentBlock;
    use tempfile::TempDir;

    use super::*;
    use crate::path::HistoryRoots;
    use crate::store::{HistoryStore, JsonlHistoryStore, ObservedHistoryStore};

    fn user(text: &str) -> LogEntry {
        LogEntry::User {
            content: vec![ContentBlock::Text {
                text: text.to_string(),
                cache_control: None,
            }],
        }
    }

    fn system(text: &str) -> LogEntry {
        LogEntry::System {
            subkind: crate::entry::SystemSubkind::Notice,
            text: text.to_string(),
        }
    }

    /// One session's jsonl, written by a store whose ids and timestamps come
    /// from a fixed environment — so two runs are comparable as bytes, not
    /// merely as parsed entries.
    async fn write_log(session: SessionId, wrap: Option<Vec<Arc<dyn AppendObserver>>>) -> Vec<u8> {
        let cwd = TempDir::new().unwrap();
        let projects = TempDir::new().unwrap();
        let inner = JsonlHistoryStore::with_roots(cwd.path(), HistoryRoots::under(projects.path()))
            .await
            .unwrap()
            .with_environment(Arc::new(
                base::interface::environment::FixedEnvironment::epoch(),
            ));
        let path = inner.session_file_path(&session);
        let store: Box<dyn HistoryStore> = match wrap {
            None => Box::new(inner),
            Some(observers) => Box::new(ObservedHistoryStore::new(Arc::new(inner), observers)),
        };

        store.append(session, user("one")).await.unwrap();
        store.append(session, system("a notice")).await.unwrap();
        store.append(session, user("two")).await.unwrap();

        std::fs::read(&path).unwrap()
    }

    /// The acceptance: a store with no observers registered writes the same
    /// file, byte for byte, as one that was never wrapped — and registering
    /// one does not move a byte either.
    #[tokio::test]
    async fn watching_the_log_never_changes_a_byte_of_it() {
        let session = SessionId::new();
        let bare = write_log(session, None).await;
        let unregistered = write_log(session, Some(vec![])).await;
        let watched = write_log(
            session,
            Some(vec![
                Arc::new(AppendCounts::new()) as Arc<dyn AppendObserver>
            ]),
        )
        .await;

        assert!(!bare.is_empty(), "the fixture wrote nothing to compare");
        assert_eq!(
            bare, unregistered,
            "a wrapper with nothing in it must be inert"
        );
        assert_eq!(
            bare, watched,
            "an observed append must produce the same file"
        );
    }

    #[tokio::test]
    async fn the_counts_are_of_what_was_actually_written() {
        let counts = Arc::new(AppendCounts::new());
        let a = SessionId::new();
        let b = SessionId::new();
        write_log(a, Some(vec![counts.clone()])).await;
        write_log(b, Some(vec![counts.clone()])).await;

        assert_eq!(counts.total(), 6);
        assert_eq!(counts.entries(a), 3);
        assert_eq!(counts.of_kind(a, "user"), 2);
        assert_eq!(counts.of_kind(a, "system"), 1);
        assert_eq!(counts.of_kind(a, "assistant"), 0);
        assert_eq!(
            counts.by_kind(a).into_iter().collect::<Vec<_>>(),
            vec![("system", 1), ("user", 2)]
        );

        let mut seen = counts.sessions();
        seen.sort_by_key(|s| s.0.to_string());
        let mut expected = vec![a, b];
        expected.sort_by_key(|s| s.0.to_string());
        assert_eq!(seen, expected);

        counts.forget(a);
        assert_eq!(counts.entries(a), 0);
        assert_eq!(counts.sessions(), vec![b]);
        assert_eq!(counts.total(), 3, "forgetting a session unspends its count");
    }
}
