//! Handlers for session lifecycle, history, entries, subscription, fork/resume
//! and prompt answering, with the error mappings they share.

use super::*;

impl DaemonServer {
    /// `session.create` params:
    /// `{"scene": "...", "project_root": "...", "options": {...}}`.
    /// `scene` omitted means the pool's default scene (`SessionPool::
    /// resolve_scene`); given, must be currently active
    /// (`SCENE_NOT_FOUND` otherwise — see `scene.list`/`scene.activate`).
    /// `project_root` disambiguates three ways — the
    /// key omitted means "this pool's default project"; present as JSON
    /// `null` means a genuine no-project session; present as a string means
    /// that project (rejected with `PROJECT_NOT_FOUND` if it doesn't exist
    /// as a directory). See `SessionPool::create_session` /
    /// `session_pool::ProjectSelector`.
    pub(super) async fn method_session_create(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
    ) -> RpcResponse {
        let project_root = match params.get("project_root") {
            None => crate::session_pool::ProjectSelector::Default,
            Some(v) if v.is_null() => crate::session_pool::ProjectSelector::NoProject,
            Some(v) => match v.as_str() {
                Some(s) => crate::session_pool::ProjectSelector::Path(std::path::PathBuf::from(s)),
                None => {
                    return RpcResponse::err(
                        id,
                        codes::INVALID_PARAMS,
                        "project_root must be a string or null",
                    )
                }
            },
        };
        let scene = params.get("scene").and_then(|v| v.as_str());
        let (options, ignored_options) = match read_options("session.create", &id, &params) {
            Ok(read) => read,
            Err(refusal) => return refusal,
        };

        let resp = match self
            .pool
            .create_session(scene, project_root, options.as_ref())
            .await
        {
            Ok(result) => RpcResponse::ok(id, result),
            Err(e) => failure(id, e),
        };
        reporting_ignored(resp, ignored_options)
    }

    /// `session.get` params: `{"session_id": "..."}`.
    ///
    /// The same summary `session.list` reports for this one session, plus
    /// `turn_state` — which only the live pool knows, and which a client
    /// needs to decide whether its send button should be enabled.
    pub(super) async fn method_session_get(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
    ) -> RpcResponse {
        let Some(session_id) = params.get("session_id").and_then(|v| v.as_str()) else {
            return RpcResponse::err(id, codes::INVALID_PARAMS, "missing session_id");
        };
        match self.pool.session_detail(session_id).await {
            Ok(detail) => RpcResponse::ok(id, detail),
            Err(e) => failure(id, e),
        }
    }

    /// `session.subscribe` params: `{"session_id": "..."}`.
    ///
    /// Starts sending this session's frames to the calling connection and
    /// returns what the caller needs to catch up: the transcript watermark
    /// and any unanswered permission asks.
    ///
    /// The order matters and is the reason `last_seq` is here at all:
    /// subscribe, *then* read `session.history` up to `last_seq`, then apply
    /// the frames that arrived in between. Reading first and subscribing
    /// second silently loses whatever the session produced in the gap.
    pub(super) async fn method_session_subscribe(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
        client: Arc<Client>,
    ) -> RpcResponse {
        let Some(session_id) = params.get("session_id").and_then(|v| v.as_str()) else {
            return RpcResponse::err(id, codes::INVALID_PARAMS, "missing session_id");
        };
        // Opt-in, and opt-in is the whole design. Resuming costs a transcript
        // read and a slot in a capped pool, and a client drawing a session
        // list subscribes to rows it is only looking at — doing this by
        // default would resume every one of them on its behalf.
        let resume = params
            .get("resume")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let mut report = match self
            .pool
            .subscribe_session(session_id, client.clone())
            .await
        {
            Ok(subscribed) => Some(watch::subscribed_json(&subscribed)),
            Err(crate::session_pool::api::PoolError::SessionNotFound { .. }) if resume => {
                let inferred = match self.pool.resume_session(session_id, false, None).await {
                    Ok(resumed) => resumed["scene_inferred"].clone(),
                    Err(e) => return failure(id, e),
                };
                return match self.pool.subscribe_session(session_id, client).await {
                    Ok(subscribed) => {
                        let mut v = watch::subscribed_json(&subscribed);
                        if let Some(obj) = v.as_object_mut() {
                            obj.insert("resumed".into(), serde_json::json!(true));
                            obj.insert("scene_inferred".into(), inferred);
                        }
                        RpcResponse::ok(id, v)
                    }
                    Err(e) => failure(id, e),
                };
            }
            Err(e) => return failure(id, e),
        };

        if let Some(obj) = report.as_mut().and_then(|v| v.as_object_mut()) {
            obj.insert("resumed".into(), serde_json::json!(false));
        }
        RpcResponse::ok(id, report.unwrap_or_else(|| serde_json::json!({})))
    }

