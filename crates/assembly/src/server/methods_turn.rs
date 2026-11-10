//! Handlers that run work: `session.run_turn`, context, sub-agents, schedules.

use super::*;

/// `session.run_turn`'s result for how the turn went.
pub(super) fn turn_result(outcome: crate::session_pool::api::TurnOutcome) -> serde_json::Value {
    use crate::session_pool::api::TurnOutcome;
    match outcome {
        TurnOutcome::Completed {
            session_id,
            turn_id,
            name,
            api_calls,
        } => serde_json::json!({
            "session_id": session_id,
            "turn_id": turn_id,
            "delivery": "turn",
            "name": name,
            "api_calls": api_calls,
        }),
        TurnOutcome::Steered {
            session_id,
            turn_id,
            into_turn_id,
        } => serde_json::json!({
            "session_id": session_id,
            "turn_id": turn_id,
            "delivery": "steer",
            "into_turn_id": into_turn_id,
        }),
        TurnOutcome::Skipped {
            session_id,
            turn_id,
        } => serde_json::json!({"session_id": session_id, "turn_id": turn_id, "skipped": true}),
    }
}

impl DaemonServer {
    pub(super) async fn method_session_run_turn(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
        client: Arc<Client>,
    ) -> RpcResponse {
        // session_id 可选：不传则自动新建 session
        let session_id = params
            .get("session_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let user_msg = match params.get("message").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => return RpcResponse::err(id, codes::INVALID_PARAMS, "missing message"),
        };

        let turn_id = params
            .get("turn_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| Id::new().to_string());
        // A turn's origin is read from its id, so a client may not use the
        // form of a goal round or a scheduled job, which the daemon starts itself.
        if base::context::goal::TurnOrigin::from_turn_id(&turn_id)
            != base::context::goal::TurnOrigin::Human
        {
            return RpcResponse::err(
                id,
                codes::INVALID_PARAMS,
                format!(
                    "turn_id `{turn_id}` has the form of a turn the daemon starts itself \
                     (a goal round or a scheduled job)"
                ),
            );
        }

        let (options, ignored_options) = match read_options("session.run_turn", &id, &params) {
            Ok(read) => read,
            Err(refusal) => return refusal,
        };

        // Optional and backwards-compatible: a client that never sends
        // `attachments` behaves exactly as before. A malformed entry is
        // dropped rather than failing the turn — the user still wants their
        // message sent, and a missing attachment is visible to them.
        let attachments: Vec<runtime::agent::Attachment> = params
            .get("attachments")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| match serde_json::from_value(v.clone()) {
                        Ok(a) => Some(a),
                        Err(e) => {
                            tracing::warn!(error = %e, "dropping malformed attachment");
                            None
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        if user_msg.trim().is_empty() && attachments.is_empty() {
            return RpcResponse::err(
                id,
                codes::INVALID_PARAMS,
                "message is empty and there are no attachments",
            );
        }

        let on_busy = match params.get("on_busy") {
            None | Some(serde_json::Value::Null) => crate::session_pool::OnBusy::default(),
            Some(v) => match serde_json::from_value(v.clone()) {
                Ok(o) => o,
                Err(_) => {
                    return RpcResponse::err(
                        id,
                        codes::INVALID_PARAMS,
                        "on_busy must be \"reject\", \"queue\" or \"steer\"",
                    )
                }
            },
        };

        let request = crate::session_pool::api::TurnRequest {
            session_id,
            message: user_msg,
            attachments,
            turn_id,
            options,
            on_busy,
            initiator: crate::session_pool::api::Initiator::Client,
        };
        let ran = self.pool.run_turn(request, Some(client)).await;
        reporting_ignored(respond(id, ran.map(turn_result)), ignored_options)
    }

    /// `session.inspectContext {session_id}`, `session.dropMessages
    /// {session_id, at:[…], total?}`, `session.editMessage {session_id, at,
    /// text, total?}` — see `SessionPool::context_request`.
    pub(super) async fn method_context(
        &self,
        id: serde_json::Value,
        method: &str,
        params: &serde_json::Value,
    ) -> RpcResponse {
        use runtime::agent::ContextRequest;
        let Some(session_id) = params.get("session_id").and_then(|v| v.as_str()) else {
            return RpcResponse::err(id, codes::INVALID_PARAMS, "missing session_id");
        };
        let total = params
            .get("total")
            .and_then(|v| v.as_u64())
            .map(|t| t as usize);
        let request = match method {
            "session.inspectContext" => ContextRequest::Inspect,
            "session.dropMessages" => {
                let at: Option<Vec<usize>> = params.get("at").and_then(|v| v.as_array()).map(|a| {
                    a.iter()
                        .filter_map(|i| i.as_u64().map(|i| i as usize))
                        .collect()
                });
                match at {
                    Some(at) if !at.is_empty() => ContextRequest::Drop { at, total },
                    _ => {
                        return RpcResponse::err(
                            id,
                            codes::INVALID_PARAMS,
                            "session.dropMessages needs `at`, a list of message positions",
                        )
                    }
                }
            }
            _ => {
                let at = params.get("at").and_then(|v| v.as_u64());
                let text = params.get("text").and_then(|v| v.as_str());
                match (at, text) {
                    (Some(at), Some(text)) => ContextRequest::Edit {
                        at: at as usize,
                        text: text.to_string(),
                        total,
                    },
                    _ => {
                        return RpcResponse::err(
                            id,
                            codes::INVALID_PARAMS,
                            "session.editMessage needs `at` and `text`",
                        )
                    }
                }
            }
        };
        match self.pool.context_request(session_id, request).await {
            Ok(result) => RpcResponse::ok(id, result),
            Err(e) => failure(id, e),
        }
    }

    /// `session.startAgent`: a sub-agent the host starts under a session.
    #[cfg(feature = "agents")]
    pub(super) async fn method_start_agent(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
    ) -> RpcResponse {
        let params: crate::rpc::StartAgentParams = match serde_json::from_value(params) {
            Ok(p) => p,
            Err(e) => {
                return RpcResponse::err(
                    id,
                    codes::INVALID_PARAMS,
                    format!("session.startAgent: {e}"),
                )
            }
        };
        if params.prompt.trim().is_empty() {
            return RpcResponse::err(
                id,
                codes::INVALID_PARAMS,
                "session.startAgent needs a prompt",
            );
        }
        if params.provider.is_some() {
            return RpcResponse::err(
                id,
                codes::INVALID_PARAMS,
                "session.startAgent: a sub-agent runs on its parent's provider; give `model` only",
            );
        }
        let session_id = params.session_id.clone();
        match self
            .pool
            .start_agent(&session_id, params.into_request())
            .await
        {
            Ok(result) => RpcResponse::ok(id, result),
            Err(e) => failure(id, e),
        }
    }

    /// `schedule.create {session_id, cron, prompt, recurring?, durable?}`: run
    /// `prompt` as a turn of `session_id` whenever `cron` (5 fields, UTC)
    /// matches. Recurring and durable by default — a host schedules work that
    /// should outlive the session's time in memory and the daemon's restarts.
    pub(super) async fn method_schedule_create(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
    ) -> RpcResponse {
        let text = |key: &str| {
            params
                .get(key)
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_string)
        };
        let flag = |key: &str| params.get(key).and_then(|v| v.as_bool()).unwrap_or(true);
        let (Some(session_id), Some(cron), Some(prompt)) =
            (text("session_id"), text("cron"), text("prompt"))
        else {
            return RpcResponse::err(
                id,
                codes::INVALID_PARAMS,
                "schedule.create needs session_id, cron and prompt",
            );
        };
        if !tools::cron::cron_expression_valid(&cron) {
            return RpcResponse::err(
                id,
                codes::INVALID_PARAMS,
                format!("invalid cron expression `{cron}`: expected 5 fields, M H DoM Mon DoW"),
            );
        }
        if !self.pool.session_exists(&session_id).await {
            return RpcResponse::err(
                id,
                codes::SESSION_NOT_FOUND,
                format!("no session `{session_id}`"),
            );
        }
        let job = self.pool.schedule_add(tools::cron::NewCronJob {
            cron,
            prompt,
            recurring: flag("recurring"),
            durable: flag("durable"),
            session_id: Some(session_id),
            expires_ms: None,
        });
        RpcResponse::ok(id, serde_json::json!({"job": job}))
    }
}
