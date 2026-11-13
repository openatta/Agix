//! `SessionPool` 的单元测试，按主题分文件。

use super::builder::SessionPoolBuilder;
use super::*;

/// A watcher that takes every frame and keeps none.
pub(super) struct NullWatcher(pub(super) u64);

impl SessionWatcher for NullWatcher {
    fn id(&self) -> WatcherId {
        WatcherId(self.0)
    }

    fn deliver(&self, _frame: &SessionFrame) -> bool {
        true
    }
}

/// An ask of `session_id` as the engine would raise it.
pub(super) fn raise_ask(
    hub: &SessionHub,
    session_id: &str,
    prompt_id: &str,
    task_id: Option<&str>,
) {
    hub.publish_ask(
        Arc::new(frame_between_turns(session_id, SessionEvent::EngineStopped)),
        prompt_id.to_string(),
        task_id.map(str::to_string),
        AskKind::Permission,
    );
}

/// Holds its first turn-loop call until released, and keeps the conversation
/// every turn-loop call was sent.
#[derive(Default)]
pub(super) struct HoldsTheFirstCall {
    release: std::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    pub(super) holding: tokio::sync::Notify,
    pub(super) sent: std::sync::Mutex<Vec<String>>,
}

impl HoldsTheFirstCall {
    /// Hold the next turn-loop call until the returned sender is used.
    pub(super) fn hold_the_first_call(&self) -> tokio::sync::oneshot::Sender<()> {
        let (release, released) = tokio::sync::oneshot::channel();
        *self.release.lock().unwrap() = Some(released);
        release
    }
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

pub(super) fn turn(sid: &str, turn_id: &str, message: &str, on_busy: OnBusy) -> TurnRequest {
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

/// Have the sessions of `pool`'s own project and scene answered by `model`,
/// with no memory extraction.
pub(super) async fn answered_by(pool: &SessionPool, model: Arc<dyn base::interface::model::Model>) {
    let mut settings = (*pool.bindings.pool_settings().await).clone();
    settings.values.memory_enabled = false;
    let mut binding = pool.bindings.binding_from(settings);
    binding.model = model;
    pool.bindings
        .insert(
            (Some(pool.cwd().to_path_buf()), "code".to_string()),
            binding,
        )
        .await;
}

mod audit3;
mod auto_name;
mod context_op;
mod eviction;
mod goal_record;
pub(in crate::session_pool) mod mcp_for_project;
/// Multiple scenes coexisting in one daemon process.
mod multi_scene;
mod pool_telemetry;
mod rebuild;
mod recorded_spec;
mod redact;
mod resolve_session_permission;
mod script_binding;
/// `session.list`'s historical half is a `HistoryStore` query, not a walk the
/// pool does itself.
mod session_listing;
mod set_model;
mod steer;
mod turn_driver;
mod with_session_lock;
