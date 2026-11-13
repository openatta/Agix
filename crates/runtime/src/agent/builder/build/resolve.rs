//! Phase 1 and the session itself: what the builder was given, and the session it opens.

use super::*;

const NO_HOST_TO_ASK: &str = "this session has no host to ask. Allow the call with a \
                              `permission_rules` allow rule in the settings, or choose a \
                              `permission_mode`; `bypassPermissions` lets every call through \
                              and still guards credentials and the managed settings";

impl Builder {
    pub(super) fn resolve(&mut self) -> Result<Resolved, EngineError> {
        let scene = self
            .spec
            .scene
            .take()
            .ok_or_else(|| EngineError::Internal("scene required".into()))?;
        let modules = crate::module::permitted_by(&self.host.modules, scene.as_ref())
            .map_err(|e| EngineError::Internal(e.to_string()))?;
        crate::module::check_order(&modules).map_err(|e| EngineError::Internal(e.to_string()))?;
        let model = self
            .spec
            .model
            .take()
            .ok_or_else(|| EngineError::Internal("model required".into()))?;
        let template: Arc<dyn base::tool::ToolRegistry> = self
            .spec
            .tools
            .take()
            .unwrap_or_else(|| Arc::new(InMemoryToolRegistry::new()));
        let tools = session_registry(template.as_ref());

        let settings = self
            .spec
            .settings
            .take()
            .ok_or_else(|| EngineError::Internal("settings required".into()))?;

        // Telemetry: use pre-built handle if injected, else noop (events
        // silently dropped).
        let telemetry_handle = self
            .host
            .telemetry_handle_override
            .take()
            .unwrap_or_else(|| {
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
            settings.values.model.model_name.clone(),
        ));
        let permission = self.host.permission.take().unwrap_or_else(|| {
            let policy = permissions::policy::Policy::from_settings(&settings, Vec::new());
            PermissionSet::of(Arc::new(if self.ext.elicitation.is_some() {
                policy
            } else {
                policy.with_no_one_to_ask(NO_HOST_TO_ASK)
            }))
        });
        let (instruction_file, instruction_provider, claude_md_content) =
            self.instruction_context(&settings);
        let environment = self
            .host
            .environment
            .clone()
            .unwrap_or_else(|| Arc::new(base::interface::environment::SystemEnvironment));
        let rules: Arc<dyn base::interface::instruction_provider::RuleProvider> = self
            .host
            .rule_provider
            .clone()
            .unwrap_or_else(|| Arc::new(base::rules::RuleTiers::for_paths(&settings.paths)));
        let memory_store = self.host.memory_store.take().unwrap_or_else(|| {
            Arc::new(MemoryStore::for_paths(
                &base::paths::ConfigPaths::from_settings(&settings.paths),
            ))
        });
        Ok(Resolved {
            scene,
            modules,
            model,
            template,
            tools,
            settings,
            telemetry_handle,
            permission,
            instruction_file,
            instruction_provider,
            claude_md_content,
            environment,
            rules,
            memory_store,
        })
    }

    pub(super) fn instruction_context(
        &self,
        settings: &LoadedSettings,
    ) -> (
        Option<PathBuf>,
        Arc<dyn base::interface::instruction_provider::InstructionProvider>,
        Option<String>,
    ) {
        let configured_instruction_file = settings.values.instruction_file.clone();
        let instruction_file = self
            .spec
            .instruction_file
            .clone()
            .or(configured_instruction_file);
        // Pre-read AGENTS.md / CLAUDE.md content for userContext injection.
        let instruction_provider: Arc<
            dyn base::interface::instruction_provider::InstructionProvider,
        > = self.host.instruction_provider.clone().unwrap_or_else(|| {
            Arc::new(
                base::interface::instruction_provider::InstructionFile::new(
                    instruction_file.clone(),
                )
                .reading_with(base::paths::PromptFiles::for_project(
                    &base::paths::ConfigPaths::from_settings(&settings.paths),
                )),
            )
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
        (instruction_file, instruction_provider, claude_md_content)
    }

    pub(super) fn open_session(
        &self,
        settings: &LoadedSettings,
    ) -> Result<SessionManager, EngineError> {
        // Session: `self.host.history_store` (set via `Builder::history_store(...)`)
        // makes this incrementally persisted to disk once per turn; `None`
        // (the default) keeps it in-memory only.
        let mut session = SessionManager::new(
            self.host.history_store.clone(),
            self.spec.session_id.clone(),
            self.spec.parent_session_id.clone(),
        );
        if let Some(counter) = self.host.token_counter.clone() {
            session = session.with_token_counter(counter);
        }
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
        if self.host.history_store.is_some() {
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
        Ok(session)
    }
}

// Per-session tool registry, seeded from whatever was passed in (a
// daemon's shared, immutable-after-startup template of built-in
// tools — see `SessionPool.tools`). Every per-session-stateful tool
// this function registers below (`Skill`/`Agent`/`TeamCreate`/
// `TeamDelete`/`TaskStop`/`TaskOutput`/MCP adapters)
// goes into *this* fresh copy, not the caller-supplied one: in a
// daemon the caller's `Arc` is shared by every session, and
// `InMemoryToolRegistry::register` appends without replacing by name.
pub(super) fn session_registry(
    template: &dyn base::tool::ToolRegistry,
) -> Arc<InMemoryToolRegistry> {
    let session_tools = Arc::new(InMemoryToolRegistry::new());
    let mut had_tool_search = false;
    for t in template.list() {
        // `ToolSearchTool` closes over the registry it searches. The
        // instance in the incoming template closes over the
        // *template*, so it would report on the template's tools —
        // which have none of the engine-state tools registered below.
        // Drop it and
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
}
