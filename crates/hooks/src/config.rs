//! Hook 配置（来自 settings.json `hooks` 字段）。
//!
//! 四种变体都会执行（`HookRunner::run_one` 逐一分发）：`command` 与 `http`
//! 自足；`prompt` 与 `agent` 需要宿主注入 `PromptHookExecutor` /
//! `AgentHookExecutor`，没注入时该 hook 被 Skip 并附原因，不是静默失败。
//! 四种都认 `if`（见 `runner::matcher`）与 `on_error`；`only_on_error`、`once`、
//! `async_rewake` 只有 `command` 有。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// 单条 hook 配置。`type` 字段做 enum tag。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HookConfig {
    /// 跑外部 shell 命令；stdin 给 JSON payload，stdout 解析为 HookResponse。
    Command {
        command: String,
        /// 默认 bash；Windows 上可指定 powershell
        #[serde(default)]
        shell: Option<String>,
        /// 毫秒；不填用 `DEFAULT_HOOK_TIMEOUT_MS`
        #[serde(default)]
        timeout: Option<u64>,
        /// 权限规则风格的过滤模式，例如 "Bash(git push:*)"。命中才跑。
        #[serde(default, rename = "if")]
        if_pattern: Option<String>,
        #[serde(default)]
        on_error: OnError,
        /// 仅当工具结果 is_error=true 时才跑（PostToolUse 用）
        #[serde(default)]
        only_on_error: Option<bool>,
        /// 一次性 hook：在这个会话（连同它的子 Agent）里真正跑过一次后不再跑；
        /// `if`、`only_on_error` 没放行的那几次不算
        #[serde(default)]
        once: Option<bool>,
        /// 异步唤醒机制。若为 true，hook 可返回 `{rewake: true}`
        /// 请求在后台工作完成时被重新执行。
        #[serde(default)]
        async_rewake: Option<bool>,
    },
    /// 跑 prompt 让小模型评估
    Prompt {
        prompt: String,
        #[serde(default)]
        timeout: Option<u64>,
        #[serde(default)]
        model: Option<String>,
        #[serde(default, rename = "if")]
        if_pattern: Option<String>,
        #[serde(default)]
        on_error: OnError,
    },
    /// HTTP webhook
    Http {
        url: String,
        #[serde(default)]
        headers: HashMap<String, String>,
        #[serde(default)]
        timeout: Option<u64>,
        #[serde(default, rename = "if")]
        if_pattern: Option<String>,
        #[serde(default)]
        on_error: OnError,
    },
    /// 跑子 agent
    Agent {
        prompt: String,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        timeout: Option<u64>,
        #[serde(default, rename = "if")]
        if_pattern: Option<String>,
        #[serde(default)]
        on_error: OnError,
    },
}

/// What a hook that fails — times out, errors, exits other than 0 or 2,
/// answers something that is not a JSON answer — amounts to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnError {
    /// The failure is logged and the hook says nothing.
    #[default]
    Allow,
    /// The failure is the hook answering `{"decision": "block"}`, with the
    /// failure as its message.
    Block,
}

impl HookConfig {
    fn if_pattern(&self) -> Option<&str> {
        match self {
            HookConfig::Command { if_pattern, .. }
            | HookConfig::Prompt { if_pattern, .. }
            | HookConfig::Http { if_pattern, .. }
            | HookConfig::Agent { if_pattern, .. } => if_pattern.as_deref(),
        }
    }

    /// Whether this hook applies to `input` at all: its `if` matches, and a
    /// `command` hook's `only_on_error` holds.
    pub(crate) fn applies_to(&self, input: &crate::payload::HookInput) -> Result<(), &'static str> {
        if let Some(pattern) = self.if_pattern() {
            if !crate::runner::matcher::if_matches(pattern, input) {
                return Err("if pattern did not match");
            }
        }
        if let HookConfig::Command {
            only_on_error: Some(true),
            ..
        } = self
        {
            if input.is_error != Some(true) {
                return Err("only_on_error=true but tool was not in error");
            }
        }
        Ok(())
    }

    pub(crate) fn on_error(&self) -> OnError {
        match self {
            HookConfig::Command { on_error, .. }
            | HookConfig::Prompt { on_error, .. }
            | HookConfig::Http { on_error, .. }
            | HookConfig::Agent { on_error, .. } => *on_error,
        }
    }

    pub(crate) fn is_once(&self) -> bool {
        matches!(
            self,
            HookConfig::Command {
                once: Some(true),
                ..
            }
        )
    }

    /// What a log line names the hook by.
    pub(crate) fn describe(&self) -> String {
        match self {
            HookConfig::Command { command, .. } => format!("command `{command}`"),
            HookConfig::Http { url, .. } => format!("http `{url}`"),
            HookConfig::Prompt { .. } => "prompt hook".into(),
            HookConfig::Agent { .. } => "agent hook".into(),
        }
    }
}

