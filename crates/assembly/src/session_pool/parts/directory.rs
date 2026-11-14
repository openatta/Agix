//! 磁盘上的会话 `SessionDirectory`：经 `HistoryStore` 读会话日志里记下的事实
//! （`Meta`、名字与计数、是否已结束），以及这个进程服务过哪些项目——`session.list`
//! 列冷会话就照这张表。不认识在内存里的会话。
//!
//! 锁序：`served` 是 std 叶子锁。

use crate::session_pool::api::PoolError;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub(in crate::session_pool) struct SessionDirectory {
    store: Option<Arc<dyn history::store::HistoryStore>>,
    /// Every project this pool has built a session for, `None` being the
    /// no-project tier. Kept apart from the bindings, which a settings
    /// change empties.
    served: std::sync::Mutex<Vec<Option<PathBuf>>>,
}

impl SessionDirectory {
    pub(in crate::session_pool) fn new(
        store: Option<Arc<dyn history::store::HistoryStore>>,
    ) -> Self {
        Self {
            store,
            served: std::sync::Mutex::new(Vec::new()),
        }
    }

    pub(in crate::session_pool) fn store(&self) -> Option<&Arc<dyn history::store::HistoryStore>> {
        self.store.as_ref()
    }

    /// The `HistoryStore` these three methods read and write, or a ready-made
    /// RPC error explaining that this daemon has no session persistence at
    /// all (`JsonlHistoryStore::with_root` failed at startup — see
    /// `daemon/src/main.rs`; `daemon.doctor`'s
    /// `session_persistence.history_store_wired` reports the same fact).
    pub(in crate::session_pool) fn require_store(
        &self,
    ) -> Result<&Arc<dyn history::store::HistoryStore>, PoolError> {
        self.store.as_ref().ok_or_else(|| {
            PoolError::Internal(
                "session history persistence is not enabled on this daemon \
                 (see daemon.doctor's session_persistence.history_store_wired)"
                    .to_string(),
            )
        })
    }

    /// Parse + load one session's raw log, mapping both failure kinds onto
    /// the error codes `docs/daemon_rpc_protocol.md` documents.
    pub(in crate::session_pool) async fn load_entries(
        &self,
        session_id: &str,
    ) -> Result<Vec<history::entry::EnvelopedEntry>, PoolError> {
        let store = self.require_store()?;
        let sid = base::session::SessionId::parse(session_id).map_err(|e| {
            PoolError::InvalidRequest(format!("invalid session_id `{session_id}`: {e}"))
        })?;
        store.load(sid).await.map_err(|e| match e {
            history::error::HistoryError::SessionNotFound(_) => {
                PoolError::session_not_found(session_id)
            }
            other => PoolError::Internal(format!("failed to read session history: {other}")),
        })
    }

    /// What the log says this session is — scene, project, kind, lineage.
    ///
    /// The empty set when there is no store, the id does not parse, or the
    /// log has no `Meta` line: none of those is a reason to fail a listing,
    /// and "we do not know" is the honest entry for a session that cannot say.
    pub(in crate::session_pool) async fn facts_of(
        &self,
        session_id: &str,
    ) -> history::store::SessionFacts {
        let Some(store) = self.store.as_ref() else {
            return history::store::SessionFacts::default();
        };
        let Ok(sid) = base::session::SessionId::parse(session_id) else {
            return history::store::SessionFacts::default();
        };
        store.session_facts(sid).await.unwrap_or_default()
    }

    /// What the store has recorded about this session — the name and the
    /// counters. The empty record when there is no store or the id does not
    /// parse, for the same reason `facts_of` answers that way: neither is a
    /// reason to fail a listing.
    pub(in crate::session_pool) async fn stats_of(
        &self,
        session_id: &str,
    ) -> history::store::SessionStats {
        let Some(store) = self.store.as_ref() else {
            return history::store::SessionStats::default();
        };
        let Ok(sid) = base::session::SessionId::parse(session_id) else {
            return history::store::SessionStats::default();
        };
        store.session_stats(sid).await.unwrap_or_default()
    }

    /// `sid`'s log, empty when it has none. A log that is not there is
    /// empty, as is one for an id no store could hold; a log that is there
    /// but does not load is an error, never a session to start over under
    /// the same id.
    pub(in crate::session_pool) async fn read_log(
        &self,
        sid: &str,
    ) -> Result<Vec<history::entry::EnvelopedEntry>, String> {
        let (Some(store), Ok(parsed)) = (self.store.as_ref(), base::session::SessionId::parse(sid))
        else {
            return Ok(Vec::new());
        };
        match store.load(parsed).await {
            Ok(entries) => Ok(entries),
            Err(history::error::HistoryError::SessionNotFound(_)) => Ok(Vec::new()),
            Err(e) => Err(format!(
                "session `{sid}` could not be brought back: its transcript could not be loaded: {e}"
            )),
        }
    }

    /// Projected message count + raw entry count for a session id, or
    /// `(0, 0)` when there's no store, the id doesn't parse, or nothing is
    /// on disk — all three mean the same thing to `session.resume`'s caller
    /// ("no prior conversation"), so they don't need distinguishing here.
    pub(in crate::session_pool) async fn history_counts(&self, session_id: &str) -> (usize, usize) {
        match self.load_entries(session_id).await {
            Ok(entries) => self.counts_in(&entries),
            Err(_) => (0, 0),
        }
    }

