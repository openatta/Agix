//! `set_model_tests` 原样搬来的用例。

use super::mcp_for_project::test_pool;
use super::*;

/// The engine's main loop is inside `process_turn` while a turn runs and
/// is not reading its input channel. A command accepted here would sit in
/// the queue and take effect at a moment nobody announced — the caller
/// told the model had changed, watching the old one answer.
#[tokio::test]
async fn a_session_with_a_turn_in_flight_refuses_rather_than_queues() {
    let (pool, _dir) = test_pool().await;
    let session_id = pool
        .create_session(None, ProjectSelector::Default, None)
        .await
        .expect("a session")
        .session_id;

    pool.live
        .with_live_mut(&session_id, |live| {
            live.turn
                .claim(RunningTurn::new("t1".into(), Initiator::Client, None))
                .unwrap()
        })
        .await
        .expect("the session is live");

    match pool
        .set_session_model(&session_id, Some("claude-opus-4-6"), None)
        .await
    {
        Err(PoolError::SessionBusy(busy)) => {
            assert_eq!(
                busy.current_turn_id.as_deref(),
                Some("t1"),
                "the refusal names the turn holding it"
            )
        }
        Err(e) => panic!("expected a busy refusal, got {e}"),
        Ok(v) => panic!("a busy session accepted a model change: {v:?}"),
    }

    assert!(
        pool.specs
            .get(&session_id)
            .and_then(|spec| spec.model)
            .is_none(),
        "a refused change must not be recorded — a later rebuild would apply it"
    );
}
