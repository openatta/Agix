//! 造引擎的 `EngineFactory`：按一个会话的规格、它那一对的绑定和门面备好的部件
//! （场景表、定时任务表、MCP 模块、进程组账本、插件脚本）配好 `Builder` 建出 Agent。
//! 只负责构造，不往会话表里插，也不写日志；不持锁。

mod namer;
mod permission_scripts;

pub(in crate::session_pool) use namer::SessionNamer;
pub(in crate::session_pool) use permission_scripts::*;

use crate::session_pool::api::SessionOptions;
use crate::session_pool::parts::bindings::ProjectBinding;
use crate::session_pool::parts::specs::SessionSpec;
use crate::session_pool::pool_telemetry::{otel_sink, recordings_root, session_clock};
use crate::session_pool::{CreateError, SessionCtx, SessionCustomizer};
use base::interface::permission::PermissionPolicy;
use base::interface::scene::SceneSpec;
use base::settings::{Divergence, LoadedSettings, RecorderConfig, RecorderMode};
use runtime::agent::{Agent, Builder, EventReceiver, InputSender};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use telemetry::file_recorder::FileRecorder;
use telemetry::recorder::RecorderModel;
use tracing::warn;

pub(in crate::session_pool) struct EngineFactory {
    /// Built-in tool registry (Bash/Read/Write/Edit/Grep/Glob/...), shared
    /// (cheap `Arc` clone) by every session `Builder`. `Builder::build()` on
    /// its own registers only Skill/TaskStop/TaskOutput/Import/Agent/MCP; see
    /// `tools::register_builtin_tools` for what's deliberately excluded.
    tools: Arc<base::tool::InMemoryToolRegistry>,
    /// The host's own policy, one tier of every session's rules (see
    /// `resolve_session_permission`).
    host_policy: Option<Arc<dyn PermissionPolicy>>,
    rust_plugins: Vec<Arc<dyn runtime::plugin::Plugin>>,
    customize: Option<SessionCustomizer>,
    /// How long a `kind:"prompt"` permission ask may sit unanswered before
    /// the engine answers it with a `Deny`. `Duration::ZERO` waits forever.
    permission_prompt_timeout: Duration,
    history_store: Option<Arc<dyn history::store::HistoryStore>>,
}

/// One session's engine to build, with what the facade gathered for it from
/// the other collaborators.
pub(in crate::session_pool) struct BuildRequest<'a> {
    pub(in crate::session_pool) session_id: &'a str,
    pub(in crate::session_pool) scene: Arc<SceneSpec>,
    pub(in crate::session_pool) binding: &'a ProjectBinding,
    /// `None` for a no-project session.
    pub(in crate::session_pool) project_root: Option<&'a Path>,
    /// What the session's scripts resolve against: its project, else the
    /// pool's own directory.
    pub(in crate::session_pool) script_root: &'a Path,
    pub(in crate::session_pool) spec: &'a SessionSpec,
    /// The session already has a history this build continues.
    pub(in crate::session_pool) resume: bool,
    pub(in crate::session_pool) scene_registry: Arc<scene::SceneRegistry>,
    pub(in crate::session_pool) cron_store: Arc<tools::cron::CronStore>,
    pub(in crate::session_pool) package_scripts: Vec<crate::plugins::PackageScripts>,
    pub(in crate::session_pool) mcp_module: Option<Arc<dyn runtime::module::Module>>,
    pub(in crate::session_pool) process_groups: Option<Arc<platform::GroupLedger>>,
}

pub(in crate::session_pool) struct BuiltEngine {
    pub(in crate::session_pool) agent: Agent,
    pub(in crate::session_pool) event_rx: EventReceiver,
    pub(in crate::session_pool) input_tx: InputSender,
    pub(in crate::session_pool) namer: SessionNamer,
    pub(in crate::session_pool) scripts: Option<Arc<base::interface::script::ScriptLedger>>,
    /// Plugin scripts that could not be bound, by plugin, for the plugin
    /// subsystem's fault records.
    pub(in crate::session_pool) script_faults: Vec<(String, String)>,
    /// The settings the engine was built with: the pair's, with this
    /// session's mode, model and prompt timeout written in.
    pub(in crate::session_pool) settings: Arc<LoadedSettings>,
}

