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
        _cancel: CancellationToken,
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

        // P1-2: Wire cache_edits into the API request.
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

        // DIRECT mode: WebSearch → built-in web_search_20250305
        let has_websearch = tools.iter().any(|t| t.name == "WebSearch");
        let anthropic_builtins: Vec<BuiltinTool> = if has_websearch {
            vec![BuiltinTool::WebSearch {
                name: "web_search".into(),
                allowed_domains: None,
                blocked_domains: None,
                max_uses: None,
            }]
        } else {
            vec![]
        };
        let mut betas: Vec<String> = if has_websearch {
            vec!["web-search-20250305-2025-03-05".into()]
        } else {
            vec![]
        };
        if has_cache_edits {
            betas.push("context-management-2025-06-27".into());
        }
        let anthropic_tools: Vec<crate::types::ToolDef> = tools
            .into_iter()
            .filter(|t| t.name != "WebSearch")
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
            ThinkingMode::On => Some(ThinkingConfig::Enabled {
                budget_tokens: 4096,
            }),
            ThinkingMode::OnBudget(n) => Some(ThinkingConfig::Enabled { budget_tokens: n }),
        };

        let mut req = MessagesRequest {
            model: params.model,
            max_tokens: params.max_tokens,
            system,
            messages: message_params,
            tools: anthropic_tools,
            anthropic_tools: anthropic_builtins,
            tool_choice: None,
            stream: true,
            thinking,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: vec![],
            metadata: None,
            betas,
            speed: None,
        };

        // N-10: the request places prompt-cache breakpoints on the tool array
        // and on the conversation prefix during serialization (see
        // `MessagesRequest`'s breakpoint helpers). Some of those use the
        // 1-hour TTL, as do the `system` blocks built from
        // `PromptBlock.cache_strategy` above — so the beta flag that gates it
        // has to be decided after the request is assembled, not while the
        // `betas` vec is being built.
        req.ensure_cache_ttl_beta();

        let stream = self.inner.stream_messages(req);
        // `scan` carries the per-block tool_use JSON accumulator across raw
        // events (needed because one incoming `StreamEvent` can now produce
        // zero or several `ModelEvent`s — see `map_stream_event`'s doc
        // comment); `flat_map` then flattens each step's `Vec` back into a
        // flat event stream.
        let mapped = stream
            .scan(StreamState::default(), |acc, result| {
                let events = match result {
                    Ok(event) => map_stream_event(event, acc),
                    Err(e) => vec![Err(map_error(e))],
                };
                futures::future::ready(Some(events))
            })
            .flat_map(futures::stream::iter);
        Ok(Box::new(mapped))
    }
}

/// What one message's events have said so far.
///
/// `tool_uses` buffers each `tool_use`/`server_tool_use` block's `id`/`name`
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

