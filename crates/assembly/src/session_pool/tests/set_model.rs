//! `set_model_tests` 原样搬来的用例。

use super::mcp_for_project::{test_pool, test_pool_with_store};
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

/// Answers every call; keeps the model each turn-loop call asked for.
#[derive(Default)]
struct RecordsTheModel(std::sync::Mutex<Vec<String>>);

#[async_trait::async_trait]
impl base::interface::model::Model for RecordsTheModel {
    fn api_type(&self) -> base::provider::ApiType {
        base::provider::ApiType::Anthropic
    }

    async fn stream(
        &self,
        _: Vec<base::prompt::PromptBlock>,
        _: Vec<base::interface::model::ToolDef>,
        _: Vec<base::interface::model::ModelMessage>,
        params: base::interface::model::StreamParams,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError> {
        use base::interface::model::ModelEvent;
        if params
            .origin
            .as_ref()
            .is_some_and(|o| o.purpose().is_none())
        {
            self.0.lock().unwrap().push(params.model.clone());
        }
        let events = vec![
            Ok(ModelEvent::TextDelta { text: "ok".into() }),
            Ok(ModelEvent::EndTurn {
                stop_reason: "end_turn".into(),
                usage: Default::default(),
            }),
        ];
        Ok(Box::new(futures::stream::iter(events)))
    }
}

/// Until the `setPrompt` rebuild spawned last has `sid`, or waits for it
/// behind a `setModel` that has it.
async fn until_the_rebuild_is_on_its_way(pool: &SessionPool, sid: &str) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let rebuilding = pool
                .live
                .with_live(sid, |live| !live.turn.is_idle())
                .await
                .unwrap_or(false);
            if rebuilding || pool.live.session_lock_holders(sid).await >= 3 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("the rebuild reached the session");
}

/// Until a caller holds `sid`'s session lock.
async fn until_the_session_lock_is_held(pool: &SessionPool, sid: &str) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !pool.live.session_lock_is_held(sid).await {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("the session lock was taken");
}

/// Choose a model for `sid` while a `setPrompt` rebuild is on its way:
/// `setModel` passes its idle check, then waits on the bindings until the
/// rebuild has reached the session.
async fn choose_a_model_while_a_rebuild_takes_the_session(pool: &Arc<SessionPool>, sid: &str) {
    let bindings = pool.bindings.hold().await;
    let chosen = {
        let (pool, sid) = (pool.clone(), sid.to_string());
        tokio::spawn(async move {
            pool.set_session_model(&sid, Some("claude-opus-4-6"), None)
                .await
                .map(|_| ())
        })
    };
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
    let rebuilt = {
        let (pool, sid) = (pool.clone(), sid.to_string());
        tokio::spawn(async move {
            pool.set_session_prompt(&sid, |p| p.append = Some("Answer briefly.".into()))
                .await
                .map(|_| ())
        })
    };
    until_the_rebuild_is_on_its_way(pool, sid).await;
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
    drop(bindings);
    assert!(chosen.await.unwrap().is_ok(), "the choice was taken");
    assert!(rebuilt.await.unwrap().is_ok(), "the session was rebuilt");
}

/// `setModel` checks the session is idle, then changes the model. A rebuild
/// that takes the session in between has read the spec already and builds
/// the next engine on the model before; the choice still reaches that engine.
#[tokio::test]
async fn a_model_chosen_while_a_rebuild_is_on_its_way_reaches_the_rebuilt_engine() {
    let (pool, _dir) = test_pool().await;
    let model = Arc::new(RecordsTheModel::default());
    answered_by(&pool, model.clone()).await;
    let sid = pool
        .create_session(None, ProjectSelector::Default, None)
        .await
        .expect("a session")
        .session_id;

    choose_a_model_while_a_rebuild_takes_the_session(&pool, &sid).await;

    pool.run_turn(turn(&sid, "t1", "hello", OnBusy::Reject), None)
        .await
        .expect("a turn");
    assert_eq!(
        model.0.lock().unwrap().as_slice(),
        ["claude-opus-4-6"],
        "the next turn asks for the model chosen"
    );
}

/// The rebuild records the spec in the log once it has taken it. What it
/// records is the spec as it stands then — a model chosen while it ran
/// included — or a session resumed from its log goes back to the model before.
#[tokio::test]
async fn a_model_chosen_while_a_rebuild_is_on_its_way_is_the_one_the_log_records() {
    let store: Arc<dyn history::store::HistoryStore> =
        Arc::new(history::store::InMemoryHistoryStore::new());
    let (pool, _dir) = test_pool_with_store(Some(store)).await;
    answered_by(&pool, Arc::new(RecordsTheModel::default())).await;
    let sid = pool
        .create_session(None, ProjectSelector::Default, None)
        .await
        .expect("a session")
        .session_id;

    choose_a_model_while_a_rebuild_takes_the_session(&pool, &sid).await;

    let recorded = pool.specs.recorded(&sid).await;
    assert_eq!(
        recorded.model.as_deref(),
        Some("claude-opus-4-6"),
        "the log records the model chosen"
    );
    assert_eq!(
        recorded.prompt.and_then(|p| p.append).as_deref(),
        Some("Answer briefly."),
        "and the prompt the rebuild was for"
    );
}

