//! Streaming executor — consume one model call's stream, running its tools
//! while it is still arriving.
//!
//! A concurrency-safe tool starts the moment its `tool_use` is complete and
//! runs while the model goes on writing; a non-concurrency-safe one is a
//! barrier that waits for the batch in flight, then runs alone — the stream is
//! still read meanwhile, and what arrives is acted on once it is done. Any
//! tool error cancels its batch's siblings. Each model reply reaches the
//! session as one assistant message plus the one user message answering its
//! tool calls.
//!
//! A call that fails — an error from the provider, a stream that ends before
//! the message does — before its reply asked for a tool leaves nothing behind,
//! so the turn can put the failure to its recovery policy and send the same
//! request again.

use crate::agent::EventSender;
use base::interface::event::AgentEvent;
use base::interface::model::{
    MessageRole, ModelContentBlock, ModelError, ModelEvent, ModelMessage, ModelStream, Usage,
};
use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// What a stream that ended without its message's end is reported as.
pub const INCOMPLETE_STREAM: &str = "the response ended before the model finished it";

/// Result of processing a single model stream.
#[derive(Debug, Clone)]
pub struct StreamResult {
    pub stop_reason: String,
    pub usage: Usage,
    pub has_tool_uses: bool,
    pub tool_calls: u32,
    /// The turn's token fired before the reply was whole. What had arrived is
    /// in the session, every `tool_use` of it answered.
    pub cancelled: bool,
    /// The provider never marked the reply's end ([`ModelEvent::Truncated`]).
    pub truncated: bool,
}

impl StreamResult {
    /// A call cancelled before anything of it arrived.
    pub fn cancelled() -> Self {
        Self {
            stop_reason: "cancelled".into(),
            usage: Usage::default(),
            has_tool_uses: false,
            tool_calls: 0,
            cancelled: true,
            truncated: false,
        }
    }
}

/// A model call that failed partway.
#[derive(Debug)]
pub struct StreamFailure {
    pub error: ModelError,
    /// Nothing of the call reached the session and no tool ran, so the
    /// request that failed can be sent again as it was.
    pub resendable: bool,
}

impl StreamFailure {
    /// A call refused before its stream opened.
    pub fn refused(error: ModelError) -> Self {
        Self {
            error,
            resendable: true,
        }
    }
}

