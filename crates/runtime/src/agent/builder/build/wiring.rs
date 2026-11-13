//! Phases 4, 6 and 8: hooks, session state and events, wired before any module runs.

use super::*;

impl Builder {
    pub(super) fn wire_hooks(
        &mut self,
        settings: &LoadedSettings,
        model: &Arc<dyn Model>,
        session: &SessionManager,
    ) -> HookWiring {
        let delegator = crate::module::LateDelegator::default();
        let session_ended = CancellationToken::new();
        let own_runner = self.ext.hooks.is_none();
        let hooks = self.ext.hooks.take().unwrap_or_else(|| {
            build_hook_runner(
                settings,
                model.clone(),
                Arc::new(delegator.clone()),
                Some(session.session_id_str().to_string()),
            )
        });
        // A runner handed in belongs to a session above this one, which wired
        // its wake channel already; taking it over would move every rewake of
        // the tree onto this session's tasks.
        let task_wake = match self.ext.wake_rx.take() {
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
        HookWiring {
            delegator,
            session_ended,
            hooks,
            task_wake,
        }
    }

    pub(super) fn engine_config(
        &self,
        settings: &LoadedSettings,
        scene: &SceneSpec,
    ) -> Arc<base::context::EngineConfig> {
        Arc::new({
            let mut c = base::context::EngineConfig::from_loaded(settings);
            c.path_style = settings.values.path_style.unwrap_or(scene.path_style);
            c.deferred_tools = Arc::new(
                scene
                    .tools
                    .deferred
                    .iter()
                    // `ToolSearch` is the only way back to a deferred schema; a host
                    // that bound an output schema wants `StructuredOutput` whole.
                    .filter(|n| *n != "ToolSearch")
                    .filter(|n| *n != "StructuredOutput" || self.spec.output_schema.is_none())
                    .cloned()
                    .collect(),
            );
            c
        })
    }

    pub(super) fn wire_session_state(
        &self,
        settings: &LoadedSettings,
        config: &Arc<base::context::EngineConfig>,
        scene: &SceneSpec,
        session: &SessionManager,
        hooks: &Arc<HookRunner>,
        instruction_file: Option<&PathBuf>,
    ) -> Result<SessionWiring, EngineError> {
        let agent_depth = self.spec.agent_depth;
        // One `SessionState` for the whole session. It is the session's mutable side: permission mode
        // (which `EnterPlanMode`/`ExitPlanMode` flip), the read cache that
        // `Edit`/`Write` consult for read-before-edit staleness, the read
        // dedup marker, todos, the task
        // list, running sub-agent tasks, and the accumulated
        // "allow writes here for this project" paths. Every tool call gets
        // the same `Arc`; none of that state works if it does not survive
        // from one call to the next.
        prepare_scratchpad(&settings.paths.local_data_dir.join("scratchpad"));
        prepare_working_dir(&settings.paths);
        let session_env = session_env(settings, config, scene);
        let (session_state, generation, injected_state) = match self.spec.session_state.clone() {
            Some((state, generation)) => (state, generation, true),
            None => (Arc::new(base::context::SessionState::new()), 0, false),
        };
        // One set for the session: its tool calls run on it, and a permission
        // judging them reads it back from the session state.
        let exec = match (&self.host.exec, &self.spec.process_groups) {
            (Some(_), Some(_)) => {
                return Err(EngineError::Internal(
                    "Builder::exec and Builder::process_groups are both set; record groups in \
                     the providers you pass (platform::local_providers_with_ledger)"
                        .into(),
                ))
            }
            (Some(exec), None) => exec.clone(),
            (None, groups) => crate::turn::exec_providers_from(config, groups.as_ref()),
        };
        let hook_ambient = hook_ambient(
            session.session_id_str().to_string(),
            settings.paths.working_dir().display().to_string(),
            session_state.clone(),
        );

        // One watcher per session tree, started by its root: a delegate shares
        // the root's runner, and a watcher of its own would fire every
        // `FileChanged` once more for each sub-agent ever spawned. The files a
        // session loads as instructions are added when it loads them
        // (`inject_session_context`); the instruction file is known now.
        if agent_depth == 0 && hooks.has_hooks_for(hooks::HookEvent::FileChanged) {
            if let Err(e) = hooks.enable_file_watching(
                &instruction_file.into_iter().cloned().collect::<Vec<_>>(),
                300,
                hook_ambient.clone(),
            ) {
                tracing::warn!(error = %e, "failed to start FileChanged watcher");
            }
        }
        Ok(SessionWiring {
            state: session_state,
            generation,
            injected: injected_state,
            env: session_env,
            exec,
            ambient: hook_ambient,
        })
    }

    pub(super) fn wire_events(&mut self) -> EventWiring {
        let (primary_tx, event_rx) = mpsc::unbounded_channel();
        let event_tx = crate::event_bus::EventBus::with_sinks(
            primary_tx,
            std::mem::take(&mut self.ext.event_sinks),
        );
        // Created before the modules run, because a sub-agent's permission
        // prompts are registered in this same map — see
        // `agents::delegate::DelegateEnv::capture`.
        let pending_permissions: PendingPermissions =
            Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        let pending_questions: crate::elicitation::PendingQuestions = Default::default();
        EventWiring {
            tx: event_tx,
            rx: event_rx,
            pending_permissions,
            pending_questions,
        }
    }
}

pub(super) fn bind_tools(
    session_env: base::context::SessionEnv,
    exec: &base::interface::exec::ExecProviders,
    generation: u64,
    tools: &Arc<InMemoryToolRegistry>,
    session_state: &Arc<base::context::SessionState>,
    injected_state: bool,
) -> base::context::EngineAttachment {
    let attachment = base::context::EngineAttachment::new(session_env, exec.clone(), generation)
        .with_tools(&(tools.clone() as Arc<dyn base::tool::ToolRegistry>));
    if !injected_state {
        session_state.attach(attachment.clone());
    }
    attachment
}

/// What this engine's settings and scene make of the session's env.
pub(super) fn session_env(
    settings: &LoadedSettings,
    config: &base::context::EngineConfig,
    scene: &SceneSpec,
) -> base::context::SessionEnv {
    let cwd = settings.paths.working_dir().to_path_buf();
    let resolve = |dir: &std::path::PathBuf| cwd.join(dir);
    base::context::SessionEnv {
        additional_writable_dirs: settings
            .values
            .sandbox
            .additional_directories
            .iter()
            .map(resolve)
            .chain(config.sandbox_policy.paths.state_working_dirs())
            .collect(),
        read_directories: settings
            .values
            .sandbox
            .read_directories
            .iter()
            .map(resolve)
            .chain(config.sandbox_policy.paths.readable_state_root())
            .collect(),
        read_anywhere: settings.values.sandbox.read_anywhere,
        cache_directories: settings.values.sandbox.cache_directories.paths(&cwd),
        unattended: !scene.interactive,
        base_mode: config.permission_mode,
        cwd,
    }
}

/// A session without a project works in a directory of its own, which has to
/// exist before its first command runs there.
pub(super) fn prepare_working_dir(paths: &base::settings::PathSettings) {
    if let base::settings::Workspace::Scratch(dir) = &paths.workspace {
        if dir.is_absolute() {
            let _ = std::fs::create_dir_all(dir);
        }
    }
}

/// The prompt tells the model the scratchpad is there to write into, so it
/// has to exist before a shell redirect into it; its `.gitignore` keeps it out
/// of the project's git status. A relative `local_data_dir` (tests, embedders
/// that never set `paths`) would land in the process's working directory, so
/// nothing is created then.
pub(super) fn prepare_scratchpad(dir: &std::path::Path) {
    if !dir.is_absolute() || dir.is_dir() {
        return;
    }
    if std::fs::create_dir_all(dir).is_ok() {
        let _ = std::fs::write(dir.join(".gitignore"), "*\n");
    }
}
