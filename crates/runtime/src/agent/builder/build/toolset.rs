//! Phases 5, 7 and 11: the engine's own tools, the session's tools, and the final tool surface.

use super::*;

impl Builder {
    pub(super) fn register_session_tools(
        &self,
        tools: &Arc<InMemoryToolRegistry>,
        settings: &LoadedSettings,
        model: &Arc<dyn Model>,
        telemetry_handle: &TelemetryHandle,
    ) -> CronWiring {
        let agent_depth = self.spec.agent_depth;
        // WebSearch searches through the session's own model unless
        // `task_models.web_search` routes it elsewhere.
        let (search_model, search_model_name) = match self
            .host
            .task_router
            .as_ref()
            .and_then(|router| Some((router, router.model_name_for(WEB_SEARCH_TASK)?)))
        {
            Some((router, name)) => (
                Arc::new(telemetry::model::TelemetryModel::new(
                    router.model_for(WEB_SEARCH_TASK),
                    telemetry_handle.clone(),
                    settings.values.model.model_name.clone(),
                )) as Arc<dyn Model>,
                name.to_string(),
            ),
            None => (model.clone(), settings.values.model.model_name.clone()),
        };
        tools::register_web_search(tools, search_model, search_model_name);

        // Extraction runs only on a model routed for it: the session's own
        // model is too costly to spend reading whole pages.
        let fetch_extraction = self.host.task_router.as_ref().and_then(|router| {
            let name = router.model_name_for(WEB_FETCH_TASK)?;
            let model = Arc::new(telemetry::model::TelemetryModel::new(
                router.model_for(WEB_FETCH_TASK),
                telemetry_handle.clone(),
                settings.values.model.model_name.clone(),
            )) as Arc<dyn Model>;
            Some((model, name.to_string()))
        });
        tools::register_web_fetch(tools, fetch_extraction);

        // One registry per session: it holds at most one active worktree.
        tools::register_worktree_tools(
            tools,
            Arc::new(tools::worktree_tools::WorktreeRegistry::new()),
        );

        // Cron tools, and — when no owner handed in a store — the scheduler
        // that drains this session's own. `~/.agix/scenes/<scope>/scheduled_tasks.json`
        // is the exact path `CronCreate`'s own prompt promises the user for
        // `durable: true` jobs; session-only jobs never touch disk. A delegate
        // gets neither: scheduled jobs belong to the session its host keeps,
        // and a delegate's own scheduler would pop that session's durable jobs.
        //
        // Known limitation of the store-per-session case only: two concurrent
        // sessions in the same scope each load the durable file and each
        // schedule their own fire for a durable job. A session pool avoids it
        // by owning one store and one scheduler (`Builder::cron_store`).
        let (cron_store, fire_jobs_here) = match (agent_depth, self.spec.cron_store.clone()) {
            (0, Some(store)) => (Some(store), false),
            (0, None) => {
                let _ = std::fs::create_dir_all(&settings.paths.user_data_dir);
                let store = Arc::new(tools::cron::CronStore::load_or_default(Some(
                    settings.paths.user_data_dir.join("scheduled_tasks.json"),
                )));
                (Some(store), true)
            }
            _ => (None, false),
        };
        if let Some(store) = &cron_store {
            tools::register_cron_tools(tools, store.clone());
        }
        CronWiring {
            store: cron_store,
            fire_here: fire_jobs_here,
        }
    }

