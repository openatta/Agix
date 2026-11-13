//! A whole session assembled with the agents module, driven by a model that
//! answers each request from the queue whose mark the request carries.

use super::*;
use base::interface::model::{CallKind, ModelEvent, ToolDef};

pub(super) enum Step {
    Text(String),
    Call(String, Value),
    /// Waits until `n` turn calls have come to the same gate, or until the
    /// bound runs out, then answers with this text.
    Gather(Arc<Gate>, String),
}

pub(super) fn text(s: &str) -> Step {
    Step::Text(s.into())
}

pub(super) fn call(tool: &str, input: Value) -> Step {
    Step::Call(tool.into(), input)
}

/// Turn calls that came to it, how many were at it at once, and a barrier
/// that lets them through together.
pub(super) struct Gate {
    barrier: tokio::sync::Barrier,
    bound: std::time::Duration,
    at: std::sync::atomic::AtomicUsize,
    pub(super) peak: std::sync::atomic::AtomicUsize,
}

impl Gate {
    pub(super) fn new(n: usize, bound: std::time::Duration) -> Arc<Self> {
        Arc::new(Self {
            barrier: tokio::sync::Barrier::new(n),
            bound,
            at: Default::default(),
            peak: Default::default(),
        })
    }

    pub(super) fn peak(&self) -> usize {
        self.peak.load(std::sync::atomic::Ordering::SeqCst)
    }

    async fn pass(&self) {
        use std::sync::atomic::Ordering::SeqCst;
        let now = self.at.fetch_add(1, SeqCst) + 1;
        self.peak.fetch_max(now, SeqCst);
        let _ = tokio::time::timeout(self.bound, self.barrier.wait()).await;
        self.at.fetch_sub(1, SeqCst);
    }
}

/// What one turn call was sent, and the mark that routed it.
#[derive(Clone)]
pub(super) struct Seen {
    pub(super) mark: Option<String>,
    pub(super) text: String,
}

#[derive(Default)]
pub(super) struct RoutedModel {
    queues: std::sync::Mutex<Vec<(String, std::collections::VecDeque<Step>)>>,
    pub(super) seen: std::sync::Mutex<Vec<Seen>>,
}

impl RoutedModel {
    /// Queues are searched in the order they were added; a request carrying
    /// none of the marks with a reply left is answered "done".
    pub(super) fn route(self, mark: &str, steps: impl IntoIterator<Item = Step>) -> Self {
        self.queues
            .lock()
            .unwrap()
            .push((mark.to_string(), steps.into_iter().collect()));
        self
    }

    pub(super) fn seen_with(&self, mark: &str) -> Vec<Seen> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.mark.as_deref() == Some(mark))
            .cloned()
            .collect()
    }
}

#[async_trait]
impl Model for RoutedModel {
    fn api_type(&self) -> base::provider::ApiType {
        base::provider::ApiType::Anthropic
    }
    async fn stream(
        &self,
        prompt_blocks: Vec<base::prompt::PromptBlock>,
        _tools: Vec<ToolDef>,
        messages: Vec<ModelMessage>,
        params: base::interface::model::StreamParams,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError> {
        let turn_call = params
            .origin
            .as_ref()
            .is_some_and(|o| matches!(o.kind, CallKind::Turn { .. }));
        let text = format!(
            "{}\n{}",
            prompt_blocks
                .iter()
                .map(|b| b.content.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
            serde_json::to_string(&messages).unwrap_or_default()
        );
        let step = if turn_call {
            let mut queues = self.queues.lock().unwrap();
            queues
                .iter_mut()
                .find(|(mark, steps)| !steps.is_empty() && text.contains(mark.as_str()))
                .map(|(mark, steps)| (mark.clone(), steps.pop_front()))
        } else {
            None
        };
        let (mark, step) = match step {
            Some((mark, step)) => (Some(mark), step),
            None => (None, None),
        };
        if turn_call {
            self.seen.lock().unwrap().push(Seen { mark, text });
        }
        let events = match step {
            Some(Step::Call(name, input)) => vec![
                Ok(ModelEvent::ToolUse {
                    id: format!("toolu_{}", base::id::Id::new()),
                    name,
                    input,
                }),
                Ok(ModelEvent::EndTurn {
                    stop_reason: "tool_use".into(),
                    usage: Default::default(),
                }),
            ],
            Some(Step::Gather(gate, text)) => {
                gate.pass().await;
                answer(text)
            }
            Some(Step::Text(text)) => answer(text),
            None => answer("done".into()),
        };
        Ok(Box::new(futures::stream::iter(events)))
    }
}

fn answer(text: String) -> Vec<Result<ModelEvent, base::interface::model::ModelError>> {
    vec![
        Ok(ModelEvent::TextDelta { text }),
        Ok(ModelEvent::EndTurn {
            stop_reason: "end_turn".into(),
            usage: Default::default(),
        }),
    ]
}

pub(super) struct Root {
    pub(super) agent: Agent,
    _events: EventReceiver,
    _input: InputSender,
    pub(super) dir: std::path::PathBuf,
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A root session over the built-in tools, assembled with the agents module
/// as a host assembles one, its settings adjusted by `edit`.
pub(super) fn assembled_root(model: Arc<RoutedModel>, edit: impl FnOnce(&mut Settings)) -> Root {
    let (mut settings, dir) = parent_settings_for_test("code");
    edit(&mut settings.values);
    let registry = Arc::new(InMemoryToolRegistry::new());
    tools::register_builtin_tools(&registry);
    let (agent, events, input) = Builder::new()
        .scene(Arc::new(scene::code::CodeScene::default()))
        .model(model)
        .settings(Arc::new(settings))
        .tools(registry)
        .skip_warmup(true)
        .module(Arc::new(crate::AgentsModule::new()))
        .build()
        .expect("the root session builds");
    Root {
        agent,
        _events: events,
        _input: input,
        dir,
    }
}

impl Root {
    pub(super) async fn turn(&mut self, message: &str) {
        self.agent
            .run_turn(
                message.into(),
                format!("turn-{}", base::id::Id::new()),
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .expect("the turn runs");
    }

    /// Waits for every task of the root session to end, within `bound`.
    pub(super) async fn tasks_settled(&self, bound: std::time::Duration) {
        let session = self.agent.session_state();
        tokio::time::timeout(bound, async {
            loop {
                let running: Vec<_> = session
                    .task_infos()
                    .into_iter()
                    .filter(|t| t.status == base::context::task::TaskStatus::Running)
                    .collect();
                if running.is_empty() {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the session's tasks end");
    }
}
