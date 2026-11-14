//! 工具运行环境：沙箱、出口、引擎 git 的装配，工具上下文，子调用分发与技能调用上下文。

use super::inline_commands::SessionInlineCommands;
use super::tool_exec::{execute_tool_with_telemetry, ToolExecCtx};
use crate::agent::Agent;
use base::tool::ToolContext;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Lower `EngineConfig`'s sandbox policy into the cross-crate view a tool
/// receives.
///
/// `deny_read`'s `None` means "use the built-in credential deny defaults",
/// and `SandboxSettings` has no way to say that — it carries a plain `Vec`,
/// with the same empty-means-defaults convention that
/// `tools::bash::to_sandbox_policy` reads on the other side. So `None` and
/// `Some(vec![])` both flatten to an empty vec here, and the distinction is
/// preserved where it is actually decided (`EngineConfig::from_loaded`).
pub(super) fn sandbox_settings_from(
    config: &base::context::EngineConfig,
) -> base::tool::SandboxSettings {
    base::tool::SandboxSettings {
        allow_read: config.sandbox_policy.allow_read.clone(),
        deny_read: config.sandbox_policy.deny_read.clone().unwrap_or_default(),
        allowed_domains: config.sandbox_policy.allowed_domains.clone(),
        network_mode: config.sandbox_policy.network_mode,
        deny_write: config.sandbox_policy.deny_write.clone(),
        allow_write: config.sandbox_policy.allow_write.clone(),
        paths: config.sandbox_policy.paths.clone(),
        require_enforcement: config.sandbox_policy.require_enforcement,
        unattended_requires_network_policy: config
            .sandbox_policy
            .unattended_requires_network_policy,
    }
}

/// How the engine starts its own git and helpers for this session: through
/// its providers, in the sandbox its tool calls run in whenever it confines
/// them.
pub(crate) fn engine_exec_from(
    config: &base::context::EngineConfig,
    exec: &base::interface::exec::ExecProviders,
    session_id: &str,
    trust: base::project_trust::ProjectTrust,
) -> base::engine_exec::EngineExec {
    tools::bash::engine_exec_for(
        &sandbox_settings_from(config),
        exec,
        config.dangerously_disable_sandbox,
        session_id,
        trust,
    )
}

/// The session's frozen environment snapshot, taken in its working directory
/// — looked up in as a project only when it is one.
pub(crate) async fn collect_frozen(
    paths: &base::settings::PathSettings,
    environment: &dyn base::interface::environment::Environment,
    engine_git: &base::engine_exec::EngineExec,
) -> base::frozen::FrozenContext {
    use base::frozen::FrozenContext;
    let config_paths = base::paths::ConfigPaths::from_settings(paths);
    let cwd = paths.working_dir().to_path_buf();
    match paths.project_root() {
        Some(_) => FrozenContext::collect(cwd, &config_paths, environment, engine_git).await,
        None => {
            FrozenContext::collect_without_project(cwd, &config_paths, environment, engine_git)
                .await
        }
    }
}

/// The execution providers a session's tool calls reach the machine through,
/// with every process group a command starts recorded in `process_groups`.
///
/// Only the egress is configured from `EngineConfig`; the other three are
/// this machine either way. `sandbox.network_mode` is a statement about where
/// the *model* may reach, which is as true of a `WebFetch` as it is of a
/// `curl` inside a sandboxed command.
pub(crate) fn exec_providers_from(
    config: &base::context::EngineConfig,
    process_groups: Option<&Arc<platform::GroupLedger>>,
) -> base::interface::exec::ExecProviders {
    let env = config.subprocess_env.clone();
    match process_groups {
        Some(ledger) => platform::local_providers_with_ledger(env, ledger.clone()),
        None => platform::local_providers_with(env),
    }
    .with_network(Arc::new(platform::LocalNetwork::for_agent_policy(
        config.sandbox_policy.network_mode,
        config.sandbox_policy.allowed_domains.clone(),
    )))
}

