//! A look at or a change to a session's context holds the session from the
//! check to the answer: a turn asked for meanwhile is refused or waits.

use super::mcp_for_project::test_pool_with_store;
use super::*;
use history::entry::{EnvelopedEntry, LogEntry};
use history::error::HistoryError;
use history::store::{HistoryStore, InMemoryHistoryStore};

/// Holds every `agix.context` write while the test holds `gate`.
struct GatedContextWrites {
    inner: InMemoryHistoryStore,
    gate: Arc<AsyncMutex<()>>,
    writing: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl HistoryStore for GatedContextWrites {
    async fn append(
        &self,
        session: base::session::SessionId,
        entry: LogEntry,
    ) -> Result<(), HistoryError> {
        if matches!(&entry, LogEntry::Extension { ns, .. } if ns == history::transcript::CONTEXT_NS)
        {
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

#[tokio::test]
async fn a_turn_asked_for_while_the_context_is_being_changed_is_refused_or_waits() {
    let store = Arc::new(GatedContextWrites {
        inner: InMemoryHistoryStore::new(),
        gate: Arc::default(),
        writing: tokio::sync::Notify::new(),
    });
    let (pool, _dir) = test_pool_with_store(Some(store.clone())).await;
    answered_by(&pool, Arc::new(HoldsTheFirstCall::default())).await;
    let sid = pool
        .create_session(None, ProjectSelector::Default, None)
        .await
        .expect("a session")
        .session_id;
    pool.run_turn(turn(&sid, "t1", "first", OnBusy::Reject), None)
        .await
        .expect("a turn");

    let gate = store.gate.clone().lock_owned().await;
    let writing = store.writing.notified();
    let edit = {
        let (pool, sid) = (pool.clone(), sid.clone());
        tokio::spawn(async move {
            pool.edit_message(&sid, 0, "first, edited".into(), None)
                .await
        })
    };
    writing.await;

    match pool
        .run_turn(turn(&sid, "t2", "second", OnBusy::Reject), None)
        .await
    {
        Err(PoolError::SessionBusy(busy)) => {
            assert_eq!(busy.reason, api::BusyReason::ContextOperation);
            assert_eq!(busy.current_turn_id, None);
        }
        other => panic!("a turn started while the context was being changed: {other:?}"),
    }
    let queued = {
        let (pool, sid) = (pool.clone(), sid.clone());
        tokio::spawn(async move {
            pool.run_turn(turn(&sid, "t3", "third", OnBusy::Queue), None)
                .await
        })
    };
    drop(gate);

    assert_eq!(edit.await.unwrap().expect("the edit"), 0);
    assert!(
        matches!(queued.await.unwrap(), Ok(TurnOutcome::Completed { ref turn_id, .. }) if turn_id == "t3"),
        "the queued turn ran once the change was done"
    );
    let logged = store
        .inner
        .load(base::session::SessionId::parse(&sid).unwrap())
        .await
        .unwrap()
        .into_iter()
        .filter(|e| {
            matches!(&e.entry, LogEntry::Extension { ns, event, .. }
                if ns == history::transcript::CONTEXT_NS && event == "edit")
        })
        .count();
    assert_eq!(logged, 1, "the engine wrote the change to the log once");
}
