//! 引擎只读配置 + 模型窗口推断。

use crate::permission::PermissionMode;
use std::path::PathBuf;

/// 引擎只读配置。从 settings.json + CLI flags 合并出来后，整个会话不再变。
#[derive(Debug, Clone)]
pub struct EngineConfig {
    // ---- model / runtime ----
    /// Provider id (e.g. "anthropic", "deepseek", "xai").
    pub provider: Option<String>,
    pub model: String,
    pub max_tokens: u32,
    /// How long a tool call waits for a permission answer before denying.
    /// `0` = wait indefinitely. See
    /// `ExecutionSettings::permission_prompt_timeout_secs`.
    pub permission_prompt_timeout_secs: u64,
    /// Maximum number of concurrency-safe tools to run in parallel
    /// within a single batch. 0 = unlimited. Default 8.
    pub max_parallelism: usize,
    /// +** — per-turn API call budget. Default **200**.
    ///   When exceeded, engine returns gracefully with
    ///   `StopReason::MaxTurnsReached` instead of an error — partial work
    ///   is preserved and surfaced to the caller. Lower this for sub-
    ///   agents you want to cap (e.g. compactors at 1, memory-extractors
    ///   at 5).
    pub max_api_calls_per_turn: u32,
    /// Maximum number of team-stage sub-agents (`crates/team`'s `TeamCreate`
    /// tool) dispatched concurrently within a single stage. Was a hardcoded
    /// `const STAGE_CONCURRENCY_LIMIT = 6` in `coordinator.rs` with no way
    /// to tune it for resource-constrained or high-throughput deployments.
    /// Default 6, matching the prior hardcoded value.
    pub team_stage_concurrency: usize,
    /// Reclaim policy for persistent team members (`Agent` tool's
    /// `team_name`+`name` mode). See `ExecutionSettings::
    /// team_max_persistent_members`'s doc comment for the policy.
    pub team_max_persistent_members: usize,
    /// See `ExecutionSettings::team_member_idle_timeout_secs`.
    pub team_member_idle_timeout_secs: u64,
    /// + §3 — auto-routing**: optional "strong" tier model. When set,
    ///   the engine starts each turn at `model` (cheap/fast tier, e.g.
    ///   `ds-v4-flash`) and **escalates to `strong_model`** mid-turn on any
    ///   of these signals:
    ///   - `tool_calls_so_far >= 3` (multi-step exploration)
    ///   - previous tool_result was an error (need smarter retry)
    ///   - turn message count >= 8 (deep conversation)
    ///
    /// Once escalated, stays at strong for rest of the turn.
    /// `None` = no routing (use `model` for everything; current default).
    /// User-facing API: surface as `--strong-model` CLI flag, or in
    /// `~/.atta/scenes/<scope>/settings.json`.
    pub strong_model: Option<String>,
    /// **P2 (Phase 2)**: optional fallback model for retry on overloaded /
    /// transport errors. When the primary model returns 503/529/transport
    /// error, the engine falls back to this model for the remaining turns.
    /// Once activated, stays active for the rest of the session to avoid
    /// oscillation.
    /// `None` = no fallback (propagate the error immediately).
    /// User-facing API: surface as `--fallback-model` CLI flag.
    pub fallback_model: Option<String>,
    /// **L1 **: thinking-mode policy.
    /// - `Auto` — current default; engine picks based on model id (DS V4
    ///   gets `Disabled` to avoid the multi-turn 400 reasoning_content
    ///   protocol mismatch; everyone else gets `None` = let model decide).
    /// - `Off` — explicit `thinking: {"type":"disabled"}` in request.
    /// - `On` — explicit `thinking: Adaptive`.
    /// - `OnBudget(N)` — explicit `thinking: Enabled{budget_tokens=N}`.
    pub thinking_mode: ThinkingModeConfig,