impl EngineFactory {
    pub(in crate::session_pool) fn new(
        tools: Arc<base::tool::InMemoryToolRegistry>,
        host_policy: Option<Arc<dyn PermissionPolicy>>,
        rust_plugins: Vec<Arc<dyn runtime::plugin::Plugin>>,
        customize: Option<SessionCustomizer>,
        permission_prompt_timeout: Duration,
        history_store: Option<Arc<dyn history::store::HistoryStore>>,
    ) -> Self {
        Self {
            tools,
            host_policy,
            rust_plugins,
            customize,
            permission_prompt_timeout,
            history_store,
        }
    }

    pub(in crate::session_pool) fn permission_prompt_timeout(&self) -> Duration {
        self.permission_prompt_timeout
    }

    pub(in crate::session_pool) fn build(
        &self,
        req: BuildRequest<'_>,
    ) -> Result<BuiltEngine, CreateError> {
        let (session_id, scene, binding, spec) = (req.session_id, req.scene, req.binding, req.spec);
        let options = spec.options.as_ref();
        let scene_id = scene.id.clone();
        let (model, task_router, model_name, clock) = models(session_id, binding, spec)?;
        let settings_snapshot =
            self.snapshot(&binding.settings, options, model_name.as_ref(), session_id);
        let namer = SessionNamer::of(
            session_id,
            task_router.as_deref(),
            &model,
            &settings_snapshot.model.model_name,
        );
        let permission =
            resolve_session_permission(self.host_policy.as_ref(), &settings_snapshot, options);

        let mut builder = Builder::new()
            .scene(scene)
            .model(model)
            .tools(self.tools.clone())
            .settings(settings_snapshot.clone())
            .permission(permission)
            .session_id(session_id.to_string())
            .commands_override(binding.commands.clone())
            .cron_store(req.cron_store)
            .question_prompts(options.is_some_and(SessionOptions::answers_questions));
        if let Some(store) = &self.history_store {
            builder = builder.history_store(store.clone());
        }
        if let Some(clock) = clock {
            builder = builder.environment(clock);
        }
        if let Some(router) = task_router {
            builder = builder.task_router(router);
        }
        builder = builder.scene_registry(req.scene_registry);
        let (mut builder, script_ledger, script_faults) = bind_session_scripts(
            builder,
            &settings_snapshot,
            req.script_root,
            &req.package_scripts,
        );

        if let Some(module) = req.mcp_module {
            builder = builder.module(module);
        }
        // After MCP, so the `Agent` tool sees the servers it may grant a
        // delegate; skills after agents, so forked skills can fork.
        builder = builder.modules(binding.modules.session_modules());

        if let Some(handle) = session_telemetry(&settings_snapshot, options, session_id) {
            builder = builder.telemetry_handle(handle);
        }
        if let Some(prompt) = spec.prompt() {
            builder = builder.session_prompt(prompt);
        }
        if let Some(schema) = options.and_then(|o| o.output_schema.clone()) {
            builder = builder.output_schema(schema);
        }
        if let Some(ledger) = &req.process_groups {
            builder = builder.process_groups(ledger.clone());
        }
        for plugin in &self.rust_plugins {
            builder = builder.plugin(plugin.clone());
        }
        if let Some(customize) = &self.customize {
            builder = customize(
                builder,
                &SessionCtx {
                    session_id,
                    scene: &scene_id,
                    project_root: req.project_root,
                    resumed: req.resume,
                },
            );
        }
        let (agent, event_rx, input_tx) = builder
            .goal_rounds_from_owner()
            .build()
            .map_err(|e| format!("build agent: {e}"))?;
        Ok(BuiltEngine {
            agent,
            event_rx,
            input_tx,
            namer,
            scripts: script_ledger,
            script_faults,
            settings: settings_snapshot,
        })
    }

