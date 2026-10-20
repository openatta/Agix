//! OpenAI Chat Completions **streaming** chunk types and their mapping onto
//! the protocol-agnostic [`ModelEvent`] stream.
//!
//! Counterpart to `crate::stream` + the `map_stream_event` half of
//! `crate::adapter`. Kept separate from HTTP so a fixture blob can be fed
//! straight in, the same contract-test style `crate::parser` uses.
//!
//! **The shape difference that drives this whole module.** Anthropic frames a
//! response as explicitly opened and closed content blocks — a `tool_use`
//! block gets a start event, JSON fragments, and a stop event, so "the tool
//! call is complete" is a signal on the wire. OpenAI has no such framing: tool
//! calls arrive as `tool_calls` deltas identified only by an integer `index`,
//! their `arguments` stream in as partial JSON strings, and nothing announces
//! that a given call is finished. The only end-of-call signal is the
//! `finish_reason` on the choice, which closes *all* in-flight calls at once.
//! Hence [`ChunkAccumulator`]: buffer per index, flush on `finish_reason`, and
//! flush again at end-of-stream for servers that omit it.

use base::interface::model::{ModelError, ModelEvent, Usage};
use bytes::Bytes;
use eventsource_stream::Eventsource;
use futures::stream::{Stream, StreamExt};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::time::Duration;

/// The literal payload OpenAI sends to close a stream. It is not JSON, so it
/// has to be recognised before the chunk parser sees it.
const DONE_SENTINEL: &str = "[DONE]";