    // ---- permission / sandbox ----
    pub permission_mode: PermissionMode,
    /// 是否禁用 BashTool 沙盒（才有效；占位）
    pub dangerously_disable_sandbox: bool,
    /// **Hardening **: Bash sandbox extended policy. Default:
    /// `default_deny_read` baked-in (~/.ssh, ~/.aws, etc), network unrestricted.
    /// Settings.json `sandbox.{deny_read,allow_read,allowed_domains,network_mode}`
    /// overrides any field. Tool layer (`attacode-tools::bash::sandbox`) reads
    /// these fields to build its platform-specific profile.
    pub sandbox_policy: SandboxPolicyConfig,
    /// Mirrors `Settings.disable_skill_shell_execution` — read by
    /// `SkillTool`'s dynamic-content-injection step via `ToolContext.config`.
    /// Unlike `dangerously_disable_sandbox` above, this one *is* correctly
    /// threaded from `Settings` at the one place `EngineConfig` gets built
    /// from real settings (`agent.rs`'s `agent_engine_config` block) — not
    /// left at its `defaults_for()` default everywhere.
    pub disable_skill_shell_execution: bool,
    /// Mirrors `Settings.sandbox.subprocess_env`.
    pub subprocess_env: crate::interface::exec::local::SubprocessEnv,

    // ---- file-op limits (grouped 2026-05-09; was 3 flat fields) ----
    /// File read/write byte/line limits. Sub-config keeps related field
    /// values together so cross-tool tuning (Read/Write/Edit) is one
    /// import path.
    pub file_limits: FileLimits,

    // ---- compaction (grouped 2026-05-09; was 2 flat fields) ----
    /// autoCompact configuration: when to fire + which model to summarize.
    pub compact: CompactSettings,

    // ---- system-prompt assembly (grouped 2026-05-09; was 5 flat fields) ----
    /// system prompt overrides + extras (AGENTS.md walk, output styles,
    /// append/override). All affect what gets baked into block [1-5] of
    /// the system prompt.
    pub system_prompt: SystemPromptSettings,

    /// 最大 agent 嵌套深度。主 agent 深度 0，每经 AgentTool spawn 加 1。
    /// 超限时 AgentTool 返回 tool error（防模型在子 agent prompt 中写"用 AgentTool"
    /// 导致无限递归）。默认 3。设 0 禁用 AgentTool。
    pub max_agent_depth: u32,

    // ---- misc behavior ----
    /// **RC-23**: 是否保留完整工具结果文本。默认 false —— 工具结果会被截断到
    /// 2KB 再送回给模型。设 true 时原样透传（长输出场景可能显著消耗 context）。
    pub verbose_tool_results: bool,
}

/// **§5 (2026-05-09)**: file-op limits sub-config. Was 3 flat fields on
/// `EngineConfig`; grouping them surfaces the "tuning the FileRead/Write
/// envelope" cluster as one knob.
#[derive(Debug, Clone)]
pub struct FileLimits {
    /// 文件读 / 写的字节上限。FileRead 检查 metadata.len() 用。
    pub max_file_read_bytes: u64,
    /// 单次 FileRead 默认行数（offset/limit 没填时）
    pub default_read_lines: usize,
    /// 单行字符数上限（超了截断 + `[truncated]` 标记）
    pub max_line_chars: usize,
    /// Tool result 文本内容字符数上限。超长的 result text 在回灌 messages 前
    /// 截断为 `[Output truncated to N chars]`，防止撑爆 context。
    /// 0 = 不截断。默认 50KB。
    pub max_tool_result_chars: usize,
}

impl Default for FileLimits {
    fn default() -> Self {
        Self {
            max_file_read_bytes: 10 * 1024 * 1024,
            default_read_lines: 2000,
            max_line_chars: 2000,
            max_tool_result_chars: 50 * 1024,
        }
    }
}

/// **§5 (2026-05-09)**: autoCompact sub-config.
#[derive(Debug, Clone)]
pub struct CompactSettings {
    /// 触发 autoCompact 的输入 token 阈值。0 = 禁用 autoCompact。
    pub threshold_tokens: usize,
    /// 执行压缩用的模型 id（比主模型便宜的 haiku 类）。空 → 复用 model
    pub model: Option<String>,
    /// **P2 **: collapse/micro compact 保留尾部多少条 messages verbatim。
    /// 默认 6（≈3 个 user/assistant pair）。调大保留更多上下文但压缩收益减少。
    pub micro_keep_recent: usize,
}

impl Default for CompactSettings {
    fn default() -> Self {
        Self {
            // 由 main.rs 按模型 context window 再覆写；这里保底给一个中等值。
            threshold_tokens: 150_000,
            model: None,
            micro_keep_recent: 6,
        }
    }
}

