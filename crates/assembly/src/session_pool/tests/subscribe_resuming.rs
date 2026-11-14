//! 恢复一个冷会话读几次日志：单独恢复只读一遍日志、一遍水位，`subscribe_resuming` 不比它多。

use super::mcp_for_project::test_pool_with_store;
use super::*;
use history::entry::{EnvelopedEntry, LogEntry};
use history::error::HistoryError;
use history::store::{HistoryStore, InMemoryHistoryStore, SessionFacts};
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Default)]
struct CountsLoads {
    inner: InMemoryHistoryStore,
    loads: AtomicUsize,
}

#[async_trait::async_trait]
impl HistoryStore for CountsLoads {
    async fn append(
        &self,
        session: base::session::SessionId,
        entry: LogEntry,
    ) -> Result<(), HistoryError> {
        self.inner.append(session, entry).await
    }

    async fn load(
        &self,
        session: base::session::SessionId,
    ) -> Result<Vec<EnvelopedEntry>, HistoryError> {
        self.loads.fetch_add(1, Ordering::SeqCst);
        self.inner.load(session).await
    }

    async fn list_sessions(&self) -> Result<Vec<base::session::SessionId>, HistoryError> {
        self.inner.list_sessions().await
    }

    async fn delete(&self, session: base::session::SessionId) -> Result<(), HistoryError> {
        self.inner.delete(session).await
    }

    async fn session_facts(
        &self,
        session: base::session::SessionId,
    ) -> Result<SessionFacts, HistoryError> {
        self.inner.session_facts(session).await
    }
}

async fn cold_session(store: &CountsLoads) -> String {
    let sid = base::session::SessionId::new();
    let text = |text: &str| {
        vec![base::message::ContentBlock::Text {
            text: text.into(),
            cache_control: None,
        }]
    };
    for entry in [
        LogEntry::User {
            content: text("hello"),
        },
        LogEntry::Assistant {
            content: text("hi"),
            stop_reason: None,
            usage: None,
            model: None,
        },
    ] {
        store.append(sid, entry).await.unwrap();
    }
    sid.to_string()
}

#[tokio::test]
async fn subscribing_with_resume_reads_the_log_no_more_than_resuming() {
    let store = Arc::new(CountsLoads::default());
    let sid = cold_session(&store).await;

    let (resuming, _dir) = test_pool_with_store(Some(store.clone())).await;
    let report = resuming.resume_session(&sid, false, None).await.unwrap();
    let resume_loads = store.loads.swap(0, Ordering::SeqCst);

    let (subscribing, _dir) = test_pool_with_store(Some(store.clone())).await;
    let subscribed = subscribing
        .subscribe_resuming(&sid, Arc::new(NullWatcher(1)))
        .await
        .unwrap();
    assert!(subscribed.resumed);
    assert_eq!(subscribed.last_seq, report.message_count);
    assert_eq!(store.loads.load(Ordering::SeqCst), resume_loads);
    assert!(subscribing
        .hub_of(&sid)
        .await
        .unwrap()
        .is_watched_by(WatcherId(1)));
}

/// Once to bring the session back, once more for the watermark, which is
/// read after the subscriber is on (`watched_report`).
#[tokio::test]
async fn resuming_a_cold_session_reads_its_log_once_and_its_watermark_once() {
    let store = Arc::new(CountsLoads::default());
    let sid = cold_session(&store).await;
    let (pool, _dir) = test_pool_with_store(Some(store.clone())).await;
    store.loads.store(0, Ordering::SeqCst);

    let report = pool.resume_session(&sid, false, None).await.unwrap();
    assert_eq!(report.message_count, 2);
    assert_eq!(store.loads.load(Ordering::SeqCst), 2);
}
