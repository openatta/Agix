//! `on_busy: "steer"` hands the message to the running turn through the
//! session's own queue, and the answer is had on the spot.

use super::mcp_for_project::test_pool;
use super::*;

const MARKER: &str = "STEERED-7f3a";

/// The engine reads its input channel six seconds late — longer than the
/// five a steer used to wait for the engine's answer before giving up and
/// queueing the message as a turn of its own, while the engine went on to
/// take it into the running turn as well.
#[tokio::test(start_paused = true)]
async fn a_steered_message_reaches_the_conversation_once_however_slow_the_engine_reads() {
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
    let first = {
        let pool = pool.clone();
        let request = turn(&sid, "t1", "start", OnBusy::Reject);
        tokio::spawn(async move { pool.run_turn(request, None).await })
    };
    holding.await;

    let (slow_tx, mut slow_rx) = tokio::sync::mpsc::unbounded_channel();
    let engine_input = pool
        .live
        .with_live_mut(&sid, |live| {
            std::mem::replace(&mut live.engine.input_tx, slow_tx)
        })
        .await
        .unwrap();
    tokio::spawn(async move {
        while let Some(message) = slow_rx.recv().await {
            tokio::time::sleep(Duration::from_secs(6)).await;
            let _ = engine_input.send(message);
        }
    });

    let steered = {
        let pool = pool.clone();
        let request = turn(&sid, "t2", MARKER, OnBusy::Steer);
        tokio::spawn(async move { pool.run_turn(request, None).await })
    };
    tokio::time::sleep(Duration::from_secs(7)).await;
    release.send(()).unwrap();

    assert!(
        matches!(
            steered.await.unwrap(),
            Ok(TurnOutcome::Steered { ref into_turn_id, .. }) if into_turn_id.as_deref() == Some("t1")
        ),
        "the running turn took the message"
    );
    assert!(first.await.unwrap().is_ok());
    let sent = model.sent.lock().unwrap().clone();
    let last = sent.last().expect("the turn called the model");
    assert_eq!(last.matches(MARKER).count(), 1, "{sent:#?}");
    assert_eq!(
        sent.len(),
        2,
        "one call before the message, one after: {sent:#?}"
    );
}

/// A close takes the session out of the table and tells its engine to stop;
/// the engine sees that only later. A steer arriving once the engine has been
/// told is refused: the turn it would join is being cut off, and the message
/// with it.
#[tokio::test(start_paused = true)]
async fn a_session_being_closed_takes_no_steered_message() {
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
    let first = {
        let pool = pool.clone();
        let request = turn(&sid, "t1", "start", OnBusy::Reject);
        tokio::spawn(async move { pool.run_turn(request, None).await })
    };
    holding.await;
    pool.live
        .with_live(&sid, |live| live.engine.cancel.cancel())
        .await
        .unwrap();

    let steered = {
        let pool = pool.clone();
        let request = turn(&sid, "t2", MARKER, OnBusy::Steer);
        tokio::spawn(async move { pool.run_turn(request, None).await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    pool.shutdown_session(&sid).await.unwrap();
    let _ = release.send(());

    let outcome = tokio::time::timeout(Duration::from_secs(30), steered)
        .await
        .expect("the steer is answered")
        .unwrap();
    assert!(
        !matches!(outcome, Ok(TurnOutcome::Steered { .. })),
        "{outcome:?}"
    );
    let _ = tokio::time::timeout(Duration::from_secs(30), first).await;
}
