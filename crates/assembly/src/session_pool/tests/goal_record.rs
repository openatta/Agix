//! `goal_record_tests` 原样搬来的用例。

use super::mcp_for_project::test_pool_with_store;
use super::*;
use base::context::goal::TurnOrigin;
use history::entry::{EnvelopedEntry, LogEntry};
use history::error::HistoryError;
use history::store::{HistoryStore, InMemoryHistoryStore};

/// A store whose goal writes wait while the test holds `gate`, telling
/// `writing` each time one starts to.
struct GatedGoalWrites {
    inner: InMemoryHistoryStore,
    gate: Arc<AsyncMutex<()>>,
    writing: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl HistoryStore for GatedGoalWrites {
    async fn append(
        &self,
        session: base::session::SessionId,
        entry: LogEntry,
    ) -> Result<(), HistoryError> {
        if matches!(&entry, LogEntry::Extension { ns, .. } if ns == base::context::goal::GOAL_NS) {
            self.writing.notify_one();
            let _open = self.gate.lock().await;
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
        self.inner.list_sessions().await
    }

    async fn delete(&self, session: base::session::SessionId) -> Result<(), HistoryError> {
        self.inner.delete(session).await
    }
}

async fn pool_with_gated_goal_writes() -> (SessionPool, Arc<GatedGoalWrites>, tempfile::TempDir) {
    let store = Arc::new(GatedGoalWrites {
        inner: InMemoryHistoryStore::new(),
        gate: Arc::default(),
        writing: tokio::sync::Notify::new(),
    });
    let (pool, dir) = test_pool_with_store(Some(store.clone())).await;
    (pool, store, dir)
}

async fn session_with_a_goal(pool: &SessionPool) -> (String, Arc<base::context::SessionState>) {
    let sid = pool
        .create_session(None, ProjectSelector::Default, None)
        .await
        .expect("a session")["session_id"]
        .as_str()
        .expect("a session id")
        .to_string();
    let state = pool.live_state(&sid).await.expect("a live session");
    state
        .goal_create("ship it".into(), Some(1), TurnOrigin::Human)
        .unwrap();
    pool.record_goal_changes(&sid, &state).await;
    (sid, state)
}

fn blocks_on_its_round_limit(state: &base::context::SessionState) {
    state.goal_claim_round(&state.goal_next_round().expect("a round").turn_id);
    assert!(state.goal_next_round().is_none());
    assert_eq!(state.goal_view()["phase"], "blocked");
}

#[tokio::test]
async fn a_closed_session_s_log_has_the_goal_change_still_being_written() {
    let (pool, store, _dir) = pool_with_gated_goal_writes().await;
    let (sid, state) = session_with_a_goal(&pool).await;
    let gate = store.gate.clone().lock_owned().await;
    blocks_on_its_round_limit(&state);

    let ((), (cold, ())) = tokio::join!(pool.record_goal_changes(&sid, &state), async {
        store.writing.notified().await;
        tokio::join!(
            async {
                pool.shutdown_session(&sid).await;
                pool.session_goal(&sid).await.unwrap()
            },
            async {
                tokio::time::sleep(Duration::from_millis(50)).await;
                drop(gate);
            },
        )
    });
    assert_eq!(cold["goal"]["phase"], "blocked", "{cold}");
}

#[tokio::test]
async fn a_closed_session_s_log_has_the_goal_change_nobody_wrote_yet() {
    let (pool, _store, _dir) = pool_with_gated_goal_writes().await;
    let (sid, state) = session_with_a_goal(&pool).await;
    blocks_on_its_round_limit(&state);

    pool.shutdown_session(&sid).await;
    let cold = pool.session_goal(&sid).await.unwrap();
    assert_eq!(cold["goal"]["phase"], "blocked", "{cold}");
}

#[tokio::test]
async fn an_evicted_session_s_engine_writes_its_goal_changes_as_it_stops() {
    let (pool, _store, _dir) = pool_with_gated_goal_writes().await;
    let (sid, state) = session_with_a_goal(&pool).await;
    blocks_on_its_round_limit(&state);

    let live = pool.sessions.lock().await.remove(&sid).unwrap();
    pool.evict(&sid, &live);
    pool.wait_until_stopped(&sid).await.unwrap();
    let cold = pool.session_goal(&sid).await.unwrap();
    assert_eq!(cold["goal"]["phase"], "blocked", "{cold}");
}