/// A rebuild that fails puts back only its own change to the spec: a model
/// chosen while it was on its way stays chosen, and the log never records the
/// prompt the failed rebuild was for.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_rebuild_keeps_a_model_chosen_while_it_was_on_its_way() {
    let store: Arc<dyn history::store::HistoryStore> =
        Arc::new(history::store::InMemoryHistoryStore::new());
    let (pool, _dir) = test_pool_with_store(Some(store)).await;
    answered_by(&pool, Arc::new(RecordsTheModel::default())).await;
    let sid = pool
        .create_session(None, ProjectSelector::Default, None)
        .await
        .expect("a session")
        .session_id;

    let bindings = pool.bindings.hold().await;
    let chosen = {
        let (pool, sid) = (pool.clone(), sid.clone());
        tokio::spawn(async move {
            pool.set_session_model(&sid, Some("claude-opus-4-6"), None)
                .await
                .map(|_| ())
        })
    };
    until_the_session_lock_is_held(&pool, &sid).await;
    let rebuilt = {
        let (pool, sid) = (pool.clone(), sid.clone());
        tokio::spawn(async move {
            pool.set_session_prompt(&sid, |p| p.append = Some("Answer briefly.".into()))
                .await
                .map(|_| ())
        })
    };
    until_the_rebuild_is_on_its_way(&pool, &sid).await;
    let shutdown = {
        let pool = pool.clone();
        tokio::spawn(async move { pool.shutdown_all().await })
    };
    while pool.live.refuse_while_shutting_down().is_ok() {
        tokio::task::yield_now().await;
    }
    drop(bindings);

    assert!(chosen.await.unwrap().is_ok(), "the choice was taken");
    assert!(
        rebuilt.await.unwrap().is_err(),
        "rebuilt during the shutdown"
    );
    shutdown.await.unwrap();
    let recorded = pool.specs.recorded(&sid).await;
    assert_eq!(
        recorded.model.as_deref(),
        Some("claude-opus-4-6"),
        "the log records the model chosen"
    );
    assert_eq!(
        recorded.prompt.and_then(|p| p.append),
        None,
        "and not the prompt of the rebuild that failed"
    );
}

/// Until `sid`'s session lock has `n` callers holding or waiting on it.
async fn until_the_session_lock_has(pool: &SessionPool, sid: &str, n: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        // The table's own entry is one of the references counted.
        while pool.live.session_lock_holders(sid).await < n + 1 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("the callers reached the session lock");
}

/// Queue a `session.delete` of `sid` and then `change` behind a holder of its
/// session lock, let the holder go, and return what `change` came to.
async fn change_queued_behind_a_delete<T: Send + 'static>(
    pool: &Arc<SessionPool>,
    sid: &str,
    change: impl FnOnce(Arc<SessionPool>, String) -> tokio::task::JoinHandle<Result<T, PoolError>>,
) -> Result<T, PoolError> {
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    let holder = {
        let (pool, sid) = (pool.clone(), sid.to_string());
        tokio::spawn(async move {
            pool.live
                .with_session_lock(&sid, async {
                    let _ = released.await;
                })
                .await
        })
    };
    until_the_session_lock_has(pool, sid, 1).await;
    let deleted = {
        let (pool, sid) = (pool.clone(), sid.to_string());
        tokio::spawn(async move { pool.delete_session(&sid, false).await })
    };
    until_the_session_lock_has(pool, sid, 2).await;
    let changed = change(pool.clone(), sid.to_string());
    until_the_session_lock_has(pool, sid, 3).await;
    release.send(()).unwrap();
    holder.await.unwrap();
    deleted.await.unwrap().expect("the session was deleted");
    changed.await.unwrap()
}

fn assert_the_session_stays_deleted(
    pool: &SessionPool,
    sid: &str,
    entries: Result<Vec<history::entry::EnvelopedEntry>, PoolError>,
) {
    assert!(
        matches!(entries, Err(PoolError::SessionNotFound { .. })),
        "a deleted session's log is not written back: {entries:?}"
    );
    assert!(
        pool.specs.get(sid).is_none(),
        "nor is its spec kept in memory"
    );
}

/// `setModel` that waited on the session lock behind `session.delete` finds
/// the session gone: it is refused, and writes no spec that would bring back
/// a log for the deleted id.
#[tokio::test]
async fn a_model_chosen_behind_a_delete_finds_the_session_gone() {
    let store: Arc<dyn history::store::HistoryStore> =
        Arc::new(history::store::InMemoryHistoryStore::new());
    let (pool, _dir) = test_pool_with_store(Some(store)).await;
    let sid = pool
        .create_session(None, ProjectSelector::Default, None)
        .await
        .expect("a session")
        .session_id;

    let chosen = change_queued_behind_a_delete(&pool, &sid, |pool, sid| {
        tokio::spawn(async move {
            pool.set_session_model(&sid, Some("claude-opus-4-6"), None)
                .await
        })
    })
    .await;

    assert!(
        matches!(chosen, Err(PoolError::SessionNotFound { .. })),
        "expected the session to be gone, got {chosen:?}"
    );
    let entries = pool.directory.load_entries(&sid).await;
    assert_the_session_stays_deleted(&pool, &sid, entries);
}

/// The same for `setPrompt` on a session that is not in memory.
#[tokio::test]
async fn a_prompt_chosen_behind_a_delete_finds_the_session_gone() {
    let store: Arc<dyn history::store::HistoryStore> =
        Arc::new(history::store::InMemoryHistoryStore::new());
    let (pool, _dir) = test_pool_with_store(Some(store)).await;
    let sid = pool
        .create_session(None, ProjectSelector::Default, None)
        .await
        .expect("a session")
        .session_id;
    pool.shutdown_session(&sid).await.expect("closed");

    let chosen = change_queued_behind_a_delete(&pool, &sid, |pool, sid| {
        tokio::spawn(async move {
            pool.set_session_prompt(&sid, |p| p.append = Some("Answer briefly.".into()))
                .await
        })
    })
    .await;

    assert!(
        matches!(chosen, Err(PoolError::SessionNotFound { .. })),
        "expected the session to be gone, got {chosen:?}"
    );
    let entries = pool.directory.load_entries(&sid).await;
    assert_the_session_stays_deleted(&pool, &sid, entries);
}