/// Hook 事件枚举。命名使用 PascalCase。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HookEvent {
    PreToolUse,
    PostToolUse,
    PostToolUseFailure,
    TurnStart,
    TurnComplete,
    SessionStart,
    SessionEnd,
    Stop,
    StopFailure,
    UserPromptSubmit,
    PreCompact,
    PostCompact,
    /// Fired when the permission system requires a user decision.
    PermissionRequested,
    /// Fired when a tool call is denied (by user or system).
    PermissionDenied,
    /// Fired during setup / initialization.
    Setup,
    /// Fired when user attention is required (typically a
    /// permission prompt is pending). Hook can play a sound, send a desktop
    /// notification, or buzz a chat channel — output is ignored.
    Notification,
    /// Fired when a sub-agent (`AgentTool`) finishes (success
    /// or error). Lets parent context react — e.g. log the summary to a tracking
    /// system, or alert the user that long-running work is done.
    SubagentStop,
    /// Fired when a sub-agent (`AgentTool`) starts. Hook can log, notify, or
    /// inject context before the sub-agent begins work.
    SubagentStart,
    /// A team member's turn ended, however it ended, and it goes idle;
    /// `team_name` / `teammate_name` say which.
    TeammateIdle,
    /// A running task started (a sub-agent, shell command, monitor, team
    /// member); `reason` carries the task as `session.tasks` lists it.
    TaskCreated,
    /// A team member's turn ended normally — the work it was given is done;
    /// `team_name` / `teammate_name` say which. Fires just before
    /// `TeammateIdle`.
    TaskCompleted,
    /// An MCP server's tool result contains an elicitation URL (`mcp://` or
    /// `elicitation://`) — the server is asking for user attention (e.g. an
    /// out-of-band authorization link). Fired by
    /// `hooks::HookRunner::run_elicitation` from the MCP adapter's tool-call
    /// path (`crates/mcp/src/adapter.rs`).
    Elicitation,
    /// The response half of an elicitation started by `Elicitation` above.
    /// See `HookRunner::run_elicitation_result`'s doc comment: no host-side
    /// response round-trip exists in this codebase yet, so this event has no
    /// production caller today.
    ///
    /// Listed in [`UNWIRED_EVENTS`], which is what makes configuring a hook
    /// for it warn instead of failing silently.
    ElicitationResult,
    /// Settings/config change.
    ///
    /// **Not fired today, and the reason is structural.** The only thing that
    /// mutates settings at runtime is the daemon's `config.reload` /
    /// `config.setProvider` RPC, which operates at the *pool* level — above
    /// any individual session, and therefore above the only place a
    /// `HookRunner` exists (one per session, built from that session's merged
    /// settings). Firing it would mean either fanning out to every live
    /// session from the pool, or giving the pool a hook runner of its own
    /// with no session to attach it to.
    ///
    /// A further wrinkle: the pool's own
    /// propagation mechanism does not offer a free firing site either. A
    /// reconfigured pool bumps `config_generation`, and a stale-but-idle
    /// session is then *destroyed and rebuilt* before its next turn — so the
    /// session that experienced the change no longer exists by the time
    /// anything could observe it, and the rebuilt one already fires
    /// `Setup`/`SessionStart`/`CwdChanged` indistinguishably from a fresh
    /// start.
    ///
    /// Listed here rather than deleted because the RPC surface for it exists
    /// and a hook for "the daemon was reconfigured under me" is genuinely
    /// useful; it is a wiring gap with a known shape, not a stray variant.
    ///
    /// Listed in [`UNWIRED_EVENTS`], which is what makes configuring a hook
    /// for it warn instead of failing silently.
    ConfigChange,
    /// Git worktree lifecycle.
    WorktreeCreate,
    WorktreeRemove,
    /// AGENTS.md loaded.
    InstructionsLoaded,
    /// Working directory changed.
    CwdChanged,
    /// An instruction file the root session watches changed: its
    /// `instruction_file` and every `AGENTS.md` it loaded.
    FileChanged,
    /// Fired after each model API sampling completes (streaming
    /// response fully consumed). Hook receives model output for real-time
    /// audit, logging, or modification.
    PostSampling,
}

