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

/// `setModel` checks the session is idle, then changes the model. A rebuild
/// that takes the session in between has read the spec already and builds
/// the next engine on the model before; the choice still reaches that engine.
#[tokio::test]
async fn a_model_chosen_while_a_rebuild_is_on_its_way_reaches_the_rebuilt_engine() {
    let (pool, _dir) = test_pool().await;
    let model = Arc::new(RecordsTheModel::default());
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
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
    let rebuilt = {
        let (pool, sid) = (pool.clone(), sid.clone());
        tokio::spawn(async move {
            pool.set_session_prompt(&sid, |p| p.append = Some("Answer briefly.".into()))
                .await
                .map(|_| ())
        })
    };
    let mut rebuilding = false;
    for _ in 0..1000 {
        tokio::task::yield_now().await;
        rebuilding = pool
            .live
            .with_live(&sid, |live| !live.turn.is_idle())
            .await
            .unwrap_or(false);
        if rebuilding {
            break;
        }
    }
    assert!(rebuilding, "the rebuild took the session");
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
    drop(bindings);
    assert!(chosen.await.unwrap().is_ok(), "the choice was taken");
    assert!(rebuilt.await.unwrap().is_ok(), "the session was rebuilt");

    pool.run_turn(
        TurnRequest {
            session_id: Some(sid),
            message: "hello".into(),
            attachments: Vec::new(),
            turn_id: "t1".into(),
            options: None,
            on_busy: OnBusy::Reject,
            initiator: Initiator::Client,
        },
        None,
    )
    .await
    .expect("a turn");
    assert_eq!(
        model.0.lock().unwrap().as_slice(),
        ["claude-opus-4-6"],
        "the next turn asks for the model chosen"
    );
}
