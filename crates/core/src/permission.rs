//! 权限模式与权限规则。一次调用的判定结果是
//! `interface::permission::PermissionOutcome`。

use serde::{Deserialize, Serialize};

/// 权限模式。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    #[default]
    Default,
    Plan,
    AcceptEdits,
    #[serde(alias = "auto", alias = "yolo")]
    BypassPermissions,
    DontAsk,
    /// Bubble permission requests up to parent agent in team/coordinator
    /// mode. Instead of prompting the user directly, the permission request is
    /// forwarded to the parent agent for decision.
    Bubble,
}

/// `settings::PermissionMode` (serde/JSON-schema-facing, what
/// `Settings`/RPC params deserialize into) and this `PermissionMode`
/// (what `Policy` actually dispatches on) are two distinct types
/// with identical variant sets.
impl From<crate::settings::PermissionMode> for PermissionMode {
    fn from(m: crate::settings::PermissionMode) -> Self {
        match m {
            crate::settings::PermissionMode::Default => Self::Default,
            crate::settings::PermissionMode::AcceptEdits => Self::AcceptEdits,
            crate::settings::PermissionMode::BypassPermissions => Self::BypassPermissions,
            crate::settings::PermissionMode::Plan => Self::Plan,
            crate::settings::PermissionMode::DontAsk => Self::DontAsk,
            crate::settings::PermissionMode::Bubble => Self::Bubble,
        }
    }
}

impl From<PermissionMode> for crate::settings::PermissionMode {
    fn from(m: PermissionMode) -> Self {
        match m {
            PermissionMode::Default => Self::Default,
            PermissionMode::AcceptEdits => Self::AcceptEdits,
            PermissionMode::BypassPermissions => Self::BypassPermissions,
            PermissionMode::Plan => Self::Plan,
            PermissionMode::DontAsk => Self::DontAsk,
            PermissionMode::Bubble => Self::Bubble,
        }
    }
}

impl PermissionMode {
    /// Whether the user can set this mode via settings.json / CLI.
    /// `Bubble` is program-only — set by the team coordinator runtime and
    /// rejected from user config.
    pub fn is_user_settable(self) -> bool {
        !matches!(self, Self::Bubble)
    }

    /// The stricter of two modes — see
    /// `settings::PermissionMode::permissiveness` for the order.
    pub fn min_permissive(self, other: Self) -> Self {
        crate::settings::PermissionMode::from(self)
            .min_permissive(other.into())
            .into()
    }

    /// See `settings::PermissionMode::for_delegate`.
    pub fn for_delegate(self, delegate: Self) -> Self {
        crate::settings::PermissionMode::from(self)
            .for_delegate(delegate.into())
            .into()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct PermissionRule {
    pub source: RuleSource,
    pub behavior: RuleBehavior,
    pub tool_name: String,
    /// 例：Bash 用 "git push:*"；Read 用 "/etc/**"
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub rule_content: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum RuleSource {
    /// `~/.agix/scenes/<scope>/settings.json`
    UserSettings,
    /// `<git-root>/.agix/settings.json`
    ProjectSettings,
    /// `<git-root>/.agix/settings.local.json`
    LocalSettings,
    /// CLI flag (`--permission-mode`, `--allow-tool` 等)
    CliArg,
    /// 会话内通过 `/permissions` 命令设置
    Session,
    /// 部署方的 managed settings（`base::managed_settings`）
    PolicySettings,
    /// 由命令展开时临时注入
    Command,
    /// 由已安装插件贡献。卸载插件时用 `Policy::remove_rules_by_source`
    /// 整批撤销。
    Plugin,
    /// 宿主自带的 `PermissionPolicy`：它的回答作为一层规则参与裁决。
    Host,
}

impl RuleSource {
    /// 同一行为有多条规则命中时，用它挑出报告哪一条：数值越大越先选中。
    /// 不参与 allow / ask / deny 之间的裁决——那只看行为，deny 永远优先。
    pub fn priority(self) -> u8 {
        match self {
            Self::PolicySettings => 70,
            Self::Host => 65,
            Self::CliArg => 60,
            Self::Session => 50,
            Self::Command => 45,
            Self::LocalSettings => 40,
            Self::ProjectSettings => 30,
            Self::UserSettings => 20,
            Self::Plugin => 5,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum RuleBehavior {
    Allow,
    Deny,
    Ask,
}

#[derive(Debug, Clone)]
pub enum ValidationResult {
    Ok,
    Err {
        message: String,
        /// 给上层决定要不要退出 / 重试用；模型也能从 toolresult 看到
        error_code: i32,
    },
}

impl ValidationResult {
    /// Validation result: input passes all checks.
    pub fn ok() -> Self {
        Self::Ok
    }
    /// Validation result: failed with a message and code (non-zero).
    pub fn err(msg: impl Into<String>, code: i32) -> Self {
        Self::Err {
            message: msg.into(),
            error_code: code,
        }
    }
    /// True if validation passed.
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_priority_ordering() {
        assert!(RuleSource::CliArg.priority() > RuleSource::LocalSettings.priority());
        assert!(RuleSource::LocalSettings.priority() > RuleSource::ProjectSettings.priority());
        assert!(RuleSource::ProjectSettings.priority() > RuleSource::UserSettings.priority());
        assert!(RuleSource::PolicySettings.priority() > RuleSource::CliArg.priority());
    }

    #[test]
    fn permission_mode_serializes_camelcase() {
        let s = serde_json::to_value(PermissionMode::BypassPermissions).unwrap();
        assert_eq!(s, serde_json::Value::String("bypassPermissions".into()));
        let back: PermissionMode = serde_json::from_value(s).unwrap();
        assert_eq!(back, PermissionMode::BypassPermissions);
    }

    #[test]
    fn auto_and_yolo_are_bypass() {
        for spelling in ["auto", "yolo"] {
            let mode: PermissionMode = serde_json::from_value(spelling.into()).unwrap();
            assert_eq!(mode, PermissionMode::BypassPermissions);
            assert_eq!(
                serde_json::to_value(mode).unwrap(),
                serde_json::Value::String("bypassPermissions".into())
            );
        }
    }

    #[test]
    fn rule_roundtrip() {
        let r = PermissionRule {
            source: RuleSource::ProjectSettings,
            behavior: RuleBehavior::Deny,
            tool_name: "Bash".into(),
            rule_content: Some("rm -rf *".into()),
        };
        let s = serde_json::to_string(&r).unwrap();
        let back: PermissionRule = serde_json::from_str(&s).unwrap();
        assert_eq!(r, back);
    }
}
