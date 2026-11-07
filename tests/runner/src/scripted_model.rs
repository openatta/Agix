//! A `Model` whose every answer is written down in the test, not recorded from
//! a provider.
//!
//! # Why not cassettes
//!
//! The recorder already gives deterministic replay, and reaching for it first
//! is the obvious move. It does not work as a *committed* regression net, for
//! two independent reasons:
//!
//! 1. `tests/fixtures/cassettes/` is gitignored on purpose. The rationale is
//!    in `.gitignore` itself: a recorded request bakes in the exact model
//!    name, prompt text and tool table, so a committed cassette goes stale the
//!    moment any of those change. Replay compares blob *ids* — content
//!    addresses — so "goes stale" means "every case fails".
//! 2. That staleness is not hypothetical for this refactor. The tickets this
//!    net exists to protect (prompt block naming, the registration contract,
//!    the assembly hook) all change prompt assembly deliberately. A cassette
//!    net would fail on every one of them for the intended reason, which
//!    trains people to regenerate the goldens without reading the diff —
//!    the failure mode that makes a regression net worse than none.
//!
//! So the model is scripted instead. The script is a few lines of Rust, it
//! commits, it costs nothing to run, and it is indifferent to prompt wording —
//! which is what lets it stay sensitive to the thing actually under test: the
//! turn loop's decisions.
//!
//! It also reaches cases a recording cannot. There is no way to ask a real
//! provider for a 529 on the third call, and the existing cassettes contain no
//! error outcomes at all, so overload/fallback, prompt-too-long recovery and
//! `max_tokens` continuation are only reproducible by scripting them.
//!
//! # What it does not cover
//!
//! Nothing here checks the *request*. Whether the assembled prompt is correct
//! is what the cassette machinery and `run_rerun.sh` are for; this asserts
//! what the engine does with a given sequence of answers.

use base::interface::model::{
    Model, ModelError, ModelEvent, ModelMessage, ModelStream, StreamParams, ToolDef, Usage,
};
use base::interface::prompt::PromptBlock;
use base::provider::ApiType;
use serde_json::Value;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

/// One scripted answer to one `Model::stream` call.
#[derive(Debug)]
pub enum Reply {
    /// Plain text, then `end_turn`.
    Text(&'static str),
    /// Text split across several deltas — token boundaries are behavior when
    /// what you are testing is a consumer of the stream.
    TextChunks(&'static [&'static str]),
    /// One tool call, then `tool_use`.
    Tool {
        id: &'static str,
        name: &'static str,
        input: Value,
    },
    /// Several tool calls in a single assistant message. Whether they run
    /// concurrently is the engine's decision (`Tool::is_concurrency_safe`),
    /// which is exactly what the trace records.
    Tools(Vec<(&'static str, &'static str, Value)>),
    /// Text, then a stop reason of the script's choosing — `max_tokens` is the
    /// interesting one, since the loop treats it as "keep going".
    TextWithStop {
        text: &'static str,
        stop_reason: &'static str,
    },
    /// The call fails before any event is produced.
    Fail(ModelError),
}

impl Reply {
    fn tool(id: &'static str, name: &'static str, input: Value) -> Vec<ModelEvent> {
        vec![
            ModelEvent::ToolUse {
                id: id.into(),
                name: name.into(),
                input,
            },
            ModelEvent::EndTurn {
                stop_reason: "tool_use".into(),
                usage: Usage::default(),
            },
        ]
    }

    fn into_events(self) -> Result<Vec<ModelEvent>, ModelError> {
        Ok(match self {
            Reply::Text(t) => vec![
                ModelEvent::TextDelta { text: t.into() },
                ModelEvent::EndTurn {
                    stop_reason: "end_turn".into(),
                    usage: Usage::default(),
                },
            ],
            Reply::TextChunks(parts) => {
                let mut v: Vec<ModelEvent> = parts
                    .iter()
                    .map(|p| ModelEvent::TextDelta {
                        text: (*p).to_string(),
                    })
                    .collect();
                v.push(ModelEvent::EndTurn {
                    stop_reason: "end_turn".into(),
                    usage: Usage::default(),
                });
                v
            }
            Reply::Tool { id, name, input } => Self::tool(id, name, input),
            Reply::Tools(calls) => {
                let mut v: Vec<ModelEvent> = calls
                    .into_iter()
                    .map(|(id, name, input)| ModelEvent::ToolUse {
                        id: id.into(),
                        name: name.into(),
                        input,
                    })
                    .collect();
                v.push(ModelEvent::EndTurn {
                    stop_reason: "tool_use".into(),
                    usage: Usage::default(),
                });
                v
            }
            Reply::TextWithStop { text, stop_reason } => vec![
                ModelEvent::TextDelta { text: text.into() },
                ModelEvent::EndTurn {
                    stop_reason: stop_reason.into(),
                    usage: Usage::default(),
                },
            ],
            Reply::Fail(e) => return Err(e),
        })
    }
}