    /// `session.rename` params: `{"session_id": "...", "name": "..."|null}`.
    ///
    /// A missing `name` key and an explicit `null` mean the same thing here —
    /// clear it — because there is nothing else a rename with no name could
    /// be asking for.
    pub(super) async fn method_session_rename(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
    ) -> RpcResponse {
        let Some(session_id) = params.get("session_id").and_then(|v| v.as_str()) else {
            return RpcResponse::err(id, codes::INVALID_PARAMS, "missing session_id");
        };
        let name = params.get("name").and_then(|v| v.as_str());
        match self.pool.rename_session(session_id, name).await {
            Ok(v) => RpcResponse::ok(id, v),
            Err(e) => failure(id, e),
        }
    }

    /// `session.setModel` params: `{"session_id": "...", "model": "..."}`.
    ///
    /// The model a session talks to was fixed when the session was built, and
    /// the only way to change it was to change the configuration and start
    /// over — which costs the conversation. The engine never had that
    /// limitation (`EngineCommand::UpdateModel`); the method table did.
    pub(super) async fn method_session_set_model(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
    ) -> RpcResponse {
        let Some(session_id) = params.get("session_id").and_then(|v| v.as_str()) else {
            return RpcResponse::err(id, codes::INVALID_PARAMS, "missing session_id");
        };
        let named = |key: &str| {
            params
                .get(key)
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
        };
        let (model, provider) = (named("model"), named("provider"));
        if model.is_none() && provider.is_none() {
            return RpcResponse::err(id, codes::INVALID_PARAMS, "missing model");
        }
        respond(
            id,
            self.pool
                .set_session_model(session_id, model, provider)
                .await,
        )
    }

    /// `session.setPrompt` params: `{"session_id": "...", "append": "...",
    /// "override": "..."}`. A string sets the field, `null` clears it back to
    /// the settings files' value, an absent key leaves it.
    pub(super) async fn method_session_set_prompt(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
    ) -> RpcResponse {
        let Some(session_id) = params.get("session_id").and_then(|v| v.as_str()) else {
            return RpcResponse::err(id, codes::INVALID_PARAMS, "missing session_id");
        };
        let mut changes: Vec<(&str, Option<String>)> = Vec::new();
        for key in ["append", "override"] {
            match params.get(key) {
                None => {}
                Some(serde_json::Value::Null) => changes.push((key, None)),
                Some(serde_json::Value::String(text)) => changes.push((key, Some(text.clone()))),
                Some(_) => {
                    return RpcResponse::err(
                        id,
                        codes::INVALID_PARAMS,
                        format!("`{key}` must be a string or null"),
                    )
                }
            }
        }
        if changes.is_empty() {
            return RpcResponse::err(id, codes::INVALID_PARAMS, "give `append` and/or `override`");
        }
        let change = move |prompt: &mut runtime::agent::SessionPrompt| {
            for (key, value) in changes {
                match key {
                    "append" => prompt.append = value,
                    _ => prompt.override_text = value,
                }
            }
        };
        respond(id, self.pool.set_session_prompt(session_id, change).await)
    }

