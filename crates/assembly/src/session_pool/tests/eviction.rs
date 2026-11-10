//! `eviction_tests` 原样搬来的用例。

use super::*;

/// A `LiveSession` with no real agent behind it — only `last_active`
/// (and `cancel`, so eviction's `.cancel()` call doesn't panic) matter
/// for these tests.
fn dummy_live_session(last_active: Instant) -> LiveSession {
    let (input_tx, _input_rx): (InputSender, _) = tokio::sync::mpsc::unbounded_channel();
    let (_event_tx, event_rx): (_, EventReceiver) = tokio::sync::mpsc::unbounded_channel();
    LiveSession {
        input_tx,
        turn: TurnSlot::new(Generations::default().next(), event_rx),
        cancel: CancellationToken::new(),
        exited: CancellationToken::new(),
        context: None,
        scene: Arc::new(scene::code::CodeScene::default()),
        project: None,
        name: None,
        created_at: last_active,
        is_first_turn: true,
        named_by_user: false,
        namer: SessionNamer::of(
            "dummy",
            None,
            &(Arc::new(AnthropicModel::new(Arc::new(
                model::mock::MockAnthropicClient::new(),
            ))) as Arc<dyn base::interface::model::Model>),
            "claude-sonnet-4-6",
        ),
        binding_stale: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        state: None,
        spawner: None,
        scripts: None,
        health: Default::default(),
        hub: Arc::new(SessionHub::active_at(last_active)),
        process_groups: None,
    }
}

#[test]
fn evict_lru_removes_the_least_recently_active_session() {
    let now = Instant::now();
    let mut sessions = HashMap::new();
    sessions.insert(
        "oldest".to_string(),
        dummy_live_session(now.checked_sub(Duration::from_secs(60)).unwrap()),
    );
    sessions.insert(
        "middle".to_string(),
        dummy_live_session(now.checked_sub(Duration::from_secs(30)).unwrap()),
    );
    sessions.insert("newest".to_string(), dummy_live_session(now));

    evict_lru(&mut sessions);

    assert_eq!(
        sessions.len(),
        2,
        "sessions: {:?}",
        sessions.keys().collect::<Vec<_>>()
    );
    assert!(!sessions.contains_key("oldest"));
    assert!(sessions.contains_key("middle"));
    assert!(sessions.contains_key("newest"));
}

#[test]
fn inserting_over_an_existing_sid_must_cancel_the_replaced_sessions_token() {
    // Mirrors the pattern `create()` uses: `sessions.insert(sid, new_live)`
    // returning `Some(old)` means a live entry already existed under that
    // exact sid — reachable via the config-generation recreate path in
    // `run_turn`. `CancellationToken` isn't tied to `Drop`, so failing to
    // call `.cancel()` on the returned old value would leak the old
    // Agent's background `tokio::spawn` loop forever. This test isolates
    // just that HashMap-replace-then-cancel pattern (not the full
    // `create()`/`SessionPool`, which needs a live client/settings/etc.).
    let mut sessions = HashMap::new();
    let old_session = dummy_live_session(Instant::now());
    let old_cancel = old_session.cancel.clone();
    sessions.insert("sid".to_string(), old_session);
    assert!(!old_cancel.is_cancelled());

    let new_session = dummy_live_session(Instant::now());
    if let Some(old) = sessions.insert("sid".to_string(), new_session) {
        old.cancel.cancel();
    }

    assert!(
        old_cancel.is_cancelled(),
        "replaced session's token must be cancelled"
    );
    assert_eq!(sessions.len(), 1);
}

fn running_a_task() -> Arc<base::context::SessionState> {
    let state = Arc::new(base::context::SessionState::new("/tmp".into()));
    state.register_task(
        "task-1",
        base::context::TaskType::Agent,
        CancellationToken::new(),
        serde_json::json!({}),
    );
    state
}

#[test]
fn eviction_passes_over_a_session_that_is_doing_something() {
    let now = Instant::now();
    let long_ago = now.checked_sub(Duration::from_secs(600)).unwrap();
    let mut sessions = HashMap::new();
    let mut in_a_turn = dummy_live_session(long_ago);
    in_a_turn
        .turn
        .claim(RunningTurn::new("t1".into(), Initiator::Client, None))
        .unwrap();
    sessions.insert("in_a_turn".to_string(), in_a_turn);
    let mut rebuilding = dummy_live_session(long_ago);
    rebuilding.turn.begin_rebuild().unwrap();
    sessions.insert("rebuilding".to_string(), rebuilding);
    let mut with_a_task = dummy_live_session(long_ago);
    with_a_task.state = Some(running_a_task());
    sessions.insert("with_a_task".to_string(), with_a_task);

    assert!(idle_session_ids(&sessions, now, Duration::from_secs(300)).is_empty());
    assert!(evict_lru(&mut sessions).is_none(), "every session is busy");
    assert_eq!(sessions.len(), 3);

    sessions.insert("idle".to_string(), dummy_live_session(now));
    assert_eq!(
        evict_lru(&mut sessions).map(|(sid, _)| sid).as_deref(),
        Some("idle")
    );
}

#[test]
fn evict_lru_on_empty_map_is_a_noop() {
    let mut sessions: HashMap<String, LiveSession> = HashMap::new();
    evict_lru(&mut sessions); // must not panic
    assert!(sessions.is_empty());
}

#[test]
fn idle_session_ids_selects_only_sessions_past_timeout() {
    let now = Instant::now();
    let mut sessions = HashMap::new();
    sessions.insert(
        "stale".to_string(),
        dummy_live_session(now.checked_sub(Duration::from_secs(600)).unwrap()),
    );
    sessions.insert("fresh".to_string(), dummy_live_session(now));

    let idle = idle_session_ids(&sessions, now, Duration::from_secs(300));
    assert_eq!(idle, vec!["stale".to_string()]);
}

#[test]
fn idle_session_ids_empty_when_nothing_past_timeout() {
    let now = Instant::now();
    let mut sessions = HashMap::new();
    sessions.insert("fresh".to_string(), dummy_live_session(now));

    let idle = idle_session_ids(&sessions, now, Duration::from_secs(300));
    assert!(idle.is_empty());
}

#[test]
fn idle_session_ids_exactly_at_timeout_boundary_is_not_evicted() {
    // 边界用 `>` 不是 `>=`（见 idle_session_ids 实现）——正好等于超时
    // 阈值的 session 这一轮还不驱逐，下一轮才会（因为到那时肯定 `>` 了）。
    let now = Instant::now();
    let mut sessions = HashMap::new();
    sessions.insert(
        "exactly_at_boundary".to_string(),
        dummy_live_session(now.checked_sub(Duration::from_secs(300)).unwrap()),
    );

    let idle = idle_session_ids(&sessions, now, Duration::from_secs(300));
    assert!(idle.is_empty(), "idle: {idle:?}");
}
