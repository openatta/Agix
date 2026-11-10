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
) -> (Generation, Arc<base::context::SessionState>) {
    let sessions = pool.sessions.lock().await;
    let live = sessions.get(sid).expect("the session is live");
    (live.turn.generation, live.state.clone().expect("a state"))
}

fn append(prompt: &mut runtime::agent::SessionPrompt) {
    prompt.append = Some("Answer briefly.".into());
}

async fn refused_prompt_change(pool: &SessionPool, sid: &str) -> Busy {
    match pool.set_session_prompt(sid, append).await {
        Err(PoolError::SessionBusy(busy)) => busy,
        Err(e) => panic!("{e}"),
        Ok(v) => panic!("a rebuild cut off a busy session: {v}"),
    }
}

#[tokio::test]
async fn a_prompt_change_is_refused_while_a_turn_runs_or_a_rebuild_holds_the_session() {
    let (pool, _dir, _store) = pool_with_history().await;
    let sid = new_session(&pool).await;
    let (generation, _) = slot_and_state(&pool, &sid).await;
    let with_turn = |change: fn(&mut TurnSlot)| {
        let pool = &pool;
        let sid = &sid;
        async move { change(&mut pool.sessions.lock().await.get_mut(sid).unwrap().turn) }
    };

    with_turn(|turn| {
        turn.claim(RunningTurn::new("t1".into(), Initiator::Client, None))
            .unwrap();
    })
    .await;
    let busy = refused_prompt_change(&pool, &sid).await;
    assert_eq!(busy.current_turn_id.as_deref(), Some("t1"));

    with_turn(|turn| {
        let g = turn.generation;
        turn.release(g, "t1").unwrap();
        turn.begin_rebuild().unwrap();
    })
    .await;
    let busy = refused_prompt_change(&pool, &sid).await;
    assert_eq!(busy.current_turn_id, None);
    assert_eq!(
        slot_and_state(&pool, &sid).await.0,
        generation,
        "the engine was replaced under a busy session"
    );

    with_turn(|turn| {
        let g = turn.generation;
        assert!(turn.abort_rebuild(g));
    })
    .await;
    pool.set_session_prompt(&sid, append)
        .await
        .unwrap_or_else(|_| panic!("an idle session takes the change"));
    let (rebuilt, _) = slot_and_state(&pool, &sid).await;
    assert_ne!(rebuilt, generation, "an idle session is rebuilt");
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
    pool.subscribe_session(&sid, Arc::new(NullWatcher(7)))
        .await
        .unwrap();
    raise_ask(
        &pool.hub_of(&sid).await.unwrap(),
        &sid,
        "p1",
        Some("task-1"),
    );

    pool.set_session_prompt(&sid, append)
        .await
        .unwrap_or_else(|_| panic!("an idle session takes the change"));
    let sessions = pool.sessions.lock().await;
    let live = sessions.get(&sid).unwrap();
    assert!(live.hub.is_watched_by(WatcherId(7)));
    assert_eq!(live.hub.pending_prompt_ids(), ["p1"]);
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
    pool.shutdown_session(&sid).await.unwrap();
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

fn turn_complete(context: base::event::ContextUsage) -> AgentEvent {
    AgentEvent::TurnComplete {
        stop_reason: "end_turn".into(),
        api_calls: 1,
        tool_calls: 0,
        usage: Default::default(),
        turn_id: "t1".into(),
        message: None,
        last_call_usage: Default::default(),
        context: Some(context),
        structured_output: None,
        seq: 0,
        truncated: false,
    }
}

/// The turn of a session closed and brought back under the same id is
/// still reading what its engine sent; what it reads last writes nothing
/// into the entry that has the id now.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_late_turn_complete_of_a_closed_session_leaves_the_resumed_one_alone() {
    let (pool, _dir, _store) = pool_with_history().await;
    let sid = new_session(&pool).await;
    let (late_tx, late_rx) = tokio::sync::mpsc::unbounded_channel();
    let (old_generation, _engine_events) = {
        let sessions = pool.sessions.lock().await;
        let live = &sessions[&sid];
        let engine_events = live.turn.rx.lock().await.replace(late_rx);
        (live.turn.generation, engine_events)
    };
    let pool = Arc::new(pool);
    let old_turn = {
        let pool = pool.clone();
        let request = TurnRequest {
            session_id: Some(sid.clone()),
            message: "hello".into(),
            attachments: Vec::new(),
            turn_id: "t1".into(),
            options: None,
            on_busy: OnBusy::Reject,
            initiator: Initiator::Client,
        };
        tokio::spawn(async move { pool.run_turn(request, None).await })
    };
    while pool.current_turn_of(&sid).await.is_none() {
        tokio::task::yield_now().await;
    }

    pool.shutdown_session(&sid).await.unwrap();
    pool.ensure_live(&sid, None).await.unwrap();
    let resumed = slot_and_state(&pool, &sid).await.0;
    assert_ne!(resumed, old_generation);
    late_tx
        .send(turn_complete(base::event::ContextUsage {
            tokens: 1000,
            compact_threshold: 2000,
            window: 4000,
        }))
        .unwrap();
    assert!(
        old_turn.await.unwrap().is_err(),
        "the closed turn completed"
    );
    assert_eq!(
        pool.adopt_generated_name(&sid, old_generation, "a guess".into())
            .await,
        None
    );

    let sessions = pool.sessions.lock().await;
    let live = &sessions[&sid];
    assert_eq!(live.turn.generation, resumed);
    assert!(live.turn.is_idle());
    assert_eq!(live.context, None, "the old turn wrote its context");
    assert_eq!(live.name, None, "the old turn named the session");
}

