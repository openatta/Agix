//! `rebuild_tests` 原样搬来的用例。

use super::mcp_for_project::test_pool_with_store;
use super::*;
use base::context::goal::TurnOrigin;
use std::sync::atomic::{AtomicUsize, Ordering};

async fn pool_with_history() -> (SessionPool, tempfile::TempDir, tempfile::TempDir) {
    let store_dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn history::store::HistoryStore> = Arc::new(
        history::store::JsonlHistoryStore::with_roots(
            store_dir.path(),
            history::path::HistoryRoots::under(store_dir.path()),
        )
        .await
        .unwrap(),
    );
    let (pool, dir) = test_pool_with_store(Some(store)).await;
    (pool, dir, store_dir)
}

async fn new_session(pool: &SessionPool) -> String {
    pool.create_session(None, ProjectSelector::Default, None)
        .await
        .expect("a session")["session_id"]
        .as_str()
        .expect("a session id")
        .to_string()
}

async fn slot_and_state(
    pool: &SessionPool,
    sid: &str,
) -> (
    Arc<AsyncMutex<Option<EventReceiver>>>,
    Arc<base::context::SessionState>,
) {
    let sessions = pool.sessions.lock().await;
    let live = sessions.get(sid).expect("the session is live");
    (live.event_rx.clone(), live.state.clone().expect("a state"))
}

fn append(prompt: &mut runtime::agent::SessionPrompt) {
    prompt.append = Some("Answer briefly.".into());
}

#[tokio::test]
async fn a_prompt_change_is_refused_once_a_turn_holds_the_slot_even_before_it_is_named() {
    let (pool, _dir, _store) = pool_with_history().await;
    let sid = new_session(&pool).await;
    let (slot, _) = slot_and_state(&pool, &sid).await;
    let held = slot.lock().await.take().expect("an idle session");

    match pool.set_session_prompt(&sid, append).await {
        Err(SetModelError::Busy {
            current_turn_id, ..
        }) => assert_eq!(current_turn_id, None),
        Err(SetModelError::Rpc((code, message))) => panic!("{code}: {message}"),
        Ok(v) => panic!("a rebuild cut off a starting turn: {v}"),
    }
    let (still, _) = slot_and_state(&pool, &sid).await;
    assert!(
        Arc::ptr_eq(&slot, &still),
        "the engine was replaced under the turn"
    );

    *slot.lock().await = Some(held);
    pool.set_session_prompt(&sid, append)
        .await
        .unwrap_or_else(|_| panic!("an idle session takes the change"));
    let (rebuilt, _) = slot_and_state(&pool, &sid).await;
    assert!(!Arc::ptr_eq(&slot, &rebuilt), "an idle session is rebuilt");
}

#[tokio::test]
async fn a_prompt_set_on_a_session_out_of_memory_leaves_no_spec_behind() {
    let (pool, _dir, _store) = pool_with_history().await;
    let sid = new_session(&pool).await;
    let live = pool.sessions.lock().await.remove(&sid).unwrap();
    pool.evict(&sid, &live);
    assert!(!pool.lock_specs().contains_key(&sid));

    let answer = pool
        .set_session_prompt(&sid, append)
        .await
        .unwrap_or_else(|_| panic!("a session out of memory takes the change"));
    assert_eq!(answer["append"], "Answer briefly.");
    assert!(!pool.lock_specs().contains_key(&sid));
    assert_eq!(
        pool.recorded_spec(&sid).await.prompt.and_then(|p| p.append),
        Some("Answer briefly.".to_string()),
        "the log gives the change back"
    );
}

#[tokio::test]
async fn a_rebuild_keeps_the_goal_armed_and_the_round_it_was_about_to_start() {
    let (pool, _dir, _store) = pool_with_history().await;
    let sid = new_session(&pool).await;
    let (_, before) = slot_and_state(&pool, &sid).await;
    before
        .goal_create("keep going".into(), Some(3), TurnOrigin::Human)
        .unwrap();
    pool.record_goal_changes(&sid, &before).await;
    let round = before.goal_next_round().expect("a round");

    pool.set_session_prompt(&sid, append)
        .await
        .unwrap_or_else(|_| panic!("an idle session takes the change"));
    let (_, after) = slot_and_state(&pool, &sid).await;
    assert!(!Arc::ptr_eq(&before, &after), "the session was rebuilt");
    assert_eq!(after.goal_view()["armed"], true, "{}", after.goal_view());
    assert!(after.goal_claim_round(&round.turn_id));
}

#[tokio::test]
async fn a_rebuild_moves_the_sessions_watchers_and_open_questions_along() {
    let (pool, _dir, _store) = pool_with_history().await;
    let sid = new_session(&pool).await;
    let watcher = Arc::new(crate::rpc::Client::new(
        crate::rpc::ConnectionId(u64::MAX - 7),
        Arc::new(Discard),
    ));
    pool.subscribe_session(&sid, watcher.clone()).await.unwrap();
    pool.remember_prompt(
        &sid,
        &("p1".to_string(), Some("task-1".to_string())),
        &serde_json::json!({"kind": "prompt"}),
    )
    .await;

    pool.set_session_prompt(&sid, append)
        .await
        .unwrap_or_else(|_| panic!("an idle session takes the change"));
    let sessions = pool.sessions.lock().await;
    let live = sessions.get(&sid).unwrap();
    assert!(live.subscribers.contains_key(&watcher.id()));
    assert!(live.pending_prompts.contains_key("p1"));
}