    /// The pair's settings with this session's permission mode, model,
    /// prompt timeout and scratch workspace written in, and the mode.
    fn snapshot(
        &self,
        base_settings: &LoadedSettings,
        options: Option<&SessionOptions>,
        model_name: Option<&String>,
        session_id: &str,
    ) -> Arc<LoadedSettings> {
        // The prompt timeout is a daemon-level flag but the wait it bounds
        // lives in the engine, so it travels as a setting rather than as a
        // daemon-side wrapper around the permission. `Settings` is behind an
        // `Arc`, so override on a private clone rather than in place.
        // The mode is decided once and written into the snapshot as well:
        // the gate, the hook payloads and `Meta` all read it from there, and a
        // snapshot still holding the settings-file mode made every one of them
        // report a mode the session was not running in.
        let permission_mode = effective_permission_mode(base_settings, options);
        {
            let mut snap = base_settings.clone();
            snap.values.execution.permission_prompt_timeout_secs =
                self.permission_prompt_timeout.as_secs();
            snap.values.permission_mode = permission_mode;
            if let Some(name) = model_name {
                snap.values.model.model_name = name.clone();
            }
            if let base::settings::Workspace::Scratch(_) = snap.paths.workspace {
                snap.paths.workspace = base::settings::Workspace::Scratch(
                    base::paths::session_scratch_dir(&snap.paths.global_data_dir, session_id),
                );
            }
            Arc::new(snap)
        }
    }
}

/// The conversation's model and the task router, each wrapped in the
/// session's recorder when it asked for one, the model name chosen for it,
/// and the clock it keeps time by.
#[allow(clippy::type_complexity)]
fn models(
    session_id: &str,
    binding: &ProjectBinding,
    spec: &SessionSpec,
) -> Result<
    (
        Arc<dyn base::interface::model::Model>,
        Option<Arc<base::provider::TaskRouter>>,
        Option<String>,
        Option<Arc<dyn base::interface::environment::Environment>>,
    ),
    CreateError,
> {
    let options = spec.options.as_ref();
    let base_settings = &binding.settings;
    let task_router = binding.task_router.clone();
    // Wrap the model in a recorder if this session asked for one.
    //
    // Both the conversation's model *and* every routed provider get
    // wrapped, sharing one `Recorder` so they write into one recording.
    // Wrapping only the conversation's model left sub-agents, compaction
    // and memory extraction — everything reached through the router —
    // recording nothing whenever `providers` was configured.
    let recorder_config =
        options
            .and_then(|o| o.recorder.as_ref())
            .map(|recorder| RecorderConfig {
                mode: match recorder.mode.as_str() {
                    "record" => RecorderMode::Record,
                    // `rerun` is driven from a recording, not from a live
                    // session, so a session asking for it just talks to the
                    // provider — see `telemetry::recorder::rerun`.
                    "rerun" => RecorderMode::Rerun,
                    _ => RecorderMode::Replay,
                },
                name: recorder.name.clone(),
                root: recordings_root(base_settings, recorder.dir.as_deref()),
                on_divergence: if recorder.strict {
                    Divergence::Strict
                } else {
                    Divergence::Warn
                },
            });
    let clock = session_clock(options, recorder_config.as_ref(), session_id);
    let provider_id = spec.provider();
    let conversation_model = match provider_id.as_deref() {
        None => binding.model.clone(),
        Some(id) => binding
            .provider_client(id)
            .ok_or_else(|| format!("provider `{id}` is not configured"))?,
    };
    let model_name = spec.model().or_else(|| {
        provider_id
            .as_ref()
            .and_then(|id| binding.settings.providers.get(id))
            .and_then(|config| config.default_model.clone())
    });
    let (model, task_router) = match recorder_config {
        None => (conversation_model, task_router),
        Some(config) => {
            let shared = spec.recorder.clone().unwrap_or_default();
            let wrap = |provider_id: Option<&str>,
                        inner: Arc<dyn base::interface::model::Model>| {
                let mut m = RecorderModel::shared(
                    inner,
                    Some(config.clone()),
                    env!("CARGO_PKG_VERSION"),
                    shared.clone(),
                );
                if let Some(id) = provider_id {
                    m = m.with_provider_id(id);
                }
                Arc::new(m) as Arc<dyn base::interface::model::Model>
            };
            (
                wrap(provider_id.as_deref(), conversation_model),
                task_router.map(|r| Arc::new(r.map_models(|id, inner| wrap(Some(id), inner)))),
            )
        }
    };
    Ok((model, task_router, model_name, clock))
}