/// Process a model stream with streaming tool execution.
///
/// Concurrency-safe tools start as soon as their `ToolUse` arrives and run
/// while the stream continues. A non-concurrency-safe tool is a barrier: the
/// batch in flight drains first, then it runs alone.
///
/// **What lands in the session**: one model reply is one assistant message —
/// thinking, text and `tool_use` blocks in the order the model produced them —
/// followed by one user message holding every `tool_result`, in submission
/// order. Tools run while the reply is still streaming, but nothing reaches
/// the session until the reply is whole: writing a tool's result between two
/// `tool_use` blocks of the same reply would record a history in which the
/// model saw that result before asking for the next tool, which it did not.
///
/// A reply that fails before it asked for a tool is dropped whole (see
/// [`StreamFailure::resendable`]); one that fails after is written with its
/// results, since those tools have run.
#[allow(clippy::too_many_arguments)]
pub async fn execute_stream<F, Fut, G>(
    stream: ModelStream,
    session: &mut session::session::SessionManager,
    event_tx: &EventSender,
    turn_id: String,
    execute_tool: F,
    is_concurrency_safe: G,
    cancel: CancellationToken,
    interceptors: &[Arc<dyn base::interface::model_interceptor::ModelInterceptor>],
    provenance: Option<&str>,
) -> Result<StreamResult, StreamFailure>
where
    F: Fn(String, String, serde_json::Value) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = base::interface::tool_middleware::ToolOutcome> + Send,
    G: Fn(&str, &serde_json::Value) -> bool + Send + Sync,
{
    use futures::StreamExt;
    let mut stream = stream;
    let mut ended: Option<(String, Usage)> = None;
    let mut has_tool_uses = false;
    let mut tool_calls: u32 = 0;
    let mut failure: Option<ModelError> = None;
    let mut cancelled = false;
    let mut truncated = false;
    // The reply's closed blocks, in arrival order.
    let mut reply: Vec<ModelContentBlock> = Vec::new();
    let mut pending_text = String::new();
    // Text of the thinking block currently open. The Anthropic API closes a
    // block by sending its signature; providers that never sign close it by
    // moving on to anything else, so every other event closes it unsigned.
    let mut pending_thinking = String::new();
    let mut queued_tool_names: Vec<String> = Vec::new();
    let mut tool_index: usize = 0;
    // Key = "(tool_name, serialized_input)".
    let mut seen_tool_calls: HashSet<String> = HashSet::new();

    let mut batch_abort = CancellationToken::new();
    let mut batch = futures::stream::FuturesUnordered::new();

    // `(submission index, block)`: concurrency-safe tools finish in completion
    // order, and the result message is part of the next request, so replay
    // needs the canonical order restored before it is written.
    let mut pending_result_blocks: Vec<(usize, ModelContentBlock)> = Vec::new();
    let mut pending_new_msgs: Vec<Vec<serde_json::Value>> = Vec::new();
    let mut read_ahead = ReadAhead::default();

    loop {
        let next = match read_ahead.events.pop_front() {
            Some(_) if cancel.is_cancelled() => {
                cancelled = true;
                break;
            }
            Some(next) => next,
            // The batch is polled here, beside the stream, or nothing would
            // run it until the stream ended.
            None => tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    cancelled = true;
                    break;
                }
                Some(done) = batch.next(), if !batch.is_empty() => {
                    let (idx, id, result): (usize, String, _) = done;
                    finish_tool(
                        idx,
                        &id,
                        &result,
                        &batch_abort,
                        &queued_tool_names,
                        event_tx,
                        &turn_id,
                        &mut pending_result_blocks,
                        &mut pending_new_msgs,
                    );
                    continue;
                }
                next = stream.next() => next,
            },
        };
        let event = match next {
            Some(Ok(event)) => event,
            Some(Err(e)) => {
                failure = Some(e);
                break;
            }
            None => break,
        };
        match event {
            ModelEvent::TextDelta { text } => {
                close_thinking(&mut pending_thinking, &mut reply, None, provenance);
                pending_text.push_str(&text);
                let _ = event_tx.send(AgentEvent::TextDelta {
                    text,
                    turn_id: turn_id.clone(),
                });
            }
            ModelEvent::ThinkingDelta { text } => {
                close_text(&mut pending_text, &mut reply);
                pending_thinking.push_str(&text);
                let _ = event_tx.send(AgentEvent::ThinkingDelta {
                    text,
                    turn_id: turn_id.clone(),
                });
            }
            ModelEvent::ThinkingSignature { signature } => {
                close_thinking(
                    &mut pending_thinking,
                    &mut reply,
                    Some(signature),
                    provenance,
                );
            }
            ModelEvent::RedactedThinking { data } => {
                close_text(&mut pending_text, &mut reply);
                close_thinking(&mut pending_thinking, &mut reply, None, provenance);
                reply.push(ModelContentBlock::RedactedThinking {
                    data,
                    provider: provenance.map(str::to_owned),
                });
            }
            ModelEvent::ToolUseStart { id, name } => {
                let _ = event_tx.send(AgentEvent::ToolUseStart {
                    id,
                    name,
                    turn_id: turn_id.clone(),
                });
            }
            ModelEvent::ToolArgsDelta { id, partial_json } => {
                let _ = event_tx.send(AgentEvent::ToolArgsDelta {
                    id,
                    partial_json,
                    turn_id: turn_id.clone(),
                });
            }
            ModelEvent::ToolUse { id, name, input } => {
                close_thinking(&mut pending_thinking, &mut reply, None, provenance);
                close_text(&mut pending_text, &mut reply);
                has_tool_uses = true;
                // Every ToolUse the model emitted is recorded, duplicate or
                // not, so each result written below has its `tool_use`.
                reply.push(ModelContentBlock::ToolUse {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                });
                let _ = event_tx.send(AgentEvent::ToolUse {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                    turn_id: turn_id.clone(),
                });

                let idx = tool_index;
                tool_index += 1;
                let safe = is_concurrency_safe(&name, &input);
                queued_tool_names.push(name.clone());

                let dedup_key = format!("({name},{input})");
                if !seen_tool_calls.insert(dedup_key.clone()) {
                    tracing::warn!(
                        tool = %name,
                        tool_use_id = %id,
                        "Skipping duplicate tool call"
                    );
                    buffer_result(
                        event_tx,
                        &turn_id,
                        &mut pending_result_blocks,
                        idx,
                        &id,
                        &name,
                        "[Duplicate tool call skipped — identical call was already made this turn.]",
                        false,
                    );
                    continue;
                }

                if safe {
                    let exec = execute_tool(id.clone(), name.clone(), input);
                    let abort = batch_abort.clone();
                    batch.push(async move {
                        let result = tokio::select! {
                            _ = abort.cancelled() => Err("cancelled by sibling error".to_string()),
                            r = exec => r,
                        };
                        (idx, id, result)
                    });
                } else {
                    let barrier = async {
                        drain_batch(
                            &mut batch,
                            &batch_abort,
                            &queued_tool_names,
                            event_tx,
                            &turn_id,
                            &mut pending_result_blocks,
                            &mut pending_new_msgs,
                        )
                        .await;
                        execute_tool(id.clone(), name.clone(), input).await
                    };
                    let result = read_ahead.during(barrier, &mut stream).await;
                    let (content, new_msgs, failed) = read_outcome(&result);
                    buffer_result(
                        event_tx,
                        &turn_id,
                        &mut pending_result_blocks,
                        idx,
                        &id,
                        &name,
                        &content,
                        failed,
                    );
                    if let Some(msgs) = new_msgs {
                        pending_new_msgs.push(msgs);
                    }
                    batch_abort = CancellationToken::new();
                }

                tool_calls += 1;
            }
            ModelEvent::MalformedToolUse {
                id,
                name,
                raw_input,
            } => {
                close_thinking(&mut pending_thinking, &mut reply, None, provenance);
                close_text(&mut pending_text, &mut reply);
                has_tool_uses = true;
                // The API takes only an object here; what the model actually
                // wrote goes back to it in the result.
                let input = serde_json::Value::Object(Default::default());
                reply.push(ModelContentBlock::ToolUse {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                });
                let _ = event_tx.send(AgentEvent::ToolUse {
                    id: id.clone(),
                    name: name.clone(),
                    input,
                    turn_id: turn_id.clone(),
                });
                let idx = tool_index;
                tool_index += 1;
                queued_tool_names.push(name.clone());
                buffer_result(
                    event_tx,
                    &turn_id,
                    &mut pending_result_blocks,
                    idx,
                    &id,
                    &name,
                    &malformed_call_answer(&raw_input),
                    true,
                );
            }
            ModelEvent::Truncated => truncated = true,
            ModelEvent::EndTurn {
                stop_reason: sr,
                usage: u,
            } => {
                ended = Some((sr, u));
            }
            _ => {}
        }
    }

    match (&failure, &ended) {
        // The message had ended; what broke was only the tail of the
        // connection after it.
        (Some(e), Some(_)) => {
            tracing::warn!(error = %e, "the model's stream failed after its message ended");
            failure = None;
        }
        (None, None) if !cancelled => {
            failure = Some(ModelError::Network(INCOMPLETE_STREAM.into()));
        }
        _ => {}
    }
    if !has_tool_uses {
        if let Some(error) = failure.take() {
            return Err(StreamFailure {
                error,
                resendable: true,
            });
        }
    }

    close_thinking(&mut pending_thinking, &mut reply, None, provenance);
    close_text(&mut pending_text, &mut reply);
    drain_batch(
        &mut batch,
        &batch_abort,
        &queued_tool_names,
        event_tx,
        &turn_id,
        &mut pending_result_blocks,
        &mut pending_new_msgs,
    )
    .await;
    let (stop_reason, usage) = match ended {
        Some(ended) => ended,
        None if cancelled => ("cancelled".to_string(), Usage::default()),
        None => (String::new(), Usage::default()),
    };
    write_reply(
        session,
        reply,
        usage,
        pending_result_blocks,
        pending_new_msgs,
        interceptors,
    );

    if let Some(error) = failure {
        return Err(StreamFailure {
            error,
            resendable: false,
        });
    }
    Ok(StreamResult {
        stop_reason,
        usage,
        has_tool_uses,
        tool_calls,
        cancelled,
        truncated,
    })
}

fn malformed_call_answer(raw_input: &str) -> String {
    const SHOWN: usize = 200;
    let shown = match raw_input.char_indices().nth(SHOWN) {
        Some((end, _)) => format!("{}…", &raw_input[..end]),
        None => raw_input.to_string(),
    };
    format!(
        "This call was not run: its arguments are not complete JSON, most likely \
         because the reply reached its output limit. Call the tool again with \
         complete arguments, splitting large content across several calls if \
         needed. The arguments received began: {shown}"
    )
}

/// What the stream delivered while a barrier tool ran, kept in order for the
/// loop to take before it reads the stream again. Reading on keeps the
/// connection drained while a long tool runs, so a provider never sees its
/// stream stall; nothing read here is acted on before the barrier is done.
#[derive(Default)]
struct ReadAhead {
    /// `None` is the stream's end.
    events: std::collections::VecDeque<Option<Result<ModelEvent, ModelError>>>,
}

