//! Handlers for session lifecycle, history, entries, subscription, fork/resume
//! and prompt answering, with the error mappings they share.

use super::*;

/// A resume that could not happen, as the wire sees it.
///
/// Shared by `session.resume` and by `session.subscribe {resume: true}`:
/// opening a session that is only on disk fails for the same reasons however
/// the caller asked, and a second copy of this mapping would be a second set
/// of error codes to keep in step.
pub(super) fn resume_failure(
    id: serde_json::Value,
    e: crate::session_pool::ResumeError,
) -> RpcResponse {
    match e {
        crate::session_pool::ResumeError::SidechainTerminal {
            session_id,
            parent_session_id,
            final_state,
        } => {
            let final_state = match final_state {
                history::entry::SessionEndState::Completed => "completed",
                history::entry::SessionEndState::Failed => "failed",
            };
            RpcResponse::err_with_data(
                id,
                codes::SIDECHAIN_TERMINAL,
                "sidechain session already ran to completion",
                serde_json::json!({
                    "session_id": session_id,
                    "parent_session_id": parent_session_id,
                    "final_state": final_state,
                }),
            )
        }
        crate::session_pool::ResumeError::Rpc((code, message)) => {
            RpcResponse::err(id, code, message)
        }
    }
}

/// `SESSION_BUSY` for a change that needs the session rebuilt: a turn holds
/// it, or background tasks run that a rebuild would stop, named in
/// `running_task_ids`.
pub(super) fn rebuild_refused(
    id: serde_json::Value,
    session_id: &str,
    current_turn_id: Option<String>,
    running_task_ids: Vec<String>,
) -> RpcResponse {
    let mut data = serde_json::json!({
        "session_id": session_id,
        "current_turn_id": current_turn_id,
    });
    if !running_task_ids.is_empty() {
        data["running_task_ids"] = serde_json::json!(running_task_ids);
    }
    RpcResponse::err_with_data(id, codes::SESSION_BUSY, "session is busy", data)
}

impl DaemonServer {
    /// Shared `SCENE_MISMATCH` guard for the session-scoped RPCs that only
    /// need a pass/fail answer (unlike `session.resume`, which also reports
    /// `scene_inferred` on success and so keeps its own inline check) —
    /// `session.close`/`.delete`/`.fork` all reject outright rather than
    /// operating across a scene boundary they weren't asked to cross.
    /// Returns the ready-made error response when the caller should stop,
    /// `None` when it's clear to proceed.
    pub(super) async fn scene_mismatch_response(
        &self,
        id: &serde_json::Value,
        session_id: &str,
    ) -> Option<RpcResponse> {
        if let crate::session_pool::SceneCheck::Mismatch(recorded_scene) =
            self.pool.check_scene(session_id).await
        {
            return Some(RpcResponse::err_with_data(
                id.clone(),
                codes::SCENE_MISMATCH,
                "scene mismatch",
                serde_json::json!({
                    "session_id": session_id,
                    "recorded_scene": recorded_scene,
                    "requested_scene": self.pool.scene_id(),
                }),
            ));
        }
        None
    }

    /// Default page size for `session.history` when the caller doesn't say.
    /// Big enough to render a typical conversation in one round trip, small
    /// enough that a forgotten `limit` can't turn one RPC into a
    /// multi-megabyte line on the socket.
    const HISTORY_DEFAULT_LIMIT: usize = 100;
    /// Hard ceiling on `session.history`'s `limit`. A request for more is
    /// clamped, not rejected — the response echoes the `limit` actually
    /// applied plus `total`/`has_more`, so a caller can always tell it was
    /// capped and page for the rest.
    const HISTORY_MAX_LIMIT: usize = 500;

