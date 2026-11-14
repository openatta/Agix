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

        let telemetry_handle = self
            .host
            .telemetry_handle_override
            .take()
            .unwrap_or_else(|| {
                let (tx, _rx) = tokio::sync::mpsc::channel(1);
                TelemetryHandle::new(tx)
            });
        // Wrapped before the sub-agent spawner takes its own copy of the
        // model, so a sub-agent's calls are accounted for too.
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
        let mut session = SessionManager::new(
            self.host.history_store.clone(),
            self.spec.session_id.clone(),
            self.spec.parent_session_id.clone(),
        );
        if let Some(counter) = self.host.token_counter.clone() {
            session = session.with_token_counter(counter);
        }
        // Only a persisting session gets a sidecar; an in-memory id does not
        // outlive the process. An id that does not parse fails the build
        // rather than degrading: `persist` parses the same id, so both
        // transcript and sidecar would be dead for the whole session.
        if self.host.history_store.is_some() {
            let sid = base::session::SessionId::parse(session.session_id_str()).map_err(|e| {
                EngineError::Internal(format!(
                    "session_id {:?} is not a valid SessionId ({e}); \
                     a history_store requires a parseable id — use \
                     `SessionId::new().to_string()` or omit `Builder::session_id`",
                    session.session_id_str()
                ))
            })?;
            // Through `history::path` like every other sidecar, or nothing
            // would look where the file lands.
            let roots = history::path::HistoryRoots::under(&settings.paths.global_data_dir);
            let path = history::path::session_memory_file(&roots.sessions, &sid);
            session = session.with_session_memory(SessionMemory::new(path));
        }
        Ok(session)
    }
}

// A fresh copy of the template: in a daemon the template is shared by every
// session, and per-session tools registered later must not land in it.
pub(super) fn session_registry(
    template: &dyn base::tool::ToolRegistry,
) -> Arc<InMemoryToolRegistry> {
    let session_tools = Arc::new(InMemoryToolRegistry::new());
    let mut had_tool_search = false;
    for t in template.list() {
        // The template's `ToolSearch` searches the template, which lacks
        // the engine-state tools; rebind one to this session's registry.
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
