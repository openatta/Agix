//! AnthropicModel — adapts `AnthropicClient` to implement `crate::Model`.

use crate::client::AnthropicClient;
use crate::types::{
    BuiltinTool, CacheControl, MessageParam, MessagesRequest, SystemBlock, ThinkingConfig,
};
use async_trait::async_trait;
use base::interface::model::{
    MessageRole, Model, ModelContentBlock, ModelError, ModelEvent, ModelMessage, ModelStream,
    StreamParams, ToolDef, Usage,
};
use base::interface::prompt::{CacheStrategy, PromptBlock};
use base::interface::settings::ThinkingMode;
use base::message::{CacheEdit, ContentBlock, Role};
use base::provider::ApiType;
use futures::StreamExt;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// The name the provider's own web search goes by in a request.
const PROVIDER_SEARCH_TOOL: &str = "web_search";

pub struct AnthropicModel {
    inner: Arc<dyn AnthropicClient>,
    provenance: Option<String>,
}

impl AnthropicModel {
    pub fn new(client: Arc<dyn AnthropicClient>) -> Self {
        Self {
            inner: client,
            provenance: None,
        }
    }

    /// See [`Model::provenance`].
    pub fn with_provenance(mut self, provider_id: impl Into<String>) -> Self {
        self.provenance = Some(provider_id.into());
        self
    }
}

#[async_trait]
impl Model for AnthropicModel {
    fn api_type(&self) -> ApiType {
        ApiType::Anthropic
    }

    fn provenance(&self) -> Option<&str> {
        self.provenance.as_deref()
    }

    async fn stream(
        &self,
        prompt_blocks: Vec<PromptBlock>,
        tools: Vec<ToolDef>,
        messages: Vec<ModelMessage>,
        params: StreamParams,
        cancel: CancellationToken,
    ) -> Result<ModelStream, ModelError> {
        let system: Vec<SystemBlock> = prompt_blocks
            .into_iter()
            .map(|pb| match pb.cache_strategy {
                Some(CacheStrategy::Ephemeral) => {
                    SystemBlock::text_cached(pb.content, CacheControl::ephemeral_1h())
                }
                Some(CacheStrategy::Global) => {
                    SystemBlock::text_cached(pb.content, CacheControl::ephemeral_1h_global())
                }
                None => SystemBlock::text(pb.content),
            })
            .collect();

        let mut message_params: Vec<MessageParam> = messages
            .into_iter()
            .map(|m| to_message_param(m, self.provenance.as_deref()))
            .filter(|m| !m.content.is_empty())
            .collect();

        // Wire cache_edits into the API request.
        // When the compaction system has cleared old tool results, we send their
        // tool_use_ids as `cache_edits` to the Anthropic API so the server can
        // delete those results from its cached prefix without invalidating the
        // global cache. Requires the `context-management-2025-06-27` beta header.
        let has_cache_edits = !params.cache_edits.is_empty();
        if has_cache_edits {
            if let Some(last_user) = message_params
                .iter_mut()
                .rev()
                .find(|m| m.role == Role::User)
            {
                last_user.content.push(ContentBlock::CacheEdits {
                    cache_edits: params
                        .cache_edits
                        .iter()
                        .map(|id| CacheEdit::DeleteToolResult {
                            tool_use_id: id.clone(),
                        })
                        .collect(),
                });
            }
        }

        let anthropic_builtins: Vec<BuiltinTool> = params
            .provider_search
            .map(|search| BuiltinTool::WebSearch {
                name: PROVIDER_SEARCH_TOOL.into(),
                allowed_domains: None,
                blocked_domains: None,
                max_uses: Some(search.max_uses),
            })
            .into_iter()
            .collect();
        let mut betas: Vec<String> = Vec::new();
        if params.provider_search.is_some() {
            betas.push("web-search-20250305-2025-03-05".into());
        }
        if has_cache_edits {
            betas.push("context-management-2025-06-27".into());
        }
        let anthropic_tools: Vec<crate::types::ToolDef> = tools
            .into_iter()
            .map(|t| crate::types::ToolDef {
                name: t.name,
                description: t.description,
                input_schema: t.input_schema,
                cache_control: None,
                defer_loading: None,
                strict: None,
            })
            .collect();

        let thinking = match params.thinking_mode {
            ThinkingMode::Auto => None,
            ThinkingMode::Off => Some(ThinkingConfig::Disabled),
            ThinkingMode::On => Some(ThinkingConfig::Adaptive),
            ThinkingMode::OnBudget(n) => Some(ThinkingConfig::Enabled { budget_tokens: n }),
        };

        // A call that is only a search has nothing else to do, so it is told
        // to search rather than left to decide whether to.
        let tool_choice =
            (params.provider_search.is_some() && anthropic_tools.is_empty()).then(|| {
                crate::types::ToolChoice::Tool {
                    name: PROVIDER_SEARCH_TOOL.into(),
                }
            });
        let mut req = MessagesRequest {
            model: params.model,
            max_tokens: params.max_tokens,
            system,
            messages: message_params,
            tools: anthropic_tools,
            anthropic_tools: anthropic_builtins,
            tool_choice,
            stream: true,
            thinking,
            effort: params.effort,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: vec![],
            metadata: None,
            betas,
            speed: None,
        };

        // The request places prompt-cache breakpoints on the tool array
        // and on the conversation prefix during serialization (see
        // `MessagesRequest`'s breakpoint helpers). Some of those use the
        // 1-hour TTL, as do the `system` blocks built from
        // `PromptBlock.cache_strategy` above — so the beta flag that gates it
        // has to be decided after the request is assembled, not while the
        // `betas` vec is being built.
        req.ensure_cache_ttl_beta();

        // The client sends the request on the first read, so this is where a
        // refusal — a status, a connection that never opened — is waited
        // for, and returned from here, where a caller's recovery looks.
        let mut raw = self.inner.stream_messages(req);
        let first = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(ModelError::Cancelled),
            first = raw.next() => first,
        };
        let first = match first {
            Some(Ok(crate::stream::StreamEvent::Error { error })) => return Err(api_error(error)),
            Some(Ok(event)) => event,
            Some(Err(e)) => return Err(map_error(e)),
            None => {
                return Err(ModelError::Network(
                    "the provider closed the response without sending anything".into(),
                ))
            }
        };
        // `scan` carries the per-block tool_use JSON accumulator across raw
        // events (needed because one incoming `StreamEvent` can produce
        // zero or several `ModelEvent`s — see `map_stream_event`'s doc
        // comment); `flat_map` then flattens each step's `Vec` back into a
        // flat event stream.
        let mapped = futures::stream::once(futures::future::ready(Ok(first)))
            .chain(raw)
            .scan(StreamState::default(), |acc, result| {
                let events = match result {
                    Ok(event) => map_stream_event(event, acc),
                    Err(e) => vec![Err(map_error(e))],
                };
                futures::future::ready(Some(events))
            })
            .flat_map(futures::stream::iter);
        Ok(Box::new(Box::pin(crate::cancel::until_cancelled(
            mapped, cancel,
        ))))
    }
}