    /// `session.history` params:
    /// `{"session_id": "...", "offset": 0, "limit": 100}` (`offset`/`limit`
    /// optional). Returns a bounded page of the projected transcript — see
    /// `SessionPool::session_history` for what "projected" means and why the
    /// on-disk log is the source rather than the live engine's memory.
    pub(super) async fn method_session_history(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
    ) -> RpcResponse {
        let session_id = match params.get("session_id").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => return RpcResponse::err(id, codes::INVALID_PARAMS, "missing session_id"),
        };
        // A negative/huge/non-integer value clamps to the default rather
        // than erroring: `limit` is a hint about response size, and failing
        // a transcript fetch over it helps nobody.
        let offset = params
            .get("offset")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
            .min(usize::MAX as u64) as usize;
        let limit = params
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| (n as usize).min(Self::HISTORY_MAX_LIMIT))
            .unwrap_or(Self::HISTORY_DEFAULT_LIMIT);

        match self.pool.session_history(&session_id, offset, limit).await {
            Ok(result) => RpcResponse::ok(id, result),
            Err((code, message)) => RpcResponse::err(id, code, message),
        }
    }

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
            Err((code, message)) => RpcResponse::err(id, code, message),
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
            Err((code, message)) => RpcResponse::err(id, code, message),
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
        if let Some(err) = self.scene_mismatch_response(&id, session_id).await {
            return err;
        }

        let mut report = match self
            .pool
            .subscribe_session(session_id, client.clone())
            .await
        {
            Ok(v) => Some(v),
            Err((code, _)) if resume && code == codes::SESSION_NOT_FOUND => {
                // `docs/daemon_rpc_protocol.md` §3.4 still applies: the mismatch check above ran first, and
                // this records the pre-v2 case the same way `session.resume`
                // reports it.
                let inferred = matches!(
                    self.pool.check_scene(session_id).await,
                    crate::session_pool::SceneCheck::Inferred
                );
                match self.pool.resume_session(session_id, false, None).await {
                    Ok(_) => match self.pool.subscribe_session(session_id, client).await {
                        Ok(mut v) => {
                            if let Some(obj) = v.as_object_mut() {
                                obj.insert("resumed".into(), serde_json::json!(true));
                                obj.insert("scene_inferred".into(), serde_json::json!(inferred));
                            }
                            return RpcResponse::ok(id, v);
                        }
                        Err((code, message)) => return RpcResponse::err(id, code, message),
                    },
                    Err(e) => return resume_failure(id, e),
                }
            }
            Err((code, message)) => return RpcResponse::err(id, code, message),
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
        if let Some(err) = self.scene_mismatch_response(&id, session_id).await {
            return err;
        }
        match self.pool.rename_session(session_id, name).await {
            Ok(v) => RpcResponse::ok(id, v),
            Err((code, message)) => RpcResponse::err(id, code, message),
        }
    }

    pub(super) async fn method_session_goal(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
    ) -> RpcResponse {
        let Some(session_id) = params.get("session_id").and_then(|v| v.as_str()) else {
            return RpcResponse::err(id, codes::INVALID_PARAMS, "missing session_id");
        };
        match self.pool.session_goal(session_id).await {
            Ok(v) => RpcResponse::ok(id, v),
            Err((code, message)) => RpcResponse::err(id, code, message),
        }
    }

    pub(super) async fn method_session_change_goal(
        &self,
        id: serde_json::Value,
        method: &str,
        params: serde_json::Value,
    ) -> RpcResponse {
        use crate::session_pool::GoalChange;
        let Some(session_id) = params.get("session_id").and_then(|v| v.as_str()) else {
            return RpcResponse::err(id, codes::INVALID_PARAMS, "missing session_id");
        };
        let objective = params
            .get("objective")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let max_rounds = match params.get("max_rounds") {
            None | Some(serde_json::Value::Null) => None,
            Some(v) => match v.as_u64().and_then(|n| u32::try_from(n).ok()) {
                Some(n) => Some(n),
                None => {
                    return RpcResponse::err(
                        id,
                        codes::INVALID_PARAMS,
                        "max_rounds must be a positive integer",
                    )
                }
            },
        };
        let change = if method == "session.setGoal" {
            let Some(objective) = objective else {
                return RpcResponse::err(id, codes::INVALID_PARAMS, "missing objective");
            };
            GoalChange::Set {
                objective,
                max_rounds,
            }
        } else {
            match params.get("action").and_then(|v| v.as_str()) {
                Some("pause") => GoalChange::Pause,
                Some("resume") => GoalChange::Resume,
                Some("clear") => GoalChange::Clear,
                Some("edit") => GoalChange::Edit {
                    objective,
                    max_rounds,
                },
                _ => {
                    return RpcResponse::err(
                        id,
                        codes::INVALID_PARAMS,
                        "action must be one of pause, resume, edit, clear",
                    )
                }
            }
        };
        if let Some(err) = self.scene_mismatch_response(&id, session_id).await {
            return err;
        }
        match self.pool.change_session_goal(session_id, change).await {
            Ok(v) => RpcResponse::ok(id, v),
            Err((code, message)) => RpcResponse::err(id, code, message),
        }
    }

    pub(super) async fn method_session_append_entry(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
    ) -> RpcResponse {
        let text = |key: &str| params.get(key).and_then(|v| v.as_str());
        let (Some(session_id), Some(ns), Some(event)) =
            (text("session_id"), text("ns"), text("event"))
        else {
            return RpcResponse::err(
                id,
                codes::INVALID_PARAMS,
                "session.appendEntry needs session_id, ns and event",
            );
        };
        if let Some(err) = self.scene_mismatch_response(&id, session_id).await {
            return err;
        }
        let payload = params
            .get("payload")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        match self.pool.append_entry(session_id, ns, event, payload).await {
            Ok(v) => RpcResponse::ok(id, v),
            Err((code, message)) => RpcResponse::err(id, code, message),
        }
    }

    pub(super) async fn method_session_entries(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
    ) -> RpcResponse {
        let Some(session_id) = params.get("session_id").and_then(|v| v.as_str()) else {
            return RpcResponse::err(id, codes::INVALID_PARAMS, "missing session_id");
        };
        let namespaces: Option<Vec<String>> = match params.get("namespaces") {
            None | Some(serde_json::Value::Null) => None,
            Some(v) => match serde_json::from_value(v.clone()) {
                Ok(list) => Some(list),
                Err(_) => {
                    return RpcResponse::err(
                        id,
                        codes::INVALID_PARAMS,
                        "namespaces is a list of strings",
                    )
                }
            },
        };
        match self
            .pool
            .list_entries(session_id, namespaces.as_deref())
            .await
        {
            Ok(v) => RpcResponse::ok(id, v),
            Err((code, message)) => RpcResponse::err(id, code, message),
        }
    }

    /// `session.export` params:
    /// `{"session_id", "path", "format"?: "zip" | "dir", "include"?: [...]}`.
    /// See `SessionPool::export_session`.
    pub(super) async fn method_session_export(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
    ) -> RpcResponse {
        let text = |key: &str| params.get(key).and_then(|v| v.as_str());
        let (Some(session_id), Some(path)) = (text("session_id"), text("path")) else {
            return RpcResponse::err(
                id,
                codes::INVALID_PARAMS,
                "session.export needs session_id and path",
            );
        };
        let format = match params.get("format") {
            None | Some(serde_json::Value::Null) => crate::export::ArchiveFormat::Zip,
            Some(v) => match v.as_str().and_then(crate::export::ArchiveFormat::parse) {
                Some(format) => format,
                None => return RpcResponse::err(id, codes::INVALID_PARAMS, "format is zip or dir"),
            },
        };
        let include = match params.get("include") {
            None | Some(serde_json::Value::Null) => crate::export::Include::default(),
            Some(v) => match serde_json::from_value::<Vec<String>>(v.clone()) {
                Ok(names) => match crate::export::Include::parse(&names) {
                    Ok(include) => include,
                    Err(message) => return RpcResponse::err(id, codes::INVALID_PARAMS, message),
                },
                Err(_) => {
                    return RpcResponse::err(
                        id,
                        codes::INVALID_PARAMS,
                        "include is a list of strings",
                    )
                }
            },
        };
        let request = crate::export::ExportRequest {
            session_id: session_id.to_string(),
            path: std::path::PathBuf::from(path),
            format,
            include,
        };
        match self.pool.export_session(request).await {
            Ok(v) => RpcResponse::ok(id, v),
            Err((code, message)) => RpcResponse::err(id, code, message),
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
        if let Some(err) = self.scene_mismatch_response(&id, session_id).await {
            return err;
        }
        match self
            .pool
            .set_session_model(session_id, model, provider)
            .await
        {
            Ok(v) => RpcResponse::ok(id, v),
            Err(crate::session_pool::SetModelError::Busy {
                current_turn_id,
                running_task_ids,
            }) => rebuild_refused(id, session_id, current_turn_id, running_task_ids),
            Err(crate::session_pool::SetModelError::Rpc((code, message))) => {
                RpcResponse::err(id, code, message)
            }
        }
    }

    /// `scene.register` params: `{"scene": {…scene.json…}, "prompt": "…"}` —
    /// `scene` as an object or as the JSON text.
    pub(super) async fn method_scene_register(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
    ) -> RpcResponse {
        let scene_json = match params.get("scene") {
            Some(serde_json::Value::String(text)) => text.clone(),
            Some(v @ serde_json::Value::Object(_)) => v.to_string(),
            _ => return RpcResponse::err(id, codes::INVALID_PARAMS, "missing scene"),
        };
        let Some(prompt) = params.get("prompt").and_then(|v| v.as_str()) else {
            return RpcResponse::err(id, codes::INVALID_PARAMS, "missing prompt");
        };
        match self
            .pool
            .register_scene_from_sources(&scene_json, prompt)
            .await
        {
            Ok(scene) => RpcResponse::ok(
                id,
                serde_json::json!({ "scene": scene, "registered": true }),
            ),
            Err((code, message)) => RpcResponse::err(id, code, message),
        }
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
        if let Some(err) = self.scene_mismatch_response(&id, session_id).await {
            return err;
        }
        let change = move |prompt: &mut runtime::agent::SessionPrompt| {
            for (key, value) in changes {
                match key {
                    "append" => prompt.append = value,
                    _ => prompt.override_text = value,
                }
            }
        };
        match self.pool.set_session_prompt(session_id, change).await {
            Ok(v) => RpcResponse::ok(id, v),
            Err(crate::session_pool::SetModelError::Busy {
                current_turn_id,
                running_task_ids,
            }) => rebuild_refused(id, session_id, current_turn_id, running_task_ids),
            Err(crate::session_pool::SetModelError::Rpc((code, message))) => {
                RpcResponse::err(id, code, message)
            }
        }
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
        self.pool.unsubscribe_session(session_id, client.id()).await;
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
        if let Some(err) = self.scene_mismatch_response(&id, session_id).await {
            return err;
        }
        match self.pool.interrupt_session(session_id).await {
            Ok(interrupted) => RpcResponse::ok(
                id,
                serde_json::json!({"session_id": session_id, "interrupted": interrupted}),
            ),
            Err((code, message)) => RpcResponse::err(id, code, message),
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
        if let Some(err) = self.scene_mismatch_response(&id, session_id).await {
            return err;
        }
        self.pool
            .run_turn(
                Some(session_id.to_string()),
                command.to_string(),
                vec![],
                Id::new().to_string(),
                client,
                id,
                None,
                crate::session_pool::OnBusy::Reject,
                "client",
            )
            .await
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
        if let Some(err) = self.scene_mismatch_response(&id, &session_id).await {
            return err;
        }

        match self.pool.fork_session(&session_id, at_message).await {
            Ok(result) => RpcResponse::ok(id, result),
            Err((code, message)) => RpcResponse::err(id, code, message),
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

        // `docs/daemon_rpc_protocol.md` §3.4: a session recorded under a different scene than this
        // daemon's must not be silently resumed into it — that's exactly
        // the "conversation ends up hanging off the wrong scene" failure
        // mode the design calls out as the one worth actively catching.
        let scene_inferred = match self.pool.check_scene(&session_id).await {
            crate::session_pool::SceneCheck::Mismatch(recorded_scene) => {
                return RpcResponse::err_with_data(
                    id,
                    codes::SCENE_MISMATCH,
                    "scene mismatch",
                    serde_json::json!({
                        "session_id": session_id,
                        "recorded_scene": recorded_scene,
                        "requested_scene": self.pool.scene_id(),
                    }),
                );
            }
            crate::session_pool::SceneCheck::Inferred => true,
            crate::session_pool::SceneCheck::Matches => false,
        };

        // A sidechain that already reached a terminal state has no
        // continuation semantics. That check happens inside `resume_session`,
        // under its per-`session_id` lock: checked out here it would race a
        // concurrent cascade delete of the same id and skip the
        // already-active fast path.
        match self
            .pool
            .resume_session(&session_id, create_if_missing, options.as_ref())
            .await
        {
            Ok(mut result) => {
                if let Some(obj) = result.as_object_mut() {
                    obj.insert(
                        "scene_inferred".to_string(),
                        serde_json::json!(scene_inferred),
                    );
                }
                reporting_ignored(RpcResponse::ok(id, result), ignored_options)
            }
            Err(e) => resume_failure(id, e),
        }
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
            Err((code, message)) => RpcResponse::err(id, code, message),
        }
    }
}
