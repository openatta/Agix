//! Doctor checks that report the daemon's safety posture: settings and build
//! facts that are legitimate but that an operator should have seen written
//! down — a permission mode that never asks, hooks that fail open, unattended
//! commands with an open network, a build whose panics end the process.

use base::context::config::NetworkModeConfig;
use base::interface::health::{CheckResult, HealthCheck};
use base::interface::settings::{PermissionMode, Settings};
use hooks::{HookConfig, HookEvent, OnError};
use std::sync::Arc;

pub struct PermissionModeCheck(Arc<Settings>);

impl PermissionModeCheck {
    pub fn new(settings: Arc<Settings>) -> Self {
        Self(settings)
    }
}

impl HealthCheck for PermissionModeCheck {
    fn name(&self) -> &str {
        "settings.permission_mode"
    }

    fn check(&self) -> CheckResult {
        let mode = self.0.permission_mode;
        let details = serde_json::json!({ "permission_mode": mode });
        if mode == PermissionMode::BypassPermissions {
            CheckResult::degraded(
                "permission_mode is bypassPermissions (also spelled auto / yolo): \
                 tool calls run without asking",
            )
            .with_details(details)
        } else {
            CheckResult::ok("permission mode asks or restricts").with_details(details)
        }
    }
}

const INTERCEPTING_EVENTS: [HookEvent; 3] = [
    HookEvent::PreToolUse,
    HookEvent::UserPromptSubmit,
    HookEvent::Stop,
];

pub struct HooksFailOpenCheck(Arc<Settings>);

impl HooksFailOpenCheck {
    pub fn new(settings: Arc<Settings>) -> Self {
        Self(settings)
    }
}

fn on_error_of(config: &HookConfig) -> OnError {
    match config {
        HookConfig::Command { on_error, .. }
        | HookConfig::Prompt { on_error, .. }
        | HookConfig::Http { on_error, .. }
        | HookConfig::Agent { on_error, .. } => *on_error,
    }
}

impl HealthCheck for HooksFailOpenCheck {
    fn name(&self) -> &str {
        "settings.hooks_fail_open"
    }

    fn check(&self) -> CheckResult {
        let Some(value) = &self.0.hooks_config else {
            return CheckResult::ok("no hooks configured");
        };
        let (parsed, _) = hooks::parse_hooks_settings(value);
        let mut open: Vec<String> = INTERCEPTING_EVENTS
            .iter()
            .filter_map(|event| {
                let count = parsed
                    .get(event)?
                    .iter()
                    .filter(|h| on_error_of(h) == OnError::Allow)
                    .count();
                (count > 0).then(|| format!("{event:?} ({count})"))
            })
            .collect();
        open.sort();
        if open.is_empty() {
            return CheckResult::ok("no intercepting hook fails open");
        }
        CheckResult::degraded(format!(
            "hooks on {} let the action through when they fail (timeout, crash, bad output); \
             set \"on_error\": \"block\" to make a failure block",
            open.join(", ")
        ))
        .with_details(serde_json::json!({ "fail_open": open }))
    }
}

/// Unattended sessions auto-run a `Bash` / `Monitor` command the sandbox
/// fully holds; with the network open and the network policy not required,
/// such a command can still send what it reads anywhere.
pub struct UnattendedNetworkCheck {
    settings: Arc<Settings>,
    unattended_scenes: Vec<String>,
}

impl UnattendedNetworkCheck {
    pub fn new(settings: Arc<Settings>, unattended_scenes: Vec<String>) -> Self {
        Self {
            settings,
            unattended_scenes,
        }
    }
}

impl HealthCheck for UnattendedNetworkCheck {
    fn name(&self) -> &str {
        "sandbox.unattended_network"
    }