/// What one message's events have said so far.
///
/// `tool_uses` buffers each `tool_use` block's `id`/`name`
/// plus its still-streaming JSON input, keyed by content-block index (a
/// single message can have several interleaved with text blocks at different
/// indices); an entry is removed once its `ContentBlockStop` arrives and the
/// accumulated JSON has been parsed into the final `ModelEvent::ToolUse`.
///
/// `input_tokens` is here for the same reason: what a call cost is reported
/// in two halves, and the halves arrive in different events.
#[derive(Default)]
struct StreamState {
    tool_uses: std::collections::HashMap<u32, PendingToolUse>,
    opening_usage: base::interface::model::Usage,
}

struct PendingToolUse {
    id: String,
    name: String,
    json: String,
}

/// Translate one protocol-agnostic message into its Anthropic wire form.
///
/// A free function rather than a closure inside `stream()` so the block
/// mapping is directly testable — the thinking-block half of it is a
/// correctness requirement, not a formatting detail.
fn to_message_param(m: ModelMessage, provenance: Option<&str>) -> MessageParam {
    MessageParam {
        role: match m.role {
            MessageRole::System | MessageRole::User => Role::User,
            MessageRole::Assistant => Role::Assistant,
        },
        content: m
            .content
            .into_iter()
            .filter(|b| !b.is_foreign_reasoning(provenance))
            .filter_map(to_content_block)
            .collect(),
    }
}

fn to_content_block(b: ModelContentBlock) -> Option<ContentBlock> {
    Some(match b {
        ModelContentBlock::Text { text } => ContentBlock::Text {
            text,
            cache_control: None,
        },
        ModelContentBlock::Image { media_type, data } => ContentBlock::Image {
            source: base::message::ImageSource::Base64 { media_type, data },
        },
        // Echoed back verbatim, signature included. When thinking is enabled
        // and the turn used tools, the API rejects a request whose prior
        // assistant turn is missing these — and rejects one it cannot verify,
        // so a block that arrived without a signature stays in the transcript
        // only.
        ModelContentBlock::Thinking {
            text,
            signature: Some(signature),
            ..
        } => ContentBlock::Thinking {
            thinking: text,
            signature: Some(signature),
            provider: None,
        },
        ModelContentBlock::Thinking {
            signature: None, ..
        } => return None,
        ModelContentBlock::RedactedThinking { data, .. } => ContentBlock::RedactedThinking {
            data,
            provider: None,
        },
        ModelContentBlock::ToolUse { id, name, input } => ContentBlock::ToolUse { id, name, input },
        ModelContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => ContentBlock::ToolResult {
            tool_use_id,
            content: base::message::ToolResultContent::Text(content),
            is_error: is_error.unwrap_or(false),
        },
    })
}

/// The pages in a `web_search_tool_result`'s content — a list of results, or
/// an error object, which found none.
fn search_hits(content: &serde_json::Value) -> Vec<base::interface::model::SearchHit> {
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| {
            Some(base::interface::model::SearchHit {
                title: r
                    .get("title")
                    .and_then(|t| t.as_str())
                    .unwrap_or_default()
                    .to_string(),
                url: r.get("url")?.as_str()?.to_string(),
            })
        })
        .collect()
}