    /// Projected message count + raw entry count of `entries`.
    pub(in crate::session_pool) fn counts_in(
        &self,
        entries: &[history::entry::EnvelopedEntry],
    ) -> (usize, usize) {
        match self.store.as_ref() {
            Some(store) => (
                history::transcript::project_messages_with(entries, store.projection()).len(),
                entries.len(),
            ),
            None => (0, 0),
        }
    }

    /// Every project this daemon has served a session for: its own, plus each
    /// one a session was created or resumed under in this process. `None` is
    /// the no-project tier.
    ///
    /// This is what `session.list` enumerates cold sessions over. Not every
    /// project on disk (`SessionScope::AllProjects`), which would report
    /// sessions belonging to daemons and working directories this one has
    /// nothing to do with; and not just the default project, which is what it
    /// listed back when every session's transcript landed there regardless of
    /// which project the session was actually bound to.
    ///
    /// Held apart from the binding cache, which a settings change empties.
    /// Not persisted: after a restart a project no session has touched since
    /// is not listed until one does — the alternative is a daemon that
    /// remembers projects it no longer serves. `session.get` still finds such
    /// a session by id.
    ///
    /// Compared resolved, as the store resolves a project before reading it:
    /// one folder named two ways (`/var/…` and `/private/var/…` on macOS) is
    /// one project.
    pub(in crate::session_pool) async fn served_projects(
        &self,
        default_project: &Path,
    ) -> Vec<Option<PathBuf>> {
        let noted: Vec<Option<PathBuf>> = self.lock_served().clone();
        let mut out = Vec::with_capacity(noted.len() + 1);
        for project in std::iter::once(Some(default_project.to_path_buf())).chain(noted) {
            let project = match project {
                Some(p) => Some(history::path::canonicalize_cwd(&p).await.unwrap_or(p)),
                None => None,
            };
            if !out.contains(&project) {
                out.push(project);
            }
        }
        out
    }

    fn lock_served(&self) -> std::sync::MutexGuard<'_, Vec<Option<PathBuf>>> {
        self.served
            .lock()
            .expect("served mutex is never held across a panic")
    }

    pub(in crate::session_pool) fn note_served(&self, project: Option<&Path>) {
        let project = project.map(Path::to_path_buf);
        let mut served = self.lock_served();
        if !served.contains(&project) {
            served.push(project);
        }
    }

    /// [`sidechain_terminal_state`] of `session_id`'s log.
    pub(in crate::session_pool) async fn sidechain_terminal_state(
        &self,
        session_id: &str,
    ) -> Option<(Option<String>, history::entry::SessionEndState)> {
        sidechain_terminal_state(&self.load_entries(session_id).await.ok()?)
    }
}

/// The first `LogEntry::Meta` line in `entries`, if any — every session
/// history has at most one (see `create()`), always first when present, but
/// a linear `find_map` rather than `entries.first()` since pre-v2 files can
/// have none at all. Shared by the handful of pool methods that only need
/// one or two `Meta` fields (`session_kind_parent_and_resumable`,
/// `session_kind_and_scene`, `recorded_project`, `check_scene`,
/// `sidechain_terminal_state`) so they don't each repeat the same scan
/// shape.
pub(in crate::session_pool) fn find_meta(
    entries: &[history::entry::EnvelopedEntry],
) -> Option<&history::entry::LogEntry> {
    entries
        .iter()
        .map(|env| &env.entry)
        .find(|entry| matches!(entry, history::entry::LogEntry::Meta { .. }))
}

/// A session's own recorded `(schema_version, project_root)`, read off
/// its `Meta` line — used by `resume_or_create` so resuming a session
/// rebuilds it against *its own* project, not this pool's default one.
/// `None` covers both "no `Meta` at all" (a session that predates this
/// daemon writing one) and "pre-v2 `Meta`" (no `project_root` field
/// existed yet) — both mean "nothing recorded, infer the pool default",
/// same spirit as `SceneCheck::Inferred`. Distinguishing those from a
/// v2 `Meta` that explicitly recorded `project_root: null` (a genuine
/// no-project session) is exactly why this returns the raw
/// `schema_version` instead of collapsing straight to `Option<PathBuf>`.
/// The recorded `scene` rides along because it comes from the same
/// `Meta` and reattaching needs both: a session's settings are resolved
/// against its own project *and* its own scene tier.
pub(in crate::session_pool) fn recorded_project(
    entries: &[history::entry::EnvelopedEntry],
) -> Option<(u32, Option<String>, Option<String>)> {
    let history::entry::LogEntry::Meta {
        schema_version,
        project_root,
        scene,
        ..
    } = find_meta(entries)?
    else {
        return None;
    };
    Some((*schema_version, project_root.clone(), scene.clone()))
}

/// A sidechain's parent and how it ended, when `entries` hold a
/// `LogEntry::SessionEnd` marker; `None` for a primary session and for a
/// sidechain still running or cut off externally.
pub(in crate::session_pool) fn sidechain_terminal_state(
    entries: &[history::entry::EnvelopedEntry],
) -> Option<(Option<String>, history::entry::SessionEndState)> {
    let history::entry::LogEntry::Meta {
        session_kind,
        parent_session_id,
        ..
    } = find_meta(entries)?
    else {
        return None;
    };
    if !matches!(session_kind, history::entry::SessionKind::Sidechain) {
        return None;
    }
    entries.iter().rev().find_map(|env| match &env.entry {
        history::entry::LogEntry::SessionEnd { state } => Some((parent_session_id.clone(), *state)),
        _ => None,
    })
}