/// Events that parse, accept hook configuration, and never fire.
///
/// Each one is a known wiring gap whose reason is written on the variant
/// itself. They are kept rather than deleted for two reasons. The shape of
/// each is right and a host that supplies the missing half has a real event
/// to subscribe to; and deleting a variant is far more destructive than it
/// looks — [`HooksSettings`] is a `HashMap` keyed by this enum with no
/// `#[serde(other)]` fallback, so one unrecognized event name fails the
/// *whole* map, and the composition root's `unwrap_or_else` then degrades to
/// an empty runner. Removing a variant would silently disable **every** hook
/// belonging to anyone who had configured that one.
///
/// The cost of keeping them is that someone configures a hook here and waits
/// for it forever. [`HookEvent::is_wired`] exists so the composition root can
/// say so out loud at startup instead.
///
/// Kept honest by `daemon/tests/hook_event_wiring.rs`, which scans the
/// production sources for firing sites and fails if this list and the code
/// disagree in either direction — so wiring one of these up without removing
/// it here is a test failure, not a stale comment.
pub const UNWIRED_EVENTS: &[HookEvent] = &[HookEvent::ElicitationResult, HookEvent::ConfigChange];

impl HookEvent {
    /// Whether anything in the engine actually fires this event.
    ///
    /// `false` means a configured hook for it will never run — see
    /// [`UNWIRED_EVENTS`].
    pub fn is_wired(self) -> bool {
        !UNWIRED_EVENTS.contains(&self)
    }
}

/// settings.json 的 `hooks` 字段：事件名 → 多个 hook 配置。
pub type HooksSettings = HashMap<HookEvent, Vec<HookConfig>>;

/// What a tolerant parse skipped, so a caller can say it out loud.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct HooksParseReport {
    /// Event names that are not [`HookEvent`] variants — typos, or hooks
    /// written for a newer engine.
    pub unknown_events: Vec<String>,
    /// `(event, error)` for entries whose *configs* would not parse.
    pub invalid_configs: Vec<(String, String)>,
}

impl HooksParseReport {
    pub fn is_clean(&self) -> bool {
        self.unknown_events.is_empty() && self.invalid_configs.is_empty()
    }
}