    /// `session.unsubscribe` params: `{"session_id": "..."}`.
    ///
    /// Unsubscribing from a session this connection was not watching is
    /// success, not an error: the caller wanted to stop receiving it and it
    /// is not receiving it.
    pub(super) async fn method_session_unsubscribe(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
        client: Arc<Client>,
    ) -> RpcResponse {
        let Some(session_id) = params.get("session_id").and_then(|v| v.as_str()) else {
            return RpcResponse::err(id, codes::INVALID_PARAMS, "missing session_id");
        };
        self.pool
            .unsubscribe_session(
                session_id,
                crate::session_pool::api::WatcherId(client.id().0),
            )
            .await;
        RpcResponse::ok(
            id,
            serde_json::json!({"session_id": session_id, "subscribed": false}),
        )
    }

    /// `session.interrupt` params: `{"session_id": "..."}`.
    ///
    /// Cancels the in-flight turn and keeps the session. No turn running is
    /// `{"interrupted": false}`, not an error: the caller wanted the session
    /// idle and it is.
    pub(super) async fn method_session_interrupt(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
    ) -> RpcResponse {
        let Some(session_id) = params.get("session_id").and_then(|v| v.as_str()) else {
            return RpcResponse::err(id, codes::INVALID_PARAMS, "missing session_id");
        };
        match self.pool.interrupt_session(session_id).await {
            Ok(interrupted) => RpcResponse::ok(
                id,
                serde_json::json!({"session_id": session_id, "interrupted": interrupted}),
            ),
            Err(e) => failure(id, e),
        }
    }

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

    /// `session.fork` params:
    /// `{"session_id": "...", "at_message": 12}` (`at_message` optional —
    /// omit to copy the whole transcript). Produces a new session id whose
    /// history is an independent copy; see `SessionPool::fork_session`.
    pub(super) async fn method_session_fork(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
    ) -> RpcResponse {
        let session_id = match params.get("session_id").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => return RpcResponse::err(id, codes::INVALID_PARAMS, "missing session_id"),
        };
        // Unlike `limit`, a malformed `at_message` *is* rejected — silently
        // forking somewhere other than where the caller asked would produce
        // a wrong-but-plausible branch, which is worse than an error.
        let at_message = match params.get("at_message") {
            None | Some(serde_json::Value::Null) => None,
            Some(v) => match v.as_u64() {
                Some(n) => Some(n as usize),
                None => {
                    return RpcResponse::err(
                        id,
                        codes::INVALID_PARAMS,
                        "at_message must be a non-negative integer",
                    )
                }
            },
        };

        // Same `docs/daemon_rpc_protocol.md` §3.4 rejection `session.resume` applies — forking a session
        // recorded under a different scene would hand back a "primary"
        // session that starts life already scene-mismatched.

        match self.pool.fork_session(&session_id, at_message).await {
            Ok(result) => RpcResponse::ok(id, result),
            Err(e) => failure(id, e),
        }
    }

    /// `session.resume` params:
    /// `{"session_id": "...", "create_if_missing": false, "options": {...}}`.
    /// The explicit form of `session.run_turn`'s implicit resume — see
    /// `SessionPool::resume_session` for why an unknown id errors here by
    /// default while `run_turn` still creates one.
    pub(super) async fn method_session_resume(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
    ) -> RpcResponse {
        let session_id = match params.get("session_id").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => return RpcResponse::err(id, codes::INVALID_PARAMS, "missing session_id"),
        };
        let create_if_missing = params
            .get("create_if_missing")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let (options, ignored_options) = match read_options("session.resume", &id, &params) {
            Ok(read) => read,
            Err(refusal) => return refusal,
        };

        let resumed = self
            .pool
            .resume_session(&session_id, create_if_missing, options.as_ref())
            .await;
        reporting_ignored(respond(id, resumed), ignored_options)
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
