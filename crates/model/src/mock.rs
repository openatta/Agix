//! 测试用 mock client。
//!
//! 预先 push 多个 turn 的事件序列；每次 `stream_messages` 弹出一组事件流。
//! 不上 feature gate —— 体量很小、跨 crate 的 engine 测试都需要它。

use crate::client::{AnthropicClient, CountFuture, EventStream};
use crate::error::AnthropicError;
use crate::stream::StreamEvent;
use crate::types::MessagesRequest;
use futures::stream;
use std::collections::VecDeque;
use std::sync::Mutex;

/// 把每次 `stream_messages` 收到的请求与回放的事件流脚本化。
pub struct MockAnthropicClient {
    turns: Mutex<VecDeque<Vec<Result<StreamEvent, AnthropicError>>>>,
    captured: Mutex<Vec<MessagesRequest>>,
}

impl MockAnthropicClient {
    pub fn new() -> Self {
        Self {
            turns: Mutex::new(VecDeque::new()),
            captured: Mutex::new(Vec::new()),
        }
    }

    /// 队尾追加一个 turn 的事件序列。所有事件都标记 Ok。
    pub fn push_turn(&self, events: Vec<StreamEvent>) {
        self.turns
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_back(events.into_iter().map(Ok).collect());
    }

    /// 队尾追加一个 turn，允许混入 Err（用来测错误传播）。
    pub fn push_turn_with_errors(&self, events: Vec<Result<StreamEvent, AnthropicError>>) {
        self.turns
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_back(events);
    }

    /// 还剩多少个 turn 没回放。
    pub fn turns_remaining(&self) -> usize {
        self.turns
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    /// 已经接收了多少次 `stream_messages` 调用。
    pub fn calls(&self) -> usize {
        self.captured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    /// 取走第 N 次调用的请求 clone。
    pub fn nth_request(&self, idx: usize) -> Option<MessagesRequest> {
        self.captured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(idx)
            .cloned()
    }
}

impl Default for MockAnthropicClient {
    fn default() -> Self {
        Self::new()
    }
}

impl AnthropicClient for MockAnthropicClient {
    fn stream_messages(&self, req: MessagesRequest) -> EventStream {
        self.captured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(req);
        let events = self
            .turns
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
            .unwrap_or_default();
        Box::pin(stream::iter(events))
    }

    fn count_tokens<'a>(&'a self, _: &'a MessagesRequest) -> CountFuture<'a> {
        Box::pin(async { Ok(0) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::{MessageStartPayload, Usage};
    use futures::StreamExt;

    #[tokio::test]
    async fn replays_pushed_events_in_order() {
        let m = MockAnthropicClient::new();
        m.push_turn(vec![
            StreamEvent::MessageStart {
                message: MessageStartPayload {
                    id: "a".into(),
                    model: "x".into(),
                    role: "assistant".into(),
                    usage: Usage::default(),
                    stop_reason: None,
                },
            },
            StreamEvent::MessageStop,
        ]);
        m.push_turn(vec![StreamEvent::MessageStop]);
        assert_eq!(m.turns_remaining(), 2);

        let s = m.stream_messages(MessagesRequest::minimal("x", "u"));
        let evs: Vec<_> = s.collect().await;
        assert_eq!(evs.len(), 2);
        assert_eq!(m.turns_remaining(), 1);
        assert_eq!(m.calls(), 1);
    }
}
