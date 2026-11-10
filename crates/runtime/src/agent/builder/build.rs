//! `Builder::build`: assembles the registry, session state, modules and the `Agent` itself.

use super::*;

impl Builder {
    pub fn build(self) -> Result<(Agent, EventReceiver, InputSender), EngineError> {
        let scene = self
            .scene
            .ok_or_else(|| EngineError::Internal("scene required".into()))?;
        let modules = crate::module::permitted_by(&self.modules, scene.as_ref())
            .map_err(|e| EngineError::Internal(e.to_string()))?;
        crate::module::check_order(&modules).map_err(|e| EngineError::Internal(e.to_string()))?;
        let model = self
            .model
            .ok_or_else(|| EngineError::Internal("model required".into()))?;
        let tools = self
            .tools
            .unwrap_or_else(|| Arc::new(InMemoryToolRegistry::new()));
        // Per-session tool registry, seeded from whatever was passed in (a
        // daemon's shared, immutable-after-startup template of built-in
        // tools — see `SessionPool.tools`). Every per-session-stateful tool
        // this function registers below (`Skill`/`Agent`/`TeamCreate`/
        // `TeamDelete`/`TaskStop`/`TaskOutput`/MCP adapters)
        // goes into *this* fresh copy, not the caller-supplied one: in a
        // daemon the caller's `Arc` is shared by every session, and
        // `InMemoryToolRegistry::register` appends without replacing by name.
        //
        // The scene's deferred policy is *not* applied here — it runs once,
        // after every registration below, so it can reach the engine-state
        // tools too. Applying it here as well would only wrap the built-ins a
        // second time.
        let tools = {
            let session_tools = Arc::new(InMemoryToolRegistry::new());
            let mut had_tool_search = false;
            for t in tools.list() {
                // `ToolSearchTool` closes over the registry it searches. The
                // instance in the incoming template closes over the
                // *template*, so it would report on the template's tools —
                // which have neither this session's deferred wrappers nor any
                // of the engine-state tools registered below. Drop it and
                // rebind one to this session's registry instead. (Same `Arc`,
                // read at call time, so later registrations are visible.)
                if t.name() == "ToolSearch" {
                    had_tool_search = true;
                    continue;
                }
                session_tools.register(t);
            }
            if had_tool_search {
                session_tools.register(Arc::new(tools::tool_search::ToolSearchTool::new(
                    session_tools.clone(),
                )));
            }
            session_tools
        };

        let settings = self
            .settings
            .ok_or_else(|| EngineError::Internal("settings required".into()))?;

        // Telemetry: use pre-built handle if injected, else noop (events
        // silently dropped).
        let telemetry_handle = self.telemetry_handle_override.unwrap_or_else(|| {
            let (tx, _rx) = tokio::sync::mpsc::channel(1);
            TelemetryHandle::new(tx)
        });
        // Every call this session makes is accounted for, including the
        // ones no turn asked for — see `telemetry::model::TelemetryModel`.
        // Wrapped here rather than further down because the sub-agent spawner
        // built below takes its own copy of the model, and a sub-agent's
        // memory extraction costs exactly as much as a top-level one's.
        let model: Arc<dyn Model> = Arc::new(telemetry::model::TelemetryModel::new(
            model,
            telemetry_handle.clone(),
            settings.model.model_name.clone(),
        ));
        let permission = self.permission.unwrap_or_else(|| {
            struct AllowAll;
            #[async_trait::async_trait]
            impl Permission for AllowAll {
                async fn check(
                    &self,
                    _: &str,
                    _: &serde_json::Value,
                    _: &std::path::Path,
                    _: &str,
                ) -> base::interface::permission::PermissionOutcome {
                    base::interface::permission::PermissionOutcome::Permit
                }
            }
            Arc::new(AllowAll)
        });
        let instruction_file = self.instruction_file.or(settings.instruction_file.clone());
        // Pre-read AGENTS.md / CLAUDE.md content for userContext injection.
        let instruction_provider: Arc<
            dyn base::interface::instruction_provider::InstructionProvider,
        > = self.instruction_provider.clone().unwrap_or_else(|| {
            Arc::new(base::interface::instruction_provider::InstructionFile::new(
                instruction_file.clone(),
            ))
        });
        let claude_md_content = {
            let docs = instruction_provider.documents();
            if docs.is_empty() {
                None
            } else {
                Some(
                    docs.into_iter()
                        .map(|d| d.content)
                        .collect::<Vec<_>>()
                        .join("\n\n"),
                )
            }
        };
        let environment = self
            .environment
            .clone()
            .unwrap_or_else(|| Arc::new(base::interface::environment::SystemEnvironment));
        let memory_store = self.memory_store.unwrap_or_else(|| {
            Arc::new(MemoryStore::for_paths(
                &base::paths::ConfigPaths::from_settings(&settings.paths),
            ))
        });
        // Session: `self.history_store` (set via `Builder::history_store(...)`)
        // makes this incrementally persisted to disk once per turn; `None`
        // (the default) keeps it in-memory only.
        let mut session = SessionManager::new(
            self.history_store.clone(),
            self.session_id,
            self.parent_session_id.clone(),
        );
        // Session-memory sidecar (`session_memory.md`) — only for sessions
        // that actually persist (matches `resume`'s only being meaningful
        // with a `HistoryStore`; an in-memory-only session's id doesn't
        // outlive the process, so a sidecar for it would just be orphaned
        // disk state nothing ever reads back). File creation itself happens
        // later, in `warmup()` — this only constructs the handle, no I/O.
        //
        // A caller-supplied id that doesn't parse fails the build outright
        // rather than degrading: `SessionManager::persist` parses the same id
        // before writing anything, so an unparseable one means transcript
        // persistence and the sidecar are both dead for the whole session.
        if self.history_store.is_some() {
            let sid = base::session::SessionId::parse(session.session_id_str()).map_err(|e| {
                EngineError::Internal(format!(
                    "session_id {:?} is not a valid SessionId ({e}); \
                     a history_store requires a parseable id — use \
                     `SessionId::new().to_string()` or omit `Builder::session_id`",
                    session.session_id_str()
                ))
            })?;
            // Every other session sidecar (metadata, input history, prompt
            // state) resolves through `history::path`, so this has to as well,
            // or the memory file lands somewhere nothing looks for it.
            let roots = history::path::HistoryRoots::under(&settings.paths.global_data_dir);
            let path = history::path::session_memory_file(&roots.sessions, &sid);
            session = session.with_session_memory(SessionMemory::new(path));
        }
        let compactor = self
            .compactor
            .unwrap_or_else(|| Arc::new(DefaultCompactor) as Arc<dyn Compactor>);
        let agent_depth = self.agent_depth;
        let crate::plugin::Layers {
            rust: mut rust_layer,
            quickjs: mut quickjs_layer,
        } = {
            let project_root = settings.paths.project_root();
            let plugin_session = crate::plugin::PluginSession {
                session_id: session.session_id_str(),
                project_root: &project_root,
                scene: scene.id(),
                depth: agent_depth,
            };
            crate::plugin::register_all(
                &self.plugins,
                &plugin_session,
                self.prompt_registry.as_deref(),
            )
            .map_err(EngineError::Internal)?
        };
        let plugin_tools: Vec<String> = rust_layer
            .tools
            .iter()
            .chain(&quickjs_layer.tools)
            .map(|t| t.name().to_string())
            .collect();
        let plugin_agent_types: Vec<base::agent_type::AgentTypeDefinition> =
            std::mem::take(&mut rust_layer.agent_types)
                .into_iter()
                .chain(std::mem::take(&mut quickjs_layer.agent_types))
                .collect();
        let agent_spawner = crate::module::LateAgentSpawner::default();
        let session_ended = CancellationToken::new();
        let own_runner = self.hooks.is_none();
        let hooks = self.hooks.unwrap_or_else(|| {
            build_hook_runner(
                &settings,
                model.clone(),
                Arc::new(agent_spawner.clone()),
                Some(session.session_id_str().to_string()),
            )
        });
        // A runner handed in belongs to a session above this one, which wired
        // its wake channel already; taking it over would move every rewake of
        // the tree onto this session's tasks.
        let task_wake = match self.wake_rx {
            Some(rx) => {
                hooks.set_wake_receiver(rx);
                None
            }
            None if own_runner && tokio::runtime::Handle::try_current().is_ok() => {
                let (tx, rx) = mpsc::unbounded_channel();
                hooks.set_wake_receiver(rx);
                Some(tx)
            }
            None => None,
        };
        let skill_catalog = crate::module::LateSkillCatalog::default();
        // Register TaskStopTool — stop running background tasks by ID
        tools.register(std::sync::Arc::new(tools::task_stop::TaskStopTool));
        // Register TaskOutputTool — retrieve output from running/completed tasks
        tools.register(std::sync::Arc::new(tools::task_output::TaskOutputTool));
        tools.register(std::sync::Arc::new(tools::task_wait::TaskWaitTool));
        tools.register(std::sync::Arc::new(tools::task_message::TaskMessageTool));

        let config = Arc::new({
            let mut c = base::context::EngineConfig::from_settings(&settings);
            c.path_style = settings.path_style.unwrap_or_else(|| scene.path_style());
            c.process_groups = self.process_groups.clone();
            c
        });

        // One `SessionState` for the whole session. It is the session's mutable side: permission mode
        // (which `EnterPlanMode`/`ExitPlanMode` flip), the read cache that
        // `Edit`/`Write` consult for read-before-edit staleness, the read
        // dedup marker, file snapshots backing `/rewind`, todos, the task
        // list, running sub-agent tasks, and the accumulated
        // "allow writes here for this project" paths. Every tool call gets
        // the same `Arc`; none of that state works if it does not survive
        // from one call to the next.
        prepare_scratchpad(&settings.paths.local_data_dir.join("scratchpad"));
        let session_state = Arc::new({
            let project = settings.paths.project_root();
            let resolve = |dir: &std::path::PathBuf| project.join(dir);
            let mut state = base::context::SessionState::new(project.clone())
                .with_permission_mode(config.permission_mode);
            state.additional_writable_dirs = settings
                .sandbox
                .additional_directories
                .iter()
                .map(resolve)
                .chain(
                    base::paths::STATE_ROOT_WORKING_DIRS
                        .iter()
                        .map(|dir| settings.paths.global_data_dir.join(dir)),
                )
                .collect();
            state.read_directories = settings
                .sandbox
                .read_directories
                .iter()
                .map(resolve)
                .chain([settings.paths.global_data_dir.clone()])
                .collect();
            state.read_anywhere = settings.sandbox.read_anywhere;
            state.cache_directories = settings.sandbox.cache_directories.paths(&project);
            state.unattended = !scene.interactive();
            state.sandbox_disabled = settings.sandbox.dangerously_disable_sandbox;
            state
        });
        let hook_ambient = hook_ambient(
            session.session_id_str().to_string(),
            settings.paths.project_root().display().to_string(),
            session_state.clone(),
        );

        // One watcher per session tree, started by its root: a delegate shares
        // the root's runner, and a watcher of its own would fire every
        // `FileChanged` once more for each sub-agent ever spawned. The files a
        // session loads as instructions are added when it loads them
        // (`inject_session_context`); the instruction file is known now.
        if agent_depth == 0 && hooks.has_hooks_for(hooks::HookEvent::FileChanged) {
            if let Err(e) = hooks.enable_file_watching(
                &instruction_file.iter().cloned().collect::<Vec<_>>(),
                300,
                hook_ambient.clone(),
            ) {
                tracing::warn!(error = %e, "failed to start FileChanged watcher");
            }
        }

        // WebSearch searches through the session's own model unless
        // `task_models.web_search` routes it elsewhere.
        let (search_model, search_model_name) = match self
            .task_router
            .as_ref()
            .and_then(|router| Some((router, router.model_name_for(WEB_SEARCH_TASK)?)))
        {
            Some((router, name)) => (
                Arc::new(telemetry::model::TelemetryModel::new(
                    router.model_for(WEB_SEARCH_TASK),
                    telemetry_handle.clone(),
                    settings.model.model_name.clone(),
                )) as Arc<dyn Model>,
                name.to_string(),
            ),
            None => (model.clone(), settings.model.model_name.clone()),
        };
        tools::register_web_search(&tools, search_model, search_model_name);

        // Extraction runs only on a model routed for it: the session's own
        // model is too costly to spend reading whole pages.
        let fetch_extraction = self.task_router.as_ref().and_then(|router| {
            let name = router.model_name_for(WEB_FETCH_TASK)?;
            let model = Arc::new(telemetry::model::TelemetryModel::new(
                router.model_for(WEB_FETCH_TASK),
                telemetry_handle.clone(),
                settings.model.model_name.clone(),
            )) as Arc<dyn Model>;
            Some((model, name.to_string()))
        });
        tools::register_web_fetch(&tools, fetch_extraction);

        // Worktree tools — one `WorktreeRegistry` per session (it holds at
        // most one active worktree; `EnterWorktree` refuses a second and
        // `ExitWorktree` cleans up what this session created).
        tools::register_worktree_tools(
            &tools,
            Arc::new(tools::worktree_tools::WorktreeRegistry::new()),
        );

        // Cron tools, and — when no owner handed in a store — the scheduler
        // that drains this session's own. `~/.agix/scenes/<scope>/scheduled_tasks.json`
        // is the exact path `CronCreate`'s own prompt promises the user for
        // `durable: true` jobs; session-only jobs never touch disk.
        //
        // Known limitation of the store-per-session case only: two concurrent
        // sessions in the same scope each load the durable file and each
        // schedule their own fire for a durable job. A session pool avoids it
        // by owning one store and one scheduler (`Builder::cron_store`).
        let fire_jobs_here = self.cron_store.is_none();
        let cron_store = match self.cron_store.clone() {
            Some(store) => store,
            None => {
                let _ = std::fs::create_dir_all(&settings.paths.user_data_dir);
                Arc::new(tools::cron::CronStore::load_or_default(Some(
                    settings.paths.user_data_dir.join("scheduled_tasks.json"),
                )))
            }
        };
        tools::register_cron_tools(&tools, cron_store.clone());

        let (primary_tx, event_rx) = mpsc::unbounded_channel();
        let event_tx = crate::event_bus::EventBus::with_sinks(primary_tx, self.event_sinks);
        // Created before the modules run, because a sub-agent's permission
        // prompts are registered in this same map — see
        // `AgentTool::set_parent_pending_permissions`.
        let pending_permissions: PendingPermissions =
            Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        let pending_questions: crate::elicitation::PendingQuestions = Default::default();

        let mut mcp_slot: Option<Box<dyn base::interface::mcp_servers::McpServers>> = None;
        {
            let session_id = session.session_id_str().to_string();
            let mut cx = crate::module::SessionContext {
                tools: &tools,
                settings: &settings,
                config: &config,
                scene: &scene,
                scene_registry: self.scene_registry.as_ref(),
                model: &model,
                permission: &permission,
                pending_permissions: &pending_permissions,
                hooks: &hooks,
                hook_ambient: &hook_ambient,
                events: &event_tx,
                telemetry: &telemetry_handle,
                session_id: &session_id,
                session_state: &session_state,
                history_store: self.history_store.as_ref(),
                instruction_file: instruction_file.as_ref(),
                depth: agent_depth,
                task_router: self.task_router.as_ref(),
                process_groups: self.process_groups.as_ref(),
                delegate_plugins: &self.plugins,
                plugin_tools: &plugin_tools,
                plugin_agent_types: &plugin_agent_types,
                mcp: &mut mcp_slot,
                modules: &modules,
                agent_spawner: &agent_spawner,
                skill_catalog: &skill_catalog,
                session_ended: &session_ended,
            };
            for module in &modules {
                module
                    .on_session(&mut cx)
                    .map_err(|e| EngineError::Internal(e.to_string()))?;
            }
        }

        let mcp = mcp_slot.unwrap_or_else(|| Box::new(base::interface::mcp_servers::NoMcpServers));
        let command_registry = self.commands_override.clone().unwrap_or_else(|| {
            let mut registry = crate::commands::CommandRegistry::from_skill_catalog(Arc::new(
                skill_catalog.clone(),
            ));
            // MCP prompts become `/mcp__<server>__<prompt>` slash commands,
            // the same way skills become slash commands above. `all_prompts()`
            // is already populated — `McpManager::connect_all` runs
            // `prompts/list` on every server it connects (see
            // `collect_prompts`).
            //
            // Only the registry *this* builder owns can be extended; a
            // `commands_override` is shared across sessions, so the turn loop
            // falls back to resolving `/mcp__…` off the live `McpManager`
            // instead — see the slash-command interception in `turn.rs`.
            let mcp_prompts = mcp.prompt_commands();
            if !mcp_prompts.is_empty() {
                tracing::info!(
                    count = mcp_prompts.len(),
                    "registering MCP prompts as slash commands"
                );
                registry.register_mcp_prompts(&mcp_prompts);
            }
            std::sync::Arc::new(registry)
        });

        // Registration is complete; everything below reads the final set.

        // Scene-contributed tools (`Scene::extra_tools`, default empty).
        //
        // Deliberately last. The collision check is the whole point of this
        // block, and it can only see names already in the registry — running
        // it before `Agent`/`Skill`/`WebSearch`/`Cron*`/`Worktree*` are
        // registered would let a scene claim one of those names, pass the
        // check against an empty slot, and then win the lookup anyway
        // (`get` returns the first match, and the engine's own instance is
        // appended after). That is precisely the shadowing this rejects.
        for t in scene.extra_tools() {
            if !admits(&self.contributed_tool_filter, t.name()) {
                continue;
            }
            if tools.get(t.name()).is_some() {
                return Err(EngineError::Internal(format!(
                    "scene {:?} contributes a tool named {:?}, which is already \
                     registered; rename the scene's tool",
                    scene.id(),
                    t.name()
                )));
            }
            tools.register(t);
        }

        // After the scene's tools. The same `Arc` arriving twice is not a
        // collision; a different tool under a taken name is.
        let plugin_tools = std::mem::take(&mut rust_layer.tools)
            .into_iter()
            .chain(std::mem::take(&mut quickjs_layer.tools));
        for t in plugin_tools {
            if !admits(&self.contributed_tool_filter, t.name()) {
                continue;
            }
            match tools.get(t.name()) {
                Some(existing) if Arc::ptr_eq(&existing, &t) => continue,
                Some(_) => {
                    return Err(EngineError::Internal(format!(
                        "a plugin contributes a tool named {:?}, which is already registered by \
                         something else",
                        t.name()
                    )));
                }
                None => {
                    tools.register(t);
                }
            }
        }

        // The scene's deferred policy, applied once over the complete set.
        //
        // It has to run here rather than on the registry the caller handed in:
        // `Agent`, `Skill`, `Cron*`, `EnterWorktree`/`ExitWorktree`,
        // `WebSearch`, `Team*`, the MCP adapters and the scene's own
        // contributions are all registered above, and those are most of what a
        // scene would want to defer.
        //
        // Substituting by name over the same `Arc` (rather than rebuilding the
        // registry) is what keeps `ToolSearchTool` working: it closes over this
        // exact registry and reads it at call time, so a tool wrapped
        // underneath it stays discoverable.
        let host_schema = self.output_schema.clone();
        match &host_schema {
            Some(schema) => tools::bind_output_schema(&tools, schema),
            None => {
                tools.remove("StructuredOutput");
            }
        }
        {
            let mut deferred = scene.deferred_tools();
            if host_schema.is_some() {
                deferred.retain(|n| n != "StructuredOutput");
            }
            if !deferred.is_empty() {
                for t in tools::deferred::apply_deferred_policy(tools.list(), &deferred) {
                    // `ToolSearch` must never be deferred — it is the only way
                    // back to a deferred tool's schema, so hiding its own
                    // schema would strand every other deferred tool.
                    if t.name() != "ToolSearch" {
                        tools.replace(t);
                    }
                }
            }
        }

        // The scene's tool policy, applied to the registry itself rather than
        // only to what is advertised: `ToolSearch` and dispatch read this same
        // registry, so a tool the scene excludes has to be absent from it.
        let attended = scene.interactive();
        let questions_answered = self.question_prompts || self.elicitation.is_some();
        for t in tools.list() {
            let waits_on_a_reply = base::interface::scene::NEEDS_A_USER.contains(&t.name())
                && (!attended || (t.name() == ASK_USER_QUESTION && !questions_answered));
            if waits_on_a_reply || !scene.permits_tool(t.name()) {
                tools.remove(t.name());
            }
        }

        // Last, so its description lists the tools this session ended up
        // with. Only the root session writes programs: a sub-agent is already
        // one delegation down.
        #[cfg(feature = "ptc")]
        if self.agent_depth == 0 && scene.permits_tool(ptc::TOOL_NAME) {
            tools.register(Arc::new(ptc::RunCodeTool::for_registry(tools.as_ref())));
        }

        // `TaskCreated` fires when a running task starts and `TaskCompleted`
        // when it ends — a sub-agent, a shell command, a monitor, a team
        // member — whatever started it. A task ending is also the background
        // work an `async_rewake` hook waits on, unless the embedder gave a wake
        // channel of its own.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let hooks = hooks.clone();
            let mut events = session_state.subscribe_tasks();
            let ambient = hook_ambient.clone();
            let ended = session_ended.clone();
            handle.spawn(async move {
                loop {
                    let event = tokio::select! {
                        _ = ended.cancelled() => return,
                        event = events.recv() => event,
                    };
                    match event {
                        Ok(event) if event["kind"] == "task_started" => {
                            if hooks.has_hooks_for(hooks::HookEvent::TaskCreated) {
                                let input = hooks::HookInput::from_ambient(&ambient, "TaskCreated")
                                    .with_reason(event["task"].to_string());
                                let _ = hooks.run(hooks::HookEvent::TaskCreated, &input).await;
                            }
                        }
                        Ok(event) if event["kind"] == "task_finished" => {
                            if hooks.has_hooks_for(hooks::HookEvent::TaskCompleted) {
                                let input =
                                    hooks::HookInput::from_ambient(&ambient, "TaskCompleted")
                                        .with_reason(event["task"].to_string());
                                let _ = hooks.run(hooks::HookEvent::TaskCompleted, &input).await;
                            }
                            if let Some(wake) = &task_wake {
                                let _ = wake.send(());
                            }
                        }
                        Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                    }
                }
            });
        }

