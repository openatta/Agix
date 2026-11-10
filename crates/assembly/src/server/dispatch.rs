//! Method routing: the `dispatch` match from method name to handler, plus the
//! small helpers handlers share for parameters and options.

use super::*;

impl DaemonServer {
    pub(super) async fn dispatch(&self, req: RpcRequest, client: Arc<Client>) -> RpcResponse {
        let id = req.id.unwrap_or(serde_json::Value::Null);
        match req.method.as_str() {
            "daemon.status" => {
                let count = self.pool.active_count().await;
                let mut result = crate::discovery::identity();
                result["uptime_secs"] = serde_json::json!(self.started_at.elapsed().as_secs());
                result["sessions"] = serde_json::json!(count);
                RpcResponse::ok(id, result)
            }
            "daemon.ping" => {
                let mut result = crate::discovery::identity();
                result["pong"] = serde_json::json!(true);
                RpcResponse::ok(id, result)
            }
            "daemon.doctor" => RpcResponse::ok(id, self.pool.doctor_report().await),
            "daemon.subscribeEvents" => self.method_daemon_subscribe_events(id, client).await,
            "config.setProvider" => self.method_config_set_provider(id, req.params).await,
            "config.getProvider" => self.method_config_get_provider(id, req.params).await,
            // No params. Re-reads all three settings.json tiers from disk
            // as-is — for a human (or another process) who hand-edited a
            // file directly instead of going through `config.setProvider`.
            // Shares `SessionPool::apply_reloaded_settings()` with
            // `config.setProvider`, so the two converge on identical
            // semantics (routing re-resolved, task_router rebuilt,
            // already-running sessions recreate themselves lazily on their
            // next turn — see `SessionPool::run_turn`).
            "config.reload" => self.method_config_reload(id, req.params).await,
            "config.get" => self.method_config_get(id, req.params).await,
            "project.status" => self.method_project_status(id, req.params).await,
            "project.trust" => self.method_project_trust(id, req.params).await,
            #[cfg(feature = "mcp")]
            "mcp.status" => RpcResponse::ok(
                id,
                serde_json::json!({"servers": self.pool.mcp_status().await}),
            ),
            #[cfg(feature = "mcp")]
            "mcp.addServer" => self.method_mcp_put_server(id, req.params, false).await,
            #[cfg(feature = "mcp")]
            "mcp.updateServer" => self.method_mcp_put_server(id, req.params, true).await,
            #[cfg(feature = "mcp")]
            "mcp.auth.start" => self.method_mcp_auth_start(id, req.params).await,
            #[cfg(feature = "mcp")]
            "mcp.auth.complete" => self.method_mcp_auth_complete(id, req.params).await,
            #[cfg(feature = "mcp")]
            "mcp.auth.cancel" => self.method_mcp_auth_cancel(id, req.params).await,
            #[cfg(feature = "mcp")]
            "mcp.auth.logout" => self.method_mcp_auth_logout(id, req.params).await,
            #[cfg(feature = "mcp")]
            "mcp.auth.setIdentity" => self.method_mcp_auth_set_identity(id, req.params).await,
            #[cfg(not(feature = "mcp"))]
            "mcp.status"
            | "mcp.addServer"
            | "mcp.updateServer"
            | "mcp.auth.start"
            | "mcp.auth.complete"
            | "mcp.auth.cancel"
            | "mcp.auth.logout"
            | "mcp.auth.setIdentity" => module_not_compiled(id, &req.method, "mcp"),
            "commands.list" => RpcResponse::ok(
                id,
                serde_json::json!({"commands": self.pool.list_commands().await}),
            ),
            "plugin.list" if !self.plugins_enabled() => self.plugins_disabled(id),
            "plugin.install" | "plugin.uninstall" | "plugin.enable" | "plugin.disable"
            | "plugin.reload"
                if !self.plugins_enabled() =>
            {
                self.plugins_disabled(id)
            }
            "plugin.list" => RpcResponse::ok(
                id,
                serde_json::json!({"plugins": self.pool.list_plugins().await}),
            ),
            "plugin.reload" => RpcResponse::ok(id, self.pool.reload_plugins().await),
            "plugin.install" => self.method_plugin_install(id, req.params).await,
            "plugin.uninstall" => self.method_plugin_uninstall(id, req.params).await,
            "plugin.enable" => self.method_plugin_set_enabled(id, req.params, true).await,
            "plugin.disable" => self.method_plugin_set_enabled(id, req.params, false).await,
            "import.list" => self.method_import_list(id, req.params).await,
            "import.run" => self.method_import_run(id, req.params).await,
            "daemon.shutdown" => {
                self.shutdown().await;
                RpcResponse::ok(id, serde_json::json!({"shutting_down":true}))
            }
            "session.list" => {
                let include_children = req
                    .params
                    .get("include_children")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let parent_session_id =
                    req.params.get("parent_session_id").and_then(|v| v.as_str());
                let sessions = self
                    .pool
                    .list_all(include_children, parent_session_id)
                    .await;
                RpcResponse::ok(id, serde_json::json!({"sessions": sessions}))
            }
            "session.close" => {
                let session_id = match req.params.get("session_id").and_then(|v| v.as_str()) {
                    Some(s) => s.to_string(),
                    None => {
                        return RpcResponse::err(id, codes::INVALID_PARAMS, "missing session_id")
                    }
                };
                match self.pool.shutdown_session(&session_id).await {
                    Ok(sidechains_deleted) => RpcResponse::ok(
                        id,
                        serde_json::json!({
                            "closed": session_id,
                            "sidechains_deleted": sidechains_deleted,
                        }),
                    ),
                    Err(e) => failure(id, e),
                }
            }
            "session.inspectContext" | "session.dropMessages" | "session.editMessage" => {
                self.method_context(id, &req.method, &req.params).await
            }
            "session.tasks" => {
                let Some(session_id) = req.params.get("session_id").and_then(|v| v.as_str()) else {
                    return RpcResponse::err(id, codes::INVALID_PARAMS, "missing session_id");
                };
                if !self.pool.session_exists(session_id).await {
                    return RpcResponse::err(
                        id,
                        codes::SESSION_NOT_FOUND,
                        format!("no session `{session_id}`"),
                    );
                }
                RpcResponse::ok(id, self.pool.session_tasks(session_id).await)
            }
            "session.taskOutput" => {
                let param = |key: &str| req.params.get(key).and_then(|v| v.as_str());
                let (Some(session_id), Some(task_id)) = (param("session_id"), param("task_id"))
                else {
                    return RpcResponse::err(
                        id,
                        codes::INVALID_PARAMS,
                        "session.taskOutput needs session_id and task_id",
                    );
                };
                let given = |key: &str| req.params.get(key).filter(|v| !v.is_null());
                let read = match (given("tail_bytes"), given("from_byte")) {
                    (Some(_), Some(_)) => {
                        return RpcResponse::err(
                            id,
                            codes::INVALID_PARAMS,
                            "session.taskOutput takes tail_bytes or from_byte, not both",
                        )
                    }
                    (None, None) => TaskOutputRead::Tail(DEFAULT_TASK_OUTPUT_TAIL),
                    (Some(v), None) => match v.as_u64() {
                        Some(n) if n > 0 => {
                            TaskOutputRead::Tail((n as usize).min(RunningTask::OUTPUT_KEPT_BYTES))
                        }
                        _ => {
                            return RpcResponse::err(
                                id,
                                codes::INVALID_PARAMS,
                                "tail_bytes must be a positive integer",
                            )
                        }
                    },
                    (None, Some(v)) => match v.as_u64() {
                        Some(n) => TaskOutputRead::From(n),
                        None => {
                            return RpcResponse::err(
                                id,
                                codes::INVALID_PARAMS,
                                "from_byte must be a non-negative integer",
                            )
                        }
                    },
                };
                respond(id, self.pool.task_output(session_id, task_id, read).await)
            }
            "session.stopTask" => {
                let param = |key: &str| req.params.get(key).and_then(|v| v.as_str());
                let (Some(session_id), Some(task_id)) = (param("session_id"), param("task_id"))
                else {
                    return RpcResponse::err(
                        id,
                        codes::INVALID_PARAMS,
                        "session.stopTask needs session_id and task_id",
                    );
                };
                RpcResponse::ok(
                    id,
                    serde_json::json!({"stopped": self.pool.stop_task(session_id, task_id).await}),
                )
            }
            "session.messageTask" => {
                let param = |key: &str| req.params.get(key).and_then(|v| v.as_str());
                let (Some(session_id), Some(task_id), Some(message)) =
                    (param("session_id"), param("task_id"), param("message"))
                else {
                    return RpcResponse::err(
                        id,
                        codes::INVALID_PARAMS,
                        "session.messageTask needs session_id, task_id and message",
                    );
                };
                match self
                    .pool
                    .message_task(session_id, task_id, message.to_string())
                    .await
                {
                    None => RpcResponse::err(
                        id,
                        codes::SESSION_NOT_FOUND,
                        format!("session `{session_id}` is not running"),
                    ),
                    Some(Ok(())) => RpcResponse::ok(id, serde_json::json!({"delivered": true})),
                    Some(Err(reason)) => RpcResponse::ok(
                        id,
                        serde_json::json!({"delivered": false, "reason": reason}),
                    ),
                }
            }
            #[cfg(feature = "agents")]
            "session.startAgent" => self.method_start_agent(id, req.params).await,
            #[cfg(not(feature = "agents"))]
            "session.startAgent" => module_not_compiled(id, &req.method, "agents"),
            "schedule.create" => self.method_schedule_create(id, req.params).await,
            "schedule.list" => {
                let session_id = req.params.get("session_id").and_then(|v| v.as_str());
                RpcResponse::ok(
                    id,
                    serde_json::json!({"jobs": self.pool.schedule_list(session_id)}),
                )
            }
            "schedule.delete" => match req.params.get("id").and_then(|v| v.as_str()) {
                Some(job) => RpcResponse::ok(
                    id,
                    serde_json::json!({"deleted": self.pool.schedule_remove(job)}),
                ),
                None => RpcResponse::err(id, codes::INVALID_PARAMS, "missing id"),
            },
            "session.delete" => {
                let session_id = match req.params.get("session_id").and_then(|v| v.as_str()) {
                    Some(s) => s.to_string(),
                    None => {
                        return RpcResponse::err(id, codes::INVALID_PARAMS, "missing session_id")
                    }
                };
                let dry_run = req
                    .params
                    .get("dry_run")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                respond(id, self.pool.delete_session(&session_id, dry_run).await)
            }
            "session.create" => self.method_session_create(id, req.params).await,
            "session.run_turn" => self.method_session_run_turn(id, req.params, client).await,
            "session.respondToPrompt" => {
                self.method_session_respond_to_prompt(id, req.params).await
            }
            "session.get" => self.method_session_get(id, req.params).await,
            "session.subscribe" => self.method_session_subscribe(id, req.params, client).await,
            "session.unsubscribe" => {
                self.method_session_unsubscribe(id, req.params, client)
                    .await
            }
            "session.interrupt" => self.method_session_interrupt(id, req.params).await,
            "session.compact" => {
                self.method_session_command(id, req.params, client, "/compact")
                    .await
            }
            "session.clear" => {
                self.method_session_command(id, req.params, client, "/clear")
                    .await
            }
            "session.history" => self.method_session_history(id, req.params).await,
            "session.fork" => self.method_session_fork(id, req.params).await,
            "session.resume" => self.method_session_resume(id, req.params).await,
            "session.setModel" => self.method_session_set_model(id, req.params).await,
            "session.setPrompt" => self.method_session_set_prompt(id, req.params).await,
            "session.rename" => self.method_session_rename(id, req.params).await,
            "session.goal" => self.method_session_goal(id, req.params).await,
            "session.setGoal" | "session.updateGoal" => {
                self.method_session_change_goal(id, &req.method, req.params)
                    .await
            }
            "session.appendEntry" => self.method_session_append_entry(id, req.params).await,
            "session.entries" => self.method_session_entries(id, req.params).await,
            "session.export" => self.method_session_export(id, req.params).await,
            "scene.reload" => RpcResponse::ok(id, self.pool.reload_scenes().await),
            "scene.register" => self.method_scene_register(id, req.params).await,
            "scene.unregister" => match req.params.get("scene").and_then(|v| v.as_str()) {
                Some(scene) => match self.pool.unregister_scene(scene).await {
                    Ok(()) => RpcResponse::ok(
                        id,
                        serde_json::json!({ "scene": scene, "registered": false }),
                    ),
                    Err(e) => failure(id, e),
                },
                None => RpcResponse::err(id, codes::INVALID_PARAMS, "missing scene"),
            },
            "scene.list" => RpcResponse::ok(
                id,
                serde_json::json!({"scenes": self.pool.list_scenes().await}),
            ),
            "scene.describe" => self.method_scene_describe(id, req.params).await,
            "scene.activate" => {
                let scene_id = match req.params.get("scene").and_then(|v| v.as_str()) {
                    Some(s) => s.to_string(),
                    None => return RpcResponse::err(id, codes::INVALID_PARAMS, "missing scene"),
                };
                match self.pool.activate_scene(&scene_id).await {
                    Ok(()) => {
                        RpcResponse::ok(id, serde_json::json!({"scene": scene_id, "active": true}))
                    }
                    Err(e) => failure(id, e),
                }
            }
            "scene.deactivate" => {
                let scene_id = match req.params.get("scene").and_then(|v| v.as_str()) {
                    Some(s) => s.to_string(),
                    None => return RpcResponse::err(id, codes::INVALID_PARAMS, "missing scene"),
                };
                match self.pool.deactivate_scene(&scene_id).await {
                    Ok(()) => RpcResponse::ok(id, serde_json::json!({"scene": scene_id})),
                    Err(e) => failure(id, e),
                }
            }
            _ => RpcResponse::err(
                id,
                codes::METHOD_NOT_FOUND,
                format!("unknown: {}", req.method),
            ),
        }
    }
}