/// Maps one raw provider `StreamEvent` to zero or more `ModelEvent`s.
///
/// **Why this returns a `Vec` instead of one event directly (bug fix,
/// 2026-08-06)**: a streamed `tool_use` content block starts empty
/// (`ContentBlockStart` with `input: {}`) and its actual arguments arrive as
/// a *sequence* of `BlockDelta::InputJsonDelta` fragments that must be
/// concatenated before parsing as JSON — `BlockDelta::InputJsonDelta`'s own
/// doc comment already said as much ("tool_use 块的输入 JSON 字符流（碎片，需
/// concat 累积）"), but the accumulation was never actually implemented: the
/// previous version of this function forwarded every `InputJsonDelta`
/// fragment straight out as a `ModelEvent::TextDelta` and emitted
/// `ModelEvent::ToolUse` immediately at `ContentBlockStart` with whatever
/// `input` that start event carried — always `{}` for a genuinely streamed
/// response. Net effect: every tool call whose arguments streamed via deltas
/// (the normal case for any call with non-trivial input) reached the tool
/// dispatcher with an empty `input` object, and the real JSON leaked into the
/// assistant's visible text instead. Confirmed via real recordings
/// against `deepseek-v4-pro[1m]` (Anthropic-compatible relay) while testing
/// the chat/research scenes: 100% of recorded tool calls in both new
/// cassettes, and (re-checked after the fact) 282/282 tool executions in the
/// pre-existing `000.c_project` coding-scene cassette, hit this path — this
/// bug has been present since the initial commit and silently broke every
/// tool call with streamed arguments start to finish.
///
/// Fix: buffer each tool_use block's fragments in `acc` (keyed by content
/// block index) as they arrive, and only emit the assembled
/// `ModelEvent::ToolUse` once `ContentBlockStop` confirms the block is
/// complete and its JSON is whole.
/// Translate one protocol-agnostic message into its Anthropic wire form.
///
/// Extracted from `stream()` (where it was an inline closure) so the block
/// mapping is directly testable — the thinking-block half of it is a
/// correctness requirement, not a formatting detail, and had no coverage while
/// it lived inside an `async fn`.
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
            ContentBlockStart::ToolUse { id, name, input }
            | ContentBlockStart::ServerToolUse { id, name, input } => {
                // A streamed tool_use block's `ContentBlockStart.input` is a
                // placeholder, not real content — this relay sends `{}` (an
                // empty object), not `null`, so checking `is_null()` alone
                // missed it: an empty-object placeholder got serialized to
                // the literal string "{}" and seeded into the accumulator,
                // and the real `InputJsonDelta` fragments then concatenated
                // onto it — "{}" + "{\"pattern\": \"*.txt\"}" is two JSON
                // values back to back, which fails to parse as one, and fell
                // back to an empty object at `ContentBlockStop`. Confirmed
                // via `ATTA_DEBUG_RAW_STREAM_EVENTS=1` against a real call.
                // Only seed from a genuinely non-empty object/array/scalar —
                // a provider that sends the whole `input` up front (no
                // deltas to follow) still round-trips correctly this way.
                let json = match &input {
                    serde_json::Value::Null => String::new(),
                    serde_json::Value::Object(o) if o.is_empty() => String::new(),
                    _ => input.to_string(),
                };
                acc.tool_uses
                    .insert(index, PendingToolUse { id, name, json });
                vec![]
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
            _ => vec![],
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
        StreamEvent::Error { error } => vec![Err(ModelError::Api {
            status: 500,
            message: error.message,
        })],
        _ => vec![],
    }
}

fn map_error(e: crate::error::AnthropicError) -> ModelError {
    use crate::error::AnthropicError;
    match e {
        AnthropicError::Auth(msg) => ModelError::Auth(msg),
        AnthropicError::RateLimited { .. } => ModelError::RateLimited,
        AnthropicError::Overloaded { .. } => ModelError::Overloaded,
        AnthropicError::Transport(e) => ModelError::Network(e.to_string()),
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

    /// Regression for the day-one bug: a streamed tool_use block starts with
    /// empty `input` and its real arguments arrive as several
    /// `InputJsonDelta` fragments — these must be concatenated and parsed as
    /// one JSON value, emitted as a single `ModelEvent::ToolUse` at
    /// `ContentBlockStop`, not leaked out as `TextDelta`s with an empty tool
    /// input (what actually happened before this fix — confirmed against
    /// real recordings, see this module's doc comment). Seeds with `{}`
    /// (empty object), not `null` — that's what real providers actually send
    /// as the placeholder (see the next test for why that distinction
    /// mattered: an earlier version of this fix only special-cased `null`
    /// and broke on `{}`).
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

    /// Regression, found live against a real relay while chasing the bug
    /// above: an empty-object `{}` placeholder in `ContentBlockStart` must
    /// NOT be serialized and prepended to the accumulator — doing so
    /// produces `"{}" + "{\"pattern\": \"*.txt\"}"`, i.e. two JSON values
    /// concatenated back to back, which fails to parse as one and silently
    /// degrades to an empty input — functionally the exact same failure as
    /// the original bug, just one seeding-condition bug away from it. Caught
    /// via `ATTA_DEBUG_RAW_STREAM_EVENTS=1` against `deepseek-v4-pro[1m]`;
    /// the first fix attempt only checked `input.is_null()`, which this
    /// relay's `{}` placeholder never satisfies.
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
        match &out[0] {
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

    /// `llm_classifier.rs` treats `ModelEvent::ContentBlockStart { block:
    /// Text, .. }` as an alternate text source (some providers send whole
    /// text blocks this way instead of via deltas) — this emission must
    /// survive the tool_use accumulation rewrite.
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