/// Sub-calls a tool makes (`RunCode`), through the same dispatch as the
/// model's own calls. Each runs under its own tool-use id derived from the
/// caller's, cannot ask a person anything, and leaves the round's batch alone:
/// a hook ending the turn or an image a sub-call returned are the caller's to
/// report, not the round's. Nor does it report progress: the host never saw
/// a `tool_use` under a sub-call's id, and the program's own progress reaches
/// it as its task's.
pub(super) struct SubCallDispatcher {
    ctx: ToolExecCtx,
    next: std::sync::atomic::AtomicU64,
}

impl SubCallDispatcher {
    fn new(ctx: &ToolExecCtx) -> Self {
        let mut ctx = ctx.clone();
        ctx.elicitation = Arc::new(NoAsking);
        ctx.discontinued = Arc::new(std::sync::atomic::AtomicBool::new(false));
        ctx.images = Arc::default();
        ctx.from_code = true;
        ctx.events = None;
        Self {
            ctx,
            next: std::sync::atomic::AtomicU64::new(1),
        }
    }

    fn callable_tool(&self, name: &str) -> Option<Arc<dyn base::tool::Tool>> {
        self.ctx
            .tools
            .get(name)
            .filter(|t| t.spec().callable_from_code)
    }
}

#[async_trait::async_trait]
impl base::interface::tool_dispatch::ToolDispatch for SubCallDispatcher {
    fn callable(&self) -> Vec<base::interface::tool_dispatch::CallableTool> {
        let mut tools: Vec<_> = self
            .ctx
            .tools
            .list()
            .into_iter()
            .filter_map(|t| {
                let spec = t.spec();
                spec.callable_from_code
                    .then(|| base::interface::tool_dispatch::CallableTool {
                        name: t.name().to_string(),
                        description: spec
                            .short_description
                            .unwrap_or_else(|| t.description().to_string()),
                        input_schema: base::tool::compact_schema(
                            t.input_schema(self.ctx.config.path_style),
                        ),
                        result: spec.code_result.map(str::to_string),
                    })
            })
            .collect();
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        tools
    }

    fn concurrency_safe(&self, name: &str, input: &serde_json::Value) -> bool {
        self.callable_tool(name)
            .is_some_and(|t| t.call_traits(input).concurrency_safe)
    }

