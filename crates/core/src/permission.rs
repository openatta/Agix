//! 权限模式与权限规则。一次调用的判定结果是
//! `interface::permission::PermissionOutcome`。

use serde::{Deserialize, Serialize};

/// Permission mode for tool execution.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, schemars::JsonSchema,
)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    /// Prompt user for each tool call that isn't explicitly allowed.
    #[default]
    Default,
    /// Allow file writes inside the project (the working directory and
    /// `sandbox.additional_directories`) and read-only calls without asking;
    /// ask about the rest, as `default` does. Writes into the project's
    /// `.git`, `.agix`, `.claude` and `.agents/skills` still ask.
    AcceptEdits,
    /// Ask nothing and skip the rules of the user's tiers. Credentials
    /// (`.ssh`, `.aws`, `.gnupg`, the state directory's settings and tokens)
    /// are still refused, the deployment's managed `deny` / `ask` rules still
    /// apply, input that is not valid is still refused, and `Bash` still runs
    /// in the OS sandbox. `auto` and `yolo` are accepted as other spellings
    /// of this mode and read back as `bypassPermissions`.
    #[serde(alias = "auto", alias = "yolo")]
    BypassPermissions,
    /// Plan mode — only allow read-only tools.
    Plan,
    /// Don't ask — deny any tool not explicitly allowed (no prompt).
    DontAsk,
    /// Bubble mode — forward permission requests up to a parent agent.
    /// **Program-only**: set by team/coordinator runtime; cannot be set by user.
    #[schemars(skip)]
    Bubble,
}

impl PermissionMode {
    /// Whether the user can set this mode via settings.json / CLI.
    /// `Bubble` is program-only — set by the team coordinator runtime and
    /// rejected from user config.
    pub fn is_user_settable(self) -> bool {
        !matches!(self, Self::Bubble)
    }

    /// How permissive this mode is, on a total order — higher grants more
    /// without asking.
    ///
    /// Exists so a host can *clamp* a mode it did not choose: the daemon's
    /// `session.create` RPC accepts a caller-supplied `permission_mode`, and
    /// without an ordering there was no way to express "a client may tighten
    /// this, but may not loosen it past what `settings.json` configured".
    /// Anyone who could reach the socket could simply ask for
    /// `bypassPermissions` and get it, whatever the daemon was configured
    /// with — see `daemon::session_pool::effective_permission_mode`.
    ///
    /// The ranking:
    /// - `DontAsk` refuses anything not explicitly allowed — nothing is more
    ///   restrictive.
    /// - `Plan` refuses every non-read-only tool.
    /// - `Default` asks about anything not explicitly allowed.
    /// - `Bubble` is `Default` with the question routed to a parent agent
    ///   rather than a human; the same set of calls still needs approval.
    /// - `AcceptEdits` additionally waves through `Write`/`Edit`.
    /// - `BypassPermissions` asks nothing at all.
    pub fn permissiveness(self) -> u8 {
        match self {
            Self::DontAsk => 0,
            Self::Plan => 1,
            Self::Default => 2,
            Self::Bubble => 3,
            Self::AcceptEdits => 4,
            Self::BypassPermissions => 5,
        }
    }

    /// The stricter (less permissive) of two modes. Ties keep `self`.
    pub fn min_permissive(self, other: Self) -> Self {
        if other.permissiveness() < self.permissiveness() {
            other
        } else {
            self
        }
    }

    /// The mode a delegate (sub-agent, team member) runs in when the session
    /// that spawned it is in `self`: its own, wider or narrower — except that a
    /// planning parent holds it to plan, a parent that asks nobody holds it to
    /// `dontAsk`, and a parent that bypasses takes it along.
    pub fn for_delegate(self, delegate: Self) -> Self {
        match self {
            Self::Plan | Self::DontAsk => delegate.min_permissive(self),
            Self::BypassPermissions => self,
            _ => delegate,
        }
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