    fn check(&self) -> CheckResult {
        let sandbox = &self.settings.sandbox;
        let details = serde_json::json!({
            "unattended_scenes": self.unattended_scenes,
            "network_mode": sandbox.network_mode,
            "unattended_requires_network_policy": sandbox.unattended_requires_network_policy,
        });
        let open = sandbox.network_mode == NetworkModeConfig::Unrestricted
            && !sandbox.unattended_requires_network_policy;
        if self.unattended_scenes.is_empty() {
            CheckResult::ok("no unattended scene is served")
        } else if !open {
            CheckResult::ok("unattended commands run without asking only under a network policy")
        } else {
            CheckResult::degraded(format!(
                "unattended scenes ({}) run sandboxed Bash / Monitor commands without asking \
                 while sandbox.network_mode is unrestricted: such a command can send what it \
                 reads anywhere. Set sandbox.unattended_requires_network_policy to true, or \
                 network_mode to deny_all / allowlist; the setting is planned to default to \
                 true in a later 0.3.x release",
                self.unattended_scenes.join(", ")
            ))
        }
        .with_details(details)
    }
}

pub struct PanicStrategyCheck;

impl PanicStrategyCheck {
    pub fn abort_build() -> bool {
        cfg!(panic = "abort")
    }
}

impl HealthCheck for PanicStrategyCheck {
    fn name(&self) -> &str {
        "build.panic"
    }

    fn check(&self) -> CheckResult {
        let abort = Self::abort_build();
        let details = serde_json::json!({ "panic": if abort { "abort" } else { "unwind" } });
        if abort {
            CheckResult::ok(
                "built with panic = \"abort\": a panic in a tool ends the process \
                 (let a supervisor restart it) instead of becoming that call's error",
            )
        } else {
            CheckResult::ok("built with panic = \"unwind\": a panic in a tool becomes its error")
        }
        .with_details(details)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings_with(f: impl FnOnce(&mut Settings)) -> Arc<Settings> {
        let mut s = Settings::defaults_for("m");
        f(&mut s);
        Arc::new(s)
    }

    #[test]
    fn only_all_three_together_flag_the_unattended_network() {
        let unattended = || vec!["code".to_string()];
        let status = |settings, scenes| {
            UnattendedNetworkCheck::new(settings, scenes)
                .check()
                .status
                .label()
        };
        assert_eq!(status(settings_with(|_| {}), unattended()), "degraded");
        assert_eq!(status(settings_with(|_| {}), Vec::new()), "ok");
        assert_eq!(
            status(
                settings_with(|s| s.sandbox.network_mode = NetworkModeConfig::Allowlist),
                unattended()
            ),
            "ok"
        );
        assert_eq!(
            status(
                settings_with(|s| s.sandbox.unattended_requires_network_policy = true),
                unattended()
            ),
            "ok"
        );
    }

    #[test]
    fn bypass_permissions_is_flagged() {
        let ask = PermissionModeCheck::new(settings_with(|_| {})).check();
        assert!(ask.status.label() == "ok", "{ask:?}");
        let bypass = PermissionModeCheck::new(settings_with(|s| {
            s.permission_mode = PermissionMode::BypassPermissions
        }))
        .check();
        assert_eq!(bypass.status.label(), "degraded");
        assert!(bypass.summary.contains("auto / yolo"));
    }

    #[test]
    fn a_pretooluse_hook_without_on_error_is_reported_and_a_blocking_one_is_not() {
        let open = settings_with(|s| {
            s.hooks_config = Some(serde_json::json!({
                "PreToolUse": [{ "type": "command", "command": "true" }],
                "PostToolUse": [{ "type": "command", "command": "true" }],
            }));
        });
        let r = HooksFailOpenCheck::new(open).check();
        assert_eq!(r.status.label(), "degraded", "{r:?}");
        assert!(r.summary.contains("PreToolUse"));
        assert!(!r.summary.contains("PostToolUse"));

        let closed = settings_with(|s| {
            s.hooks_config = Some(serde_json::json!({
                "PreToolUse": [{ "type": "command", "command": "true", "on_error": "block" }],
            }));
        });
        assert_eq!(HooksFailOpenCheck::new(closed).check().status.label(), "ok");
        assert_eq!(
            HooksFailOpenCheck::new(settings_with(|_| {}))
                .check()
                .status
                .label(),
            "ok"
        );
    }

    #[test]
    fn the_panic_strategy_is_reported_as_built() {
        let r = PanicStrategyCheck.check();
        let expected = if cfg!(panic = "abort") {
            "abort"
        } else {
            "unwind"
        };
        assert_eq!(r.details["panic"], expected);
    }
}
