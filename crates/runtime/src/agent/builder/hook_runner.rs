//! Hook runner construction from `Settings::hooks_config`.

use super::*;

/// Build a `HookRunner` from `settings.hooks_config` (raw JSON from
/// settings.json's `hooks` section) when the caller didn't inject one
/// explicitly via `Builder::hooks(...)`.
///
/// A malformed value degrades to no hooks + a `tracing::warn!` rather than
/// failing the build — same soft-degrade philosophy as the rest of
/// settings.json. Also wires the `.agix/hooks/` three-tier search path
/// (project > scene > global) so `Command` hooks can reference a bare
/// script name — see `hooks::HookRunner::with_hooks_search_dirs` docs.
///
/// `model`/`agent_tool` back the `"type": "prompt"` / `"type": "agent"`
/// hook executors (`crate::hook_executors`).
pub(in crate::agent) fn build_hook_runner(
    settings: &Settings,
    model: Arc<dyn Model>,
    agent_spawner: Arc<dyn base::interface::agent_spawner::AgentSpawner>,
    session_id: Option<String>,
) -> Arc<HookRunner> {
    // Key by key, not whole-map. `HooksSettings` is keyed by `HookEvent`,
    // which has no serde fallback, so one unrecognized event name would
    // otherwise fail the entire map and silently turn off every hook.
    let parsed: hooks::HooksSettings = match &settings.hooks_config {
        Some(v) => {
            let (parsed, report) = hooks::parse_hooks_settings(v);
            for name in &report.unknown_events {
                tracing::warn!(
                    event = %name,
                    "settings.json `hooks` names an event this engine does not have; \
                     that entry is ignored, the rest are unaffected"
                );
            }
            for (event, err) in &report.invalid_configs {
                tracing::warn!(
                    event = %event,
                    error = %err,
                    "settings.json `hooks` entry failed to parse; that entry is ignored, \
                     the rest are unaffected"
                );
            }
            parsed
        }
        None => Default::default(),
    };
    // A hook configured for an event nothing fires is indistinguishable, from
    // the outside, from a hook that runs and does nothing — the author waits
    // for it forever. The engine knows which events those are, so it says so
    // here rather than leaving the answer in a doc comment on the enum that
    // whoever wrote the config is not reading.
    for (event, configs) in &parsed {
        if !event.is_wired() {
            tracing::warn!(
                event = ?event,
                hooks = configs.len(),
                "hook configured for an event this engine never fires — it will not run; \
                 see `hooks::UNWIRED_EVENTS` for why"
            );
        }
    }
    // An untrusted project's hooks config is already gone from `settings`;
    // its `hooks/` directory goes too, or a user hook naming a script would
    // run the project's file of that name.
    let project_untrusted = settings.project_layer.trust.is_untrusted();
    let project_hooks = settings
        .paths
        .project_root()
        .filter(|_| !project_untrusted)
        .map(|project| project.join(".agix").join("hooks"));
    let hooks_search_dirs: Vec<_> = project_hooks
        .into_iter()
        .chain([
            settings.paths.user_data_dir.join("hooks"),
            settings.paths.global_data_dir.join("hooks"),
        ])
        .collect();
    let prompt_executor = Arc::new(crate::hook_executors::ModelPromptHookExecutor::new(
        model,
        settings.model.model_name.clone(),
        settings.model.max_tokens,
        session_id,
    ));
    let agent_executor = Arc::new(crate::hook_executors::AgentSpawnerHookExecutor::new(
        agent_spawner,
        settings.paths.working_dir().to_path_buf(),
    ));
    let runner = HookRunner::new(parsed)
        .with_hooks_search_dirs(hooks_search_dirs)
        .with_project_untrusted(project_untrusted)
        .with_subprocess_env(settings.sandbox.subprocess_env.clone())
        .with_prompt_executor(prompt_executor)
        .with_agent_executor(agent_executor);
    Arc::new(runner)
}

/// The session's id, cwd and permission mode, read each time a payload is
/// built. Holds the state weakly: the closure lives in tasks and watchers that
/// must not keep the session alive.
pub(in crate::agent) fn hook_ambient(
    session_id: String,
    cwd: String,
    state: Arc<base::context::SessionState>,
) -> hooks::payload::Ambient {
    let state = Arc::downgrade(&state);
    Arc::new(move || {
        let mode = state
            .upgrade()
            .map(|s| crate::turn::hook_permission_mode(s.permission_mode()))
            .unwrap_or_else(|| "default".into());
        hooks::HookInput::lifecycle(String::new(), session_id.clone(), cwd.clone(), mode)
    })
}