impl ReadAhead {
    async fn during<T>(&mut self, work: impl Future<Output = T>, stream: &mut ModelStream) -> T {
        use futures::StreamExt;
        tokio::pin!(work);
        loop {
            let finished = matches!(self.events.back(), Some(None | Some(Err(_))));
            tokio::select! {
                biased;
                out = &mut work => return out,
                next = stream.next(), if !finished => self.events.push_back(next),
            }
        }
    }
}

/// Buffer one finished tool's result. An `Err` (as opposed to a tool
/// reporting its own failure) cancels the siblings still running.
#[allow(clippy::too_many_arguments)]
fn finish_tool(
    idx: usize,
    tool_use_id: &str,
    result: &base::interface::tool_middleware::ToolOutcome,
    batch_abort: &CancellationToken,
    queued_tool_names: &[String],
    event_tx: &EventSender,
    turn_id: &str,
    pending_result_blocks: &mut Vec<(usize, ModelContentBlock)>,
    pending_new_msgs: &mut Vec<Vec<serde_json::Value>>,
) {
    if result.is_err() {
        batch_abort.cancel();
    }
    let (content, new_msgs, failed) = read_outcome(result);
    let tname = queued_tool_names.get(idx).cloned().unwrap_or_default();
    buffer_result(
        event_tx,
        turn_id,
        pending_result_blocks,
        idx,
        tool_use_id,
        &tname,
        &content,
        failed,
    );
    if let Some(msgs) = new_msgs {
        pending_new_msgs.push(msgs);
    }
}

/// Wait for every tool in the current concurrent batch and buffer its result.
#[allow(clippy::too_many_arguments)]
async fn drain_batch<S>(
    batch: &mut S,
    batch_abort: &CancellationToken,
    queued_tool_names: &[String],
    event_tx: &EventSender,
    turn_id: &str,
    pending_result_blocks: &mut Vec<(usize, ModelContentBlock)>,
    pending_new_msgs: &mut Vec<Vec<serde_json::Value>>,
) where
    S: futures::Stream<Item = (usize, String, base::interface::tool_middleware::ToolOutcome)>
        + Unpin,
{
    use futures::StreamExt;
    while let Some((idx, id, result)) = batch.next().await {
        finish_tool(
            idx,
            &id,
            &result,
            batch_abort,
            queued_tool_names,
            event_tx,
            turn_id,
            pending_result_blocks,
            pending_new_msgs,
        );
    }
}

/// One outcome, read the two ways the loop needs it: what the model is shown,
/// and whether that is a failure.
///
/// The second is not `is_err`. A tool that ran and calls its own result a
/// failure — a non-zero exit, an MCP server's `isError`, a trapped guest —
/// belongs on the wire with `is_error: true` and does *not* cancel the
/// siblings sharing its batch, which is what an `Err` here means and what the
/// caller keeps reading `is_err` for.
fn read_outcome(
    outcome: &base::interface::tool_middleware::ToolOutcome,
) -> (String, Option<Vec<serde_json::Value>>, bool) {
    match outcome {
        Ok(a) => (a.text.clone(), a.new_messages.clone(), a.reported_failure),
        Err(e) => (e.clone(), None, true),
    }
}

/// Buffer a tool result (not yet pushed to the session — see `write_reply`)
/// and emit the live `AgentEvent::ToolResult` for UI feedback immediately.
/// `idx` is the tool's submission index, used to restore a deterministic
/// order at write time regardless of real completion order.
#[allow(clippy::too_many_arguments)]
fn buffer_result(
    event_tx: &EventSender,
    turn_id: &str,
    pending_result_blocks: &mut Vec<(usize, ModelContentBlock)>,
    idx: usize,
    tool_use_id: &str,
    tool_name: &str,
    content: &str,
    is_error: bool,
) {
    pending_result_blocks.push((
        idx,
        ModelContentBlock::ToolResult {
            tool_use_id: tool_use_id.to_string(),
            content: content.to_string(),
            is_error: Some(is_error),
        },
    ));
    let _ = event_tx.send(AgentEvent::ToolResult {
        id: tool_use_id.to_string(),
        name: tool_name.to_string(),
        content: content.to_string(),
        is_error: Some(is_error),
        turn_id: turn_id.to_string(),
    });
}

/// A signature arriving with no thinking before it must not produce an empty
/// `Thinking` block, which the API rejects on the next request.
fn close_thinking(
    pending: &mut String,
    reply: &mut Vec<ModelContentBlock>,
    signature: Option<String>,
    provenance: Option<&str>,
) {
    if !pending.is_empty() {
        reply.push(ModelContentBlock::Thinking {
            text: std::mem::take(pending),
            signature,
            provider: provenance.map(str::to_owned),
        });
    }
}

fn close_text(pending: &mut String, reply: &mut Vec<ModelContentBlock>) {
    if !pending.is_empty() {
        reply.push(ModelContentBlock::Text {
            text: std::mem::take(pending),
        });
    }
}

/// Write the finished reply and, if it asked for tools, the one message that
/// answers all of them, then whatever messages the tools asked to inject.
fn write_reply(
    session: &mut session::session::SessionManager,
    reply: Vec<ModelContentBlock>,
    usage: Usage,
    mut results: Vec<(usize, ModelContentBlock)>,
    new_msgs: Vec<Vec<serde_json::Value>>,
    interceptors: &[Arc<dyn base::interface::model_interceptor::ModelInterceptor>],
) {
    if reply.is_empty() {
        return;
    }
    let asked_for_tools = reply
        .iter()
        .any(|b| matches!(b, ModelContentBlock::ToolUse { .. }));
    // The message is whole here, which is the only point at which it is worth
    // showing to anyone: a hook on the deltas that built it would run
    // thousands of times and could produce something that never existed as a
    // coherent message.
    let mut message = ModelMessage {
        role: MessageRole::Assistant,
        content: reply,
    };
    base::interface::model_interceptor::intercept_message(interceptors, &mut message);
    session.push_reply(message, usage);
    if asked_for_tools {
        results.sort_by_key(|(idx, _)| *idx);
        session.push_message(ModelMessage {
            role: MessageRole::User,
            content: results.into_iter().map(|(_, block)| block).collect(),
        });
    }
    for msgs in new_msgs {
        inject_new_messages(session, &msgs);
    }
}