/// Maps one raw provider `StreamEvent` to zero or more `ModelEvent`s.
///
/// Returns a `Vec` because a streamed `tool_use` content block starts empty
/// (`ContentBlockStart` with `input: {}`) and its arguments arrive as a
/// *sequence* of `BlockDelta::InputJsonDelta` fragments that must be
/// concatenated before parsing as JSON. Each block's fragments are buffered
/// in `acc` (keyed by content block index), and the assembled
/// `ModelEvent::ToolUse` is emitted only once `ContentBlockStop` confirms the
/// block is complete.
fn map_stream_event(
    event: crate::stream::StreamEvent,
    acc: &mut StreamState,
) -> Vec<Result<ModelEvent, ModelError>> {
    use crate::stream::{BlockDelta, ContentBlockStart, StreamEvent};
    match event {
        StreamEvent::ContentBlockStart {
            index,
            content_block,
        } => match content_block {
            ContentBlockStart::Text { text } => vec![Ok(ModelEvent::ContentBlockStart {
                index: index as usize,
                block: ModelContentBlock::Text { text },
            })],
            // The provider runs its own search and answers it inside the
            // call; there is nothing for the engine to run.
            ContentBlockStart::ServerToolUse { .. } => vec![],
            ContentBlockStart::WebSearchToolResult { content, .. } => {
                vec![Ok(ModelEvent::SearchResults {
                    hits: search_hits(&content),
                })]
            }
            ContentBlockStart::ToolUse { id, name, input } => {
                // A streamed tool_use block's `ContentBlockStart.input` is a
                // placeholder, not real content — this relay sends `{}` (an
                // empty object), not `null`, so checking `is_null()` alone
                // missed it: an empty-object placeholder got serialized to
                // the literal string "{}" and seeded into the accumulator,
                // and the real `InputJsonDelta` fragments then concatenated
                // onto it — "{}" + "{\"pattern\": \"*.txt\"}" is two JSON
                // values back to back, which fails to parse as one, and fell
                // back to an empty object at `ContentBlockStop`. Confirmed
                // via `AGIX_DEBUG_RAW_STREAM_EVENTS=1` against a real call.
                // Only seed from a genuinely non-empty object/array/scalar —
                // a provider that sends the whole `input` up front (no
                // deltas to follow) still round-trips correctly this way.
                let json = match &input {
                    serde_json::Value::Null => String::new(),
                    serde_json::Value::Object(o) if o.is_empty() => String::new(),
                    _ => input.to_string(),
                };
                let start = ModelEvent::ToolUseStart {
                    id: id.clone(),
                    name: name.clone(),
                };
                acc.tool_uses
                    .insert(index, PendingToolUse { id, name, json });
                vec![Ok(start)]
            }
            // Thinking blocks were parsed by `stream.rs` but died here, in a
            // `_ => vec![]` arm — never reaching `ModelEvent`, so hosts could
            // not render reasoning and, worse, the blocks never made it into
            // the transcript. See `ModelContentBlock::Thinking` for why the
            // transcript half is a correctness issue and not just a UI gap.
            //
            // The opening event usually carries empty text (content arrives as
            // deltas), but a provider may front-load it, so it is forwarded
            // rather than dropped.
            ContentBlockStart::Thinking { thinking } => {
                if thinking.is_empty() {
                    vec![]
                } else {
                    vec![Ok(ModelEvent::ThinkingDelta { text: thinking })]
                }
            }
            ContentBlockStart::RedactedThinking { data } => {
                vec![Ok(ModelEvent::RedactedThinking { data })]
            }
            ContentBlockStart::Unknown => vec![],
        },
        StreamEvent::ContentBlockDelta { index, delta } => match delta {
            BlockDelta::TextDelta { text } => vec![Ok(ModelEvent::TextDelta { text })],
            BlockDelta::ThinkingDelta { thinking } => {
                vec![Ok(ModelEvent::ThinkingDelta { text: thinking })]
            }
            // Sent once, closing the thinking block. Load-bearing: the
            // signature must survive into the transcript or the next request
            // in a tool-use turn is rejected.
            BlockDelta::SignatureDelta { signature } => {
                vec![Ok(ModelEvent::ThinkingSignature { signature })]
            }
            BlockDelta::InputJsonDelta { partial_json } => match acc.tool_uses.get_mut(&index) {
                // The fragment is forwarded *as well as* accumulated: the
                // engine still acts on the assembled `ToolUse` emitted at
                // `ContentBlockStop`, but concatenation is lossy in the one
                // case worth investigating afterwards — arguments that never
                // parsed. Observers see the bytes; nothing else changes.
                Some(pending) => {
                    pending.json.push_str(&partial_json);
                    vec![Ok(ModelEvent::ToolArgsDelta {
                        id: pending.id.clone(),
                        partial_json,
                    })]
                }
                // No corresponding tool_use block open (shouldn't happen for
                // a spec-conformant provider) — nothing sensible to do with
                // an orphan JSON fragment, so it's dropped rather than
                // leaked into the visible text like before.
                None => vec![],
            },
            _ => vec![],
        },
        StreamEvent::ContentBlockStop { index } => {
            let Some(pending) = acc.tool_uses.remove(&index) else {
                return vec![];
            };
            let input = if pending.json.trim().is_empty() {
                serde_json::Value::Object(Default::default())
            } else {
                match serde_json::from_str(&pending.json) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            tool = %pending.name,
                            raw = %pending.json,
                            "failed to parse accumulated tool_use input JSON"
                        );
                        serde_json::Value::Object(Default::default())
                    }
                }
            };
            vec![Ok(ModelEvent::ToolUse {
                id: pending.id,
                name: pending.name,
                input,
            })]
        }
        // The opening event is the only place the input count appears on the
        // real wire — `message_delta` repeats the output count and, usually,
        // nothing else. Kept rather than emitted: a caller wants one usage
        // figure per call, at the end, and that is what `EndTurn` is.
        StreamEvent::MessageStart { message } => {
            acc.opening_usage = Usage {
                input_tokens: message.usage.input_tokens as u32,
                output_tokens: 0,
                cache_creation_input_tokens: message.usage.cache_creation_input_tokens.unwrap_or(0)
                    as u32,
                cache_read_input_tokens: message.usage.cache_read_input_tokens.unwrap_or(0) as u32,
            };
            vec![]
        }
        StreamEvent::MessageDelta { delta, usage } => {
            let stop_reason = delta
                .stop_reason
                .map(|sr| match sr {
                    base::message::StopReason::EndTurn => "end_turn",
                    base::message::StopReason::MaxTokens => "max_tokens",
                    base::message::StopReason::ToolUse => "tool_use",
                    base::message::StopReason::StopSequence => "stop_sequence",
                    base::message::StopReason::PauseTurn => "pause_turn",
                    _ => "unknown",
                })
                .unwrap_or("unknown")
                .to_string();
            let reported = usage.unwrap_or_default();
            // A relay that repeats an input-side count here is the later word
            // on the same call and wins; the usual wire says nothing about
            // any of them here, and the opening event answers instead.
            let later_or_opening = |reported: u64, opening: u32| {
                if reported > 0 {
                    reported as u32
                } else {
                    opening
                }
            };
            vec![Ok(ModelEvent::EndTurn {
                stop_reason,
                usage: Usage {
                    input_tokens: later_or_opening(
                        reported.input_tokens,
                        acc.opening_usage.input_tokens,
                    ),
                    output_tokens: reported.output_tokens as u32,
                    cache_creation_input_tokens: later_or_opening(
                        reported.cache_creation_input_tokens.unwrap_or(0),
                        acc.opening_usage.cache_creation_input_tokens,
                    ),
                    cache_read_input_tokens: later_or_opening(
                        reported.cache_read_input_tokens.unwrap_or(0),
                        acc.opening_usage.cache_read_input_tokens,
                    ),
                },
            })]
        }
        StreamEvent::Error { error } => vec![Err(api_error(error))],
        _ => vec![],
    }
}