#[tokio::test]
async fn a_background_tasks_question_outlives_the_turn_and_goes_with_its_task() {
    let (pool, _dir, _store) = pool_with_history().await;
    let sid = new_session(&pool).await;
    let (generation, slot) = {
        let mut sessions = pool.sessions.lock().await;
        let turn = &mut sessions.get_mut(&sid).unwrap().turn;
        let generation = turn
            .claim(RunningTurn::new("t1".into(), Initiator::Client, None))
            .unwrap();
        (generation, turn.rx.clone())
    };
    let rx = slot.lock().await.take().unwrap();
    let hub = pool.hub_of(&sid).await.unwrap();
    raise_ask(&hub, &sid, "own", None);
    raise_ask(&hub, &sid, "background", Some("task-1"));

    let ticket = TurnTicket {
        sid: sid.clone(),
        generation,
        turn_id: "t1".into(),
        rx: slot,
    };
    assert!(pool.release_turn_slot(&ticket, rx).await);
    assert_eq!(
        pending_ids(&pool, &sid).await,
        vec!["background".to_string()]
    );
    hub.forget_asks_of_task("task-1");
    assert!(pending_ids(&pool, &sid).await.is_empty());
}

async fn pending_ids(pool: &SessionPool, sid: &str) -> Vec<String> {
    pool.hub_of(sid).await.unwrap().pending_prompt_ids()
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
    pool.shutdown_session(&sid).await.unwrap();
    let broken = break_log(&transcript_of(store.path(), &sid));

    for create_if_missing in [false, true] {
        match pool.resume_session(&sid, create_if_missing, None).await {
            Err(PoolError::Internal(message)) => {
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
    let (generation, state) = slot_and_state(&pool, &sid).await;
    break_log(&transcript_of(store.path(), &sid));

    assert!(matches!(
        pool.rebuild_idle(&sid, None).await,
        Err(Rebuild::Failed(_))
    ));
    let (still_generation, still_state) = slot_and_state(&pool, &sid).await;
    assert!(generation == still_generation && Arc::ptr_eq(&state, &still_state));
    assert!(
        pool.sessions.lock().await[&sid].turn.is_idle(),
        "the session was given back"
    );
}