/// Inject new messages into the session after a tool result.
/// Some tools (e.g. SkillTool) return new_messages that should be
/// injected as user messages into the conversation. The model sees these as new
/// instructions rather than as tool output.
fn inject_new_messages(
    session: &mut session::session::SessionManager,
    messages: &[serde_json::Value],
) {
    for msg in messages {
        let role_str = msg.get("role").and_then(|v| v.as_str()).unwrap_or("user");
        let content_str = msg.get("content").and_then(|v| v.as_str()).unwrap_or("");
        let role = match role_str {
            "assistant" => MessageRole::Assistant,
            _ => MessageRole::User,
        };
        session.push_message(ModelMessage {
            role,
            content: vec![ModelContentBlock::Text {
                text: content_str.to_string(),
            }],
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::interface::model::{ModelError, ModelEvent, Usage};
    use base::interface::tool_middleware::ToolAnswer;

    /// Thinking blocks must survive into the transcript, with their
    /// signatures, and must lead the assistant turn: on a
    /// `thinking + tool_use` turn the API requires the previous assistant
    /// turn's thinking blocks to be replayed with signatures intact.
    #[tokio::test]
    async fn thinking_blocks_lead_the_assistant_turn_and_keep_their_signature() {
        let events: Vec<Result<ModelEvent, ModelError>> = vec![
            Ok(ModelEvent::ThinkingDelta {
                text: "I should ".into(),
            }),
            Ok(ModelEvent::ThinkingDelta {
                text: "read the file".into(),
            }),
            Ok(ModelEvent::ThinkingSignature {
                signature: "sig_xyz".into(),
            }),
            Ok(ModelEvent::TextDelta {
                text: "Let me look.".into(),
            }),
            Ok(ModelEvent::ToolUse {
                id: "read_1".into(),
                name: "Read".into(),
                input: serde_json::json!({"file_path": "a.txt"}),
            }),
            Ok(ModelEvent::EndTurn {
                stop_reason: "tool_use".into(),
                usage: Usage::default(),
            }),
        ];
        let stream: base::interface::model::ModelStream = Box::new(futures::stream::iter(events));

        let mut session = session::session::SessionManager::in_memory(None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::event_bus::EventBus::new(tx);

        execute_stream(
            stream,
            &mut session,
            &tx,
            "test-turn".into(),
            |_id, _name, _input| async move { Ok(ToolAnswer::text("ok")) },
            |_name, _input| true,
            CancellationToken::new(),
            &[],
            None,
        )
        .await
        .expect("execute_stream should succeed");

        // The first assistant message carries the thinking block first, then
        // the text — the order the API requires.
        let first_assistant = session
            .messages()
            .iter()
            .find(|m| m.role == MessageRole::Assistant)
            .expect("an assistant message");

        assert!(
            matches!(
                &first_assistant.content[0],
                ModelContentBlock::Thinking { text, signature, .. }
                    if text == "I should read the file" && signature.as_deref() == Some("sig_xyz")
            ),
            "thinking must lead the turn, got {:?}",
            first_assistant.content[0]
        );
        assert!(
            matches!(
                &first_assistant.content[1],
                ModelContentBlock::Text { text } if text == "Let me look."
            ),
            "text must follow thinking, got {:?}",
            first_assistant.content[1]
        );

        assert!(
            matches!(&first_assistant.content[2], ModelContentBlock::ToolUse { id, .. } if id == "read_1"),
            "the tool call belongs to the same reply, got {:?}",
            first_assistant.content
        );
        assert_eq!(session.messages().len(), 2);
        assert_adjacent_tool_pairing(session.messages());
    }

    /// A signature with no accumulated thinking would produce a `Thinking`
    /// block with empty text, which the API rejects on replay. The API does
    /// not send that shape, but a provider shim might.
    #[tokio::test]
    async fn orphan_signature_does_not_create_an_empty_thinking_block() {
        let events: Vec<Result<ModelEvent, ModelError>> = vec![
            Ok(ModelEvent::ThinkingSignature {
                signature: "sig_orphan".into(),
            }),
            Ok(ModelEvent::TextDelta {
                text: "hello".into(),
            }),
            Ok(ModelEvent::EndTurn {
                stop_reason: "end_turn".into(),
                usage: Usage::default(),
            }),
        ];
        let stream: base::interface::model::ModelStream = Box::new(futures::stream::iter(events));

        let mut session = session::session::SessionManager::in_memory(None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::event_bus::EventBus::new(tx);

        execute_stream(
            stream,
            &mut session,
            &tx,
            "test-turn".into(),
            |_id, _name, _input| async move { Ok(ToolAnswer::text("ok")) },
            |_name, _input| true,
            CancellationToken::new(),
            &[],
            None,
        )
        .await
        .expect("execute_stream should succeed");

        for msg in session.messages() {
            for block in &msg.content {
                assert!(
                    !matches!(block, ModelContentBlock::Thinking { .. }),
                    "no thinking block should exist, got {block:?}"
                );
            }
        }
    }

    /// Reasoning from a provider that never signs it (an OpenAI-compatible
    /// endpoint's `reasoning_content`, a relay that strips signatures) is
    /// closed by whatever comes next and kept, unsigned, ahead of the text —
    /// stamped with the provider that produced it.
    #[tokio::test]
    async fn unsigned_thinking_is_kept_ahead_of_the_text() {
        let events: Vec<Result<ModelEvent, ModelError>> = vec![
            Ok(ModelEvent::ThinkingDelta {
                text: "weigh the ".into(),
            }),
            Ok(ModelEvent::ThinkingDelta {
                text: "options".into(),
            }),
            Ok(ModelEvent::TextDelta {
                text: "Done.".into(),
            }),
            Ok(ModelEvent::EndTurn {
                stop_reason: "end_turn".into(),
                usage: Usage::default(),
            }),
        ];
        let stream: base::interface::model::ModelStream = Box::new(futures::stream::iter(events));

        let mut session = session::session::SessionManager::in_memory(None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::event_bus::EventBus::new(tx);

        execute_stream(
            stream,
            &mut session,
            &tx,
            "test-turn".into(),
            |_id, _name, _input| async move { Ok(ToolAnswer::text("ok")) },
            |_name, _input| true,
            CancellationToken::new(),
            &[],
            Some("deepseek"),
        )
        .await
        .expect("execute_stream should succeed");

        let assistant = session
            .messages()
            .iter()
            .find(|m| m.role == MessageRole::Assistant)
            .expect("an assistant message");
        assert_eq!(
            assistant.content,
            vec![
                ModelContentBlock::Thinking {
                    text: "weigh the options".into(),
                    signature: None,
                    provider: Some("deepseek".into()),
                },
                ModelContentBlock::Text {
                    text: "Done.".into(),
                },
            ]
        );
    }

    #[tokio::test]
    async fn a_call_with_broken_arguments_is_answered_with_an_error_and_not_run() {
        let events: Vec<Result<ModelEvent, ModelError>> = vec![
            Ok(ModelEvent::MalformedToolUse {
                id: "call_1".into(),
                name: "Write".into(),
                raw_input: r#"{"file_path": "a.txt", "content": "half"#.into(),
            }),
            Ok(ModelEvent::EndTurn {
                stop_reason: "max_tokens".into(),
                usage: Usage::default(),
            }),
        ];
        let stream: base::interface::model::ModelStream = Box::new(futures::stream::iter(events));
        let mut session = session::session::SessionManager::in_memory(None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::event_bus::EventBus::new(tx);
        let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ran_probe = ran.clone();

        let result = execute_stream(
            stream,
            &mut session,
            &tx,
            "test-turn".into(),
            move |_id, _name, _input| {
                ran_probe.store(true, std::sync::atomic::Ordering::SeqCst);
                async move { Ok(ToolAnswer::text("ran")) }
            },
            |_name, _input| false,
            CancellationToken::new(),
            &[],
            None,
        )
        .await
        .expect("execute_stream should succeed");

        assert!(!ran.load(std::sync::atomic::Ordering::SeqCst));
        assert!(result.has_tool_uses);
        assert_eq!(result.tool_calls, 0);
        assert_adjacent_tool_pairing(session.messages());
        match &session.messages()[1].content[..] {
            [ModelContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            }] => {
                assert_eq!(tool_use_id, "call_1");
                assert_eq!(*is_error, Some(true));
                assert!(content.contains("output limit"), "{content}");
                assert!(content.contains(r#""content": "half"#), "{content}");
            }
            other => panic!("expected one tool_result, got {other:?}"),
        }
    }

    /// When the model emits the same (name, input) tool call twice in one
    /// response, the duplicate-skip branch still answers the duplicate's
    /// `tool_use_id`, so its `ToolUse` must be recorded too: a `tool_result`
    /// with no preceding `tool_use` is rejected by every real provider
    /// (`unexpected tool_use_id found in tool_result blocks`). Every
    /// `ToolUse` the model emits — duplicate or not — must be recorded
    /// before any matching `ToolResult`.
    #[tokio::test]
    async fn duplicate_tool_call_still_gets_a_paired_tool_use_block() {
        let events: Vec<Result<ModelEvent, ModelError>> = vec![
            Ok(ModelEvent::ToolUse {
                id: "call_1".into(),
                name: "Write".into(),
                input: serde_json::json!({"path": "a.txt"}),
            }),
            Ok(ModelEvent::ToolUse {
                id: "call_2".into(),
                name: "Write".into(),
                input: serde_json::json!({"path": "a.txt"}), // identical → duplicate of call_1
            }),
            Ok(ModelEvent::EndTurn {
                stop_reason: "tool_use".into(),
                usage: Usage::default(),
            }),
        ];
        let stream: base::interface::model::ModelStream = Box::new(futures::stream::iter(events));

        let mut session = session::session::SessionManager::in_memory(None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::event_bus::EventBus::new(tx);

        let result = execute_stream(
            stream,
            &mut session,
            &tx,
            "test-turn".into(),
            |_id, _name, _input| async move { Ok(ToolAnswer::text("ok")) },
            |_name, _input| true,
            CancellationToken::new(),
            &[],
            None,
        )
        .await
        .expect("execute_stream should succeed");

        assert!(result.has_tool_uses);
        // Only the real (non-duplicate) execution counts toward tool_calls —
        // the duplicate is skipped before that counter increments. That's
        // fine; the point of this test is the message-pairing invariant below.
        assert_eq!(result.tool_calls, 1);

        assert_adjacent_tool_pairing(session.messages());

        let mut seen_tool_use_ids: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        for msg in session.messages() {
            for block in &msg.content {
                if let ModelContentBlock::ToolUse { id, .. } = block {
                    seen_tool_use_ids.insert(id.clone());
                }
            }
        }
        assert!(seen_tool_use_ids.contains("call_1"));
        assert!(seen_tool_use_ids.contains("call_2"));
    }

    /// One response emits a concurrency-safe tool (Glob) then a
    /// non-concurrency-safe one (Bash) — the mix that forces the "drain
    /// batch then execute sequential" path. The tool_use blocks must be
    /// grouped: if each tool_use/tool_result pair were its own session
    /// message, the second tool's ToolUse would land before the first tool's
    /// ToolResult — the API rejects that (`tool_use ids were found without
    /// tool_result blocks immediately after`).
    #[tokio::test]
    async fn mixed_concurrent_and_sequential_tool_calls_keep_strict_adjacency() {
        let events: Vec<Result<ModelEvent, ModelError>> = vec![
            Ok(ModelEvent::ToolUse {
                id: "glob_1".into(),
                name: "Glob".into(),
                input: serde_json::json!({"pattern": "*.c"}),
            }),
            Ok(ModelEvent::ToolUse {
                id: "bash_1".into(),
                name: "Bash".into(),
                input: serde_json::json!({"command": "make build"}),
            }),
            Ok(ModelEvent::EndTurn {
                stop_reason: "tool_use".into(),
                usage: Usage::default(),
            }),
        ];
        let stream: base::interface::model::ModelStream = Box::new(futures::stream::iter(events));

        let mut session = session::session::SessionManager::in_memory(None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::event_bus::EventBus::new(tx);

        execute_stream(
            stream,
            &mut session,
            &tx,
            "test-turn".into(),
            |_id, _name, _input| async move { Ok(ToolAnswer::text("ok")) },
            |name, _input| name == "Glob", // Glob concurrency-safe, Bash is not
            CancellationToken::new(),
            &[],
            None,
        )
        .await
        .expect("execute_stream should succeed");

        assert_adjacent_tool_pairing(session.messages());
        // Exactly one assistant/user pair, both tool_use ids grouped together.
        assert_eq!(session.messages().len(), 2);
    }

    /// All-concurrency-safe batch (no sequential barrier at all): both tools
    /// only get flushed at the final drain, after the stream ends.
    #[tokio::test]
    async fn all_concurrent_batch_flushes_together_at_stream_end() {
        let events: Vec<Result<ModelEvent, ModelError>> = vec![
            Ok(ModelEvent::ToolUse {
                id: "glob_1".into(),
                name: "Glob".into(),
                input: serde_json::json!({"pattern": "*.c"}),
            }),
            Ok(ModelEvent::ToolUse {
                id: "grep_1".into(),
                name: "Grep".into(),
                input: serde_json::json!({"pattern": "main"}),
            }),
            Ok(ModelEvent::EndTurn {
                stop_reason: "tool_use".into(),
                usage: Usage::default(),
            }),
        ];
        let stream: base::interface::model::ModelStream = Box::new(futures::stream::iter(events));

        let mut session = session::session::SessionManager::in_memory(None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::event_bus::EventBus::new(tx);

        execute_stream(
            stream,
            &mut session,
            &tx,
            "test-turn".into(),
            |_id, _name, _input| async move { Ok(ToolAnswer::text("ok")) },
            |_name, _input| true,
            CancellationToken::new(),
            &[],
            None,
        )
        .await
        .expect("execute_stream should succeed");

        assert_adjacent_tool_pairing(session.messages());
        assert_eq!(session.messages().len(), 2);
    }

    /// Three concurrency-safe tools submitted in order A, B, C but made to
    /// *finish* in the reverse order C, B, A (artificial per-call delays,
    /// simulating real-world scheduling where a fast `Glob` beats a slower
    /// one). The flushed `ToolResult` message must come out A, B, C
    /// (`idx`-sorted at flush time, mirroring `dispatch.rs`): the API doesn't
    /// care about the order, but replay does — a different exact message
    /// makes every later request differ between two runs of the same
    /// conversation.
    #[tokio::test]
    async fn concurrent_tool_results_flush_in_submission_order_not_completion_order() {
        let events: Vec<Result<ModelEvent, ModelError>> = vec![
            Ok(ModelEvent::ToolUse {
                id: "a".into(),
                name: "Glob".into(),
                input: serde_json::json!({"pattern": "*.a"}),
            }),
            Ok(ModelEvent::ToolUse {
                id: "b".into(),
                name: "Glob".into(),
                input: serde_json::json!({"pattern": "*.b"}),
            }),
            Ok(ModelEvent::ToolUse {
                id: "c".into(),
                name: "Glob".into(),
                input: serde_json::json!({"pattern": "*.c"}),
            }),
            Ok(ModelEvent::EndTurn {
                stop_reason: "tool_use".into(),
                usage: Usage::default(),
            }),
        ];
        let stream: base::interface::model::ModelStream = Box::new(futures::stream::iter(events));

        let mut session = session::session::SessionManager::in_memory(None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::event_bus::EventBus::new(tx);

        execute_stream(
            stream,
            &mut session,
            &tx,
            "test-turn".into(),
            |_id, _name, input| async move {
                // Reverse completion order relative to submission: the tool
                // matching "*.a" (submitted first) sleeps longest, "*.c"
                // (submitted last) doesn't sleep at all — so real completion
                // order is c, b, a, the exact opposite of submission order a, b, c.
                let pattern = input["pattern"].as_str().unwrap_or("").to_string();
                let delay_ms: u64 = match pattern.as_str() {
                    "*.a" => 30,
                    "*.b" => 15,
                    _ => 0,
                };
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                Ok(ToolAnswer::text(format!("done:{pattern}")))
            },
            |_name, _input| true,
            CancellationToken::new(),
            &[],
            None,
        )
        .await
        .expect("execute_stream should succeed");

        assert_adjacent_tool_pairing(session.messages());
        let result_msg = &session.messages()[1];
        let ids: Vec<&str> = result_msg
            .content
            .iter()
            .map(|b| match b {
                ModelContentBlock::ToolResult { tool_use_id, .. } => tool_use_id.as_str(),
                other => panic!("expected ToolResult, got {other:?}"),
            })
            .collect();
        assert_eq!(
            ids,
            vec!["a", "b", "c"],
            "tool_result blocks must stay in original submission order regardless of completion timing"
        );
    }

    /// The final concurrent batch of a turn — the common case, since most
    /// turns simply end after their last round of tool calls — is drained
    /// after stream consumption ends, not at a mid-stream barrier, and that
    /// drain must also call `batch_abort.cancel()` on an error ("any tool
    /// error cancels all siblings in the same batch"). Here: a fast-erroring
    /// tool and a slow tool submitted in the same batch — the slow one must
    /// observe cancellation (and return quickly) rather than running to
    /// completion.
    #[tokio::test]
    async fn end_of_stream_drain_cancels_slow_sibling_after_a_fast_error() {
        let events: Vec<Result<ModelEvent, ModelError>> = vec![
            Ok(ModelEvent::ToolUse {
                id: "fails_fast".into(),
                name: "Bash".into(),
                input: serde_json::json!({"command": "false"}),
            }),
            Ok(ModelEvent::ToolUse {
                id: "would_succeed_if_not_cancelled".into(),
                name: "Bash".into(),
                input: serde_json::json!({"command": "sleep 30 && echo done"}),
            }),
            Ok(ModelEvent::EndTurn {
                stop_reason: "tool_use".into(),
                usage: Usage::default(),
            }),
        ];
        let stream: base::interface::model::ModelStream = Box::new(futures::stream::iter(events));

        let mut session = session::session::SessionManager::in_memory(None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::event_bus::EventBus::new(tx);

        let started = std::time::Instant::now();
        execute_stream(
            stream,
            &mut session,
            &tx,
            "test-turn".into(),
            |_id, _name, input| async move {
                if input["command"] == "false" {
                    Err("simulated PostToolUse denial".to_string())
                } else {
                    // Would take 30s if actually run to completion — the
                    // test's own timeout below is the proof it didn't.
                    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                    Ok(ToolAnswer::text("done"))
                }
            },
            |_name, _input| true,
            CancellationToken::new(),
            &[],
            None,
        )
        .await
        .expect("execute_stream should succeed");

        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "the slow sibling should have been cancelled almost immediately after the fast \
             error, not run to its full (simulated) 30s completion — took {:?}",
            started.elapsed()
        );

        assert_adjacent_tool_pairing(session.messages());
        let result_msg = &session.messages()[1];
        let slow_result_is_error = result_msg.content.iter().any(|b| {
            matches!(
                b,
                ModelContentBlock::ToolResult { tool_use_id, is_error, .. }
                    if tool_use_id == "would_succeed_if_not_cancelled" && *is_error == Some(true)
            )
        });
        assert!(
            slow_result_is_error,
            "the slow sibling's tool_result should reflect cancellation (an error), not the \
             success it would have returned had it been allowed to finish: {:?}",
            result_msg.content
        );
    }

    /// Two sequential tools in one reply run one after the other, but the
    /// reply is still one assistant message answered by one user message:
    /// the model asked for both before it saw either result.
    #[tokio::test]
    async fn two_sequential_tool_calls_of_one_reply_share_one_message_pair() {
        let events: Vec<Result<ModelEvent, ModelError>> = vec![
            Ok(ModelEvent::ToolUse {
                id: "bash_1".into(),
                name: "Bash".into(),
                input: serde_json::json!({"command": "make build"}),
            }),
            Ok(ModelEvent::ToolUse {
                id: "bash_2".into(),
                name: "Bash".into(),
                input: serde_json::json!({"command": "make run"}),
            }),
            Ok(ModelEvent::EndTurn {
                stop_reason: "tool_use".into(),
                usage: Usage::default(),
            }),
        ];
        let stream: base::interface::model::ModelStream = Box::new(futures::stream::iter(events));

        let mut session = session::session::SessionManager::in_memory(None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::event_bus::EventBus::new(tx);

        execute_stream(
            stream,
            &mut session,
            &tx,
            "test-turn".into(),
            |_id, _name, _input| async move { Ok(ToolAnswer::text("ok")) },
            |_name, _input| false,
            CancellationToken::new(),
            &[],
            None,
        )
        .await
        .expect("execute_stream should succeed");

        assert_adjacent_tool_pairing(session.messages());
        assert_eq!(session.messages().len(), 2);
    }

    fn block_kinds(m: &ModelMessage) -> Vec<String> {
        m.content
            .iter()
            .map(|b| match b {
                ModelContentBlock::Thinking { text, .. } => format!("thinking:{text}"),
                ModelContentBlock::Text { text } => format!("text:{text}"),
                ModelContentBlock::ToolUse { id, .. } => format!("tool_use:{id}"),
                ModelContentBlock::ToolResult { tool_use_id, .. } => {
                    format!("tool_result:{tool_use_id}")
                }
                other => format!("{other:?}"),
            })
            .collect()
    }

    /// Interleaved thinking: a reply that thinks, calls a tool that has to
    /// run alone, thinks again and calls a second one keeps exactly that
    /// order in one assistant message. The first tool's result is not written
    /// between the two calls even though it finished before the second was
    /// streamed.
    #[tokio::test]
    async fn interleaved_reply_keeps_its_block_order_in_one_message() {
        let events: Vec<Result<ModelEvent, ModelError>> = vec![
            Ok(ModelEvent::ThinkingDelta {
                text: "first".into(),
            }),
            Ok(ModelEvent::ThinkingSignature {
                signature: "s1".into(),
            }),
            Ok(ModelEvent::TextDelta {
                text: "Building.".into(),
            }),
            Ok(ModelEvent::ToolUse {
                id: "a".into(),
                name: "Bash".into(),
                input: serde_json::json!({"command": "make"}),
            }),
            Ok(ModelEvent::ThinkingDelta {
                text: "second".into(),
            }),
            Ok(ModelEvent::ThinkingSignature {
                signature: "s2".into(),
            }),
            Ok(ModelEvent::ToolUse {
                id: "b".into(),
                name: "Glob".into(),
                input: serde_json::json!({"pattern": "*"}),
            }),
            Ok(ModelEvent::EndTurn {
                stop_reason: "tool_use".into(),
                usage: Usage::default(),
            }),
        ];
        let stream: base::interface::model::ModelStream = Box::new(futures::stream::iter(events));

        let mut session = session::session::SessionManager::in_memory(None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::event_bus::EventBus::new(tx);

        execute_stream(
            stream,
            &mut session,
            &tx,
            "test-turn".into(),
            |id, _name, _input| async move { Ok(ToolAnswer::text(format!("done {id}"))) },
            |name, _input| name == "Glob",
            CancellationToken::new(),
            &[],
            None,
        )
        .await
        .expect("execute_stream should succeed");

        let messages = session.messages();
        assert_eq!(messages.len(), 2, "{messages:#?}");
        assert_eq!(
            block_kinds(&messages[0]),
            vec![
                "thinking:first",
                "text:Building.",
                "tool_use:a",
                "thinking:second",
                "tool_use:b"
            ]
        );
        assert_eq!(
            block_kinds(&messages[1]),
            vec!["tool_result:a", "tool_result:b"]
        );
    }

    /// A stream that fails after a tool already ran still leaves the reply
    /// and that tool's result in the transcript, paired, and reports the
    /// failure.
    #[tokio::test]
    async fn a_stream_error_still_writes_what_already_happened() {
        let events: Vec<Result<ModelEvent, ModelError>> = vec![
            Ok(ModelEvent::TextDelta {
                text: "Running.".into(),
            }),
            Ok(ModelEvent::ToolUse {
                id: "a".into(),
                name: "Bash".into(),
                input: serde_json::json!({"command": "make"}),
            }),
            Err(ModelError::Network("connection reset".into())),
        ];
        let stream: base::interface::model::ModelStream = Box::new(futures::stream::iter(events));

        let mut session = session::session::SessionManager::in_memory(None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::event_bus::EventBus::new(tx);

        let result = execute_stream(
            stream,
            &mut session,
            &tx,
            "test-turn".into(),
            |_id, _name, _input| async move { Ok(ToolAnswer::text("built")) },
            |_name, _input| false,
            CancellationToken::new(),
            &[],
            None,
        )
        .await;

        assert!(result.is_err());
        let messages = session.messages();
        assert_eq!(messages.len(), 2, "{messages:#?}");
        assert_eq!(
            block_kinds(&messages[0]),
            vec!["text:Running.", "tool_use:a"]
        );
        assert_adjacent_tool_pairing(messages);
    }

    async fn run_plain(
        stream: base::interface::model::ModelStream,
        session: &mut session::session::SessionManager,
        cancel: CancellationToken,
    ) -> Result<StreamResult, StreamFailure> {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::event_bus::EventBus::new(tx);
        execute_stream(
            stream,
            session,
            &tx,
            "test-turn".into(),
            |_id, _name, _input| async move { Ok(ToolAnswer::text("ok")) },
            |_name, _input| true,
            cancel,
            &[],
            None,
        )
        .await
    }

    /// A reply that breaks off before it asked for a tool has nothing worth
    /// keeping, and keeping half of it would make the retry send it back.
    #[tokio::test]
    async fn a_reply_that_fails_before_asking_for_a_tool_leaves_nothing_behind() {
        let events: Vec<Result<ModelEvent, ModelError>> = vec![
            Ok(ModelEvent::TextDelta {
                text: "half an ans".into(),
            }),
            Err(ModelError::Overloaded),
        ];
        let mut session = session::session::SessionManager::in_memory(None);
        let failure = run_plain(
            Box::new(futures::stream::iter(events)),
            &mut session,
            CancellationToken::new(),
        )
        .await
        .expect_err("an error event is a failure");
        assert!(failure.resendable);
        assert!(matches!(failure.error, ModelError::Overloaded));
        assert!(session.messages().is_empty(), "{:#?}", session.messages());
    }

    /// No `EndTurn` means the message never ended: whatever arrived is a
    /// fragment, with no stop reason and no usage, not an answer.
    #[tokio::test]
    async fn a_stream_that_ends_before_its_message_does_is_a_failure() {
        let mut session = session::session::SessionManager::in_memory(None);
        let failure = run_plain(
            Box::new(futures::stream::iter(vec![Ok(ModelEvent::TextDelta {
                text: "NEVER-WHOLE".into(),
            })])),
            &mut session,
            CancellationToken::new(),
        )
        .await
        .expect_err("a cut stream is a failure");
        assert!(failure.resendable);
        assert!(
            matches!(&failure.error, ModelError::Network(m) if m == INCOMPLETE_STREAM),
            "{:?}",
            failure.error
        );
        assert!(session.messages().is_empty(), "{:#?}", session.messages());

        let failure = run_plain(
            Box::new(futures::stream::iter(vec![Ok(ModelEvent::ToolUse {
                id: "g".into(),
                name: "Glob".into(),
                input: serde_json::json!({"pattern": "*"}),
            })])),
            &mut session,
            CancellationToken::new(),
        )
        .await
        .expect_err("a cut stream is a failure");
        assert!(!failure.resendable, "the tool ran");
        assert_eq!(session.messages().len(), 2, "{:#?}", session.messages());
        assert_adjacent_tool_pairing(session.messages());
    }

    /// The message's end is what makes it whole; a connection that breaks
    /// after it has lost nothing.
    #[tokio::test]
    async fn a_failure_after_the_message_ended_is_not_one() {
        let events: Vec<Result<ModelEvent, ModelError>> = vec![
            Ok(ModelEvent::TextDelta {
                text: "whole".into(),
            }),
            Ok(ModelEvent::EndTurn {
                stop_reason: "end_turn".into(),
                usage: Usage::default(),
            }),
            Err(ModelError::Network("stream interrupted".into())),
        ];
        let mut session = session::session::SessionManager::in_memory(None);
        let done = run_plain(
            Box::new(futures::stream::iter(events)),
            &mut session,
            CancellationToken::new(),
        )
        .await
        .expect("the message had ended");
        assert_eq!(done.stop_reason, "end_turn");
        assert_eq!(session.messages().len(), 1);
    }

    /// The barrier tool below cannot finish until the stream has been read
    /// past its call; the text that arrived meanwhile still lands after it.
    #[tokio::test]
    async fn a_barrier_tool_keeps_the_stream_read_and_acts_on_it_afterwards() {
        use futures::StreamExt;
        let read_on = Arc::new(tokio::sync::Notify::new());
        let waits = Arc::clone(&read_on);
        let head = futures::stream::iter(vec![Ok::<_, ModelError>(ModelEvent::ToolUse {
            id: "w".into(),
            name: "Write".into(),
            input: serde_json::json!({"file_path": "a.txt"}),
        })]);
        let tail = futures::stream::once(async move {
            read_on.notify_one();
            Ok(ModelEvent::TextDelta {
                text: "after".into(),
            })
        })
        .chain(futures::stream::iter(vec![Ok(ModelEvent::EndTurn {
            stop_reason: "tool_use".into(),
            usage: Usage::default(),
        })]));
        let stream: base::interface::model::ModelStream = Box::new(Box::pin(head.chain(tail)));

        let mut session = session::session::SessionManager::in_memory(None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::event_bus::EventBus::new(tx);
        let done = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            execute_stream(
                stream,
                &mut session,
                &tx,
                "test-turn".into(),
                move |_id, _name, _input| {
                    let waits = Arc::clone(&waits);
                    async move {
                        waits.notified().await;
                        Ok(ToolAnswer::text("wrote"))
                    }
                },
                |_name, _input| false,
                CancellationToken::new(),
                &[],
                None,
            ),
        )
        .await
        .expect("the stream was not read while the barrier tool ran")
        .expect("the call succeeds");
        assert_eq!(done.stop_reason, "tool_use");
        assert_adjacent_tool_pairing(session.messages());
        assert!(matches!(
            &session.messages()[0].content[..],
            [ModelContentBlock::ToolUse { .. }, ModelContentBlock::Text { text }] if text == "after"
        ));
    }

    /// The stream below cannot end until the tool has started. If the tool
    /// only ran once the stream ended, this would never finish.
    #[tokio::test]
    async fn a_concurrency_safe_tool_runs_while_the_stream_is_still_arriving() {
        use futures::StreamExt;
        let started = Arc::new(tokio::sync::Notify::new());
        let waits = Arc::clone(&started);
        let head = futures::stream::iter(vec![Ok::<_, ModelError>(ModelEvent::ToolUse {
            id: "g".into(),
            name: "Glob".into(),
            input: serde_json::json!({"pattern": "*"}),
        })]);
        let tail = futures::stream::once(async move {
            waits.notified().await;
            Ok(ModelEvent::EndTurn {
                stop_reason: "tool_use".into(),
                usage: Usage::default(),
            })
        });
        let stream: base::interface::model::ModelStream = Box::new(Box::pin(head.chain(tail)));

        let mut session = session::session::SessionManager::in_memory(None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::event_bus::EventBus::new(tx);
        let done = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            execute_stream(
                stream,
                &mut session,
                &tx,
                "test-turn".into(),
                move |_id, _name, _input| {
                    let started = Arc::clone(&started);
                    async move {
                        started.notify_one();
                        Ok(ToolAnswer::text("found"))
                    }
                },
                |_name, _input| true,
                CancellationToken::new(),
                &[],
                None,
            ),
        )
        .await
        .expect("the tool waited for the stream to end")
        .expect("the call succeeds");
        assert!(done.has_tool_uses);
        assert_adjacent_tool_pairing(session.messages());
    }

    /// A model that has gone quiet holds nothing up once the turn is
    /// cancelled.
    #[tokio::test]
    async fn a_silent_stream_ends_as_soon_as_its_turn_is_cancelled() {
        use futures::StreamExt;
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            trigger.cancel();
        });
        let head = futures::stream::iter(vec![Ok::<_, ModelError>(ModelEvent::TextDelta {
            text: "so far".into(),
        })]);
        let stream: base::interface::model::ModelStream =
            Box::new(head.chain(futures::stream::pending()));
        let mut session = session::session::SessionManager::in_memory(None);
        let done = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            run_plain(stream, &mut session, cancel),
        )
        .await
        .expect("the cancel was not honoured")
        .expect("a cancelled call is not a failure");
        assert!(done.cancelled);
        assert_eq!(
            block_kinds(&session.messages()[0]),
            vec!["text:so far"],
            "what the host was shown stays"
        );
    }

    /// Every assistant message with ToolUse block(s) must be immediately
    /// followed by a user message whose ToolResult ids are exactly that set
    /// — the real constraint the Anthropic-compatible API enforces.
    fn assert_adjacent_tool_pairing(messages: &[ModelMessage]) {
        for (i, msg) in messages.iter().enumerate() {
            if msg.role != MessageRole::Assistant {
                continue;
            }
            let use_ids: std::collections::HashSet<String> = msg
                .content
                .iter()
                .filter_map(|b| match b {
                    ModelContentBlock::ToolUse { id, .. } => Some(id.clone()),
                    _ => None,
                })
                .collect();
            if use_ids.is_empty() {
                continue;
            }
            let next = messages.get(i + 1).unwrap_or_else(|| {
                panic!("assistant message {i} has ToolUse {use_ids:?} but there is no next message at all: {messages:#?}")
            });
            let result_ids: std::collections::HashSet<String> = next
                .content
                .iter()
                .filter_map(|b| match b {
                    ModelContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.clone()),
                    _ => None,
                })
                .collect();
            assert_eq!(
                use_ids,
                result_ids,
                "message {i} (assistant) has ToolUse {use_ids:?} but the immediately following \
                 message {} has ToolResult {result_ids:?} — not an exact match: {messages:#?}",
                i + 1
            );
        }
    }
}