/// An `error` event, classified as the same error arriving as an HTTP status
/// would be: its `type` names the status.
fn api_error(error: crate::stream::ApiErrorPayload) -> ModelError {
    let status = match error.kind.as_str() {
        "overloaded_error" => return ModelError::Overloaded,
        "rate_limit_error" => return ModelError::RateLimited,
        "authentication_error" | "permission_error" => return ModelError::Auth(error.message),
        "invalid_request_error" => 400,
        "billing_error" => 402,
        "not_found_error" => 404,
        "request_too_large" => 413,
        "timeout_error" => 504,
        _ => 500,
    };
    ModelError::Api {
        status,
        message: error.message,
    }
}

fn map_error(e: crate::error::AnthropicError) -> ModelError {
    use crate::error::AnthropicError;
    match e {
        AnthropicError::Auth(msg) => ModelError::Auth(msg),
        AnthropicError::RateLimited { .. } => ModelError::RateLimited,
        AnthropicError::Overloaded { .. } => ModelError::Overloaded,
        AnthropicError::Transport(e) => ModelError::Network(e.to_string()),
        e @ AnthropicError::StreamInterrupted => ModelError::Network(e.to_string()),
        AnthropicError::Server { status, body } => ModelError::Api {
            status,
            message: body,
        },
        AnthropicError::Cancelled => ModelError::Cancelled,
        other => ModelError::Internal(other.to_string()),
    }
}

#[cfg(test)]
mod map_stream_event_tests {
    use super::*;
    use crate::stream::{BlockDelta, ContentBlockStart, StreamEvent};

    fn run(events: Vec<StreamEvent>) -> Vec<ModelEvent> {
        let mut acc = StreamState::default();
        events
            .into_iter()
            .flat_map(|e| map_stream_event(e, &mut acc))
            .map(|r| r.expect("no errors in these fixtures"))
            .collect()
    }

    fn end_turn_usage(out: &[ModelEvent]) -> Usage {
        out.iter()
            .find_map(|e| match e {
                ModelEvent::EndTurn { usage, .. } => Some(*usage),
                _ => None,
            })
            .expect("the fixtures all end a message")
    }

    /// The two halves of a usage report arrive in different events: the input
    /// count in `message_start`, the output count in `message_delta`. A
    /// reader of only the second one reports every call as having read
    /// nothing — and input is most of what a call costs.
    ///
    /// The OpenAI-compatible path has carried its own accumulator for this
    /// since it was written (`openai::stream`); this is the same claim on
    /// this side of the parity.
    #[test]
    fn the_input_count_comes_from_message_start_and_the_output_from_message_delta() {
        let out = run(vec![
            StreamEvent::MessageStart {
                message: crate::stream::MessageStartPayload {
                    id: "msg_1".into(),
                    model: "claude-sonnet-4-6".into(),
                    role: "assistant".into(),
                    usage: crate::stream::Usage {
                        input_tokens: 101,
                        output_tokens: 0,
                        cache_creation_input_tokens: None,
                        cache_read_input_tokens: None,
                    },
                    stop_reason: None,
                },
            },
            StreamEvent::MessageDelta {
                delta: crate::stream::MessageDeltaPayload {
                    stop_reason: Some(base::message::StopReason::EndTurn),
                    stop_sequence: None,
                },
                // What the wire really carries here: the output count, and no
                // input count at all.
                usage: Some(crate::stream::Usage {
                    input_tokens: 0,
                    output_tokens: 17,
                    cache_creation_input_tokens: None,
                    cache_read_input_tokens: None,
                }),
            },
        ]);

        let usage = end_turn_usage(&out);
        assert_eq!(usage.input_tokens, 101, "the input count was dropped");
        assert_eq!(usage.output_tokens, 17);
    }

    /// A cached read is billed at a fraction of a fresh one, and a cache
    /// write at a premium. Folding either into `input_tokens` misprices the
    /// call in opposite directions, so both travel as themselves.
    #[test]
    fn the_cache_counts_travel_with_the_input_count() {
        let out = run(vec![
            StreamEvent::MessageStart {
                message: crate::stream::MessageStartPayload {
                    id: "msg_1".into(),
                    model: "m".into(),
                    role: "assistant".into(),
                    usage: crate::stream::Usage {
                        input_tokens: 20,
                        output_tokens: 0,
                        cache_creation_input_tokens: Some(7),
                        cache_read_input_tokens: Some(13),
                    },
                    stop_reason: None,
                },
            },
            StreamEvent::MessageDelta {
                delta: crate::stream::MessageDeltaPayload {
                    stop_reason: Some(base::message::StopReason::EndTurn),
                    stop_sequence: None,
                },
                usage: Some(crate::stream::Usage {
                    input_tokens: 0,
                    output_tokens: 4,
                    cache_creation_input_tokens: None,
                    cache_read_input_tokens: None,
                }),
            },
        ]);

        let usage = end_turn_usage(&out);
        assert_eq!(usage.input_tokens, 20);
        assert_eq!(usage.cache_creation_input_tokens, 7);
        assert_eq!(usage.cache_read_input_tokens, 13);
    }