        let (input_tx, input_rx) = mpsc::unbounded_channel();

        // The scheduler half of the cron bridge: without this, `CronCreate`
        // would happily report "Scheduled recurring job <id>" for a job that
        // could never fire. Ticks once a minute (the resolution of a 5-field
        // cron expression) and feeds each due job's prompt back in as a user
        // turn. Ends by itself when the session's input channel closes.
        //
        // `Handle::try_current` because `build()` is sync and reachable from
        // non-async contexts, where `tokio::spawn` would panic; there the
        // tools still create/list/delete jobs, they just never fire.
        match tokio::runtime::Handle::try_current() {
            Ok(_) if !fire_jobs_here => {}
            Ok(handle) => {
                let store = cron_store.clone();
                let tx = input_tx.clone();
                handle.spawn(async move {
                    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(60));
                    ticker.tick().await; // first tick is immediate — skip it
                    loop {
                        ticker.tick().await;
                        for job in store.pop_due() {
                            let msg = InputMessage::User {
                                content: job.prompt.clone(),
                                attachments: Vec::new(),
                                turn_id: format!("cron-{}", job.id),
                            };
                            if tx.send(msg).is_err() {
                                return; // session gone
                            }
                            tracing::info!(job_id = %job.id, cron = %job.cron, "cron job fired");
                        }
                    }
                });
            }
            Err(_) => {
                tracing::debug!(
                    "no tokio runtime at Builder::build(); cron jobs will be stored but not fired"
                );
            }
        }
        // The `Permission` this builder was handed was constructed before
        // the session registry above existed — in a daemon it was built over
        // `SessionPool.tools`, the pool-level template of self-contained
        // built-ins. Unbound, everything registered into `tools` in this
        // function (`Skill`, `Agent`, `Team*`, `WebSearch`, worktree, cron and
        // task tools, `Import`, every `mcp__*` adapter) would be invisible to
        // it and pass its "unknown tool" branch unchecked. Bind it to the
        // registry dispatch actually uses, which is complete at this point.
        permission.bind_tool_registry(tools.clone());
        // And to the session's live state, so `EnterPlanMode`/`ExitPlanMode`
        // (which flip `SessionState.permission_mode`) actually move the gate
        // instead of being decorative.
        permission.bind_session_state(session_state.clone());
        session_state.bind_tools(&(tools.clone() as Arc<dyn base::tool::ToolRegistry>));

        // Built before the `Agent` literal takes ownership of `scene` and
        // `settings`. The ceilings are read once per session, which is when
        // they are decided — neither can change mid-session.
        let recovery_policy: Arc<dyn base::interface::recovery_policy::RecoveryPolicy> =
            self.recovery_policy.clone().unwrap_or_else(|| {
                Arc::new(base::interface::recovery_policy::DefaultRecovery::new(
                    settings.model.fallback_model.clone(),
                ))
            });
        let turn_policy: Arc<dyn base::interface::turn_policy::TurnPolicy> =
            self.turn_policy.clone().unwrap_or_else(|| {
                Arc::new(base::interface::turn_policy::LimitsPolicy::new(
                    settings.execution.max_api_calls_per_turn,
                    scene.execution_params().max_api_calls_per_turn,
                    5,
                ))
            });

        let budget_policy: Arc<dyn base::interface::budget_policy::BudgetPolicy> =
            self.budget_policy.clone().unwrap_or_else(|| {
                Arc::new(base::interface::budget_policy::EngineBudget::from_settings(
                    &settings,
                ))
            });

        let settings = match &self.session_prompt {
            Some(prompt) => Arc::new(prompt.applied_to(&settings)),
            None => settings,
        };

        // Capture feature flags before settings is moved into the Agent struct
        let cached_mc_enabled = settings.compaction.cached_microcompact;

        let prompt_registry: Arc<dyn base::interface::prompt_registry::PromptRegistry> =
            if rust_layer.registers_on_prompt_registry()
                || quickjs_layer.registers_on_prompt_registry()
            {
                use base::interface::prompt_registry::PromptRegistry as _;
                let own = base::interface::prompt_registry::InMemoryPromptRegistry::new();
                for block in std::mem::take(&mut rust_layer.prompt_blocks) {
                    own.register_block(block);
                }
                for (name, provider, _) in std::mem::take(&mut rust_layer.prompt_variables) {
                    own.register_variable(&name, provider);
                }
                for (hook, authority) in std::mem::take(&mut rust_layer.assembly_hooks) {
                    own.register_assembly_hook(hook, authority);
                }
                for (hook, authority) in std::mem::take(&mut rust_layer.async_assembly_hooks) {
                    own.register_async_assembly_hook(hook, authority);
                }
                Arc::new(
                    base::interface::prompt_registry::LayeredPromptRegistry::new(
                        self.prompt_registry,
                        own,
                        base::interface::prompt_registry::ScriptContributions {
                            blocks: std::mem::take(&mut quickjs_layer.prompt_blocks),
                            variables: std::mem::take(&mut quickjs_layer.prompt_variables),
                            assembly_hooks: std::mem::take(&mut quickjs_layer.assembly_hooks),
                            async_assembly_hooks: std::mem::take(
                                &mut quickjs_layer.async_assembly_hooks,
                            ),
                        },
                    ),
                )
            } else {
                self.prompt_registry
                    .unwrap_or_else(|| Arc::new(base::interface::prompt_registry::NoRegistrations))
            };
        // Rust first in a chain and innermost around a tool call; QuickJS
        // after it and outside it.
        let tool_middleware: Vec<_> = quickjs_layer
            .tool_middleware
            .into_iter()
            .chain(self.tool_middleware)
            .chain(rust_layer.tool_middleware)
            .collect();
        let result_transformers: Vec<_> = self
            .result_transformers
            .into_iter()
            .chain(rust_layer.tool_results)
            .chain(quickjs_layer.tool_results)
            .collect();
        let model_interceptors: Vec<_> = self
            .model_interceptors
            .into_iter()
            .chain(rust_layer.model_interceptors)
            .chain(quickjs_layer.model_interceptors)
            .collect();
        let retrieval_hooks: Vec<_> = self
            .retrieval_hooks
            .into_iter()
            .chain(rust_layer.retrieval_hooks)
            .chain(quickjs_layer.retrieval_hooks)
            .collect();
        let prompt_middleware: Vec<_> = self
            .prompt_middleware
            .into_iter()
            .chain(rust_layer.prompt_middleware)
            .chain(quickjs_layer.prompt_middleware)
            .collect();
        let health_checks: Vec<_> = self
            .health_checks
            .into_iter()
            .chain(rust_layer.health_checks)
            .chain(quickjs_layer.health_checks)
            .collect();
        let turn_starts: Vec<_> = rust_layer
            .turn_starts
            .into_iter()
            .chain(quickjs_layer.turn_starts)
            .collect();
        let session_end = SessionEnd {
            ended: session_ended,
            tools: tools.clone(),
        };

        Ok((
            Agent {
                turn_starts: Arc::new(turn_starts),
                scene,
                model,
                tools,
                settings,
                config,
                agent_depth,
                permission,
                elicitation: self.elicitation.unwrap_or_else(|| {
                    let channel = crate::elicitation::ChannelElicitation::new(
                        event_tx.clone(),
                        pending_permissions.clone(),
                    );
                    Arc::new(if self.question_prompts {
                        channel.with_questions(pending_questions.clone())
                    } else {
                        channel
                    })
                }),
                prompt_registry,
                prompt_assembler: self.prompt_assembler.unwrap_or_else(|| {
                    Arc::new(base::interface::prompt_assembler::DefaultAssembler)
                }),
                tool_middleware: Arc::new(tool_middleware),
                prompt_middleware: Arc::new(prompt_middleware),
                result_transformers: Arc::new(result_transformers),
                turn_policy,
                budget_policy,
                health: Arc::new(base::interface::health::HealthChecks::from_vec(
                    health_checks,
                )),
                environment,
                recovery_policy,
                model_interceptors: Arc::new(model_interceptors),
                memory_retriever: self
                    .memory_retriever
                    .unwrap_or_else(|| Arc::new(base::interface::memory_contracts::LlmRetriever)),
                retrieval_hooks: Arc::new(retrieval_hooks),
                pending_permissions,
                pending_questions,
                current_turn_cancel: Arc::new(std::sync::Mutex::new(CancellationToken::new())),
                steering: Default::default(),
                memory_store,
                session,
                session_state,
                drive_goals: !self.goal_rounds_from_owner,
                agent_spawner: Arc::new(agent_spawner),
                _session_end: session_end,
                perf: Arc::new(PerfCollector::new()),
                compactor,
                hooks,
                mcp,
                skills: Arc::new(skill_catalog),
                commands: command_registry,
                telemetry_handle,
                current_turn_id: String::new(),
                frozen: self.frozen, // pre-seeded or lazily computed on first turn
                claude_md_content,
                session_context_injected: false,
                pending_input: None,
                turn_started_at: None,
                agent_type: self.agent_type,
                invoked_skills: Vec::new(),
                announced_skills: None,
                slash_skill_grants: Vec::new(),
                request_overhead_tokens: 0,
                last_had_tool_uses: true, // true → first turn scans for skills
                last_call_usage: None,
                turn_truncated: false,
                spent: SessionSpend::default(),
                permission_denial_count: Arc::new(std::sync::atomic::AtomicU32::new(0)),
                compact_warning_issued: false,
                tool_results_ever_cleared: false,
                mcp_shadow_warned: std::collections::HashSet::new(),
                time_based_mc_config: compaction::time_based_mc::TimeBasedMcConfig::default(),
                cached_mc: CachedMicroCompact::new(compaction::cached::CachedMcConfig {
                    enabled: cached_mc_enabled,
                    ..Default::default()
                }),
                compaction_state: compaction::reactive::CompactionState::default(),
                team_seat: self.team_seat,
                orphaned_permission: None,
                has_handled_orphaned_permission: false,
                output_token_target: None,
                output_token_target_unannounced: false,
                repeat_guard: Default::default(),
                accumulated_output_tokens: 0,
                token_budget_continuation_count: 0,
                last_delta_tokens: 0,
                input_rx,
                event_tx,
                skip_warmup: self.skip_warmup,
                task_router: self.task_router,
            },
            event_rx,
            input_tx,
        ))
    }
}

/// The prompt tells the model the scratchpad is there to write into, so it
/// has to exist before a shell redirect into it; its `.gitignore` keeps it out
/// of the project's git status. A relative `local_data_dir` (tests, embedders
/// that never set `paths`) would land in the process's working directory, so
/// nothing is created then.
fn prepare_scratchpad(dir: &std::path::Path) {
    if !dir.is_absolute() || dir.is_dir() {
        return;
    }
    if std::fs::create_dir_all(dir).is_ok() {
        let _ = std::fs::write(dir.join(".gitignore"), "*\n");
    }
}
