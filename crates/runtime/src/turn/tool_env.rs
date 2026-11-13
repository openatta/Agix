//! 工具运行环境：沙箱、出口、引擎 git 的装配，工具上下文，子调用分发与技能调用上下文。

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

/// The context a call to `tool` runs with: the same for validating its input
/// as for running it.
pub(super) fn tool_context_for(ctx: &ToolExecCtx, tool: &Arc<dyn base::tool::Tool>) -> ToolContext {
    let additional_writable_dirs = ctx.session_state.additional_writable_dirs.clone();
    ToolContext {
        cwd: ctx.cwd.clone(),
        session_id: ctx.session_id.clone(),
        turn_no: ctx.turn_no,
        sandbox: sandbox_settings_from(&ctx.config),
        cancel: ctx.cancel.clone(),
        additional_writable_dirs,
        snapshot_file: None,
        effects: None,
        running_tasks: Some(ctx.session_state.clone()),
        dangerously_disable_sandbox: ctx.config.dangerously_disable_sandbox,
        max_file_read_bytes: ctx.config.file_limits.max_file_read_bytes as usize,
        // The *live* mode, not the session's opening value — plan mode is
        // entered and left mid-session.
        permission_mode: ctx.session_state.permission_mode(),
        config: ctx.config.clone(),
        session: ctx.session_state.clone(),
        tool_use_id: ctx.tool_use_id.clone(),
        agent: None,
        parent_messages: None,
        agent_depth: ctx.agent_depth,
        events_tx: None,
        elicitation: Some(Arc::clone(&ctx.elicitation)),
        exec: ctx.exec.clone(),
        dispatch: tool.spec().needs_dispatch.then(|| {
            Arc::new(SubCallDispatcher::new(ctx))
                as Arc<dyn base::interface::tool_dispatch::ToolDispatch>
        }),
        from_code: ctx.from_code,
        confinement: Default::default(),
    }
}

impl Agent {
    /// What a skill's `` !`cmd` `` runs with when a `/name` command expands
    /// it outside any tool call: the context a `Bash` call this turn would get.
    pub(super) fn skill_invocation_context(&self, cancel: CancellationToken) -> ToolContext {
        ToolContext {
            cwd: self.settings.paths.working_dir().to_path_buf(),
            session_id: self.session.session_id.clone(),
            turn_no: self.session.turn_count,
            sandbox: sandbox_settings_from(&self.config),
            cancel,
            additional_writable_dirs: self.session_state.additional_writable_dirs.clone(),
            snapshot_file: None,
            effects: None,
            running_tasks: Some(self.session_state.clone()),
            dangerously_disable_sandbox: self.config.dangerously_disable_sandbox,
            max_file_read_bytes: self.config.file_limits.max_file_read_bytes as usize,
            permission_mode: self.session_state.permission_mode(),
            config: self.config.clone(),
            session: self.session_state.clone(),
            tool_use_id: String::new(),
            agent: None,
            parent_messages: None,
            agent_depth: self.identity.agent_depth,
            events_tx: None,
            elicitation: Some(Arc::clone(&self.gate.elicitation)),
            exec: self.exec.clone(),
            dispatch: None,
            from_code: false,
            confinement: Default::default(),
        }
    }
}