    /// A relay that repeats the input count in `message_delta` is believed
    /// over the opening one — it is the later word on the same call, and
    /// some Anthropic-compatible endpoints only report it there.
    #[test]
    fn a_final_input_count_wins_over_the_opening_one() {
        let out = run(vec![
            StreamEvent::MessageStart {
                message: crate::stream::MessageStartPayload {
                    id: "msg_1".into(),
                    model: "m".into(),
                    role: "assistant".into(),
                    usage: crate::stream::Usage {
                        input_tokens: 5,
                        output_tokens: 0,
                        cache_creation_input_tokens: None,
                        cache_read_input_tokens: None,
                    },
                    stop_reason: None,
                },
            },
            StreamEvent::MessageDelta {
                delta: crate::stream::MessageDeltaPayload {
                    stop_reason: Some(base::message::StopReason::EndTurn),
                    stop_sequence: None,
                },
                usage: Some(crate::stream::Usage {
                    input_tokens: 900,
                    output_tokens: 3,
                    cache_creation_input_tokens: None,
                    cache_read_input_tokens: None,
                }),
            },
        ]);

        assert_eq!(end_turn_usage(&out).input_tokens, 900);
    }

    /// The assembled tool calls in `out` — what the engine dispatches on.
    /// Argument fragments are forwarded alongside them for observers, so a
    /// test about assembly has to say which of the two it means.
    fn tool_uses(out: &[ModelEvent]) -> Vec<&ModelEvent> {
        out.iter()
            .filter(|e| matches!(e, ModelEvent::ToolUse { .. }))
            .collect()
    }

    /// The raw argument fragments, in arrival order.
    fn arg_fragments(out: &[ModelEvent]) -> Vec<&str> {
        out.iter()
            .filter_map(|e| match e {
                ModelEvent::ToolArgsDelta { partial_json, .. } => Some(partial_json.as_str()),
                _ => None,
            })
            .collect()
    }

    /// Argument fragments must never surface as assistant text. They did once:
    /// raw tool JSON appeared in the user's output while every streamed tool
    /// call reached the dispatcher with an empty input.
    fn assert_no_text_leaked(out: &[ModelEvent]) {
        assert!(
            !out.iter()
                .any(|e| matches!(e, ModelEvent::TextDelta { .. })),
            "tool argument fragments leaked into visible text: {out:?}"
        );
    }

    /// `stream.rs` has always parsed thinking blocks, and `parser.rs` has
    /// contract tests proving it — but this mapping layer dropped them in a
    /// `_ => vec![]` arm, so they never became `ModelEvent`s and never reached
    /// the transcript. That made `thinking + tool_use` turns malformed on the
    /// following request, because the API requires the prior assistant turn's
    /// thinking blocks (with signatures) to be replayed.
    #[test]
    fn thinking_deltas_and_signature_reach_model_events() {
        let events = run(vec![
            StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlockStart::Thinking {
                    thinking: String::new(),
                },
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: BlockDelta::ThinkingDelta {
                    thinking: "let me check ".into(),
                },
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: BlockDelta::ThinkingDelta {
                    thinking: "the config".into(),
                },
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: BlockDelta::SignatureDelta {
                    signature: "sig_abc".into(),
                },
            },
        ]);