/// One `data:` payload from the SSE stream. Every field is optional/defaulted:
/// gateways differ on which of them they bother to send, and an unknown extra
/// field must never break the stream (same tolerance as
/// `crate::stream::StreamEvent`'s `#[serde(other)]` fallback).
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct ChatCompletionChunk {
    #[serde(default)]
    pub choices: Vec<ChunkChoice>,
    /// Only present on the final chunk, and only when the request asked for it
    /// via `stream_options.include_usage`.
    #[serde(default)]
    pub usage: Option<ChunkUsage>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct ChunkChoice {
    #[serde(default)]
    pub delta: ChunkDelta,
    /// `stop` | `length` | `tool_calls` | `content_filter` | …
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct ChunkDelta {
    #[serde(default)]
    pub content: Option<String>,
    /// Non-standard but near-universal among reasoning-capable
    /// OpenAI-compatible endpoints (DeepSeek, vLLM, most relays): the
    /// chain-of-thought text, streamed alongside `content`. Mapped to
    /// [`ModelEvent::ThinkingDelta`] so hosts can render reasoning on these
    /// providers instead of dropping it on the floor. Absent on OpenAI proper,
    /// where it simply never fires.
    #[serde(default)]
    pub reasoning_content: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCallDelta>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct ToolCallDelta {
    /// Position of this call within the assistant turn. **This, not `id`, is
    /// the accumulation key** — `id` and `name` are typically sent only on the
    /// first fragment of a call, while `index` is on every one.
    #[serde(default)]
    pub index: u32,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub function: Option<FunctionDelta>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct FunctionDelta {
    #[serde(default)]
    pub name: Option<String>,
    /// A *fragment* of the arguments JSON. Must be concatenated across chunks
    /// before parsing — exactly the trap that
    /// `crate::adapter::map_stream_event` documents for the Anthropic path.
    #[serde(default)]
    pub arguments: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct ChunkUsage {
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
    #[serde(default)]
    pub prompt_tokens_details: Option<PromptTokensDetails>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct PromptTokensDetails {
    /// Tokens served from the provider's automatic prefix cache, forwarded as
    /// `Usage::cache_read_input_tokens` — the same field the Anthropic path
    /// fills from its own cache figures, since a cached read is priced the
    /// same way whichever protocol reported it.
    #[serde(default)]
    pub cached_tokens: Option<u32>,
}

/// Buffers a tool call's `id`/`name` plus its still-streaming arguments JSON,
/// keyed by the delta `index`.
#[derive(Debug, Default)]
struct PendingToolCall {
    id: String,
    name: String,
    arguments: String,
    /// `ToolUseStart` has gone out — it waits for both `id` and `name`, which
    /// a gateway may split across chunks.
    announced: bool,
}

/// Turns a sequence of [`ChatCompletionChunk`]s into [`ModelEvent`]s.
///
/// Stateful by necessity (see the module doc): tool-call fragments and the
/// terminal usage figures arrive across different chunks, so the mapping
/// cannot be a pure per-chunk function the way most of the Anthropic path is.
#[derive(Debug, Default)]
pub struct ChunkAccumulator {
    /// `BTreeMap` rather than `HashMap`: tool calls are emitted in `index`
    /// order so a parallel fan-out reaches the dispatcher in the order the
    /// model wrote it, deterministically.
    pending: BTreeMap<u32, PendingToolCall>,
    stop_reason: Option<String>,
    usage: Usage,
    /// Set once a terminal event has been produced, so a stray trailing chunk
    /// after `finish_reason` cannot emit a second `EndTurn`.
    finished: bool,
}

impl ChunkAccumulator {
    /// Feed one chunk; returns the events it produced (often none).
    pub fn push(&mut self, chunk: ChatCompletionChunk) -> Vec<ModelEvent> {
        let mut out = Vec::new();

        if let Some(u) = chunk.usage {
            self.usage = Usage {
                input_tokens: u.prompt_tokens,
                output_tokens: u.completion_tokens,
                // This protocol has no notion of a cache *write*: the prefix
                // cache is the provider's own and nobody is billed for
                // filling it.
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: u
                    .prompt_tokens_details
                    .and_then(|d| d.cached_tokens)
                    .unwrap_or(0),
            };
        }

        for choice in chunk.choices {
            if let Some(text) = choice.delta.content {
                // Gateways send `"content": ""` on the opening role-only chunk;
                // forwarding it would emit a stream of empty text deltas.
                if !text.is_empty() {
                    out.push(ModelEvent::TextDelta { text });
                }
            }
            if let Some(text) = choice.delta.reasoning_content {
                if !text.is_empty() {
                    out.push(ModelEvent::ThinkingDelta { text });
                }
            }
            for delta in choice.delta.tool_calls {
                let entry = self.pending.entry(delta.index).or_default();
                if let Some(id) = delta.id {
                    entry.id = id;
                }
                let mut args = None;
                if let Some(f) = delta.function {
                    if let Some(name) = f.name {
                        entry.name = name;
                    }
                    args = f.arguments;
                }
                if !entry.announced && !entry.id.is_empty() && !entry.name.is_empty() {
                    entry.announced = true;
                    out.push(ModelEvent::ToolUseStart {
                        id: entry.id.clone(),
                        name: entry.name.clone(),
                    });
                    if !entry.arguments.is_empty() {
                        out.push(ModelEvent::ToolArgsDelta {
                            id: entry.id.clone(),
                            partial_json: entry.arguments.clone(),
                        });
                    }
                }
                if let Some(args) = args.filter(|a| !a.is_empty()) {
                    entry.arguments.push_str(&args);
                    if entry.announced {
                        out.push(ModelEvent::ToolArgsDelta {
                            id: entry.id.clone(),
                            partial_json: args,
                        });
                    }
                }
            }
            if let Some(reason) = choice.finish_reason {
                self.stop_reason = Some(map_finish_reason(&reason));
                // `finish_reason` closes every in-flight tool call at once —
                // there is no per-call stop event to wait for.
                out.extend(self.drain_tool_calls());
            }
        }

        out
    }

    /// Flush anything still buffered and emit the terminal [`ModelEvent::EndTurn`].
    ///
    /// Called once the stream ends (on `[DONE]` or EOF), not on
    /// `finish_reason`: the usage chunk is sent *after* the chunk carrying
    /// `finish_reason`, so emitting `EndTurn` any earlier reports zero tokens.
    pub fn finish(&mut self) -> Vec<ModelEvent> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;

        // Defensive: a server that ends the stream without ever sending
        // `finish_reason` would otherwise silently swallow its own tool calls.
        let mut out = self.drain_tool_calls();

        out.push(ModelEvent::EndTurn {
            // No `finish_reason` at all means the stream ended cleanly without
            // the server saying why — treat that as a normal end of turn
            // rather than a failure, which is how every gateway that omits the
            // field behaves in practice.
            stop_reason: self
                .stop_reason
                .clone()
                .unwrap_or_else(|| "end_turn".to_string()),
            usage: self.usage,
        });
        out
    }

    fn drain_tool_calls(&mut self) -> Vec<ModelEvent> {
        std::mem::take(&mut self.pending)
            .into_values()
            .map(|p| {
                let input = if p.arguments.trim().is_empty() {
                    serde_json::Value::Object(Default::default())
                } else {
                    match serde_json::from_str(&p.arguments) {
                        Ok(v) => v,
                        Err(e) => {
                            // Same degradation as the Anthropic path: hand the
                            // dispatcher an empty input so it fails input
                            // validation with a clear message, rather than
                            // dropping the call or killing the turn.
                            tracing::warn!(
                                error = %e,
                                tool = %p.name,
                                raw = %p.arguments,
                                "failed to parse accumulated tool_call arguments"
                            );
                            serde_json::Value::Object(Default::default())
                        }
                    }
                };
                ModelEvent::ToolUse {
                    id: p.id,
                    name: p.name,
                    input,
                }
            })
            .collect()
    }
}

/// OpenAI `finish_reason` → the Anthropic-flavoured strings the rest of the
/// engine already speaks (`crate::adapter` emits the same vocabulary).
///
/// Unrecognized values are passed through verbatim rather than flattened to
/// `"unknown"`: they end up in traces and hook payloads, and a provider-specific
/// reason like `content_filter` is far more actionable there than a placeholder.
fn map_finish_reason(reason: &str) -> String {
    match reason {
        "stop" => "end_turn",
        "length" => "max_tokens",
        // `function_call` is the pre-`tool_calls` spelling, still emitted by
        // some older compatible gateways.
        "tool_calls" | "function_call" => "tool_use",
        other => other,
    }
    .to_string()
}

/// SSE bytes → [`ModelEvent`] stream. The whole read path in one place,
/// independent of HTTP so a fixture blob can be fed straight in (mirrors
/// `crate::parser::parse_sse`).
///
/// `idle_timeout` bounds the gap *between* events, not the total duration:
/// once response headers are in, nothing else protects against a connection
/// that goes silent mid-stream without closing — the caller would wait
/// forever, at 0% CPU, indistinguishable from a model that is thinking. Same
/// hazard and same remedy as `crate::client::STREAM_IDLE_TIMEOUT`.
///
/// Malformed chunks are logged and skipped rather than failing the stream: a
/// gateway that emits one bad frame mid-response should cost that frame, not
/// the whole turn.
pub fn events_from_bytes<S>(
    byte_stream: S,
    idle_timeout: Duration,
) -> impl Stream<Item = Result<ModelEvent, ModelError>> + Send
where
    S: Stream<Item = Result<Bytes, ModelError>> + Send + 'static,
{
    async_stream::stream! {
        let mut events = Box::pin(byte_stream.eventsource());
        let mut acc = ChunkAccumulator::default();

        loop {
            let next = match tokio::time::timeout(idle_timeout, events.next()).await {
                Ok(next) => next,
                Err(_elapsed) => {
                    yield Err(ModelError::Network(format!(
                        "stream idle for {}s",
                        idle_timeout.as_secs()
                    )));
                    return;
                }
            };
            let Some(item) = next else { break };
            match item {
                Err(e) => {
                    yield Err(ModelError::Network(e.to_string()));
                    return;
                }
                Ok(event) => {
                    let data = event.data.trim();
                    if data.is_empty() {
                        continue;
                    }
                    if data == DONE_SENTINEL {
                        break;
                    }
                    match serde_json::from_str::<ChatCompletionChunk>(data) {
                        Ok(chunk) => {
                            for e in acc.push(chunk) {
                                yield Ok(e);
                            }
                        }
                        Err(e) => tracing::warn!(
                            error = %e,
                            raw = %data,
                            "skipping unparseable chat.completion.chunk"
                        ),
                    }
                }
            }
        }

        for e in acc.finish() {
            yield Ok(e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn run(chunks: &[serde_json::Value]) -> Vec<ModelEvent> {
        let mut acc = ChunkAccumulator::default();
        let mut out: Vec<ModelEvent> = chunks
            .iter()
            .flat_map(|c| acc.push(serde_json::from_value(c.clone()).expect("valid fixture")))
            .collect();
        out.extend(acc.finish());
        out
    }

    fn collected_text(events: &[ModelEvent]) -> String {
        events
            .iter()
            .filter_map(|e| match e {
                ModelEvent::TextDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    /// The prefix cache count is part of what the call cost, not a detail of
    /// how the provider served it: it is priced separately and has to reach
    /// the same place the other counts do.
    #[test]
    fn a_cached_prompt_read_reaches_end_turn() {
        let events = run(&[
            json!({"choices": [{"index": 0, "delta": {"content": "hi"}, "finish_reason": "stop"}]}),
            json!({"choices": [], "usage": {
                "prompt_tokens": 300,
                "completion_tokens": 9,
                "prompt_tokens_details": {"cached_tokens": 256}
            }}),
        ]);

        match events.last().expect("EndTurn") {
            ModelEvent::EndTurn { usage, .. } => {
                assert_eq!(usage.input_tokens, 300);
                assert_eq!(
                    usage.cache_read_input_tokens, 256,
                    "the cached read was dropped"
                );
            }
            other => panic!("expected EndTurn, got {other:?}"),
        }
    }

    /// Text deltas concatenate, the role-only opening chunk contributes
    /// nothing, and usage from the trailing chunk lands on `EndTurn` — even
    /// though it arrives *after* the chunk carrying `finish_reason`.
    #[test]
    fn text_stream_with_trailing_usage_chunk() {
        let events = run(&[
            json!({"choices": [{"index": 0, "delta": {"role": "assistant", "content": ""}, "finish_reason": null}]}),
            json!({"choices": [{"index": 0, "delta": {"content": "Hello"}}]}),
            json!({"choices": [{"index": 0, "delta": {"content": " world"}}]}),
            json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
            json!({"choices": [], "usage": {"prompt_tokens": 101, "completion_tokens": 17}}),
        ]);

        assert_eq!(collected_text(&events), "Hello world");
        match events.last().expect("EndTurn") {
            ModelEvent::EndTurn { stop_reason, usage } => {
                assert_eq!(stop_reason, "end_turn");
                assert_eq!(usage.input_tokens, 101);
                assert_eq!(usage.output_tokens, 17);
            }
            other => panic!("expected EndTurn, got {other:?}"),
        }
    }

    /// The core OpenAI-specific hazard: a tool call whose `id`/`name` appear
    /// only on the first fragment and whose arguments arrive as partial JSON
    /// across several chunks. Must collapse into exactly one `ToolUse` with
    /// the whole input parsed.
    #[test]
    fn tool_call_assembled_across_chunks() {
        let events = run(&[
            json!({"choices": [{"index": 0, "delta": {"role": "assistant", "content": null}}]}),
            json!({"choices": [{"index": 0, "delta": {"tool_calls": [
                {"index": 0, "id": "call_abc", "type": "function",
                 "function": {"name": "Write", "arguments": ""}}
            ]}}]}),
            json!({"choices": [{"index": 0, "delta": {"tool_calls": [
                {"index": 0, "function": {"arguments": "{\"file_path\": \"notes.md\", "}}
            ]}}]}),
            json!({"choices": [{"index": 0, "delta": {"tool_calls": [
                {"index": 0, "function": {"arguments": "\"content\": \"hello\"}"}}
            ]}}]}),
            json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
            json!({"choices": [], "usage": {"prompt_tokens": 42, "completion_tokens": 9}}),
        ]);

        let tool_uses: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, ModelEvent::ToolUse { .. }))
            .collect();
        assert_eq!(
            tool_uses.len(),
            1,
            "fragments must collapse into one event: {events:?}"
        );
        match tool_uses[0] {
            ModelEvent::ToolUse { id, name, input } => {
                assert_eq!(id, "call_abc");
                assert_eq!(name, "Write");
                assert_eq!(input["file_path"], "notes.md");
                assert_eq!(input["content"], "hello");
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
        match events.last().expect("EndTurn") {
            ModelEvent::EndTurn { stop_reason, usage } => {
                assert_eq!(stop_reason, "tool_use");
                assert_eq!(usage.output_tokens, 9);
            }
            other => panic!("expected EndTurn, got {other:?}"),
        }
    }

    /// Parallel tool calls interleave their fragments; `index` — not `id`,
    /// which most fragments omit — keeps them apart, and they come out in
    /// index order.
    #[test]
    fn parallel_tool_calls_stay_isolated_and_ordered() {
        let events = run(&[
            json!({"choices": [{"index": 0, "delta": {"tool_calls": [
                {"index": 1, "id": "call_b", "function": {"name": "Grep", "arguments": ""}},
                {"index": 0, "id": "call_a", "function": {"name": "Glob", "arguments": ""}}
            ]}}]}),
            json!({"choices": [{"index": 0, "delta": {"tool_calls": [
                {"index": 1, "function": {"arguments": "{\"pattern\": \"foo\"}"}}
            ]}}]}),
            json!({"choices": [{"index": 0, "delta": {"tool_calls": [
                {"index": 0, "function": {"arguments": "{\"pattern\": \"*.rs\"}"}}
            ]}}]}),
            json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
        ]);

        let uses: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                ModelEvent::ToolUse { id, name, input } => {
                    Some((id.as_str(), name.as_str(), input))
                }
                _ => None,
            })
            .collect();
        assert_eq!(uses.len(), 2);
        assert_eq!(uses[0].0, "call_a");
        assert_eq!(uses[0].1, "Glob");
        assert_eq!(uses[0].2["pattern"], "*.rs");
        assert_eq!(uses[1].0, "call_b");
        assert_eq!(uses[1].1, "Grep");
        assert_eq!(uses[1].2["pattern"], "foo");
    }

    /// `reasoning_content` is how DeepSeek/vLLM-class endpoints stream
    /// chain-of-thought; it must surface as thinking, not as visible text.
    #[test]
    fn reasoning_content_maps_to_thinking_delta() {
        let events = run(&[
            json!({"choices": [{"index": 0, "delta": {"reasoning_content": "let me check "}}]}),
            json!({"choices": [{"index": 0, "delta": {"reasoning_content": "the config"}}]}),
            json!({"choices": [{"index": 0, "delta": {"content": "Done."}, "finish_reason": "stop"}]}),
        ]);

        let thinking: String = events
            .iter()
            .filter_map(|e| match e {
                ModelEvent::ThinkingDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(thinking, "let me check the config");
        assert_eq!(collected_text(&events), "Done.");
    }

    #[test]
    fn finish_reasons_map_to_the_engine_vocabulary() {
        assert_eq!(map_finish_reason("stop"), "end_turn");
        assert_eq!(map_finish_reason("length"), "max_tokens");
        assert_eq!(map_finish_reason("tool_calls"), "tool_use");
        assert_eq!(map_finish_reason("function_call"), "tool_use");
        // Passed through so it stays visible in traces.
        assert_eq!(map_finish_reason("content_filter"), "content_filter");
    }

    /// `prompt_tokens_details.cached_tokens` parses, and the surrounding usage
    /// still reaches `EndTurn`. (The cached count itself has nowhere to go —
    /// see [`PromptTokensDetails`].)
    #[test]
    fn cached_tokens_parse_without_disturbing_usage() {
        let events = run(&[
            json!({"choices": [{"index": 0, "delta": {"content": "hi"}, "finish_reason": "stop"}]}),
            json!({"choices": [], "usage": {
                "prompt_tokens": 1000, "completion_tokens": 20, "total_tokens": 1020,
                "prompt_tokens_details": {"cached_tokens": 896}
            }}),
        ]);
        match events.last().expect("EndTurn") {
            ModelEvent::EndTurn { usage, .. } => {
                assert_eq!(usage.input_tokens, 1000);
                assert_eq!(usage.output_tokens, 20);
            }
            other => panic!("expected EndTurn, got {other:?}"),
        }
    }

    /// A stream that ends without `finish_reason` (some relays just stop)
    /// must still flush buffered tool calls and terminate the turn.
    #[test]
    fn stream_ending_without_finish_reason_still_flushes_and_ends() {
        let events = without_progress(run(&[
            json!({"choices": [{"index": 0, "delta": {"tool_calls": [
                {"index": 0, "id": "c1", "function": {"name": "Read", "arguments": "{\"file_path\":\"a\"}"}}
            ]}}]}),
        ]));

        assert!(
            matches!(events[0], ModelEvent::ToolUse { .. }),
            "got {events:?}"
        );
        match &events[1] {
            ModelEvent::EndTurn { stop_reason, .. } => assert_eq!(stop_reason, "end_turn"),
            other => panic!("expected EndTurn, got {other:?}"),
        }
    }

    /// Unparseable accumulated arguments degrade to an empty object rather
    /// than panicking or dropping the call — same contract as the Anthropic
    /// path, so the dispatcher reports a clean validation failure.
    #[test]
    fn malformed_arguments_fall_back_to_empty_object() {
        let events = run(&[
            json!({"choices": [{"index": 0, "delta": {"tool_calls": [
                {"index": 0, "id": "c", "function": {"name": "Bash", "arguments": "{not valid"}}
            ]}}]}),
            json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
        ]);
        let events = without_progress(events);
        match &events[0] {
            ModelEvent::ToolUse { input, .. } => {
                assert_eq!(input, &serde_json::Value::Object(Default::default()))
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    fn without_progress(events: Vec<ModelEvent>) -> Vec<ModelEvent> {
        events
            .into_iter()
            .filter(|e| {
                !matches!(
                    e,
                    ModelEvent::ToolUseStart { .. } | ModelEvent::ToolArgsDelta { .. }
                )
            })
            .collect()
    }

    /// A gateway may send a call's id, its name and its arguments in separate
    /// chunks. The call is announced once both id and name are known, with
    /// whatever arguments were already buffered, and every later fragment
    /// follows as it arrives.
    #[test]
    fn a_call_is_announced_once_its_id_and_name_are_both_known() {
        let events = run(&[
            json!({"choices": [{"index": 0, "delta": {"tool_calls": [
                {"index": 0, "id": "c9", "function": {"arguments": "{\"pa"}}
            ]}}]}),
            json!({"choices": [{"index": 0, "delta": {"tool_calls": [
                {"index": 0, "function": {"name": "Glob", "arguments": "ttern\":"}}
            ]}}]}),
            json!({"choices": [{"index": 0, "delta": {"tool_calls": [
                {"index": 0, "function": {"arguments": "\"*\"}"}}
            ]}}]}),
            json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
        ]);
        let progress: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                ModelEvent::ToolUseStart { id, name } => Some(format!("start {id} {name}")),
                ModelEvent::ToolArgsDelta { partial_json, .. } => {
                    Some(format!("args {partial_json}"))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            progress,
            [
                "start c9 Glob",
                "args {\"pa",
                "args ttern\":",
                "args \"*\"}"
            ]
        );
        assert!(events
            .iter()
            .any(|e| matches!(e, ModelEvent::ToolUse { input, .. } if input["pattern"] == "*")));
    }

    /// Unknown fields anywhere in the payload must not break deserialization —
    /// gateways add their own, and a hard failure here kills the whole turn.
    #[test]
    fn unknown_fields_are_ignored() {
        let chunk: ChatCompletionChunk = serde_json::from_value(json!({
            "id": "chatcmpl-1", "object": "chat.completion.chunk",
            "created": 1, "model": "gpt-4o", "system_fingerprint": "fp_x",
            "choices": [{"index": 0, "delta": {"content": "ok", "refusal": null},
                         "logprobs": null, "finish_reason": "stop"}],
            "some_future_field": {"nested": true}
        }))
        .unwrap();
        assert_eq!(chunk.choices[0].delta.content.as_deref(), Some("ok"));
        assert_eq!(chunk.choices[0].finish_reason.as_deref(), Some("stop"));
    }

    /// A second terminal call must not emit a duplicate `EndTurn` — the
    /// engine loop treats it as the end of the turn.
    #[test]
    fn finish_is_idempotent() {
        let mut acc = ChunkAccumulator::default();
        assert_eq!(acc.finish().len(), 1);
        assert!(acc.finish().is_empty());
    }

    // ── End-to-end SSE contract ──
    //
    // Everything above tests the chunk→event mapping with already-parsed
    // fixtures. These drive the full byte path, the way `crate::parser`'s
    // contract test does for the Anthropic wire format.

    fn chunked(blob: &'static [u8], chunk: usize) -> impl Stream<Item = Result<Bytes, ModelError>> {
        futures::stream::iter(
            blob.chunks(chunk)
                .map(|c| Ok(Bytes::copy_from_slice(c)))
                .collect::<Vec<_>>(),
        )
    }

    /// One realistic response covering everything at once: reasoning deltas,
    /// visible text, a tool call assembled across three frames, the
    /// `finish_reason` frame, the trailing usage frame (with cached tokens),
    /// and the `[DONE]` sentinel — split at a byte size that puts SSE line
    /// boundaries mid-chunk.
    #[tokio::test]
    async fn sse_contract_fixture_covers_text_tool_call_and_usage() {
        const FIXTURE: &[u8] = b"\
data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"},\"finish_reason\":null}]}\n\
\n\
data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"need a tool\"}}]}\n\
\n\
data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Reading \"}}]}\n\
\n\
data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"the file.\"}}]}\n\
\n\
data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_01\",\"type\":\"function\",\"function\":{\"name\":\"read_file\",\"arguments\":\"\"}}]}}]}\n\
\n\
data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"path\\\":\"}}]}}]}\n\
\n\
data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"Cargo.toml\\\"}\"}}]}}]}\n\
\n\
data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\
\n\
data: {\"choices\":[],\"usage\":{\"prompt_tokens\":101,\"completion_tokens\":17,\"total_tokens\":118,\"prompt_tokens_details\":{\"cached_tokens\":64}}}\n\
\n\
data: [DONE]\n\
\n\
";
        let events: Vec<ModelEvent> =
            events_from_bytes(chunked(FIXTURE, 23), Duration::from_secs(90))
                .map(|r| r.expect("no errors in this fixture"))
                .collect()
                .await;

        assert_eq!(collected_text(&events), "Reading the file.");
        let thinking: String = events
            .iter()
            .filter_map(|e| match e {
                ModelEvent::ThinkingDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(thinking, "need a tool");

        let uses: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, ModelEvent::ToolUse { .. }))
            .collect();
        assert_eq!(uses.len(), 1, "got {events:?}");
        match uses[0] {
            ModelEvent::ToolUse { id, name, input } => {
                assert_eq!(id, "call_01");
                assert_eq!(name, "read_file");
                assert_eq!(input["path"], "Cargo.toml");
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }

        match events.last().expect("EndTurn") {
            ModelEvent::EndTurn { stop_reason, usage } => {
                assert_eq!(stop_reason, "tool_use");
                assert_eq!(usage.input_tokens, 101);
                assert_eq!(usage.output_tokens, 17);
            }
            other => panic!("expected EndTurn, got {other:?}"),
        }
    }

    /// The `[DONE]` sentinel is not JSON; feeding it to the chunk parser would
    /// log a spurious warning on every single response.
    #[tokio::test]
    async fn done_sentinel_terminates_without_a_parse_error() {
        const FIXTURE: &[u8] = b"\
data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\
\n\
data: [DONE]\n\
\n\
";
        let events: Vec<Result<ModelEvent, ModelError>> =
            events_from_bytes(chunked(FIXTURE, 7), Duration::from_secs(90))
                .collect()
                .await;
        assert!(events.iter().all(|e| e.is_ok()), "got {events:?}");
        let events: Vec<ModelEvent> = events.into_iter().map(Result::unwrap).collect();
        assert_eq!(collected_text(&events), "hi");
        assert!(matches!(events.last(), Some(ModelEvent::EndTurn { .. })));
    }

    /// A connection that goes silent after some output must surface a
    /// retryable network error instead of hanging forever. `start_paused`
    /// makes the 90s wait elapse instantly.
    #[tokio::test(start_paused = true)]
    async fn idle_timeout_fires_when_the_stream_goes_silent() {
        const HEAD: &[u8] =
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"}}]}\n\n";
        let bytes = chunked(HEAD, 64).chain(futures::stream::pending());

        let results: Vec<Result<ModelEvent, ModelError>> =
            events_from_bytes(bytes, Duration::from_secs(90))
                .collect()
                .await;

        assert!(matches!(results[0], Ok(ModelEvent::TextDelta { .. })));
        assert!(
            matches!(results.last(), Some(Err(ModelError::Network(_)))),
            "expected a network error after the idle window, got {results:?}"
        );
    }

    /// One unparseable frame in the middle must not take the turn down with
    /// it — the surrounding text and the terminal event still come through.
    #[tokio::test]
    async fn malformed_frame_is_skipped_not_fatal() {
        const FIXTURE: &[u8] = b"\
data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"a\"}}]}\n\
\n\
data: not-json\n\
\n\
data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"b\"},\"finish_reason\":\"stop\"}]}\n\
\n\
data: [DONE]\n\
\n\
";
        let events: Vec<ModelEvent> =
            events_from_bytes(chunked(FIXTURE, 31), Duration::from_secs(90))
                .map(|r| r.expect("malformed frames are skipped, not surfaced"))
                .collect()
                .await;
        assert_eq!(collected_text(&events), "ab");
        assert!(matches!(events.last(), Some(ModelEvent::EndTurn { .. })));
    }
}
