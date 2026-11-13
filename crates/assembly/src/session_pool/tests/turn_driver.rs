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