/// Parse `settings.hooks_config`, keeping every entry that is well-formed.
///
/// Deserializing straight into [`HooksSettings`] cannot do this. It is a
/// `HashMap` keyed by [`HookEvent`], which has no `#[serde(other)]` fallback,
/// so serde fails the **whole map** on the first unrecognized key — and the
/// composition root's `unwrap_or_else` then falls back to an empty runner.
/// One typo, or one hook copied from a newer version of the engine, silently
/// disabled *every* hook the user had configured, with a single warn line as
/// the only evidence.
///
/// Going key by key confines the damage to the entry that is actually broken.
/// Nothing is repaired or guessed at: an unrecognized name is dropped and
/// reported, never mapped to a "closest" event.
pub fn parse_hooks_settings(value: &serde_json::Value) -> (HooksSettings, HooksParseReport) {
    let mut out = HooksSettings::new();
    let mut report = HooksParseReport::default();

    let Some(map) = value.as_object() else {
        report.invalid_configs.push((
            "<root>".into(),
            "expected an object of event name -> array of hook configs".into(),
        ));
        return (out, report);
    };

    for (name, configs) in map {
        let Ok(event) =
            serde_json::from_value::<HookEvent>(serde_json::Value::String(name.clone()))
        else {
            report.unknown_events.push(name.clone());
            continue;
        };
        match serde_json::from_value::<Vec<HookConfig>>(configs.clone()) {
            Ok(list) => {
                out.entry(event).or_default().extend(list);
            }
            Err(e) => report.invalid_configs.push((name.clone(), e.to_string())),
        }
    }

    (out, report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_hooks_settings_from_json() {
        let v = json!({
            "PreToolUse": [
                {
                    "type": "command",
                    "command": "./scripts/check.sh",
                    "timeout": 5000,
                    "if": "Bash(git push:*)"
                }
            ],
            "PostToolUse": [],
            "SessionStart": [
                {
                    "type": "command",
                    "command": "echo session-started"
                }
            ]
        });
        let settings: HooksSettings = serde_json::from_value(v).unwrap();
        assert_eq!(settings[&HookEvent::PreToolUse].len(), 1);
        assert_eq!(settings[&HookEvent::SessionStart].len(), 1);
        assert_eq!(settings[&HookEvent::PostToolUse].len(), 0);
    }

    #[test]
    fn command_variant_full_fields() {
        let v = json!({
            "type": "command",
            "command": "x",
            "shell": "bash",
            "timeout": 1000,
            "if": "Bash",
            "only_on_error": true,
            "once": false,
            "async_rewake": true,
            "on_error": "block"
        });
        let h: HookConfig = serde_json::from_value(v).unwrap();
        match h {
            HookConfig::Command {
                command,
                shell,
                timeout,
                if_pattern,
                on_error,
                only_on_error,
                once,
                async_rewake,
            } => {
                assert_eq!(command, "x");
                assert_eq!(shell.as_deref(), Some("bash"));
                assert_eq!(timeout, Some(1000));
                assert_eq!(if_pattern.as_deref(), Some("Bash"));
                assert_eq!(only_on_error, Some(true));
                assert_eq!(once, Some(false));
                assert_eq!(async_rewake, Some(true));
                assert_eq!(on_error, OnError::Block);
            }
            _ => panic!("expected Command variant"),
        }
    }

    #[test]
    fn prompt_variant_decodes_but_runner_will_skip() {
        let v = json!({
            "type": "prompt",
            "prompt": "is this safe?",
            "model": "claude-haiku-4-5"
        });
        let h: HookConfig = serde_json::from_value(v).unwrap();
        assert!(matches!(h, HookConfig::Prompt { .. }));
    }
}

#[cfg(test)]
mod tolerant_parse_tests {
    use super::*;
    use serde_json::json;

    /// One bad name must not take every other hook down with it.
    #[test]
    fn an_unknown_event_name_does_not_disable_the_other_hooks() {
        let cfg = json!({
            "PreToolUse":  [{"type": "command", "command": "echo pre"}],
            "NoSuchEvent": [{"type": "command", "command": "echo nope"}],
            "SessionEnd":  [{"type": "command", "command": "echo end"}],
        });
        let (parsed, report) = parse_hooks_settings(&cfg);

        assert!(parsed.contains_key(&HookEvent::PreToolUse));
        assert!(parsed.contains_key(&HookEvent::SessionEnd));
        assert_eq!(parsed.len(), 2);
        assert_eq!(report.unknown_events, vec!["NoSuchEvent".to_string()]);

        // The whole-map parse still fails on the same input.
        assert!(serde_json::from_value::<HooksSettings>(cfg).is_err());
    }

    /// A malformed config under a *valid* event is confined the same way.
    #[test]
    fn a_malformed_entry_does_not_disable_the_others() {
        let (parsed, report) = parse_hooks_settings(&json!({
            "PreToolUse": [{"type": "command", "command": "echo pre"}],
            "Stop":       [{"type": "not_a_hook_kind"}],
        }));
        assert_eq!(parsed.len(), 1);
        assert!(parsed.contains_key(&HookEvent::PreToolUse));
        assert_eq!(report.invalid_configs.len(), 1);
        assert_eq!(report.invalid_configs[0].0, "Stop");
    }

    /// Nothing is guessed at. A near-miss is dropped, not corrected to the
    /// event it resembles — silently running a hook on an event the author
    /// did not name is worse than not running it.
    #[test]
    fn a_near_miss_name_is_dropped_not_corrected() {
        let (parsed, report) = parse_hooks_settings(&json!({
            "pretooluse": [{"type": "command", "command": "echo x"}],
        }));
        assert!(parsed.is_empty());
        assert_eq!(report.unknown_events, vec!["pretooluse".to_string()]);
    }

    #[test]
    fn a_clean_config_reports_clean() {
        let (parsed, report) = parse_hooks_settings(&json!({
            "PreToolUse": [{"type": "command", "command": "echo pre"}],
        }));
        assert_eq!(parsed.len(), 1);
        assert!(report.is_clean());
    }

    #[test]
    fn a_non_object_is_reported_rather_than_panicking() {
        let (parsed, report) = parse_hooks_settings(&json!("not-a-map"));
        assert!(parsed.is_empty());
        assert_eq!(report.invalid_configs.len(), 1);
    }
}