/// `params[name]`, parsed; the message says which of missing or malformed.
pub(super) fn parse_param<T: serde::de::DeserializeOwned>(
    params: &serde_json::Value,
    name: &str,
) -> Result<T, String> {
    match params.get(name).cloned().map(serde_json::from_value) {
        Some(Ok(v)) => Ok(v),
        Some(Err(e)) => Err(format!("bad {name}: {e}")),
        None => Err(format!("missing {name}")),
    }
}

/// A method this protocol defines whose module this build was compiled
/// without: `METHOD_NOT_FOUND`, with the module named so a client can tell
/// "not in this build" from a typo.
#[cfg_attr(all(feature = "mcp", feature = "agents"), allow(dead_code))]
pub(super) fn module_not_compiled(
    id: serde_json::Value,
    method: &str,
    module: &'static str,
) -> RpcResponse {
    failure(
        id,
        crate::session_pool::api::PoolError::ModuleNotCompiled {
            method: method.to_string(),
            module,
        },
    )
}

/// `params.options` as [`crate::rpc::SessionOptions::from_params`] reads it,
/// with a warning for each field it left out — or, when the options ask for
/// `strict`, an `INVALID_PARAMS` response naming them instead.
#[allow(clippy::result_large_err)]
pub(super) fn read_options(
    method: &str,
    id: &serde_json::Value,
    params: &serde_json::Value,
) -> Result<
    (
        Option<crate::rpc::SessionOptions>,
        Vec<crate::rpc::IgnoredOption>,
    ),
    RpcResponse,
