//! SSE 字节流 → StreamEvent 流。
//!
//! 独立于 HTTP，方便单测：fixture 字节直接喂进来即可断言事件序列。
//! mock anthropic 的测试也走这条解析。

use crate::error::AnthropicError;
use crate::stream::StreamEvent;
use bytes::Bytes;
use eventsource_stream::{EventStreamError, Eventsource};
use futures::stream::{Stream, StreamExt};
use std::pin::Pin;

/// 解析后的事件流类型别名 —— 同 `client::EventStream`。
pub type ParsedStream =
    Pin<Box<dyn Stream<Item = Result<StreamEvent, AnthropicError>> + Send + 'static>>;

/// 输入：字节流（典型来自 `Network::open` 的响应体或测试 fixture）
/// 输出：StreamEvent 流（已 Box::pin，调用方直接 `.next()`）
///
/// 错误处理：
/// - SSE 行解析错 → `AnthropicError::Sse(...)`
/// - data 不是合法 JSON → `AnthropicError::Schema(...)`
/// - 上游 byte 错 → 透传（已是 `AnthropicError`）
///
/// 兼容性：
/// - `event:` 字段被忽略（同信息在 data JSON 的 `type` 里）
/// - 未知事件类型走 `StreamEvent::Unknown`，不报错
/// - 空 data 行（保活注释）由 eventsource-stream 过滤
pub fn parse_sse<S>(byte_stream: S) -> ParsedStream
where
    S: Stream<Item = Result<Bytes, AnthropicError>> + Send + 'static,
{
    let mapped = byte_stream.eventsource().map(|res| match res {
        Err(EventStreamError::Transport(e)) => Err(e),
        Err(e) => Err(AnthropicError::Sse(format!("{e}"))),
        Ok(event) => {
            if event.data.is_empty() {
                return Ok(StreamEvent::Unknown);
            }
            serde_json::from_str::<StreamEvent>(&event.data).map_err(AnthropicError::Schema)
        }
    });
    Box::pin(mapped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::BlockDelta;
    use base::message::StopReason;
    use futures::stream;

    /// 把一条 SSE blob 切成不规则的字节段，构造一个流喂给 parser。
    fn chunked_stream(
        blob: &'static [u8],
        chunk: usize,
    ) -> impl Stream<Item = Result<Bytes, AnthropicError>> + Send + 'static {
        let chunks: Vec<Bytes> = blob.chunks(chunk).map(Bytes::copy_from_slice).collect();
        stream::iter(chunks.into_iter().map(Ok))
    }

    const SAMPLE: &[u8] = b"\
event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_01\",\"model\":\"claude-sonnet-4-6\",\"role\":\"assistant\",\"usage\":{\"input_tokens\":10,\"output_tokens\":0}}}\n\
\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" world\"}}\n\
\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\",\"index\":0}\n\
\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":0,\"output_tokens\":15}}\n\
\n\
event: message_stop\n\
data: {\"type\":\"message_stop\"}\n\
\n\
";

    #[tokio::test]
    async fn parses_full_turn_sequence() -> Result<(), AnthropicError> {
        let stream = chunked_stream(SAMPLE, 64);
        let events: Vec<StreamEvent> = parse_sse(stream).map(|r| r.unwrap()).collect().await;

        assert_eq!(events.len(), 7);

        assert!(matches!(events[0], StreamEvent::MessageStart { .. }));
        assert!(matches!(events[1], StreamEvent::ContentBlockStart { .. }));

        // 两个 text delta，文本拼起来是 "Hello world"
        let mut text = String::new();
        for ev in &events {
            if let StreamEvent::ContentBlockDelta {
                delta: BlockDelta::TextDelta { text: t },
                ..
            } = ev
            {
                text.push_str(t);
            }
        }
        assert_eq!(text, "Hello world");

        assert!(matches!(
            events[4],
            StreamEvent::ContentBlockStop { index: 0 }
        ));

        match &events[5] {
            StreamEvent::MessageDelta { delta, usage } => {
                assert_eq!(delta.stop_reason, Some(StopReason::EndTurn));
                assert_eq!(usage.as_ref().unwrap().output_tokens, 15);
                Ok(())
            }
            other => Err(AnthropicError::ParseError(format!(
                "expected MessageDelta at idx 5, got {:?}",
                other
            ))),
        }
    }

    /// 切得很碎也要正确（SSE 行边界跨多个 chunk）
    #[tokio::test]
    async fn parses_with_tiny_chunks() {
        let stream = chunked_stream(SAMPLE, 7);
        let events: Vec<StreamEvent> = parse_sse(stream).map(|r| r.unwrap()).collect().await;
        assert_eq!(events.len(), 7);
    }

    #[tokio::test]
    async fn unknown_event_type_does_not_break() {
        const BAD: &[u8] = b"\
event: future_event\n\
data: {\"type\":\"future_event\",\"foo\":42}\n\
\n\
event: message_stop\n\
data: {\"type\":\"message_stop\"}\n\
\n\
";
        let stream = chunked_stream(BAD, 64);
        let events: Vec<StreamEvent> = parse_sse(stream).map(|r| r.unwrap()).collect().await;
        assert_eq!(events.len(), 2);
        assert_eq!(events[0], StreamEvent::Unknown);
        assert_eq!(events[1], StreamEvent::MessageStop);
    }

    #[tokio::test]
    async fn malformed_data_yields_schema_error_but_keeps_stream() {
        const BAD_DATA: &[u8] = b"\
event: message_start\n\
data: not-json\n\
\n\
event: message_stop\n\
data: {\"type\":\"message_stop\"}\n\
\n\
";
        let stream = chunked_stream(BAD_DATA, 64);
        let results: Vec<Result<StreamEvent, AnthropicError>> = parse_sse(stream).collect().await;
        assert_eq!(results.len(), 2);
        assert!(matches!(results[0], Err(AnthropicError::Schema(_))));
        assert!(matches!(results[1], Ok(StreamEvent::MessageStop)));
    }

    #[tokio::test]
    async fn contract_fixture_covers_thinking_tool_use_and_cache_usage(
    ) -> Result<(), AnthropicError> {
        const CONTRACT: &[u8] = b"\
event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_contract\",\"model\":\"claude-sonnet-4-6\",\"role\":\"assistant\",\"usage\":{\"input_tokens\":101,\"output_tokens\":0,\"cache_creation_input_tokens\":12,\"cache_read_input_tokens\":34},\"future_field\":\"ignored\"}}\n\
\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"},\"unknown\":\"ignored\"}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"need a tool\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig_01\"}}\n\
\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\",\"index\":0}\n\
\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_01\",\"name\":\"read_file\",\"input\":{}}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"Cargo.toml\\\"}\"}}\n\
\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\",\"index\":1}\n\
\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":17,\"cache_read_input_tokens\":34}}\n\
\n\
event: message_stop\n\
data: {\"type\":\"message_stop\"}\n\
\n\
";

        let stream = chunked_stream(CONTRACT, 23);
        let events: Vec<StreamEvent> = parse_sse(stream).map(|r| r.unwrap()).collect().await;

        assert_eq!(events.len(), 11);
        match &events[0] {
            StreamEvent::MessageStart { message } => {
                assert_eq!(message.id, "msg_contract");
                assert_eq!(message.usage.input_tokens, 101);
                assert_eq!(message.usage.cache_creation_input_tokens, Some(12));
                assert_eq!(message.usage.cache_read_input_tokens, Some(34));
            }
            other => {
                return Err(AnthropicError::ParseError(format!(
                    "expected message_start, got {other:?}"
                )))
            }
        }
        assert!(matches!(
            events[1],
            StreamEvent::ContentBlockStart {
                content_block: crate::stream::ContentBlockStart::Thinking { .. },
                ..
            }
        ));
        assert!(matches!(
            events[2],
            StreamEvent::ContentBlockDelta {
                delta: BlockDelta::ThinkingDelta { .. },
                ..
            }
        ));
        assert!(matches!(
            events[3],
            StreamEvent::ContentBlockDelta {
                delta: BlockDelta::SignatureDelta { .. },
                ..
            }
        ));
        match &events[5] {
            StreamEvent::ContentBlockStart { content_block, .. } => match content_block {
                crate::stream::ContentBlockStart::ToolUse { id, name, input } => {
                    assert_eq!(id, "toolu_01");
                    assert_eq!(name, "read_file");
                    assert_eq!(input, &serde_json::json!({}));
                }
                other => {
                    return Err(AnthropicError::ParseError(format!(
                        "expected tool_use, got {other:?}"
                    )))
                }
            },
            other => {
                return Err(AnthropicError::ParseError(format!(
                    "expected content_block_start, got {other:?}"
                )))
            }
        }

        let mut input_json = String::new();
        for ev in &events {
            if let StreamEvent::ContentBlockDelta {
                delta: BlockDelta::InputJsonDelta { partial_json },
                ..
            } = ev
            {
                input_json.push_str(partial_json);
            }
        }
        assert_eq!(input_json, "{\"path\":\"Cargo.toml\"}");

        match &events[9] {
            StreamEvent::MessageDelta { delta, usage } => {
                assert_eq!(delta.stop_reason, Some(StopReason::ToolUse));
                assert_eq!(usage.as_ref().unwrap().output_tokens, 17);
                assert_eq!(usage.as_ref().unwrap().cache_read_input_tokens, Some(34));
            }
            other => {
                return Err(AnthropicError::ParseError(format!(
                    "expected message_delta, got {other:?}"
                )))
            }
        }
        Ok(())
    }

    /// Redacted thinking content block through the SSE pipeline.
    #[tokio::test]
    async fn redacted_thinking_parses_through_sse() -> Result<(), AnthropicError> {
        const INPUT: &[u8] = b"\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"redacted_thinking\",\"data\":\"encrypted==\"}}\n\
\n\
data: {\"type\":\"content_block_stop\",\"index\":0}\n\
\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}\n\
\n\
data: {\"type\":\"message_stop\"}\n\
\n\
";
        let stream = chunked_stream(INPUT, 64);
        let events: Vec<StreamEvent> = parse_sse(stream).map(|r| r.unwrap()).collect().await;

        assert_eq!(events.len(), 4);
        match &events[0] {
            StreamEvent::ContentBlockStart {
                content_block: crate::stream::ContentBlockStart::RedactedThinking { data },
                ..
            } => assert_eq!(data, "encrypted=="),
            other => {
                return Err(AnthropicError::ParseError(format!(
                    "expected redacted_thinking, got {other:?}"
                )))
            }
        }
        assert_eq!(events[3], StreamEvent::MessageStop);
        Ok(())
    }

    /// CitationsDelta in the delta stream .
    #[tokio::test]
    async fn citations_delta_parses_through_sse() -> Result<(), AnthropicError> {
        const INPUT: &[u8] = b"\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\
\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"citations_delta\",\"citation\":{\"start\":0,\"end\":5}}}\n\
\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"cited\"}}\n\
\n\
data: {\"type\":\"content_block_stop\",\"index\":0}\n\
\n\
data: {\"type\":\"message_stop\"}\n\
\n\
";
        let stream = chunked_stream(INPUT, 47);
        let events: Vec<StreamEvent> = parse_sse(stream).map(|r| r.unwrap()).collect().await;

        assert_eq!(events.len(), 5);
        match &events[1] {
            StreamEvent::ContentBlockDelta {
                delta: BlockDelta::CitationsDelta { citation },
                ..
            } => assert_eq!(citation["start"], 0),
            other => {
                return Err(AnthropicError::ParseError(format!(
                    "expected citations_delta, got {other:?}"
                )))
            }
        }
        match &events[2] {
            StreamEvent::ContentBlockDelta {
                delta: BlockDelta::TextDelta { text },
                ..
            } => assert_eq!(text, "cited"),
            other => {
                return Err(AnthropicError::ParseError(format!(
                    "expected text_delta, got {other:?}"
                )))
            }
        }
        Ok(())
    }

    /// StopReason::MaxTokens through the SSE stream.
    #[tokio::test]
    async fn max_tokens_stop_reason_through_sse() -> Result<(), AnthropicError> {
        const INPUT: &[u8] = b"\
data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_mt\",\"model\":\"claude-sonnet-4-6\",\"role\":\"assistant\",\"usage\":{\"input_tokens\":10,\"output_tokens\":0}}}\n\
\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\
\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}\n\
\n\
data: {\"type\":\"content_block_stop\",\"index\":0}\n\
\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"max_tokens\"},\"usage\":{\"output_tokens\":5}}\n\
\n\
data: {\"type\":\"message_stop\"}\n\
\n\
";
        let stream = chunked_stream(INPUT, 31);
        let events: Vec<StreamEvent> = parse_sse(stream).map(|r| r.unwrap()).collect().await;

        assert_eq!(events.len(), 6);
        match &events[4] {
            StreamEvent::MessageDelta { delta, .. } => {
                assert_eq!(delta.stop_reason, Some(StopReason::MaxTokens));
            }
            other => {
                return Err(AnthropicError::ParseError(format!(
                    "expected MessageDelta with max_tokens, got {other:?}"
                )))
            }
        }
        assert_eq!(events[5], StreamEvent::MessageStop);
        Ok(())
    }

    /// SSE data with no `event:` prefix (the API sometimes omits it).
    #[tokio::test]
    async fn data_only_lines_parse_correctly() {
        const INPUT: &[u8] = b"\
data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_no\",\"model\":\"m\",\"role\":\"assistant\",\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\
\n\
data: {\"type\":\"message_stop\"}\n\
\n\
";
        let stream = chunked_stream(INPUT, 64);
        let events: Vec<StreamEvent> = parse_sse(stream).map(|r| r.unwrap()).collect().await;

        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], StreamEvent::MessageStart { .. }));
        assert_eq!(events[1], StreamEvent::MessageStop);
    }
}