/// **§5 (2026-05-09)**: system-prompt assembly knobs sub-config.
#[derive(Debug, Clone, Default)]
pub struct SystemPromptSettings {
    pub append: Option<String>,
    pub override_text: Option<String>,
    /// Preferred response language. None = use the user's language/context.
    pub language: Option<String>,
    /// Enable Anthropic prompt-caching `scope: "global"` for the static system
    /// prompt prefix. Requires the prompt-caching-scope beta header and may not
    /// be supported by non-Anthropic-compatible endpoints.
    pub global_cache_scope: bool,
    /// True when at least one connected MCP tool is rendered in the tool list.
    /// MCP tools are per-user/session, so system prompt global caching is
    /// downgraded to normal org/provider caching in this case.
    pub mcp_tools_present: bool,
    /// Instructions returned by connected MCP servers during initialize.
    pub mcp_instructions: Vec<McpServerInstruction>,
    /// 是否在 cwd 之上向上爬找 AGENTS.md。默认 true。
    /// monorepo 子目录不想吃父级 monorepo 上下文时设 false。
    pub memory_walk_up: bool,
    /// **A-5 **: 选中的 output style 名称（user/project 级别均可）。
    /// 引擎在 collect FrozenContext 时按此名称从
    /// `~/.atta/scenes/<scope>/output-styles/<name>.md` 或 `<cwd>/.atta/output-styles/<name>.md`
    /// 读取内容，注入 system prompt 末段。None = 不注入额外 style。
    pub output_style: Option<String>,
    /// When true (default), use TaskCreate/Update/List/Get/Stop instead of
    /// TodoWrite for task tracking. Mirrors TS's `isTodoV2Enabled()` gating.
    /// When false, TodoWrite is used and the V2 Task tools are hidden.
    pub todo_v2_enabled: bool,
    /// When true, register TeamCreate/TeamDelete for multi-agent orchestration.
    /// Mirrors TS's `isAgentSwarmsEnabled()` gating. Default false.
    pub agent_teams_enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct McpServerInstruction {
    pub name: String,
    pub instructions: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ThinkingModeConfig {
    /// Engine decides based on model id. Default.
    #[default]
    Auto,
    /// Explicit "no thinking" — sends `thinking: {type: "disabled"}`.
    Off,
    /// Explicit adaptive thinking.
    On,
    /// Explicit budgeted thinking.
    OnBudget(u32),
}

/// **Hardening **: data-only sandbox policy in core (so `EngineConfig`
/// doesn't need to depend on the tools crate). Lifted to a real `SandboxPolicy`
/// inside `attacode-tools::bash::sandbox` at call-site.
#[derive(Debug, Clone, Default)]
pub struct SandboxPolicyConfig {
    pub allow_read: Vec<PathBuf>,
    /// Empty = use built-in defaults. Non-empty = use this list verbatim
    /// (caller can disable defaults entirely by passing `[PathBuf::from("")]`
    /// — actually no, we use Option semantics: None means "use defaults",
    /// Some(vec) means "use exactly this").
    pub deny_read: Option<Vec<PathBuf>>,
    pub network_mode: NetworkModeConfig,
    pub allowed_domains: Vec<String>,
    /// Extra filename segments no tool may write. See
    /// `base::settings::SandboxConfig::deny_write`.
    pub deny_write: Vec<String>,
    /// Paths exempt from the write deny rules. See
    /// `base::settings::SandboxConfig::allow_write`.
    pub allow_write: Vec<PathBuf>,
    /// This instance's global state root, so the profile can protect the
    /// settings.json files that actually govern it. `None` means the caller
    /// didn't say, and the sandbox falls back to `$HOME/.atta` — which is
    /// only right when the instance happens to live there.
    pub state_root: Option<PathBuf>,
    /// See `base::settings::SandboxConfig::require_enforcement`.
    pub require_enforcement: bool,
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Default,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum NetworkModeConfig {
    #[default]
    Unrestricted,
    DenyAll,
    Allowlist,
}

impl EngineConfig {
    /// 合理默认值；具体使用方根据 CLI 与 settings 覆盖。
    pub fn defaults_for(model: impl Into<String>) -> Self {
        let model = model.into();
        let compact_threshold = default_auto_compact_threshold(&model, 65535);
        Self {
            // model / runtime
            provider: None,
            model,
            max_tokens: 16384,
            permission_prompt_timeout_secs: 300,
            // **P7 **: 4 → 8 to align closer to TS reference (which defaults
            // to 10). 8 leaves a 2-slot safety margin for tokio scheduler under
            // heavy parallel Read/Glob/Grep batches.
            max_parallelism: 10,
            max_api_calls_per_turn: 25,
            team_stage_concurrency: 6,
            team_max_persistent_members: 20,
            team_member_idle_timeout_secs: 1800,
            strong_model: None,
            fallback_model: None,
            thinking_mode: ThinkingModeConfig::default(),

            // permission / sandbox
            permission_mode: PermissionMode::Default,
            dangerously_disable_sandbox: false,
            sandbox_policy: SandboxPolicyConfig::default(),
            disable_skill_shell_execution: false,
            subprocess_env: Default::default(),

            // grouped sub-configs
            file_limits: FileLimits::default(),
            compact: CompactSettings {
                threshold_tokens: compact_threshold,
                ..Default::default()
            },
            system_prompt: SystemPromptSettings {
                memory_walk_up: true,
                todo_v2_enabled: true,
                agent_teams_enabled: false,
                ..Default::default()
            },

            // misc
            max_agent_depth: 3,
            verbose_tool_results: false,
        }
    }

    /// Derive a real `EngineConfig` from a loaded `Settings` — the
    /// production tool-dispatch call site (`runtime::turn::execute_tool_inner`)
    /// previously hardcoded `defaults_for("unknown")` here, silently discarding whatever the user
    /// actually configured in `settings.json` (permission mode, sandbox
    /// policy, `disable_skill_shell_execution`, etc.) for every single tool
    /// call. Starts from `defaults_for()`'s baseline and overlays every field
    /// `Settings` actually carries; fields `Settings` has no equivalent for
    /// (e.g. `strong_model`, `file_limits`, `max_agent_depth`) are left at
    /// their `defaults_for()` value, same as before this existed.
    pub fn from_settings(settings: &crate::settings::Settings) -> Self {
        let mut c = Self::defaults_for(settings.model.model_name.clone());
        c.max_tokens = settings.model.max_tokens;
        c.fallback_model = settings.model.fallback_model.clone();
        c.thinking_mode = match settings.model.thinking_mode {
            crate::settings::ThinkingMode::Auto => ThinkingModeConfig::Auto,
            crate::settings::ThinkingMode::Off => ThinkingModeConfig::Off,
            crate::settings::ThinkingMode::On => ThinkingModeConfig::On,
            crate::settings::ThinkingMode::OnBudget(n) => ThinkingModeConfig::OnBudget(n),
        };

        c.permission_prompt_timeout_secs = settings.execution.permission_prompt_timeout_secs;
        c.max_parallelism = settings.execution.max_parallelism;
        c.max_api_calls_per_turn = settings.execution.max_api_calls_per_turn;
        c.team_stage_concurrency = settings.execution.team_stage_concurrency;
        c.team_max_persistent_members = settings.execution.team_max_persistent_members;
        c.team_member_idle_timeout_secs = settings.execution.team_member_idle_timeout_secs;

        // `Settings.permission_mode` (`interface::settings::PermissionMode`,
        // serde/JSON-schema-facing) and `EngineConfig.permission_mode`
        // (`permission::PermissionMode`, what `PermissionGate` actually
        // dispatches on) are two distinct types with identical variant sets
        // — a pre-existing split, not something to unify here. Conversion
        // lives on `permission::PermissionMode` (`impl From<...>`) so other
        // call sites (e.g. daemon's per-session `RuleSetPermission`) share it.
        c.permission_mode = settings.permission_mode.into();
        c.dangerously_disable_sandbox = settings.sandbox.dangerously_disable_sandbox;
        // `Settings.sandbox.deny_read` is a plain `Vec`, empty when the user
        // never configured it — mapping that to `Some(vec![])` would read as
        // "use exactly this empty list", which *disables* the sandbox's
        // built-in deny defaults (~/.ssh, ~/.aws, etc). Empty must map to
        // `None` ("use built-in defaults"), matching `SandboxPolicyConfig`'s
        // own documented Option semantics.
        c.sandbox_policy = SandboxPolicyConfig {
            allow_read: settings.sandbox.allow_read.clone(),
            deny_write: settings.sandbox.deny_write.clone(),
            allow_write: settings.sandbox.allow_write.clone(),
            deny_read: if settings.sandbox.deny_read.is_empty() {
                None
            } else {
                Some(settings.sandbox.deny_read.clone())
            },
            // Was hardcoded to the type default, which silently ignored a
            // configured `sandbox.network_mode` — carry it through like every
            // other field.
            network_mode: settings.sandbox.network_mode,
            allowed_domains: settings.sandbox.allowed_domains.clone(),
            // The instance's own root, so the profile denies writes to the
            // settings.json this session actually reads — not to whatever
            // sits under the invoking user's home.
            state_root: Some(settings.paths.global_data_dir.clone()),
            require_enforcement: settings.sandbox.require_enforcement,
        };
        c.disable_skill_shell_execution = settings.disable_skill_shell_execution;
        c.subprocess_env = settings.sandbox.subprocess_env.clone();

        c.compact = CompactSettings {
            threshold_tokens: settings.compaction.threshold_tokens,
            micro_keep_recent: settings.compaction.keep_recent,
            ..c.compact
        };
        c.system_prompt = SystemPromptSettings {
            append: settings.prompt_append.clone(),
            override_text: settings.prompt_override.clone(),
            ..c.system_prompt
        };

        c
    }
}

/// 依据模型名推断其上下文窗口大小。
///
/// 依据"模型能装多少"来确定 auto-compact / blocking limit 阈值，
/// 而非对所有模型使用相同的固定值。
///
/// 当前实现是保守启发式，允许用户通过 CLI / settings 覆盖。
pub fn infer_context_window_tokens(model: &str) -> usize {
    let m = model.to_ascii_lowercase();
    if m.contains("deepseek") || m.starts_with("ds-v") || m.starts_with("ds_") {
        1_000_000
    } else if m.contains("codex") {
        400_000
    } else {
        200_000
    }
}

/// Proactive compact threshold:
/// effective_window - 13k buffer.
pub fn default_auto_compact_threshold(model: &str, max_tokens: u32) -> usize {
    let effective_window = infer_context_window_tokens(model).saturating_sub(max_tokens as usize);
    effective_window.saturating_sub(13_000)
}

/// Blocking limit:
/// effective_window - 3k buffer, used to preserve room for manual /compact.
pub fn default_blocking_limit(model: &str, max_tokens: u32) -> usize {
    let effective_window = infer_context_window_tokens(model).saturating_sub(max_tokens as usize);
    effective_window.saturating_sub(3_000)
}

#[cfg(test)]
mod sandbox_wiring_tests {
    use super::*;
    use crate::settings::Settings;

    fn settings_with_sandbox(f: impl FnOnce(&mut crate::settings::SandboxConfig)) -> Settings {
        let mut s = Settings::defaults_for("test-model");
        f(&mut s.sandbox);
        s
    }

    /// The whole point of `settings.sandbox` is that a user can configure it.
    /// Each field below reaches `EngineConfig`, which `runtime::turn` copies
    /// into `ToolContext.sandbox` and `tools::bash` lifts into a real
    /// `SandboxPolicy` — a gap anywhere on that chain makes the setting inert
    /// while still appearing in the schema.
    #[test]
    fn every_sandbox_setting_reaches_the_engine_config() {
        let s = settings_with_sandbox(|sb| {
            sb.network_mode = NetworkModeConfig::Allowlist;
            sb.allowed_domains = vec!["api.example.com".into()];
            sb.allow_read = vec![PathBuf::from("/tmp/ok")];
            sb.deny_read = vec![PathBuf::from("/tmp/secret")];
            sb.dangerously_disable_sandbox = true;
        });
        let c = EngineConfig::from_settings(&s);

        assert_eq!(c.sandbox_policy.network_mode, NetworkModeConfig::Allowlist);
        assert_eq!(c.sandbox_policy.allowed_domains, ["api.example.com"]);
        assert_eq!(c.sandbox_policy.allow_read, [PathBuf::from("/tmp/ok")]);
        assert_eq!(
            c.sandbox_policy.deny_read,
            Some(vec![PathBuf::from("/tmp/secret")])
        );
        assert!(c.dangerously_disable_sandbox);
    }

    /// An unconfigured `deny_read` must arrive as `None` ("use the built-in
    /// credential deny defaults"), never as `Some(vec![])` — which reads as
    /// "deny exactly nothing" and would switch those defaults off, making
    /// `~/.ssh` and `~/.aws` readable by any command the classifier allows.
    #[test]
    fn empty_deny_read_means_defaults_not_an_empty_deny_list() {
        let c = EngineConfig::from_settings(&settings_with_sandbox(|sb| {
            sb.deny_read = Vec::new();
        }));
        assert_eq!(c.sandbox_policy.deny_read, None);
    }
}