> {
    let (options, ignored) = crate::rpc::SessionOptions::from_params(params);
    if !ignored.is_empty() && crate::rpc::SessionOptions::strict_requested(params) {
        let fields: Vec<&str> = ignored
            .iter()
            .map(|i| i.field.as_deref().unwrap_or("options"))
            .collect();
        return Err(RpcResponse::err_with_data(
            id.clone(),
            codes::INVALID_PARAMS,
            format!("options that do not read: {}", fields.join(", ")),
            serde_json::json!({ "invalid_options": ignored }),
        ));
    }
    for left_out in &ignored {
        warn!(
            method,
            field = left_out.field.as_deref().unwrap_or("options"),
            error = %left_out.error,
            "ignored part of `options`"
        );
    }
    Ok((options, ignored))
}

/// `resp` with what `options` left out listed in its result as
/// `ignored_options`.
pub(super) fn reporting_ignored(
    mut resp: RpcResponse,
    ignored: Vec<crate::rpc::IgnoredOption>,
) -> RpcResponse {
    if ignored.is_empty() {
        return resp;
    }
    if let Some(result) = resp.result.as_mut().and_then(|r| r.as_object_mut()) {
        result.insert(
            "ignored_options".to_string(),
            serde_json::to_value(ignored).unwrap_or_default(),
        );
    }
    resp
}
