//! `session.compact` / `session.clear` 和 `session.respondToPrompt` 的处理函数。

use super::*;

impl DaemonServer {
    /// `session.compact` / `session.clear`: the slash command run as a turn
    /// of its own, so it streams and finishes like one and is refused like
    /// one while another turn holds the session.
    pub(super) async fn method_session_command(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
        client: Arc<Client>,
        command: &str,
    ) -> RpcResponse {
        let Some(session_id) = params.get("session_id").and_then(|v| v.as_str()) else {
            return RpcResponse::err(id, codes::INVALID_PARAMS, "missing session_id");
        };
        let request = crate::session_pool::api::TurnRequest {
            session_id: Some(session_id.to_string()),
            message: command.to_string(),
            attachments: vec![],
            turn_id: Id::new().to_string(),
            options: None,
            on_busy: crate::session_pool::OnBusy::Reject,
            initiator: crate::session_pool::api::Initiator::Client,
        };
        let ran = self.pool.run_turn(request, Some(client)).await;
        respond(id, ran.map(super::methods_turn::turn_result))
    }

    /// `session.respondToPrompt` — answers a pending `kind:"prompt"`
    /// `session.event`: a `decision` for a `permission` prompt, an `answer`
    /// for a `question`.
    pub(super) async fn method_session_respond_to_prompt(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
    ) -> RpcResponse {
        let session_id = match params.get("session_id").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => return RpcResponse::err(id, codes::INVALID_PARAMS, "missing session_id"),
        };
        let prompt_id = match params.get("prompt_id").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => return RpcResponse::err(id, codes::INVALID_PARAMS, "missing prompt_id"),
        };
        let prompt_type = params
            .get("prompt_type")
            .and_then(|v| v.as_str())
            .unwrap_or(crate::rpc::PROMPT_TYPE_PERMISSION);
        let answer = match prompt_type {
            crate::rpc::PROMPT_TYPE_PERMISSION => match parse_param(&params, "decision") {
                Ok(d) => crate::session_pool::PromptAnswer::Permission(d),
                Err(e) => return RpcResponse::err(id, codes::INVALID_PARAMS, e),
            },
            crate::rpc::PROMPT_TYPE_QUESTION => match parse_param(&params, "answer") {
                Ok(a) => crate::session_pool::PromptAnswer::Question(a),
                Err(e) => return RpcResponse::err(id, codes::INVALID_PARAMS, e),
            },
            other => {
                return RpcResponse::err(
                    id,
                    codes::INVALID_PARAMS,
                    format!("unsupported prompt_type: {other}"),
                )
            }
        };

        match self
            .pool
            .respond_to_prompt(&session_id, prompt_id.clone(), answer)
            .await
        {
            Ok(()) => RpcResponse::ok(id, serde_json::json!({"prompt_id": prompt_id})),
            Err(e) => failure(id, e),
        }
    }
}
