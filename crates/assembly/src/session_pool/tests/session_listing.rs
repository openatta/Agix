//! `session_listing_tests` 原样搬来的用例。

use super::mcp_for_project::test_pool_with_store;
use super::*;
use history::entry::{EnvelopedEntry, LogEntry};
use history::error::HistoryError;
use history::query::SessionQuery;
use history::store::{HistoryStore, InMemoryHistoryStore, SessionSummary};
use std::sync::atomic::{AtomicUsize, Ordering};

/// A store that answers "which sessions are there" from an index it keeps
/// as entries arrive, and counts every fall-through to the enumeration
/// the default would use.
struct IndexedStore {
    inner: InMemoryHistoryStore,
    index: std::sync::Mutex<Vec<base::session::SessionId>>,
    enumerations: AtomicUsize,
}

impl IndexedStore {
    fn new() -> Self {
        Self {
            inner: InMemoryHistoryStore::new(),
            index: std::sync::Mutex::new(Vec::new()),
            enumerations: AtomicUsize::new(0),
        }
    }

    fn indexed_order(&self) -> Vec<base::session::SessionId> {
        self.index.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl HistoryStore for IndexedStore {
    async fn append(
        &self,
        session: base::session::SessionId,
        entry: LogEntry,
    ) -> Result<(), HistoryError> {
        {
            let mut index = self.index.lock().unwrap();
            index.retain(|s| *s != session);
            index.insert(0, session);
        }
        self.inner.append(session, entry).await
    }

    async fn load(
        &self,
        session: base::session::SessionId,
    ) -> Result<Vec<EnvelopedEntry>, HistoryError> {
        self.inner.load(session).await
    }

    async fn list_sessions(&self) -> Result<Vec<base::session::SessionId>, HistoryError> {
        self.enumerations.fetch_add(1, Ordering::SeqCst);
        self.inner.list_sessions().await
    }

    async fn delete(&self, session: base::session::SessionId) -> Result<(), HistoryError> {
        self.inner.delete(session).await
    }

    async fn find_sessions(
        &self,
        query: &SessionQuery,
    ) -> Result<Vec<SessionSummary>, HistoryError> {
        Ok(self
            .indexed_order()
            .into_iter()
            .take(query.limit)
            .map(|session_id| SessionSummary {
                session_id,
                last_modified: "1970-01-01T00:00:00Z".into(),
                entry_count: 0,
                message_count: 0,
                preview: String::new(),
                canonical_cwd: None,
                title: None,
                total_input_tokens: None,
                total_output_tokens: None,
                compact_count: 0,
            })
            .collect())
    }
}

async fn seed(store: &IndexedStore, count: usize) -> Vec<base::session::SessionId> {
    let mut ids = Vec::new();
    for i in 0..count {
        let sid = base::session::SessionId::new();
        store
            .append(
                sid,
                LogEntry::User {
                    content: vec![base::message::ContentBlock::Text {
                        text: format!("session {i}"),
                        cache_control: None,
                    }],
                },
            )
            .await
            .unwrap();
        ids.push(sid);
    }
    ids
}

/// A session on disk under a project no session in this process has
/// touched: `session.resume` finds it by id, and so must `session.get`.
#[tokio::test]
async fn session_get_finds_a_cold_session_of_a_project_not_served_yet() {
    let store: Arc<dyn HistoryStore> = Arc::new(InMemoryHistoryStore::new());
    let (pool, _dir) = test_pool_with_store(Some(store.clone())).await;
    let sid = base::session::SessionId::new();
    store
        .append(
            sid,
            LogEntry::Meta {
                cwd: "/elsewhere/proj".into(),
                started_at: time::OffsetDateTime::now_utc(),
                model: "m".into(),
                permission_mode: "default".into(),
                engine_version: "0".into(),
                agix_version: "0".into(),
                parent_session_id: None,
                scene: Some("code".into()),
                project_root: Some("/elsewhere/proj".into()),
                session_kind: history::entry::SessionKind::Primary,
                schema_version: history::entry::CURRENT_META_SCHEMA_VERSION,
            },
        )
        .await
        .unwrap();

    let detail = pool.session_detail(&sid.to_string()).await.unwrap();
    assert_eq!(detail.info.project_root.as_deref(), Some("/elsewhere/proj"));
    assert!(matches!(
        detail.info.status,
        crate::session_pool::SessionStatus::Inactive
    ));

    let missing = base::session::SessionId::new().to_string();
    assert!(matches!(
        pool.session_detail(&missing).await.unwrap_err(),
        PoolError::SessionNotFound { .. }
    ));
}

/// The projects `session.list` covers are the ones sessions were built
/// under, and a settings change does not make the pool forget them.
#[tokio::test]
async fn served_projects_outlive_a_settings_change() {
    let (pool, dir) = test_pool_with_store(None).await;
    let other = dir.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    pool.directory.note_served(Some(&other));
    pool.invalidate_bindings(None).await;
    let other = std::fs::canonicalize(&other).unwrap();
    assert!(pool
        .directory
        .served_projects(pool.cwd())
        .await
        .contains(&Some(other)));
}

/// A project served under two spellings of its path — a symlink, or
/// macOS's `/var` against `/private/var` — is one project, and each of
/// its cold sessions is listed once.
#[tokio::test]
async fn a_project_served_under_two_spellings_lists_each_session_once() {
    let state = tempfile::tempdir().unwrap();
    let store: Arc<dyn HistoryStore> = Arc::new(
        history::store::JsonlHistoryStore::with_roots(
            state.path(),
            history::path::HistoryRoots::under(state.path()),
        )
        .await
        .unwrap(),
    );
    let (pool, dir) = test_pool_with_store(Some(store.clone())).await;
    let work = dir.path().join("work");
    let alias = dir.path().join("alias");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&work, &alias).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(&work, &alias).unwrap();

    let sid = base::session::SessionId::new();
    store
        .append(
            sid,
            LogEntry::Meta {
                cwd: alias.display().to_string(),
                started_at: time::OffsetDateTime::now_utc(),
                model: "m".into(),
                permission_mode: "default".into(),
                engine_version: "0".into(),
                agix_version: "0".into(),
                parent_session_id: None,
                scene: Some("code".into()),
                project_root: Some(alias.display().to_string()),
                session_kind: history::entry::SessionKind::Primary,
                schema_version: history::entry::CURRENT_META_SCHEMA_VERSION,
            },
        )
        .await
        .unwrap();
    pool.directory.note_served(Some(&alias));

    let listed: Vec<String> = pool
        .list_all(false, None)
        .await
        .into_iter()
        .map(|info| info.session_id)
        .collect();
    assert_eq!(listed, vec![sid.to_string()]);
}

#[tokio::test]
async fn a_store_with_an_index_answers_session_list_without_being_enumerated() {
    let store = Arc::new(IndexedStore::new());
    seed(&store, 3).await;
    let expected: Vec<String> = store
        .indexed_order()
        .iter()
        .map(|s| s.to_string())
        .collect();

    let (pool, _dir) = test_pool_with_store(Some(store.clone())).await;
    let listed: Vec<String> = pool
        .list_all(false, None)
        .await
        .into_iter()
        .map(|info| info.session_id)
        .collect();

    assert_eq!(
        listed, expected,
        "the index's answer, in the index's order, is what the RPC reports"
    );
    assert_eq!(
        store.enumerations.load(Ordering::SeqCst),
        0,
        "a backend that answered `find_sessions` must not also be walked session by session"
    );
}

/// The fields a `SessionSummary` could fill in stay empty for a session
/// that is only on disk — clients have always read them that way. The
/// identity fields are a different question and answered separately (see
/// `SessionFacts`); this seeded session has no `Meta` line at all, so
/// they are `null` rather than invented.
#[tokio::test]
async fn an_inactive_session_reports_the_same_empty_summary_it_always_has() {
    let store = Arc::new(IndexedStore::new());
    let ids = seed(&store, 1).await;

    let (pool, _dir) = test_pool_with_store(Some(store.clone())).await;
    let listed = pool.list_all(false, None).await;
    assert_eq!(listed.len(), 1);

    assert_eq!(
        serde_json::to_value(&listed[0]).unwrap(),
        serde_json::json!({
            "session_id": ids[0].to_string(),
            "name": null,
            "preview": null,
            "message_count": 0,
            "created_at": "",
            "last_active": "",
            "status": "inactive",
            "session_kind": "primary",
            "scene": null,
            "project_root": null,
            "compact_count": 0,
            "resumable": true,
        })
    );
}