        // The empty opening block contributes nothing; the two deltas and the
        // signature come through in order.
        assert_eq!(events.len(), 3, "got {events:?}");
        assert!(matches!(
            &events[0],
            ModelEvent::ThinkingDelta { text } if text == "let me check "
        ));
        assert!(matches!(
            &events[1],
            ModelEvent::ThinkingDelta { text } if text == "the config"
        ));
        assert!(matches!(
            &events[2],
            ModelEvent::ThinkingSignature { signature } if signature == "sig_abc"
        ));
    }

    #[test]
    fn redacted_thinking_is_forwarded_whole() {
        let events = run(vec![StreamEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlockStart::RedactedThinking {
                data: "encrypted==".into(),
            },
        }]);
        assert!(matches!(
            &events[0],
            ModelEvent::RedactedThinking { data } if data == "encrypted=="
        ));
    }

    /// A provider that front-loads the whole thinking text on the opening
    /// event (rather than streaming deltas) must not have it dropped.
    #[test]
    fn non_empty_opening_thinking_block_is_not_dropped() {
        let events = run(vec![StreamEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlockStart::Thinking {
                thinking: "front-loaded".into(),
            },
        }]);
        assert!(matches!(
            &events[0],
            ModelEvent::ThinkingDelta { text } if text == "front-loaded"
        ));
    }

    /// The wire-format layer (`base::message::ContentBlock`) always had Image
    /// and Thinking variants; the protocol-agnostic layer did not, so the
    /// adapter had nothing to map. Now that both exist, check the mapping is
    /// the identity the API expects — in particular that the signature is
    /// carried, since that is the field whose loss breaks the next request,
    /// and that a block without one is not sent at all.
    #[test]
    fn image_and_thinking_blocks_map_to_wire_format() {
        use base::interface::model::{MessageRole, ModelMessage};
        use base::message::{ContentBlock, ImageSource};

        let msgs = vec![ModelMessage {
            role: MessageRole::Assistant,
            content: vec![
                ModelContentBlock::Thinking {
                    text: "reasoning".into(),
                    signature: Some("sig_1".into()),
                    provider: None,
                },
                ModelContentBlock::Thinking {
                    text: "unsigned".into(),
                    signature: None,
                    provider: None,
                },
                ModelContentBlock::RedactedThinking {
                    data: "opaque".into(),
                    provider: None,
                },
                ModelContentBlock::Image {
                    media_type: "image/png".into(),
                    data: "AAAA".into(),
                },
            ],
        }];

        let params = to_message_param(msgs.into_iter().next().unwrap(), None);
        let content = &params.content;
        assert!(matches!(
            &content[0],
            ContentBlock::Thinking { thinking, signature, .. }
                if thinking == "reasoning" && signature.as_deref() == Some("sig_1")
        ));
        assert_eq!(
            content.len(),
            3,
            "the unsigned block is not sent: {content:?}"
        );
        assert!(matches!(
            &content[1],
            ContentBlock::RedactedThinking { data, .. } if data == "opaque"
        ));
        assert!(matches!(
            &content[2],
            ContentBlock::Image {
                source: ImageSource::Base64 { media_type, data }
            } if media_type == "image/png" && data == "AAAA"
        ));
    }

    /// Reasoning another provider produced cannot be verified here; only the
    /// blocks this provider (or an unknown one) produced are sent.
    #[test]
    fn reasoning_from_another_provider_is_not_sent() {
        use base::interface::model::{MessageRole, ModelMessage};
        use base::message::ContentBlock;

        let thinking = |text: &str, provider: Option<&str>| ModelContentBlock::Thinking {
            text: text.into(),
            signature: Some("sig".into()),
            provider: provider.map(Into::into),
        };
        let msg = ModelMessage {
            role: MessageRole::Assistant,
            content: vec![
                thinking("mine", Some("claude")),
                thinking("theirs", Some("deepseek")),
                thinking("unknown", None),
                ModelContentBlock::RedactedThinking {
                    data: "opaque".into(),
                    provider: Some("deepseek".into()),
                },
            ],
        };

        let sent: Vec<String> = to_message_param(msg, Some("claude"))
            .content
            .into_iter()
            .map(|b| match b {
                ContentBlock::Thinking {
                    thinking, provider, ..
                } => {
                    assert_eq!(provider, None, "provenance never reaches the wire");
                    thinking
                }
                other => panic!("unexpected block {other:?}"),
            })
            .collect();
        assert_eq!(sent, ["mine", "unknown"]);
    }

    /// A streamed tool_use block starts with empty `input` and its real
    /// arguments arrive as several `InputJsonDelta` fragments — these must be
    /// concatenated and parsed as one JSON value, emitted as a single
    /// `ModelEvent::ToolUse` at `ContentBlockStop`, not leaked out as
    /// `TextDelta`s with an empty tool input. Seeds with `{}` (empty object),
    /// not `null` — that's what real providers actually send as the
    /// placeholder.
    #[test]
    fn streamed_tool_use_input_is_assembled_from_fragments() {
        let events = vec![
            StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlockStart::ToolUse {
                    id: "call_1".into(),
                    name: "Write".into(),
                    input: serde_json::json!({}),
                },
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: BlockDelta::InputJsonDelta {
                    partial_json: r#"{"file_path": "notes.md", "#.into(),
                },
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: BlockDelta::InputJsonDelta {
                    partial_json: r#""content": "hello"}"#.into(),
                },
            },
            StreamEvent::ContentBlockStop { index: 0 },
        ];

        let out = run(events);
        assert_no_text_leaked(&out);
        assert_eq!(
            arg_fragments(&out),
            vec![r#"{"file_path": "notes.md", "#, r#""content": "hello"}"#],
            "fragments must be forwarded verbatim, unjoined"
        );
        let uses = tool_uses(&out);
        assert_eq!(
            uses.len(),
            1,
            "fragments must assemble into exactly one call"
        );
        match uses[0] {
            ModelEvent::ToolUse { id, name, input } => {
                assert_eq!(id, "call_1");
                assert_eq!(name, "Write");
                assert_eq!(input["file_path"], "notes.md");
                assert_eq!(input["content"], "hello");
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    /// An empty-object `{}` placeholder in `ContentBlockStart` must NOT be
    /// serialized and prepended to the accumulator — that produces
    /// `"{}" + "{\"pattern\": \"*.txt\"}"`, two JSON values back to back,
    /// which fails to parse as one and silently degrades to an empty input.
    /// Checking only `input.is_null()` is not enough: relays send `{}`.
    #[test]
    fn empty_object_placeholder_in_content_block_start_does_not_corrupt_deltas() {
        let events = vec![
            StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlockStart::ToolUse {
                    id: "call_glob".into(),
                    name: "Glob".into(),
                    input: serde_json::json!({}),
                },
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: BlockDelta::InputJsonDelta {
                    partial_json: r#"{"pattern": "*.txt"}"#.into(),
                },
            },
            StreamEvent::ContentBlockStop { index: 0 },
        ];

        let out = run(events);
        assert_no_text_leaked(&out);
        match tool_uses(&out)[0] {
            ModelEvent::ToolUse { input, .. } => assert_eq!(input["pattern"], "*.txt"),
            other => panic!("expected ToolUse with pattern, got {other:?}"),
        }
    }

    /// A provider that sends the whole `input` up front in `ContentBlockStart`
    /// (no deltas at all) must still round-trip — the accumulator seeds from
    /// that non-empty starting JSON.
    #[test]
    fn tool_use_with_input_sent_whole_in_content_block_start_still_works() {
        let events = vec![
            StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlockStart::ToolUse {
                    id: "call_2".into(),
                    name: "Read".into(),
                    input: serde_json::json!({"file_path": "a.rs"}),
                },
            },
            StreamEvent::ContentBlockStop { index: 0 },
        ];

        let out = run(events);
        assert!(
            matches!(&out[0], ModelEvent::ToolUseStart { id, name } if id == "call_2" && name == "Read"),
            "the call is announced as soon as its block opens: {out:?}"
        );
        match &out[1] {
            ModelEvent::ToolUse { input, .. } => assert_eq!(input["file_path"], "a.rs"),
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    /// Two tool_use blocks streaming concurrently at different indices (a
    /// model can emit several tool calls in one response) must not cross-
    /// contaminate each other's accumulated JSON.
    #[test]
    fn interleaved_tool_use_blocks_at_different_indices_stay_isolated() {
        let events = vec![
            StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlockStart::ToolUse {
                    id: "call_a".into(),
                    name: "Glob".into(),
                    input: serde_json::Value::Null,
                },
            },
            StreamEvent::ContentBlockStart {
                index: 1,
                content_block: ContentBlockStart::ToolUse {
                    id: "call_b".into(),
                    name: "Grep".into(),
                    input: serde_json::Value::Null,
                },
            },
            StreamEvent::ContentBlockDelta {
                index: 1,
                delta: BlockDelta::InputJsonDelta {
                    partial_json: r#"{"pattern": "foo"}"#.into(),
                },
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: BlockDelta::InputJsonDelta {
                    partial_json: r#"{"pattern": "*.rs"}"#.into(),
                },
            },
            StreamEvent::ContentBlockStop { index: 1 },
            StreamEvent::ContentBlockStop { index: 0 },
        ];

        let out = run(events);
        assert_no_text_leaked(&out);
        let uses = tool_uses(&out);
        assert_eq!(uses.len(), 2);
        match uses[0] {
            ModelEvent::ToolUse { id, name, input } => {
                assert_eq!(id, "call_b");
                assert_eq!(name, "Grep");
                assert_eq!(input["pattern"], "foo");
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
        match uses[1] {
            ModelEvent::ToolUse { id, name, input } => {
                assert_eq!(id, "call_a");
                assert_eq!(name, "Glob");
                assert_eq!(input["pattern"], "*.rs");
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    /// Plain text streaming is unaffected by the tool_use accumulation
    /// change — deltas still flow straight through as `TextDelta`.
    #[test]
    fn text_delta_streams_unchanged() {
        let events = vec![
            StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlockStart::Text {
                    text: String::new(),
                },
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: BlockDelta::TextDelta {
                    text: "hello ".into(),
                },
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: BlockDelta::TextDelta {
                    text: "world".into(),
                },
            },
            StreamEvent::ContentBlockStop { index: 0 },
        ];

        let out = run(events);
        let text: String = out
            .iter()
            .filter_map(|e| match e {
                ModelEvent::TextDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "hello world");
    }

    /// Some providers send whole text blocks as `ContentBlockStart { Text }`
    /// instead of via deltas, so this emission must survive the tool_use
    /// accumulation.
    #[test]
    fn content_block_start_with_text_is_still_emitted() {
        let events = vec![StreamEvent::ContentBlockStart {
            index: 2,
            content_block: ContentBlockStart::Text {
                text: "whole block text".into(),
            },
        }];

        let out = run(events);
        match &out[0] {
            ModelEvent::ContentBlockStart { index, block } => {
                assert_eq!(*index, 2);
                assert_eq!(
                    block,
                    &ModelContentBlock::Text {
                        text: "whole block text".into()
                    }
                );
            }
            other => panic!("expected ContentBlockStart, got {other:?}"),
        }
    }

    /// Unparseable accumulated JSON must degrade to an empty object instead
    /// of panicking or silently dropping the tool call — the dispatcher
    /// downstream then fails tool input validation with a clear error
    /// instead of the whole turn crashing.
    #[test]
    fn malformed_accumulated_json_falls_back_to_empty_object() {
        let events = vec![
            StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlockStart::ToolUse {
                    id: "call_3".into(),
                    name: "Bash".into(),
                    input: serde_json::Value::Null,
                },
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: BlockDelta::InputJsonDelta {
                    partial_json: "{not valid json".into(),
                },
            },
            StreamEvent::ContentBlockStop { index: 0 },
        ];

        let out = run(events);
        assert_no_text_leaked(&out);
        // The unparseable bytes still reach an observer verbatim — that is the
        // case a recording exists for.
        assert_eq!(arg_fragments(&out), vec!["{not valid json"]);
        match tool_uses(&out)[0] {
            ModelEvent::ToolUse { input, .. } => {
                assert_eq!(input, &serde_json::Value::Object(Default::default()))
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    /// `ContentBlockStop` for an index that was never opened as a tool_use
    /// block (e.g. a text block, or an index the accumulator never saw)
    /// must not emit a spurious event.
    #[test]
    fn content_block_stop_for_untracked_index_emits_nothing() {
        let out = run(vec![StreamEvent::ContentBlockStop { index: 5 }]);
        assert!(out.is_empty());
    }
}

#[cfg(test)]
mod provider_search_tests {
    use super::*;
    use crate::mock::MockAnthropicClient;
    use crate::stream::{ContentBlockStart, StreamEvent};

    fn params(provider_search: Option<base::interface::model::ProviderSearch>) -> StreamParams {
        StreamParams {
            model: "m".into(),
            max_tokens: 100,
            thinking_mode: ThinkingMode::Off,
            effort: None,
            fallback_model: None,
            cache_edits: Vec::new(),
            origin: None,
            input_map: None,
            provider_search,
        }
    }

    fn tool(name: &str) -> ToolDef {
        ToolDef {
            name: name.into(),
            description: String::new(),
            input_schema: serde_json::json!({"type": "object"}),
            source: None,
        }
    }

    async fn sent(
        tools: Vec<ToolDef>,
        provider_search: Option<base::interface::model::ProviderSearch>,
    ) -> MessagesRequest {
        let client = Arc::new(MockAnthropicClient::new());
        client.push_turn(vec![StreamEvent::MessageStop]);
        let model = AnthropicModel::new(client.clone());
        let _ = model
            .stream(
                vec![],
                tools,
                vec![],
                params(provider_search),
                CancellationToken::new(),
            )
            .await
            .expect("the request goes out");
        client.nth_request(0).expect("one request")
    }

    #[tokio::test]
    async fn the_engines_web_search_tool_goes_out_under_its_own_name() {
        let req = sent(vec![tool("Read"), tool("WebSearch")], None).await;
        let names: Vec<&str> = req.tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["Read", "WebSearch"]);
        assert!(
            req.anthropic_tools.is_empty(),
            "no search of the provider's own"
        );
        assert!(req.betas.iter().all(|b| !b.starts_with("web-search")));
        assert!(req.tool_choice.is_none());
    }

    #[tokio::test]
    async fn a_search_call_has_the_provider_search_and_nothing_else() {
        let req = sent(
            vec![],
            Some(base::interface::model::ProviderSearch { max_uses: 3 }),
        )
        .await;
        assert!(matches!(
            req.anthropic_tools.as_slice(),
            [BuiltinTool::WebSearch { name, max_uses: Some(3), .. }] if name == PROVIDER_SEARCH_TOOL
        ));
        assert!(req
            .betas
            .contains(&"web-search-20250305-2025-03-05".to_string()));
        assert!(matches!(
            req.tool_choice,
            Some(crate::types::ToolChoice::Tool { ref name }) if name == PROVIDER_SEARCH_TOOL
        ));
    }

    #[test]
    fn the_providers_search_is_reported_as_results_not_as_a_call_to_run() {
        let mut acc = StreamState::default();
        let out: Vec<ModelEvent> = [
            StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlockStart::ServerToolUse {
                    id: "srvtoolu_1".into(),
                    name: "web_search".into(),
                    input: serde_json::json!({}),
                },
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: crate::stream::BlockDelta::InputJsonDelta {
                    partial_json: "{\"query\":\"rust\"}".into(),
                },
            },
            StreamEvent::ContentBlockStop { index: 0 },
            StreamEvent::ContentBlockStart {
                index: 1,
                content_block: ContentBlockStart::WebSearchToolResult {
                    tool_use_id: "srvtoolu_1".into(),
                    content: serde_json::json!([
                        {"type": "web_search_result", "title": "Rust", "url": "https://rust-lang.org"},
                        {"type": "web_search_result", "url": "https://untitled.example"},
                        {"type": "web_search_result", "title": "no address"}
                    ]),
                },
            },
            StreamEvent::ContentBlockStop { index: 1 },
        ]
        .into_iter()
        .flat_map(|e| map_stream_event(e, &mut acc))
        .map(|r| r.expect("no errors"))
        .collect();
        assert!(
            out.iter().all(|e| !matches!(
                e,
                ModelEvent::ToolUse { .. } | ModelEvent::ToolUseStart { .. }
            )),
            "{out:?}"
        );
        match out.as_slice() {
            [ModelEvent::SearchResults { hits }] => {
                let urls: Vec<&str> = hits.iter().map(|h| h.url.as_str()).collect();
                assert_eq!(
                    urls,
                    ["https://rust-lang.org", "https://untitled.example"],
                    "a page is its address; one without is nothing to show"
                );
            }
            other => panic!("{other:?}"),
        }
    }
}

#[cfg(test)]
mod call_failure_tests {
    use super::*;
    use crate::client::{CountFuture, EventStream};
    use crate::error::AnthropicError;
    use crate::mock::MockAnthropicClient;
    use crate::stream::{ApiErrorPayload, StreamEvent};
    use std::time::Duration;

    fn params() -> StreamParams {
        StreamParams {
            model: "m".into(),
            max_tokens: 100,
            thinking_mode: ThinkingMode::Off,
            effort: None,
            fallback_model: None,
            cache_edits: Vec::new(),
            origin: None,
            input_map: None,
            provider_search: None,
        }
    }

    async fn call(
        model: &AnthropicModel,
        cancel: CancellationToken,
    ) -> Result<ModelStream, ModelError> {
        model.stream(vec![], vec![], vec![], params(), cancel).await
    }

    fn opening() -> StreamEvent {
        StreamEvent::MessageStart {
            message: crate::stream::MessageStartPayload {
                id: "msg".into(),
                model: "m".into(),
                role: "assistant".into(),
                usage: crate::stream::Usage::default(),
                stop_reason: None,
            },
        }
    }

    fn errored(kind: &str, message: &str) -> StreamEvent {
        StreamEvent::Error {
            error: ApiErrorPayload {
                kind: kind.into(),
                message: message.into(),
            },
        }
    }

    /// The client sends on the first read, so a refusal only exists once
    /// something has read; `stream()` is that reader, and the refusal is its
    /// answer rather than the first item of a stream that looked fine.
    #[tokio::test]
    async fn a_refused_request_is_the_calls_error() {
        let client = Arc::new(MockAnthropicClient::new());
        client.push_turn_with_errors(vec![Err(AnthropicError::Overloaded { status: 529 })]);
        client.push_turn_with_errors(vec![Err(AnthropicError::Server {
            status: 400,
            body: "prompt is too long".into(),
        })]);
        let model = AnthropicModel::new(client);
        assert!(matches!(
            call(&model, CancellationToken::new()).await,
            Err(ModelError::Overloaded)
        ));
        assert!(matches!(
            call(&model, CancellationToken::new()).await,
            Err(ModelError::Api { status: 400, .. })
        ));
    }

    /// An `error` event says what kind of failure it is, as a status would
    /// have; flattening every one to a 500 loses the overload a fallback model
    /// is for and the size refusal compaction is for.
    #[tokio::test]
    async fn an_error_event_keeps_its_kind() {
        let client = Arc::new(MockAnthropicClient::new());
        client.push_turn(vec![opening(), errored("overloaded_error", "busy")]);
        client.push_turn(vec![
            opening(),
            errored("invalid_request_error", "prompt is too long: 212034 tokens"),
        ]);
        client.push_turn(vec![errored("rate_limit_error", "slow down")]);
        let model = AnthropicModel::new(client);

        let mut first = call(&model, CancellationToken::new())
            .await
            .expect("opened");
        assert!(matches!(
            first.next().await,
            Some(Err(ModelError::Overloaded))
        ));
        let mut second = call(&model, CancellationToken::new())
            .await
            .expect("opened");
        assert!(matches!(
            second.next().await,
            Some(Err(ModelError::Api { status: 400, message })) if message.contains("prompt is too long")
        ));
        assert!(matches!(
            call(&model, CancellationToken::new()).await,
            Err(ModelError::RateLimited)
        ));
    }

    /// A provider that never answers.
    struct Silent;

    impl crate::client::AnthropicClient for Silent {
        fn stream_messages(&self, _req: MessagesRequest) -> EventStream {
            Box::pin(futures::stream::pending())
        }

        fn count_tokens<'a>(&'a self, _: &'a MessagesRequest) -> CountFuture<'a> {
            Box::pin(async { Ok(0) })
        }
    }

    #[tokio::test]
    async fn a_call_still_waiting_for_its_answer_ends_when_cancelled() {
        let model = AnthropicModel::new(Arc::new(Silent));
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            trigger.cancel();
        });
        let answer = tokio::time::timeout(Duration::from_secs(5), call(&model, cancel))
            .await
            .expect("the cancel was not honoured");
        assert!(matches!(answer, Err(ModelError::Cancelled)));
    }
}