#[tokio::test]
async fn eviction_keeps_only_the_specs_a_log_cannot_give_back() {
    let (pool, _dir, _store) = pool_with_history().await;
    let logged = new_session(&pool).await;
    let recording = new_session(&pool).await;
    pool.lock_specs().get_mut(&recording).unwrap().recorder =
        Some(telemetry::recorder::Recorder::new());
    for sid in [&logged, &recording] {
        let live = pool.sessions.lock().await.remove(sid).unwrap();
        pool.evict(sid, &live);
    }
    assert!(!pool.lock_specs().contains_key(&logged));
    assert!(pool.lock_specs().contains_key(&recording));

    let unlogged: Vec<String> = (0..pool.cap).map(|i| format!("no log {i}")).collect();
    for sid in &unlogged {
        pool.lock_specs()
            .insert(sid.clone(), SessionSpec::default());
        pool.release_spec(sid);
    }
    assert!(
        !pool.lock_specs().contains_key(&recording),
        "the oldest kept spec makes room"
    );
    assert!(unlogged
        .iter()
        .all(|sid| pool.lock_specs().contains_key(sid)));
    assert_eq!(pool.lock_kept_specs().len(), pool.cap);
}

#[tokio::test]
async fn two_callers_reviving_one_cold_session_build_one_engine() {
    let (pool, _dir, _store) = pool_with_history().await;
    let builds = Arc::new(AtomicUsize::new(0));
    let counted = builds.clone();
    let pool = pool.with_session_customizer(Arc::new(move |builder, _| {
        counted.fetch_add(1, Ordering::SeqCst);
        builder
    }));
    let sid = new_session(&pool).await;
    pool.shutdown_session(&sid).await;
    assert!(!pool.sessions.lock().await.contains_key(&sid));

    let (a, b) = tokio::join!(pool.ensure_live(&sid, None), pool.ensure_live(&sid, None));
    assert_eq!((a.unwrap(), b.unwrap()), (sid.clone(), sid.clone()));
    assert_eq!(
        builds.load(Ordering::SeqCst),
        2,
        "one build to create, one to revive"
    );
    let (_, state) = slot_and_state(&pool, &sid).await;
    assert!(state.goal_view().is_null());
}

#[tokio::test]
async fn a_background_tasks_question_outlives_the_turn_and_goes_with_its_task() {
    let (pool, _dir, _store) = pool_with_history().await;
    let sid = new_session(&pool).await;
    let (slot, _) = slot_and_state(&pool, &sid).await;
    let rx = slot.lock().await.take().unwrap();
    pool.sessions
        .lock()
        .await
        .get_mut(&sid)
        .unwrap()
        .current_turn = Some("t1".into());
    pool.remember_prompt(&sid, &("own".into(), None), &serde_json::json!({}))
        .await;
    pool.remember_prompt(
        &sid,
        &("background".into(), Some("task-1".into())),
        &serde_json::json!({}),
    )
    .await;

    assert!(pool.release_turn_slot(&sid, &slot, rx, "t1").await);
    assert_eq!(
        pending_ids(&pool, &sid).await,
        vec!["background".to_string()]
    );
    pool.forget_prompts_of_task(&sid, "task-1").await;
    assert!(pending_ids(&pool, &sid).await.is_empty());
}

async fn pending_ids(pool: &SessionPool, sid: &str) -> Vec<String> {
    let sessions = pool.sessions.lock().await;
    sessions[sid].pending_prompts.keys().cloned().collect()
}

fn transcript_of(store_dir: &std::path::Path, sid: &str) -> std::path::PathBuf {
    let name = format!("{sid}.jsonl");
    let mut dirs = vec![store_dir.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.file_name().is_some_and(|n| n == name.as_str()) {
                return path;
            }
        }
    }
    panic!("no transcript for {sid} under {}", store_dir.display())
}

fn break_log(path: &std::path::Path) -> Vec<u8> {
    use std::io::Write as _;
    std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(b"\xff\xfe\n")
        .unwrap();
    std::fs::read(path).unwrap()
}

#[tokio::test]
async fn session_resume_refuses_a_log_that_does_not_load_even_with_create_if_missing() {
    let (pool, _dir, store) = pool_with_history().await;
    let sid = new_session(&pool).await;
    pool.shutdown_session(&sid).await;
    let broken = break_log(&transcript_of(store.path(), &sid));

    for create_if_missing in [false, true] {
        match pool.resume_session(&sid, create_if_missing, None).await {
            Err(ResumeError::Rpc((code, message))) => {
                assert_eq!(code, codes::INTERNAL_ERROR, "{message}");
                assert!(message.contains("could not be loaded"), "{message}");
            }
            Err(_) => panic!("refused for some other reason"),
            Ok(v) => panic!("resumed a log that does not load: {v}"),
        }
    }
    assert!(!pool.sessions.lock().await.contains_key(&sid));
    assert_eq!(
        std::fs::read(transcript_of(store.path(), &sid)).unwrap(),
        broken
    );
}

#[tokio::test]
async fn a_rebuild_whose_log_does_not_load_leaves_the_session_running_as_it_was() {
    let (pool, _dir, store) = pool_with_history().await;
    let sid = new_session(&pool).await;
    let (slot, state) = slot_and_state(&pool, &sid).await;
    break_log(&transcript_of(store.path(), &sid));

    assert!(matches!(
        pool.rebuild_idle(&sid, None).await,
        Err(Rebuild::Failed(_))
    ));
    let (still_slot, still_state) = slot_and_state(&pool, &sid).await;
    assert!(Arc::ptr_eq(&slot, &still_slot) && Arc::ptr_eq(&state, &still_state));
    assert!(slot.lock().await.is_some(), "the turn slot was given back");
}