    async fn call(
        &self,
        name: &str,
        input: serde_json::Value,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<base::interface::tool_dispatch::SubCallAnswer, String> {
        if self.callable_tool(name).is_none() {
            return Err(format!("{name} cannot be called from code in this session"));
        }
        let n = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut ctx = self.ctx.clone();
        ctx.tool_use_id = format!("{}.{n}", self.ctx.tool_use_id);
        ctx.cancel = cancel;
        execute_tool_with_telemetry(&ctx, name, input)
            .await
            .map(|a| base::interface::tool_dispatch::SubCallAnswer {
                text: a.text,
                structured: a.structured,
                reported_failure: a.reported_failure,
            })
    }
}

/// The approval a sub-call would need cannot be asked for from inside a
/// program; the model has to make that call itself.
pub(super) struct NoAsking;

#[async_trait::async_trait]
impl base::interface::elicitation::Elicitation for NoAsking {
    async fn ask(
        &self,
        _: base::interface::elicitation::ElicitRequest,
    ) -> base::interface::elicitation::ElicitOutcome {
        base::interface::elicitation::ElicitOutcome::declined(
            "this call needs the user's approval, which a program cannot ask for — \
             make this call directly instead of from code",
        )
    }
}

/// Whether anyone can be asked about a call the session makes. The one place
/// that reads `unattended` to decide it.
pub(crate) fn attendance_of(
    session: &base::context::SessionState,
    from_code: bool,
) -> base::interface::permission::Attendance {
    use base::interface::permission::Attendance;
    if session.env().unattended {
        Attendance::Unattended
    } else if from_code {
        Attendance::NoOneToAsk(
            "this call needs the user's approval, which a program cannot ask for — make this \
             call directly instead of from code",
        )
    } else {
        Attendance::Attended
    }
}

/// What the session's sandbox holds a command to, probed the first time a
/// call of the session asks.
fn confinement_of(
    session: &base::context::SessionState,
    config: &base::context::EngineConfig,
    exec: &base::interface::exec::ExecProviders,
    cwd: &std::path::Path,
    session_id: &str,
) -> base::interface::exec::ConfinementReport {
    session.confinement(|| {
        tools::bash::confinement_report(
            &sandbox_settings_from(config),
            exec,
            config.dangerously_disable_sandbox,
            cwd,
            session_id,
        )
    })
}

/// The context a call to `tool` runs with: the same for validating its input
/// and for judging it as for running it.
pub(super) fn tool_context_for(ctx: &ToolExecCtx, tool: &Arc<dyn base::tool::Tool>) -> ToolContext {
    let additional_writable_dirs = ctx.session_state.env().additional_writable_dirs.clone();
    let mut tool_ctx = ToolContext::new(
        ctx.cwd.clone(),
        ctx.cancel.clone(),
        ctx.exec.clone(),
        ctx.config.clone(),
        ctx.session_state.clone(),
    );
    tool_ctx.session_id = ctx.session_id.clone();
    tool_ctx.turn_no = ctx.turn_no;
    tool_ctx.sandbox = sandbox_settings_from(&ctx.config);
    tool_ctx.additional_writable_dirs = additional_writable_dirs;
    tool_ctx.running_tasks = Some(ctx.session_state.clone());
    // The *live* mode, not the session's opening value — plan mode is
    // entered and left mid-session.
    tool_ctx.permission_mode = ctx.session_state.permission_mode();
    tool_ctx.tool_use_id = ctx.tool_use_id.clone();
    tool_ctx.agent_depth = ctx.agent_depth;
    tool_ctx.elicitation = Some(Arc::clone(&ctx.elicitation));
    tool_ctx.dispatch = tool.spec().needs_dispatch.then(|| {
        Arc::new(SubCallDispatcher::new(ctx))
            as Arc<dyn base::interface::tool_dispatch::ToolDispatch>
    });
    tool_ctx.from_code = ctx.from_code;
    tool_ctx.confinement = confinement_of(
        &ctx.session_state,
        &ctx.config,
        &ctx.exec,
        &ctx.cwd,
        &ctx.session_id,
    );
    tool_ctx.attendance = attendance_of(&ctx.session_state, ctx.from_code);
    tool_ctx.turn = ctx.turn.clone();
    tool_ctx.inline_commands = Some(Arc::new(SessionInlineCommands {
        hooks: ctx.hooks.clone(),
        session_id: ctx.session_id.clone(),
        cwd: ctx.cwd.clone(),
        session_state: ctx.session_state.clone(),
        events: ctx.events.clone(),
        turn_id: Some(ctx.turn_id.clone()),
        discontinued: Some(ctx.discontinued.clone()),
    }));
    tool_ctx
}

impl Agent {
    /// What a skill's `` !`cmd` `` runs with when a `/name` command expands
    /// it outside any tool call: the context a `Bash` call this turn would get.
    pub(super) fn skill_invocation_context(&self, cancel: CancellationToken) -> ToolContext {
        let mut ctx = ToolContext::new(
            self.settings.paths.working_dir().to_path_buf(),
            cancel,
            self.exec.clone(),
            self.config.clone(),
            self.session_state.clone(),
        );
        ctx.session_id = self.session.session_id.clone();
        ctx.turn_no = self.session.turn_count;
        ctx.sandbox = sandbox_settings_from(&self.config);
        ctx.additional_writable_dirs = self.session_state.env().additional_writable_dirs.clone();
        ctx.running_tasks = Some(self.session_state.clone());
        ctx.permission_mode = self.session_state.permission_mode();
        ctx.agent_depth = self.identity.agent_depth();
        ctx.elicitation = Some(Arc::clone(self.gate.elicitation()));
        ctx.confinement = confinement_of(
            &self.session_state,
            &self.config,
            &self.exec,
            self.settings.paths.working_dir(),
            &self.session.session_id,
        );
        ctx.attendance = attendance_of(&self.session_state, false);
        ctx.turn = self.turn.scope.clone();
        ctx.inline_commands = Some(Arc::new(SessionInlineCommands {
            hooks: self.gate.hooks().clone(),
            session_id: self.session.session_id.clone(),
            cwd: self.settings.paths.working_dir().to_path_buf(),
            session_state: self.session_state.clone(),
            events: Some(self.channels.event_tx.clone()),
            turn_id: None,
            discontinued: None,
        }));
        ctx
    }
}
