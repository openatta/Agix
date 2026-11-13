//! `goal_record_tests` 原样搬来的用例。

use super::mcp_for_project::test_pool_with_store;
use super::*;
use base::context::goal::TurnOrigin;
use history::entry::{EnvelopedEntry, LogEntry};
use history::error::HistoryError;
use history::store::{HistoryStore, InMemoryHistoryStore};

/// A store whose goal writes wait while the test holds `gate`, telling
/// `writing` each time one starts to.
struct GatedGoalWrites {
    inner: InMemoryHistoryStore,
    gate: Arc<AsyncMutex<()>>,
    writing: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl HistoryStore for GatedGoalWrites {
    async fn append(
        &self,
        session: base::session::SessionId,
        entry: LogEntry,
    ) -> Result<(), HistoryError> {
        if matches!(&entry, LogEntry::Extension { ns, .. } if ns == base::context::goal::GOAL_NS) {
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

async fn pool_with_gated_goal_writes() -> (Arc<SessionPool>, Arc<GatedGoalWrites>, tempfile::TempDir)
{
    let store = Arc::new(GatedGoalWrites {
        inner: InMemoryHistoryStore::new(),
        gate: Arc::default(),
        writing: tokio::sync::Notify::new(),
    });
    let (pool, dir) = test_pool_with_store(Some(store.clone())).await;
    (pool, store, dir)
}

async fn session_with_a_goal(pool: &SessionPool) -> (String, Arc<base::context::SessionState>) {
    let sid = pool
        .create_session(None, ProjectSelector::Default, None)
        .await
        .expect("a session")
        .session_id;
    let state = pool.live_state(&sid).await.expect("a live session");
    state
        .goal_create("ship it".into(), Some(1), TurnOrigin::Human)
        .unwrap();
    pool.record_goal_changes(&sid, &state).await;
    (sid, state)
}

fn blocks_on_its_round_limit(state: &base::context::SessionState) {
    state.goal_claim_round(&state.goal_next_round().expect("a round").turn_id);
    assert!(state.goal_next_round().is_none());
    assert_eq!(
        state.goal_view().unwrap().goal.phase,
        base::context::goal::GoalPhase::Blocked
    );
}

#[tokio::test]
async fn a_closed_session_s_log_has_the_goal_change_still_being_written() {
    let (pool, store, _dir) = pool_with_gated_goal_writes().await;
    let (sid, state) = session_with_a_goal(&pool).await;
    let gate = store.gate.clone().lock_owned().await;
    blocks_on_its_round_limit(&state);

    let ((), (cold, ())) = tokio::join!(pool.record_goal_changes(&sid, &state), async {
        store.writing.notified().await;
        tokio::join!(
            async {
                pool.shutdown_session(&sid).await.unwrap();
                pool.session_goal(&sid).await.unwrap()
            },
            async {
                tokio::time::sleep(Duration::from_millis(50)).await;
                drop(gate);
            },
        )
    });
    assert_eq!(
        cold.as_ref().map(|g| g.goal.phase),
        Some(base::context::goal::GoalPhase::Blocked),
        "{cold:?}"
    );
}

#[tokio::test]
async fn a_closed_session_s_log_has_the_goal_change_nobody_wrote_yet() {
    let (pool, _store, _dir) = pool_with_gated_goal_writes().await;
    let (sid, state) = session_with_a_goal(&pool).await;
    blocks_on_its_round_limit(&state);

    pool.shutdown_session(&sid).await.unwrap();
    let cold = pool.session_goal(&sid).await.unwrap();
    assert_eq!(
        cold.as_ref().map(|g| g.goal.phase),
        Some(base::context::goal::GoalPhase::Blocked),
        "{cold:?}"
    );
}

#[tokio::test]
async fn an_evicted_session_s_engine_writes_its_goal_changes_as_it_stops() {
    let (pool, _store, _dir) = pool_with_gated_goal_writes().await;
    let (sid, state) = session_with_a_goal(&pool).await;
    blocks_on_its_round_limit(&state);

    let live = pool.live.remove(&sid).await.unwrap();
    pool.evict(&sid, &live);
    pool.live.wait_until_stopped(&sid).await.unwrap();
    let cold = pool.session_goal(&sid).await.unwrap();
    assert_eq!(
        cold.as_ref().map(|g| g.goal.phase),
        Some(base::context::goal::GoalPhase::Blocked),
        "{cold:?}"
    );
}

/// Answers every call with a line of text, counting the turn-loop calls.
#[derive(Default)]
struct CountsTurnCalls {
    turn_calls: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl base::interface::model::Model for CountsTurnCalls {
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
            self.turn_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        let events = vec![
            Ok(ModelEvent::TextDelta {
                text: "working on it".into(),
            }),
            Ok(ModelEvent::EndTurn {
                stop_reason: "end_turn".into(),
                usage: Default::default(),
            }),
        ];
        Ok(Box::new(futures::stream::iter(events)))
    }
}

async fn answered_by_a_counting_model(pool: &SessionPool) -> Arc<CountsTurnCalls> {
    let model = Arc::new(CountsTurnCalls::default());
    let mut settings = (*pool.bindings.pool_settings().await).clone();
    settings.values.memory_enabled = false;
    let mut binding = pool.bindings.binding_from(settings);
    binding.model = model.clone();
    pool.bindings
        .insert(
            (Some(pool.cwd().to_path_buf()), "code".to_string()),
            binding,
        )
        .await;
    model
}

/// `session.interrupt` lands on the turn `turn_started` reported. A goal
/// round reports it, then writes the claimed round to the log before its
/// message reaches the engine; an interrupt in between is answered
/// `interrupted: true`, so the round must not then run to the model.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn audit2_an_interrupt_while_a_goal_round_records_its_claim_stops_that_round() {
    let (pool, store, _dir) = pool_with_gated_goal_writes().await;
    let model = answered_by_a_counting_model(&pool).await;
    let (sid, state) = session_with_a_goal(&pool).await;
    let round = state.goal_next_round().expect("a round");
    pool.record_goal_changes(&sid, &state).await;

    let gate = store.gate.clone().lock_owned().await;
    let _earlier_write = futures::FutureExt::now_or_never(store.writing.notified());
    let writing = store.writing.notified();
    tokio::pin!(writing);
    writing.as_mut().enable();
    let running = {
        let pool = pool.clone();
        let sid = sid.clone();
        tokio::spawn(async move { pool.run_goal_round(sid, round).await })
    };
    writing.await;
    assert!(
        pool.current_turn_of(&sid).await.is_some(),
        "the round was not reported started"
    );
    let interrupted = pool.interrupt_session(&sid).await.expect("the session");
    drop(gate);
    running.await.unwrap();

    assert!(interrupted, "the interrupt found no turn");
    assert_eq!(
        model.turn_calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the interrupt was answered true, yet the goal round it was sent for ran to the model"
    );
}

/// Keeps how each turn it watches ended.
#[derive(Default)]
struct EndsOfTurns(std::sync::Mutex<Vec<String>>);

impl SessionWatcher for EndsOfTurns {
    fn id(&self) -> WatcherId {
        WatcherId(41)
    }

    fn deliver(&self, frame: &SessionFrame) -> bool {
        if let SessionEvent::Agent(AgentEvent::TurnComplete { stop_reason, .. }) = &frame.event {
            self.0.lock().unwrap().push(stop_reason.clone());
        }
        true
    }
}

/// A turn holds the session from the moment it is claimed, before its
/// message reaches the engine; an interrupt then lands on it, and the engine
/// ends the turn as cancelled when the message arrives.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_interrupt_before_a_claimed_turn_s_message_reaches_the_engine_cancels_that_turn() {
    let (pool, store, _dir) = pool_with_gated_goal_writes().await;
    let model = answered_by_a_counting_model(&pool).await;
    let (sid, state) = session_with_a_goal(&pool).await;
    let round = state.goal_next_round().expect("a round");
    pool.record_goal_changes(&sid, &state).await;

    let gate = store.gate.clone().lock_owned().await;
    let _earlier_write = futures::FutureExt::now_or_never(store.writing.notified());
    let writing = store.writing.notified();
    tokio::pin!(writing);
    writing.as_mut().enable();
    let ends = Arc::new(EndsOfTurns::default());
    let running = {
        let pool = pool.clone();
        let watcher: Arc<dyn SessionWatcher> = ends.clone();
        let request = TurnRequest {
            session_id: Some(sid.clone()),
            message: round.message,
            attachments: Vec::new(),
            turn_id: round.turn_id.clone(),
            options: None,
            on_busy: OnBusy::Queue,
            initiator: Initiator::Goal,
        };
        tokio::spawn(async move { pool.run_turn(request, Some(watcher)).await })
    };
    writing.await;
    assert_eq!(
        pool.current_turn_of(&sid).await.as_deref(),
        Some(round.turn_id.as_str())
    );
    assert!(pool.interrupt_session(&sid).await.expect("the session"));
    drop(gate);

    match running.await.unwrap() {
        Ok(TurnOutcome::Completed { turn_id, .. }) => assert_eq!(turn_id, round.turn_id),
        other => panic!("the claimed turn did not end as a turn: {other:?}"),
    }
    assert_eq!(*ends.0.lock().unwrap(), ["cancelled"]);
    assert_eq!(
        model.turn_calls.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
}

/// A store that edits the goal once the turn's reply is on disk, and again
/// as the engine records that edit, then holds the next goal write — the
/// pool's, made after the turn ended and before it gives its slot back —
/// while the test holds `gate`.
struct HoldsTheGoalWriteAfterTheTurn {
    inner: InMemoryHistoryStore,
    state: std::sync::OnceLock<Arc<base::context::SessionState>>,
    edits: std::sync::atomic::AtomicUsize,
    gate: Arc<AsyncMutex<()>>,
    writing: tokio::sync::Notify,
}

impl HoldsTheGoalWriteAfterTheTurn {
    fn edit_the_goal(&self) {
        let state = self.state.get().expect("the session's state");
        let view = state.goal_view().unwrap();
        state
            .goal_update(
                &view.goal.id,
                view.goal.revision,
                base::context::goal::GoalAction::Edit {
                    objective: Some("ship it, again".into()),
                    max_rounds: None,
                },
                TurnOrigin::Human,
            )
            .unwrap();
    }
}

#[async_trait::async_trait]
impl HistoryStore for HoldsTheGoalWriteAfterTheTurn {
    async fn append(
        &self,
        session: base::session::SessionId,
        entry: LogEntry,
    ) -> Result<(), HistoryError> {
        use std::sync::atomic::Ordering::SeqCst;
        let goal_write =
            matches!(&entry, LogEntry::Extension { ns, .. } if ns == base::context::goal::GOAL_NS);
        match (&entry, self.edits.load(SeqCst)) {
            (LogEntry::Assistant { .. }, 0) => {
                self.edits.store(1, SeqCst);
                self.edit_the_goal();
            }
            (_, 1) if goal_write => {
                self.edits.store(2, SeqCst);
                self.edit_the_goal();
            }
            (_, 2) if goal_write => {
                self.edits.store(3, SeqCst);
                self.writing.notify_one();
                let _open = self.gate.lock().await;
            }
            _ => {}
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

/// The other end of the round: it has answered and the pool is recording
/// the goal before it gives the slot back and starts the next round. An
/// interrupt then is answered `interrupted: true`, so no next round follows.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_interrupt_while_an_answered_goal_round_records_the_goal_stops_the_next_round() {
    let store = Arc::new(HoldsTheGoalWriteAfterTheTurn {
        inner: InMemoryHistoryStore::new(),
        state: Default::default(),
        edits: Default::default(),
        gate: Arc::default(),
        writing: tokio::sync::Notify::new(),
    });
    let (pool, _dir) = test_pool_with_store(Some(store.clone())).await;
    let model = answered_by_a_counting_model(&pool).await;
    let sid = pool
        .create_session(None, ProjectSelector::Default, None)
        .await
        .expect("a session")
        .session_id;
    let state = pool.live_state(&sid).await.expect("a live session");
    state
        .goal_create("ship it".into(), Some(5), TurnOrigin::Human)
        .unwrap();
    pool.record_goal_changes(&sid, &state).await;
    store.state.set(state.clone()).unwrap();
    let round = state.goal_next_round().expect("a round");

    let gate = store.gate.clone().lock_owned().await;
    let writing = store.writing.notified();
    tokio::pin!(writing);
    writing.as_mut().enable();
    let running = {
        let pool = pool.clone();
        let sid = sid.clone();
        tokio::spawn(async move { pool.run_goal_round(sid, round).await })
    };
    writing.await;
    let interrupted = pool.interrupt_session(&sid).await.expect("the session");
    drop(gate);
    running.await.unwrap();

    assert!(interrupted, "the interrupt found no turn");
    assert_eq!(
        state.goal_view().map(|view| view.armed),
        Some(false),
        "the interrupt was answered true, yet the goal stays armed for the next round"
    );
    assert_eq!(
        model.turn_calls.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
}
