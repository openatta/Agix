//! A turn runs on a driver the pool spawns: the caller that started it may go
//! away, and a driver that fails takes its engine out with it instead of
//! leaving the session marked as running.

use super::mcp_for_project::test_pool;
use super::*;

#[tokio::test]
async fn a_turn_whose_caller_went_away_still_ends_and_gives_the_session_back() {
    let (pool, _dir) = test_pool().await;
    let model = Arc::new(HoldsTheFirstCall::default());
    let release = model.hold_the_first_call();
    answered_by(&pool, model.clone()).await;
    let sid = pool
        .create_session(None, ProjectSelector::Default, None)
        .await
        .expect("a session")
        .session_id;

    let holding = model.holding.notified();
    let caller = {
        let pool = pool.clone();
        let request = turn(&sid, "t1", "first", OnBusy::Reject);
        tokio::spawn(async move { pool.run_turn(request, None).await })
    };
    holding.await;
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    release.send(()).unwrap();

    let next = pool
        .run_turn(turn(&sid, "t2", "second", OnBusy::Queue), None)
        .await;
    assert!(
        matches!(next, Ok(TurnOutcome::Completed { ref turn_id, .. }) if turn_id == "t2"),
        "{next:?}"
    );
    assert_eq!(model.sent.lock().unwrap().len(), 2);
}

/// Takes frames until the first one of a turn, then falls over.
struct FallsOver;

impl SessionWatcher for FallsOver {
    fn id(&self) -> WatcherId {
        WatcherId(13)
    }

    fn deliver(&self, frame: &SessionFrame) -> bool {
        assert!(frame.turn_id.is_none(), "a watcher fell over");
        true
    }
}

#[tokio::test]
async fn a_turn_whose_driver_fails_ends_internal_and_takes_its_engine_out() {
    let (pool, _dir) = test_pool().await;
    answered_by(&pool, Arc::new(HoldsTheFirstCall::default())).await;
    let sid = pool
        .create_session(None, ProjectSelector::Default, None)
        .await
        .expect("a session")
        .session_id;
    let mut events = pool.subscribe_events();

    let failed = pool
        .run_turn(
            turn(&sid, "t1", "first", OnBusy::Reject),
            Some(Arc::new(FallsOver)),
        )
        .await;
    match failed {
        Err(PoolError::Engine(failure)) => {
            assert_eq!(
                failure.kind,
                base::event::FailureKind::Internal,
                "{failure}"
            )
        }
        other => panic!("expected an internal failure, got {other:?}"),
    }
    let evicted = loop {
        match events.recv().await.expect("the pool's events") {
            api::DaemonEvent::SessionEvicted { session_id, reason } if session_id == sid => {
                break reason
            }
            _ => {}
        }
    };
    assert_eq!(evicted, api::EvictReason::EngineFault);
    assert!(!pool.live.contains(&sid).await);

    let next = pool
        .run_turn(turn(&sid, "t2", "second", OnBusy::Reject), None)
        .await;
    assert!(
        matches!(next, Ok(TurnOutcome::Completed { .. })),
        "{next:?}"
    );
}

/// Nothing a session and its turn leave running holds the pool: once the
/// turn is over, the caller's `Arc` is the only one.
#[tokio::test]
async fn a_session_and_its_finished_turn_leave_the_pool_held_only_by_its_owner() {
    let (pool, _dir) = test_pool().await;
    answered_by(&pool, Arc::new(HoldsTheFirstCall::default())).await;
    let sid = pool
        .create_session(None, ProjectSelector::Default, None)
        .await
        .expect("a session")
        .session_id;
    pool.run_turn(turn(&sid, "t1", "first", OnBusy::Reject), None)
        .await
        .expect("a turn");
    pool.inspect_context(&sid)
        .await
        .expect("a look at the context");

    assert_eq!(Arc::strong_count(&pool), 1);
}

/// Holds every write of an entry that mentions `first` while the test holds
/// `gate`.
struct GatedFirstWrites {
    inner: history::store::InMemoryHistoryStore,
    gate: Arc<AsyncMutex<()>>,
    writing: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl history::store::HistoryStore for GatedFirstWrites {
    async fn append(
        &self,
        session: base::session::SessionId,
        entry: history::entry::LogEntry,
    ) -> Result<(), history::error::HistoryError> {
        if format!("{entry:?}").contains("first") {
            self.writing.notify_one();
            let _open = self.gate.lock().await;
        }
        self.inner.append(session, entry).await
    }

    async fn load(
        &self,
        session: base::session::SessionId,
    ) -> Result<Vec<history::entry::EnvelopedEntry>, history::error::HistoryError> {
        self.inner.load(session).await
    }

    async fn list_sessions(
        &self,
    ) -> Result<Vec<base::session::SessionId>, history::error::HistoryError> {
        self.inner.list_sessions().await
    }

    async fn delete(
        &self,
        session: base::session::SessionId,
    ) -> Result<(), history::error::HistoryError> {
        self.inner.delete(session).await
    }
}

/// A closed session's engine stuck writing its log past the exit grace: the
/// session does not come back under its id until that engine has stopped,
/// and once it is back nothing of the old engine's turn reaches it or its log.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_engine_past_its_grace_stays_out_of_the_session_that_comes_back() {
    let store = Arc::new(GatedFirstWrites {
        inner: history::store::InMemoryHistoryStore::new(),
        gate: Arc::default(),
        writing: tokio::sync::Notify::new(),
    });
    let (pool, _dir) = super::mcp_for_project::test_pool_with_store(Some(store.clone())).await;
    answered_by(&pool, Arc::new(HoldsTheFirstCall::default())).await;
    let sid = pool
        .create_session(None, ProjectSelector::Default, None)
        .await
        .expect("a session")
        .session_id;

    let gate = store.gate.clone().lock_owned().await;
    let writing = store.writing.notified();
    let first = {
        let pool = pool.clone();
        let request = turn(&sid, "t1", "first", OnBusy::Reject);
        tokio::spawn(async move { pool.run_turn(request, None).await })
    };
    writing.await;
    pool.shutdown_session(&sid).await.expect("closed");

    let watcher = AgentEvents::new(7);
    let early = pool
        .run_turn(
            turn(&sid, "t2", "second", OnBusy::Reject),
            Some(watcher.clone()),
        )
        .await;
    assert!(
        matches!(early, Err(PoolError::SessionBusy(ref busy)) if busy.reason == api::BusyReason::PreviousEngine),
        "{early:?}"
    );
    assert!(!pool.live.contains(&sid).await);

    drop(gate);
    tokio::time::timeout(Duration::from_secs(10), async {
        while pool.live.stopping_engines() > 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the old engine stopped");
    assert!(first.await.unwrap().is_err(), "the closed turn completed");
    let back = pool
        .run_turn(
            turn(&sid, "t2", "second", OnBusy::Reject),
            Some(watcher.clone()),
        )
        .await;
    assert!(
        matches!(back, Ok(TurnOutcome::Completed { .. })),
        "{back:?}"
    );

    assert!(
        !watcher
            .seen()
            .iter()
            .any(|event| format!("{event:?}").contains("first")),
        "the old engine's turn reached the session that came back"
    );
    let entries: Vec<String> = pool
        .directory
        .load_entries(&sid)
        .await
        .expect("the log")
        .iter()
        .map(|env| format!("{:?}", env.entry))
        .collect();
    let second = entries
        .iter()
        .position(|e| e.contains("second"))
        .expect("the second turn is logged");
    assert!(
        !entries[second..].iter().any(|e| e.contains("first")),
        "the old engine wrote after the session came back: {entries:#?}"
    );
}