/// The session's telemetry pipeline: its JSONL file, plus OTel when an
/// endpoint is configured. `None` records nothing.
fn session_telemetry(
    settings_snapshot: &LoadedSettings,
    options: Option<&SessionOptions>,
    session_id: &str,
) -> Option<telemetry::TelemetryHandle> {
    // On by default: `Builder::build()`'s fallback handle drops its
    // receiver on the spot, so without a recorder every event is lost.
    // The caller's `output` only chooses *where*, and
    // `telemetry_enabled: false` in settings.json opts out.
    //
    // `TELEMETRY_DIR/<session-id>.jsonl` under the local data dir keeps
    // one file per session, next to the transcripts, so a recording is
    // findable by session id without the caller having had to plan for
    // it in advance.
    // The default location is only used when `local_data_dir` is a real
    // absolute path. It is not always set (tests, embedders that never
    // configure `paths`), and joining onto an empty or relative value
    // would scatter `telemetry/` directories into whatever the process's
    // working directory happens to be — including a user's repository.
    // Recording nothing is much better than writing somewhere surprising.
    let telemetry_path = match options.and_then(|o| o.telemetry.as_ref()) {
        Some(t) => Some(std::path::PathBuf::from(&t.output)),
        None => {
            let dir = &settings_snapshot.paths.local_data_dir;
            dir.is_absolute()
                .then(|| dir.join("telemetry").join(format!("{session_id}.jsonl")))
        }
    };
    if let (true, Some(telemetry_path)) = (settings_snapshot.telemetry_enabled, telemetry_path) {
        match FileRecorder::new(&telemetry_path) {
            Ok(rec) => {
                let data_dir = &settings_snapshot.paths.local_data_dir;
                if data_dir.is_absolute() && telemetry_path.starts_with(data_dir) {
                    base::paths::ensure_local_data_gitignore(data_dir);
                }
                // `telemetry` crate never touches the network itself —
                // this is where the daemon decides where events go:
                // always the per-session JSONL file, plus OTel when an
                // endpoint is configured. `start_otel` installs the
                // global meter/tracer providers, so that one is a
                // process-level side effect done once, guarded by a
                // `OnceLock` rather than per session (see `otel_sink`).
                let mut recorders: Vec<std::sync::Arc<dyn telemetry::TelemetryRecorder>> =
                    vec![std::sync::Arc::new(rec)];
                if let Some(otel) = otel_sink(settings_snapshot.telemetry_url.as_deref()) {
                    recorders.push(otel);
                }
                let telemetry_config = telemetry::TelemetryConfig {
                    enabled: true,
                    mode: telemetry::TelemetryMode::Enabled,
                    default_event_enabled: true,
                    ..Default::default()
                };
                match telemetry::spawn(telemetry_config, recorders) {
                    Ok((handle, consumer)) => {
                        tokio::spawn(consumer);
                        return Some(handle);
                    }
                    Err(e) => {
                        warn!(error = %e, "failed to start telemetry pipeline; this session records nothing");
                    }
                }
            }
            Err(e) => {
                warn!(
                    path = %telemetry_path.display(),
                    error = %e,
                    "could not open the session telemetry file; this session records nothing"
                );
            }
        }
    }
    None
}

/// The session's scripts — the settings' own and the plugins' — bound into
/// `builder`, what they did kept in the returned ledger, and the plugin
/// scripts that could not be bound.
#[allow(clippy::type_complexity)]
fn bind_session_scripts(
    builder: Builder,
    settings_snapshot: &Arc<LoadedSettings>,
    script_root: &Path,
    package_scripts: &[crate::plugins::PackageScripts],
) -> (
    Builder,
    Option<Arc<base::interface::script::ScriptLedger>>,
    Vec<(String, String)>,
) {
    #[cfg_attr(not(feature = "scripts"), allow(unused_mut))]
    let mut builder = builder;
    let script_faults: Vec<(String, String)>;
    #[cfg(feature = "scripts")]
    let script_ledger = {
        let (bound, faults) = bind_scripts(settings_snapshot, script_root, package_scripts);
        script_faults = faults;
        match bound {
            Some(bound) => {
                let ledger = bound.ledger.clone();
                builder = builder.plugin(Arc::new(script_host::ScriptPlugin::new(bound)));
                Some(ledger)
            }
            None => None,
        }
    };
    #[cfg(not(feature = "scripts"))]
    let script_ledger = {
        let (_, faults) = bind_scripts(settings_snapshot, script_root, package_scripts);
        script_faults = faults;
        None
    };
    (builder, script_ledger, script_faults)
}
