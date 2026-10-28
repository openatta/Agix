//! Hook 进程 stdin / stdout 协议。
//!
//! - **stdin**：CLI 写一行 JSON `HookInput`
//! - **stdout**：hook 写一行 JSON `HookResponse`（不合法 → 视为 default {continue: true}）

use serde::{Deserialize, Serialize};

/// CLI 喂给 hook 子进程的 payload。字段集随事件变化，所有字段都 optional。
#[derive(Debug, Clone, Serialize, Default)]
pub struct HookInput {
    pub hook_event_name: String, // "PreToolUse" / "PostToolUse" / ...
    pub session_id: String,
    pub cwd: String,
    pub permission_mode: String,

    // PreToolUse / PostToolUse
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_input: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,

    // PostToolUse only
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,

    // UserPromptSubmit
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_prompt: Option<String>,
    /// How the message reached the session: `"turn"` opens a turn, `"steer"`
    /// joins the running one at a step boundary.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivery: Option<String>,

    // ── Lifecycle events ──
    // TurnStart / TurnComplete / SessionStart / SessionEnd / SubagentStart /
    // SubagentStop / PermissionRequested / PermissionDenied. These are
    // notification-only: `HookRunner::run`'s return value is discarded at
    // every one of their firing sites, so a hook cannot block or rewrite from
    // them. That is deliberate — a lifecycle hook that could veto a turn
    // boundary would need a well-defined recovery path at each site, and
    // there isn't one.
    /// Turn identifier, for events scoped to one turn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    /// Why a turn ended (`end_turn`, `max_turns`, `stopped_by_hook`, ...).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    /// Named agent type of a spawned sub-agent, when the spawn selected one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    /// Human-readable explanation — a permission prompt's message, or the
    /// reason a permission check denied a call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl HookInput {
    /// Build the payload for a lifecycle event.
    ///
    /// The nine lifecycle firing sites would otherwise each repeat fourteen
    /// fields, thirteen of them `None`, which is how a site ends up silently
    /// omitting one that matters. Callers set only what their event carries:
    ///
    /// ```ignore
    /// HookInput::lifecycle("TurnStart", &session_id, &cwd, &mode)
    ///     .with_turn_id(&turn_id)
    /// ```
    pub fn lifecycle(
        event: impl Into<String>,
        session_id: impl Into<String>,
        cwd: impl Into<String>,
        permission_mode: impl Into<String>,
    ) -> Self {
        Self {
            hook_event_name: event.into(),
            session_id: session_id.into(),
            cwd: cwd.into(),
            permission_mode: permission_mode.into(),
            ..Default::default()
        }
    }

    pub fn with_turn_id(mut self, turn_id: impl Into<String>) -> Self {
        self.turn_id = Some(turn_id.into());
        self
    }

    pub fn with_stop_reason(mut self, reason: impl Into<String>) -> Self {
        self.stop_reason = Some(reason.into());
        self
    }

    pub fn with_agent_type(mut self, agent_type: Option<String>) -> Self {
        self.agent_type = agent_type;
        self
    }

    pub fn with_tool(mut self, name: impl Into<String>, input: serde_json::Value) -> Self {
        self.tool_name = Some(name.into());
        self.tool_input = Some(input);
        self
    }

    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }
}

/// hook 写回的响应。所有字段都 optional；缺省视为"什么都不干"。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct HookResponse {
    /// 是否继续这一 turn。false → engine 中止整个 turn。
    #[serde(default, rename = "continue")]
    pub r#continue: Option<bool>,

    /// `PreToolUse`：改写权限决定，`Block` 直接拒绝这次调用（工具不会执行）。
    /// `PostToolUse`：`Block` 把这次调用的结果替换成一条 denial（工具已经跑完，
    /// 改写的是模型看到的 tool_result，不是"不让跑"），并触发同一并发批次里
    /// 其它工具调用的取消（复用 `execute_stream` 已有的"任意工具出错取消同批
    /// 兄弟调用"机制——见 `runtime::turn::execute_tool_inner` 里的用法）。
    #[serde(default)]
    pub decision: Option<HookDecision>,

    /// 给用户 / 模型看的解释。
    #[serde(default)]
    pub message: Option<String>,

    /// 改写工具入参（仅 PreToolUse；hook 想 redirect 路径等用）
    #[serde(default)]
    pub updated_input: Option<serde_json::Value>,

    /// 不把 hook 的 stdout 进 transcript（仅日志）
    #[serde(default)]
    pub suppress_output: Option<bool>,

    /// P2: 异步唤醒。hook 返回 true 表示请求在后台工作完成时被重新执行。
    /// 需要 hook 配置中 `async_rewake: true` 配合。
    #[serde(default)]
    pub rewake: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HookDecision {
    Approve,
    Block,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn decodes_full_response() {
        let v = json!({
            "continue": true,
            "decision": "approve",
            "message": "ok",
            "updated_input": {"file_path": "/safe/path"},
            "suppress_output": false
        });
        let r: HookResponse = serde_json::from_value(v).unwrap();
        assert_eq!(r.r#continue, Some(true));
        assert_eq!(r.decision, Some(HookDecision::Approve));
        assert_eq!(r.message.as_deref(), Some("ok"));
        assert!(r.updated_input.is_some());
    }

    #[test]
    fn decodes_block_decision() {
        let v = json!({"decision": "block", "message": "no way"});
        let r: HookResponse = serde_json::from_value(v).unwrap();
        assert_eq!(r.decision, Some(HookDecision::Block));
    }

    #[test]
    fn empty_object_is_default_response() {
        let v = json!({});
        let r: HookResponse = serde_json::from_value(v).unwrap();
        assert!(r.r#continue.is_none());
        assert!(r.decision.is_none());
    }

    #[test]
    fn input_serializes_with_tool_fields() {
        let i = HookInput {
            hook_event_name: "PreToolUse".into(),
            session_id: "sess1".into(),
            cwd: "/tmp".into(),
            permission_mode: "default".into(),
            tool_name: Some("Bash".into()),
            tool_input: Some(json!({"command": "ls"})),
            tool_use_id: Some("toolu_01".into()),
            tool_result: None,
            is_error: None,
            user_prompt: None,
            ..Default::default()
        };
        let v = serde_json::to_value(&i).unwrap();
        assert_eq!(v["hook_event_name"], "PreToolUse");
        assert_eq!(v["tool_name"], "Bash");
        assert!(v.get("tool_result").is_none());
    }
}
