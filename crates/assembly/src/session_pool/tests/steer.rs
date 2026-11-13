//! `on_busy: "steer"` hands the message to the running turn through the
//! session's own queue, and the answer is had on the spot.

use super::mcp_for_project::test_pool;
use super::*;
use std::sync::Mutex;

const MARKER: &str = "STEERED-7f3a";

/// Holds its first turn-loop call until released, and keeps the conversation
/// every turn-loop call was sent.
#[derive(Default)]
struct HoldsTheFirstCall {
    release: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    holding: tokio::sync::Notify,
    sent: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl base::interface::model::Model for HoldsTheFirstCall {
    fn api_type(&self) -> base::provider::ApiType {
        base::provider::ApiType::Anthropic
    }

    async fn stream(
        &self,
        _: Vec<base::prompt::PromptBlock>,
        _: Vec<base::interface::model::ToolDef>,
        messages: Vec<base::interface::model::ModelMessage>,
        params: base::interface::model::StreamParams,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError> {
        use base::interface::model::ModelEvent;
        if params
            .origin
            .as_ref()
            .is_some_and(|o| o.purpose().is_none())
        {
            self.sent.lock().unwrap().push(format!("{messages:?}"));
            let held = self.release.lock().unwrap().take();
            if let Some(release) = held {
                self.holding.notify_one();
                let _ = release.await;
            }
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

fn turn(sid: &str, turn_id: &str, message: &str, on_busy: OnBusy) -> TurnRequest {
    TurnRequest {
        session_id: Some(sid.to_string()),
        message: message.to_string(),
        attachments: Vec::new(),
        turn_id: turn_id.to_string(),
        options: None,
        on_busy,
        initiator: Initiator::Client,
    }
}

/// The engine reads its input channel six seconds late — longer than the
/// five a steer used to wait for the engine's answer before giving up and
/// queueing the message as a turn of its own, while the engine went on to
/// take it into the running turn as well.
#[tokio::test(start_paused = true)]
async fn a_steered_message_reaches_the_conversation_once_however_slow_the_engine_reads() {
    let (pool, _dir) = test_pool().await;
    let model = Arc::new(HoldsTheFirstCall::default());
    let (release, released) = tokio::sync::oneshot::channel();
    *model.release.lock().unwrap() = Some(released);
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