/// Answers `Model::stream` from a fixed list, in order.
///
/// Running past the end is a panic rather than a default answer: a case that
/// makes more calls than its script accounts for has changed behavior, and
/// that is the whole thing being watched. Silently returning "" would let the
/// loop spin and the test still pass.
pub struct ScriptedModel {
    replies: Mutex<std::collections::VecDeque<Reply>>,
    /// What each call reports having spent, in order.
    ///
    /// A provider reports usage per call, and a turn is several calls — so a
    /// model that reports the same number every time cannot tell "the sum" and
    /// "the last one" apart, which is the whole question when a host budgets
    /// on it. Empty means zero, which is what every case that does not care
    /// about tokens gets.
    usages: Mutex<std::collections::VecDeque<Usage>>,
    /// Every request the engine made, in order. Several decisions are visible
    /// only here, because they change what is *sent* and nothing about what
    /// comes back: the fallback-model switch, compaction shortening the
    /// history, the tool-result budget truncating one of its entries, a
    /// continuation nudge being appended.
    calls: Mutex<Vec<Call>>,
    /// The same requests as text, prompt blocks and messages together.
    ///
    /// `Call` deliberately reduces a request to counts, which is what keeps
    /// the behavior net's goldens readable. An extension that leaves a mark
    /// in the prompt is invisible in counts, so the text is kept alongside
    /// rather than folded in — a case asserts on one or the other, and the
    /// goldens stay counts.
    request_texts: Mutex<Vec<String>>,
    /// The tools each request offered, by name, in order.
    ///
    /// Kept beside the text for the same reason the text is kept beside the
    /// counts: a point whose only effect is to withdraw a tool leaves nothing
    /// in the prompt and nothing in a count, so without this there is no
    /// channel a case about it could assert on.
    request_tools: Mutex<Vec<Vec<String>>>,
}

/// One model request, reduced to the parts a decision can move.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Call {
    pub model: String,
    /// What an auxiliary call is for (`memory`, `title`, ...); `None` for a
    /// step of a turn.
    pub purpose: Option<String>,
    pub max_tokens: u32,
    /// How many messages went out. Compaction and nudges move this.
    pub messages: usize,
    /// Total bytes of message text. Truncation moves this and nothing else
    /// does — an untruncated 60 KB tool result and a truncated one differ
    /// here by three orders of magnitude.
    pub content_bytes: usize,
    /// Whether any content carries the tool-result budget's truncation
    /// marker. Recorded separately from the byte count so the trace says
    /// *which* mechanism shortened things, not just that something did.
    pub truncated_results: usize,
}

impl ScriptedModel {
    pub fn new(replies: Vec<Reply>) -> Arc<Self> {
        Arc::new(Self {
            replies: Mutex::new(replies.into()),
            usages: Mutex::new(std::collections::VecDeque::new()),
            calls: Mutex::new(Vec::new()),
            request_texts: Mutex::new(Vec::new()),
            request_tools: Mutex::new(Vec::new()),
        })
    }

    /// Report `(input, output)` tokens on successive calls.
    ///
    /// Different numbers per call on purpose: equal ones would let a sum and a
    /// last-value pass the same assertion.
    pub fn reporting_usage(self: Arc<Self>, per_call: &[(u32, u32)]) -> Arc<Self> {
        {
            let mut q = self.usages.lock().unwrap_or_else(|e| e.into_inner());
            q.clear();
            q.extend(per_call.iter().map(|(i, o)| Usage {
                input_tokens: *i,
                output_tokens: *o,
                ..Default::default()
            }));
        }
        self
    }

    /// Each request the engine made, in order.
    pub fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Everything the engine sent, as text: one entry per call, holding that
    /// call's system prompt and every message in it.
    pub fn request_texts(&self) -> Vec<String> {
        self.request_texts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// The tools each request offered, by name, one entry per call.
    pub fn request_tools(&self) -> Vec<Vec<String>> {
        self.request_tools
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Scripted replies never consumed. A case that ends with leftovers took a
    /// shorter path than it was written for, which is as much a regression as
    /// taking a longer one.
    pub fn unconsumed(&self) -> usize {
        self.replies.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

#[async_trait::async_trait]
impl Model for ScriptedModel {
    fn api_type(&self) -> ApiType {
        ApiType::Anthropic
    }

    async fn stream(
        &self,
        prompt_blocks: Vec<PromptBlock>,
        tools: Vec<ToolDef>,
        messages: Vec<ModelMessage>,
        params: StreamParams,
        _cancel: CancellationToken,
    ) -> Result<ModelStream, ModelError> {
        let mut text = String::new();
        for b in &prompt_blocks {
            text.push_str(&b.content);
            text.push('\n');
        }
        let mut content_bytes = 0usize;
        let mut truncated_results = 0usize;
        for m in &messages {
            for block in &m.content {
                let text_of = match block {
                    base::interface::model::ModelContentBlock::Text { text } => Some(text.as_str()),
                    base::interface::model::ModelContentBlock::ToolResult { content, .. } => {
                        Some(content.as_str())
                    }
                    _ => None,
                };
                if let Some(block_text) = text_of {
                    content_bytes += block_text.len();
                    if block_text.starts_with("[Tool result truncated:") {
                        truncated_results += 1;
                    }
                    text.push_str(block_text);
                    text.push('\n');
                }
            }
        }
        self.request_texts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(text);
        self.request_tools
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(tools.iter().map(|t| t.name.clone()).collect());
        self.calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Call {
                model: params.model.clone(),
                purpose: params
                    .origin
                    .as_ref()
                    .and_then(|o| o.purpose())
                    .map(str::to_string),
                max_tokens: params.max_tokens,
                messages: messages.len(),
                content_bytes,
                truncated_results,
            });

        let reply = self
            .replies
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
            .unwrap_or_else(|| {
                panic!(
                    "the engine made more model calls than the script has replies \
                     (call #{}, model={}). The loop took a longer path than this case \
                     was written for.",
                    self.calls.lock().unwrap_or_else(|e| e.into_inner()).len(),
                    params.model
                )
            });

        let mut events = reply.into_events()?;
        // The provider reports what a call spent on the event that ends it.
        if let Some(spent) = self
            .usages
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
        {
            for event in events.iter_mut() {
                if let ModelEvent::EndTurn { usage, .. } = event {
                    *usage = spent;
                }
            }
        }
        Ok(Box::new(futures::stream::iter(
            events.into_iter().map(Ok).collect::<Vec<_>>(),
        )))
    }
}
