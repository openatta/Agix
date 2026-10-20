//! Streaming executor — consume model stream, execute tools concurrently.
//!
//! v2: Streaming tool execution.
//! Concurrency-safe tools start executing during stream consumption — they run
//! while the model continues streaming text and more tool_use blocks. Sequential
//! (non-concurrency-safe) tools still wait until their batch completes.
//! Sibling abort on error (any tool error cancels all siblings in the same batch).

use crate::agent::EventSender;
use base::interface::event::AgentEvent;
use base::interface::model::{
    MessageRole, ModelContentBlock, ModelEvent, ModelMessage, ModelStream, Usage,
};
use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Result of processing a single model stream.
#[derive(Debug, Clone)]
pub struct StreamResult {
    pub stop_reason: String,
    pub usage: Usage,
    pub has_tool_uses: bool,
    pub tool_calls: u32,
}

/// A tool invocation queued for execution.
#[derive(Debug)]
struct QueuedTool {
    #[allow(dead_code)]
    id: String,
    name: String,
    #[allow(dead_code)]
    input: serde_json::Value,
    #[allow(dead_code)]
    concurrency_safe: bool,
}

/// Process a model stream with streaming tool execution.
///
/// v2: Concurrency-safe tools are spawned into `FuturesUnordered` immediately
/// when the `ToolUse` event arrives, while the stream is still being consumed.
/// This is the TS `StreamingToolExecutor` pattern:
///   - ToolUse arrives → spawn execution into background batch
///   - Stream continues → more text/tool_use events arrive
///   - Stream ends → await remaining tool futures, then execute sequential tools
///
/// Non-concurrency-safe tools still wait: they execute after all prior
/// concurrent-safe tools in their batch have completed.
///
/// **Session message grouping**: every `ToolUse` block emitted since the last
/// flush is buffered (not pushed to the session immediately) until its whole
/// batch resolves, then flushed as exactly one assistant message (all the
/// `ToolUse` blocks) immediately followed by exactly one user message (all the
/// matching `ToolResult` blocks). The Anthropic-compatible API rejects a
/// request where an assistant `tool_use` isn't immediately followed by its
/// `tool_result` — pushing one session message per tool call (the original
/// approach) broke that invariant as soon as a single response mixed a
/// concurrency-safe tool with a non-concurrency-safe one, since the
/// non-concurrency-safe tool's `ToolUse` message landed *before* the
/// concurrency-safe tool's `ToolResult` message in session order. Concurrent
/// execution timing is unaffected — only when results get written to the
/// session changed.
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
) -> Result<StreamResult, crate::turn::TurnError>
where
    F: Fn(String, String, serde_json::Value) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = base::interface::tool_middleware::ToolOutcome> + Send,
    G: Fn(&str, &serde_json::Value) -> bool + Send + Sync,
{
    use futures::StreamExt;
    tokio::pin!(stream);
    let mut stop_reason = String::new();
    let mut usage = Usage::default();
    let mut has_tool_uses = false;
    let mut tool_calls: u32 = 0;
    let mut pending_text = String::new();
    // Extended-thinking blocks completed during this stream, in arrival order.
    // These must be replayed to the API on the next request of a tool-use turn
    // (signature included) or it rejects the call — see
    // `ModelContentBlock::Thinking`. They are therefore transcript state, not
    // just display state.
    let mut completed_thinking: Vec<ModelContentBlock> = Vec::new();
    // Text of the thinking block currently open. The API closes a block by
    // sending its signature, at which point this plus the signature become one
    // `Thinking` block.
    let mut pending_thinking = String::new();
    let mut queued_tools: Vec<QueuedTool> = Vec::new();
    let mut tool_index: usize = 0;
    // P1: Dedup consecutive identical tool calls within a turn.
    // Key = "(tool_name, serialized_input)".
    let mut seen_tool_calls: HashSet<String> = HashSet::new();

    // Streaming execution: concurrent-safe tools run during stream consumption.
    // Each batch of consecutive concurrency-safe tools gets its own FuturesUnordered.
    // A non-concurrency-safe tool creates a barrier — all prior batches must drain first.
    let mut batch_abort = CancellationToken::new();
    let mut batch_futures = futures::stream::FuturesUnordered::new();

    // Not-yet-flushed tool_use/tool_result blocks for the current batch — see
    // the "Session message grouping" doc above.
    let mut pending_use_blocks: Vec<ModelContentBlock> = Vec::new();
    // `(original tool_index, block)` — concurrency-safe tools finish in
    // completion order, not submission order (`FuturesUnordered::next()`),
    // so the index is carried alongside each block and used to restore
    // submission order at flush time (see `flush_tool_batch`). The API
    // itself doesn't care about tool_result order within the message (only
    // that the *set* of ids matches the preceding ToolUse blocks), but replay
    // does: the message content is part of the request, so two runs of the
    // same turn with the same tools completing in a different real-world
    // order used to produce different requests and desync every replay after
    // that point — found via a real multi-Glob turn diverging on strict
    // replay for no code-level reason. Fixed the same way an unused sibling
    // implementation (since removed) had already solved it: carry the
    // original submission index alongside each result and restore order by
    // index (`results[idx] = ...`) instead of relying on completion order.
    let mut pending_result_blocks: Vec<(usize, ModelContentBlock)> = Vec::new();
    let mut pending_new_msgs: Vec<Vec<serde_json::Value>> = Vec::new();

    // Phase 1: Consume stream, spawn concurrent-safe tools immediately
    while let Some(event) = stream.next().await {
        if cancel.is_cancelled() {
            break;
        }
        match event.map_err(|e| crate::turn::TurnError::Model(e.to_string()))? {
            ModelEvent::TextDelta { text } => {
                pending_text.push_str(&text);
                let _ = event_tx.send(AgentEvent::TextDelta {
                    text,
                    turn_id: turn_id.clone(),
                });
            }
            ModelEvent::ThinkingDelta { text } => {
                pending_thinking.push_str(&text);
                let _ = event_tx.send(AgentEvent::ThinkingDelta {
                    text,
                    turn_id: turn_id.clone(),
                });
            }
            ModelEvent::ThinkingSignature { signature } => {
                // The signature closes the block. An empty accumulator here
                // would mean a signature with no thinking, which the API does
                // not produce; guard anyway rather than emit a malformed block
                // that would be rejected on the next request.
                if !pending_thinking.is_empty() {
                    completed_thinking.push(ModelContentBlock::Thinking {
                        text: std::mem::take(&mut pending_thinking),
                        signature,
                    });
                }
            }
            ModelEvent::RedactedThinking { data } => {
                completed_thinking.push(ModelContentBlock::RedactedThinking { data });
            }
            ModelEvent::ToolUse { id, name, input } => {
                has_tool_uses = true;
                // Flush thinking + pending text to session before tool block.
                // This is the flush that matters for correctness: it is a
                // tool-use turn, so the thinking blocks emitted before the
                // tool call have to be in the transcript ahead of the
                // `tool_use` block, or the *next* request in this turn is
                // rejected for a missing/misordered thinking block.
                flush_assistant_prefix(
                    session,
                    &mut completed_thinking,
                    &mut pending_text,
                    interceptors,
                );
                // Buffer every ToolUse the model actually emitted — duplicate or
                // not — it gets flushed together with its ToolResult once the
                // batch resolves (see module-level doc comment).
                pending_use_blocks.push(ModelContentBlock::ToolUse {
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

                // This ToolUse's position among every ToolUse this turn has
                // emitted so far (duplicate or not) — the one stable,
                // deterministic ordering key available (unlike real-world
                // completion time for concurrency-safe tools). `queued_tools`
                // gets an entry here too, unconditionally, so `idx` always
                // indexes it correctly at drain time below.
                let idx = tool_index;
                tool_index += 1;
                let safe = is_concurrency_safe(&name, &input);
                queued_tools.push(QueuedTool {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                    concurrency_safe: safe,
                });

                // P1: Dedup consecutive identical tool calls within a turn.
                let dedup_key = format!("({name},{input})");
                if !seen_tool_calls.insert(dedup_key.clone()) {
                    // Duplicate tool call — skip execution, buffer a synthetic
                    // tool result (paired with the ToolUse block just buffered
                    // above) so the model doesn't get stuck waiting.
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
                    // Spawn immediately into the current concurrent batch
                    let exec = execute_tool(id.clone(), name.clone(), input);
                    let abort = batch_abort.clone();
                    batch_futures.push(async move {
                        let result = tokio::select! {
                            _ = abort.cancelled() => Err("cancelled by sibling error".to_string()),
                            r = exec => r,
                        };
                        (idx, id, result)
                    });
                } else {
                    // Non-concurrency-safe: drain current batch first (results
                    // buffered, not pushed to session yet), then execute alone,
                    // then flush the whole batch as one paired message pair.
                    if !batch_futures.is_empty() {
                        while let Some((idx, tid, result)) = batch_futures.next().await {
                            let (content, new_msgs, failed) = read_outcome(&result);
                            // Only an `Err` takes the batch down with it. A
                            // tool that ran and reports a failure has not
                            // said anything about its siblings.
                            if result.is_err() {
                                batch_abort.cancel();
                            }
                            let tname = queued_tools
                                .get(idx)
                                .map(|t| t.name.clone())
                                .unwrap_or_default();
                            buffer_result(
                                event_tx,
                                &turn_id,
                                &mut pending_result_blocks,
                                idx,
                                &tid,
                                &tname,
                                &content,
                                failed,
                            );
                            if let Some(msgs) = new_msgs {
                                pending_new_msgs.push(msgs);
                            }
                        }
                    }
                    // Execute the sequential tool
                    let result = execute_tool(id.clone(), name.clone(), input).await;
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

                    flush_tool_batch(
                        session,
                        &mut pending_use_blocks,
                        &mut pending_result_blocks,
                        &mut pending_new_msgs,
                        interceptors,
                    );

                    // Start a new batch for subsequent concurrent-safe tools
                    batch_abort = CancellationToken::new();
                    batch_futures = futures::stream::FuturesUnordered::new();
                }

                tool_calls += 1;
            }
            ModelEvent::EndTurn {
                stop_reason: sr,
                usage: u,
            } => {
                stop_reason = sr;
                usage = u;
            }
            _ => {}
        }
    }

    // Flush remaining thinking + pending text. On a no-tool turn this is the
    // only flush, and it is what puts the reasoning into the transcript so a
    // later turn (or a resume) still sees it.
    flush_assistant_prefix(
        session,
        &mut completed_thinking,
        &mut pending_text,
        interceptors,
    );

    // Drain any remaining in-flight concurrent tools, then flush the final batch.
    //
    // ⚠️ Fixed: this loop used to drain without ever calling
    // `batch_abort.cancel()` on an error — unlike the mid-stream
    // non-concurrency-safe-barrier drain above, which does. Since most
    // turns end with their last concurrent batch draining right here (the
    // stream simply ends after the tool_use events), that meant this
    // module's own doc comment ("any tool error cancels all siblings in the
    // same batch") was true only for the less common mid-stream-barrier
    // case, not for the typical end-of-stream one — an error (including a
    // `PostToolUse` hook's deliberate denial, see `execute_tool_inner`)
    // wouldn't actually cancel its still-running siblings here. Now it does,
    // consistently with the other drain path.
    while let Some((idx, tid, result)) = batch_futures.next().await {
        let is_err = result.is_err();
        if is_err {
            batch_abort.cancel();
        }
        let (content, new_msgs, failed) = read_outcome(&result);
        let tname = queued_tools
            .get(idx)
            .map(|t| t.name.clone())
            .unwrap_or_default();
        buffer_result(
            event_tx,
            &turn_id,
            &mut pending_result_blocks,
            idx,
            &tid,
            &tname,
            &content,
            failed,
        );
        if let Some(msgs) = new_msgs {
            pending_new_msgs.push(msgs);
        }
    }
    flush_tool_batch(
        session,
        &mut pending_use_blocks,
        &mut pending_result_blocks,
        &mut pending_new_msgs,
        interceptors,
    );

    Ok(StreamResult {
        stop_reason,
        usage,
        has_tool_uses,
        tool_calls,
    })
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

/// Buffer a tool result (not yet pushed to the session — see
/// `flush_tool_batch`) and emit the live `AgentEvent::ToolResult` for UI
/// feedback immediately (that's independent of session message structure).
/// `idx` is the tool's original submission index (its position among this
/// turn's `ToolUse` events) — used to restore a deterministic order at flush
/// time regardless of real completion order.
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

/// Flush the current batch: one assistant message with every buffered
/// `ToolUse` block, immediately followed by one user message with every
/// buffered `ToolResult` block — sorted back into original submission order
/// first (see `pending_result_blocks`'s doc comment: the API tolerates any
/// order since it only checks the *set* of ids, but replay determinism
/// doesn't, so this restores a canonical order even though it's not
/// API-required), then any deferred `new_messages` in resolution order.
/// No-op if nothing is pending.
/// Push the assistant-turn prefix — completed thinking blocks followed by any
/// buffered plain text — as one assistant message, draining both accumulators.
///
/// Thinking must lead: the Anthropic API requires an assistant turn's thinking
/// blocks to precede its text and `tool_use` blocks, and rejects a request
/// whose replayed turn violates that. Callers therefore invoke this *before*
/// buffering tool_use blocks, not after.
///
/// No-op when both accumulators are empty, so it is safe to call at every
/// flush point unconditionally.
fn flush_assistant_prefix(
    session: &mut session::session::SessionManager,
    completed_thinking: &mut Vec<ModelContentBlock>,
    pending_text: &mut String,
    interceptors: &[Arc<dyn base::interface::model_interceptor::ModelInterceptor>],
) {
    if completed_thinking.is_empty() && pending_text.is_empty() {
        return;
    }
    let mut content: Vec<ModelContentBlock> = std::mem::take(completed_thinking);
    if !pending_text.is_empty() {
        content.push(ModelContentBlock::Text {
            text: std::mem::take(pending_text),
        });
    }
    // The message is whole here, which is the only point at which it is worth
    // showing to anyone: a hook on the deltas that built it would run
    // thousands of times and could produce something that never existed as a
    // coherent message.
    let mut message = ModelMessage {
        role: MessageRole::Assistant,
        content,
    };
    base::interface::model_interceptor::intercept_message(interceptors, &mut message);
    session.push_message(message);
}

fn flush_tool_batch(
    session: &mut session::session::SessionManager,
    pending_use_blocks: &mut Vec<ModelContentBlock>,
    pending_result_blocks: &mut Vec<(usize, ModelContentBlock)>,
    pending_new_msgs: &mut Vec<Vec<serde_json::Value>>,
    interceptors: &[Arc<dyn base::interface::model_interceptor::ModelInterceptor>],
) {
    if pending_use_blocks.is_empty() {
        return;
    }
    let mut message = ModelMessage {
        role: MessageRole::Assistant,
        content: std::mem::take(pending_use_blocks),
    };
    base::interface::model_interceptor::intercept_message(interceptors, &mut message);
    session.push_message(message);
    let mut results = std::mem::take(pending_result_blocks);
    results.sort_by_key(|(idx, _)| *idx);
    session.push_message(ModelMessage {
        role: MessageRole::User,
        content: results.into_iter().map(|(_, block)| block).collect(),
    });
    for msgs in pending_new_msgs.drain(..) {
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
    /// signatures, and must lead the assistant turn.
    ///
    /// Before this, `stream.rs` parsed thinking, `parser.rs` had contract
    /// tests for that parsing, and then the adapter dropped it on the floor —
    /// so `ModelContentBlock` had no thinking to carry and the assistant
    /// message rebuilt here contained only text and tool_use. On a
    /// `thinking + tool_use` turn that makes the *next* request malformed:
    /// the API requires the previous assistant turn's thinking blocks to be
    /// replayed with signatures intact.
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
                ModelContentBlock::Thinking { text, signature }
                    if text == "I should read the file" && signature == "sig_xyz"
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

        // And the tool_use block still lands in a later message, so the
        // existing tool pairing invariant is untouched.
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

    /// Regression for the bug that produced a real API 400 during a live
    /// recording run: `unexpected tool_use_id found in tool_result blocks —
    /// Each tool_result block must have a corresponding tool_use block in the
    /// previous message`. Root cause: when the model emits the same
    /// (name, input) tool call twice in one response, the duplicate-skip
    /// branch pushed a synthetic `ToolResult` for the duplicate's
    /// `tool_use_id` but never pushed a matching `ToolUse` block (that push
    /// happened *after* the dedup check, which `continue`d past it) — so the
    /// session ended up with a `tool_result` whose `tool_use_id` had no
    /// preceding `tool_use`, which every real provider rejects on the next
    /// request. Every `ToolUse` the model emits — duplicate or not — must
    /// now be recorded before any matching `ToolResult`.
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

    /// Regression for the real API 400 hit during a live recording run:
    /// `tool_use ids were found without tool_result blocks immediately
    /// after`. One response emits a concurrency-safe tool (Glob) then a
    /// non-concurrency-safe one (Bash) — the mix that forces the "drain
    /// batch then execute sequential" path. Before the grouping fix, each
    /// tool_use/tool_result pair was its own session message, so the second
    /// tool's ToolUse message landed before the first tool's ToolResult
    /// message — exactly what the API rejects.
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
        )
        .await
        .expect("execute_stream should succeed");

        assert_adjacent_tool_pairing(session.messages());
        assert_eq!(session.messages().len(), 2);
    }

    /// Regression for a real replay-determinism bug: three
    /// concurrency-safe tools submitted in order A, B, C but made to
    /// *finish* in the reverse order C, B, A (artificial per-call delays,
    /// simulating real-world scheduling where a fast `Glob` beats a slower
    /// one). Before the fix, `pending_result_blocks` was pushed to in
    /// `FuturesUnordered` completion order, so the flushed `ToolResult`
    /// message came out C, B, A — same *set* of ids (the API doesn't care),
    /// but a different exact message, so every request after this point
    /// differed between two runs of the same conversation. Found via a real
    /// recorded turn (3 parallel `Glob` calls) that diverged on replay for no
    /// code-level reason — the model's own request was identical, only the
    /// tool completion timing differed between record and replay. The fix
    /// (`idx`-sorting at flush time, mirroring `dispatch.rs`) must produce
    /// A, B, C regardless of completion order.
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

    /// Regression test: the final concurrent batch of a turn — the common
    /// case, since most turns simply end after their last round of tool
    /// calls — used to drain (the loop right after "Phase 1" stream
    /// consumption ends) without ever calling `batch_abort.cancel()` on an
    /// error, unlike the mid-stream non-concurrency-safe-barrier drain,
    /// which does. So this module's own doc comment ("any tool error
    /// cancels all siblings in the same batch") was only true for the less
    /// common barrier-interrupted case. Here: a fast-erroring tool and a
    /// slow tool submitted in the same batch — the slow one must observe
    /// cancellation (and return quickly) rather than running to completion,
    /// proving `batch_abort` actually fires from this drain path now.
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

    /// Two separate sequential tools back-to-back: two independent
    /// assistant/user pairs, each internally consistent.
    #[tokio::test]
    async fn two_sequential_tool_calls_produce_two_paired_batches() {
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
        )
        .await
        .expect("execute_stream should succeed");

        assert_adjacent_tool_pairing(session.messages());
        assert_eq!(session.messages().len(), 4);
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
