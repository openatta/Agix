//! 一条连接作为会话的订阅者：池交来的事件投影成 `session.event` 帧，非阻塞地
//! 放进这条连接的出站队列。响应走同一个队列，所以 `turn_complete` 一定排在
//! `run_turn` 的响应前面。

use crate::rpc::events::SessionEventNotification;
use crate::rpc::methods::session::SubscribeResult;
use crate::rpc::Client;
use crate::session_pool::api::{SessionFrame, SessionWatcher, Subscribed, WatcherId};

impl SessionWatcher for Client {
    fn id(&self) -> WatcherId {
        WatcherId(Client::id(self).0)
    }

    fn deliver(&self, frame: &SessionFrame) -> bool {
        let Some(notification) = SessionEventNotification::of(frame) else {
            return true;
        };
        match serde_json::to_string(&notification) {
            Ok(json) => self.try_send_for(&frame.session_id, json),
            Err(e) => {
                tracing::warn!(error = %e, "could not serialize a session frame; dropping it");
                true
            }
        }
    }
}

/// `session.subscribe`'s result: each open ask as the frame that raised it.
pub(super) fn subscribe_result(subscribed: Subscribed) -> SubscribeResult {
    let pending_prompts = subscribed
        .pending
        .iter()
        .filter_map(|ask| SessionEventNotification::of(&ask.frame))
        .filter_map(|frame| serde_json::to_value(frame).ok())
        .collect();
    SubscribeResult {
        session_id: subscribed.session_id,
        last_seq: subscribed.last_seq,
        pending_prompts,
        resumed: subscribed.resumed,
        scene_inferred: subscribed.scene_inferred,
    }
}