    pub(super) fn finalize_tools(
        &self,
        registry: Arc<InMemoryToolRegistry>,
        scene: &Arc<SceneSpec>,
        plugin_tools: Vec<Arc<dyn base::tool::Tool>>,
        plugin_tool_owners: &std::collections::HashMap<String, crate::plugin::ToolOwner>,
        questions_answered: bool,
    ) -> Result<SessionTools, EngineError> {
        let tools = &registry;
        admit_extra_tools(tools, scene, plugin_tools)?;

        let tool_policy = Arc::new(crate::tool_policy::ToolPolicy::new(
            scene,
            self.spec.tool_selector.clone(),
            plugin_tool_owners
                .iter()
                .filter(|(_, owner)| owner.admitted)
                .map(|(name, _)| name.clone()),
        ));

        let host_schema = self.spec.output_schema.clone();
        match &host_schema {
            Some(schema) => tools::bind_output_schema(tools, schema),
            None => {
                tools.remove("StructuredOutput");
            }
        }
        // The tool policy, applied to the registry itself rather than
        // only to what is advertised: `ToolSearch` and dispatch read this same
        // registry, so a tool the scene excludes has to be absent from it.
        let attended = scene.interactive;
        for t in tools.list() {
            let waits_on_a_reply = base::interface::scene::NEEDS_A_USER.contains(&t.name())
                && (!attended || (t.name() == ASK_USER_QUESTION && !questions_answered));
            let excluded = !tool_policy.permits(t.as_ref());
            if waits_on_a_reply || excluded {
                tools.remove(t.name());
                tracing::debug!(
                    tool = t.name(),
                    scene = %scene.id,
                    plugin = plugin_tool_owners.get(t.name()).map(|o| o.plugin.as_str()),
                    reason = if excluded {
                        "not permitted by the session's tool policy"
                    } else {
                        "waits on a reply no one will give"
                    },
                    "tool removed from the session"
                );
            }
        }

        // Last, so its description lists the tools this session ended up
        // with. Only the root session writes programs: a sub-agent is already
        // one delegation down.
        #[cfg(feature = "ptc")]
        if self.spec.agent_depth == 0 && tool_policy.scene_permits(ptc::TOOL_NAME) {
            tools.register(run_code_for(tools.as_ref(), self.spec.run_slots.clone()));
        }

        let names = tools.names();
        let mut seen = std::collections::HashSet::new();
        let twice: Vec<&String> = names.iter().filter(|n| !seen.insert(n.as_str())).collect();
        debug_assert!(twice.is_empty(), "tools registered twice: {twice:?}");
        if !twice.is_empty() {
            tracing::error!(
                tools = ?twice,
                "tools registered twice; dispatch reaches only the first of each"
            );
        }
        Ok(SessionTools {
            tools: registry,
            policy: tool_policy,
        })
    }
}

pub(super) fn admit_extra_tools(
    tools: &InMemoryToolRegistry,
    scene: &SceneSpec,
    plugin_tools: Vec<Arc<dyn base::tool::Tool>>,
) -> Result<(), EngineError> {
    // Scene-contributed tools (`SceneSpec::extra_tools`, default empty).
    //
    // Deliberately last. The collision check is the whole point of this
    // block, and it can only see names already in the registry — running
    // it before `Agent`/`Skill`/`WebSearch`/`Cron*`/`Worktree*` are
    // registered would let a scene claim one of those names, pass the
    // check against an empty slot, and then win the lookup anyway
    // (`get` returns the first match, and the engine's own instance is
    // appended after). That is precisely the shadowing this rejects.
    for t in scene.extra_tools.iter().cloned() {
        if tools.get(t.name()).is_some() {
            return Err(EngineError::Internal(format!(
                "scene {:?} contributes a tool named {:?}, which is already \
                     registered; rename the scene's tool",
                scene.id,
                t.name()
            )));
        }
        tools.register(t);
    }

    // After the scene's tools. The same `Arc` arriving twice is not a
    // collision; a different tool under a taken name is.
    for t in plugin_tools {
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
    Ok(())
}

pub(super) fn register_task_tools(tools: &InMemoryToolRegistry) {
    tools.register(std::sync::Arc::new(tools::task_stop::TaskStopTool));
    tools.register(std::sync::Arc::new(tools::task_output::TaskOutputTool));
    tools.register(std::sync::Arc::new(tools::task_wait::TaskWaitTool));
    tools.register(std::sync::Arc::new(tools::task_message::TaskMessageTool));
}
