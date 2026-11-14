//! The hook events as data: when each one fires, what its payload carries on
//! top of the ambient `session_id` / `cwd` / `permission_mode`, and what a
//! hook's answer can do there.
//!
//! `docs/hooks.md`'s event table is [rendered](render_markdown) from this,
//! and `daemon/tests/hook_events_doc.rs` fails when the two differ — a table
//! of thirty events typed by hand is wrong about one of them within a release.
//! [`HookEvent::spec`] is an exhaustive `match`, so a new variant does not
//! compile until it has a row.

use crate::config::HookEvent;

/// What a hook's answer can do at one event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    /// The answer is read for nothing; the hook can only observe.
    Ignored,
    /// What the answer can do.
    Acts(&'static str),
}

/// One row of the event table.
#[derive(Debug, Clone, Copy)]
pub struct EventSpec {
    pub event: HookEvent,
    pub fires: &'static str,
    /// Fields beyond the ambient three, as the payload names them.
    pub payload: &'static str,
    pub answer: Answer,
}

impl HookEvent {
    /// Every variant, in declaration order.
    pub const ALL: [HookEvent; 30] = [
        HookEvent::PreToolUse,
        HookEvent::PostToolUse,
        HookEvent::PostToolUseFailure,
        HookEvent::TurnStart,
        HookEvent::TurnComplete,
        HookEvent::SessionStart,
        HookEvent::SessionEnd,
        HookEvent::Stop,
        HookEvent::StopFailure,
        HookEvent::UserPromptSubmit,
        HookEvent::PreCompact,
        HookEvent::PostCompact,
        HookEvent::PermissionRequested,
        HookEvent::PermissionDenied,
        HookEvent::Setup,
        HookEvent::Notification,
        HookEvent::SubagentStop,
        HookEvent::SubagentStart,
        HookEvent::TeammateIdle,
        HookEvent::TaskCreated,
        HookEvent::TaskCompleted,
        HookEvent::Elicitation,
        HookEvent::ElicitationResult,
        HookEvent::ConfigChange,
        HookEvent::WorktreeCreate,
        HookEvent::WorktreeRemove,
        HookEvent::InstructionsLoaded,
        HookEvent::CwdChanged,
        HookEvent::FileChanged,
        HookEvent::PostSampling,
    ];

    /// The name a settings file keys the event by, and the payload's
    /// `hook_event_name`.
    pub fn name(self) -> String {
        format!("{self:?}")
    }

    pub fn spec(self) -> EventSpec {
        use Answer::{Acts, Ignored};
        let (fires, payload, answer) = match self {
            HookEvent::PreToolUse => (
                "每次工具调用分发之前,在输入校验与权限检查之前;子 Agent 的调用也算,skill 正文里的 `` !`cmd` `` 按一次 `Bash` 调用算(没有 `tool_use_id`)",
                "`tool_name`、`tool_input`、`tool_use_id`",
                Acts(
                    "`decision: \"block\"`(或 exit 2):这次调用不执行,模型收到 \
                     `Blocked by PreToolUse hook: <message>`;`updated_input`:换掉入参,换过的照常校验、过权限;\
                     `decision: \"approve\"`:权限检查要询问时不再问人,直接放行(规则拒绝的照样拒绝,managed 层的 `ask` 照样问人;\
                     `prompt` / `agent` hook 要写 `may_approve: true`)",
                ),
            ),
            HookEvent::PostToolUse => (
                "工具真正跑完之后(成功或失败);被 PreToolUse、权限拦下的调用不发",
                "`tool_name`、`tool_input`、`tool_use_id`、`tool_result`(结果文本)、`is_error`",
                Acts(
                    "`decision: \"block\"`:结果换成 `Denied by PostToolUse hook: <message>`,同一批并发的\
                     其他调用被取消;`continue: false`:这一批工具跑完后结束这一轮(`stopped_by_hook`)",
                ),
            ),
            HookEvent::PostToolUseFailure => (
                "工具调用以错误结束(工具返回错误或 panic)时,在 PostToolUse 之前;\
                 工具自己报告失败的结果(如 Bash 非零退出)不算",
                "`tool_name`、`tool_input`、`reason`(错误文本)、`turn_id`",
                Ignored,
            ),
            HookEvent::TurnStart => (
                "一轮开始,UserPromptSubmit 放行之后、第一次调模型之前",
                "`turn_id`",
                Ignored,
            ),
            HookEvent::TurnComplete => (
                "一轮正常走完(模型不再要工具,Stop hook 也放行了)",
                "`turn_id`、`stop_reason`",
                Ignored,
            ),
            HookEvent::SessionStart => (
                "会话开始运行,紧接在 `Setup` 之后",
                "`source`:宿主为什么启动这个引擎——`new`(`session.create`、第一条消息建的会话)、`resume`(从日志接回来,包括重启或被驱逐之后再用)、`rebuild`(`setPrompt`、跨 provider 的 `setModel`、设置变了而给还在内存里的会话换引擎);嵌入方启动时没给原因的不带",
                Ignored,
            ),
            HookEvent::SessionEnd => (
                "会话的主循环退出:收到关闭、输入通道关闭、被取消,三种都发",
                "`reason`:宿主为什么停下这个引擎——`close`(`session.close` / `session.delete`)、`evict`(空闲回收或池满挤出)、`rebuild`(`setPrompt`、跨 provider 的 `setModel`、设置变了而换引擎)、`shutdown`(daemon 关停);引擎自己结束的(输入通道关闭、收到 `Shutdown` 命令)、嵌入方停下时没给原因的不带",
                Ignored,
            ),
            HookEvent::Stop => (
                "模型这一步不再要工具、这一轮要结束时;每次要结束都问一遍",
                "`stop_hook_active`(这一轮已经被 Stop hook 送回去过)",
                Acts(
                    "`continue: false`:结束这一轮(`stopped_by_hook`,随后发 `StopFailure`);\
                     `decision: \"block\"`(或 exit 2):不让结束,`message` 作为提醒交给模型接着做",
                ),
            ),
            HookEvent::StopFailure => (
                "一轮被 Stop hook 的 `continue: false` 截停时",
                "`reason`",
                Ignored,
            ),
            HookEvent::UserPromptSubmit => (
                "用户消息进转录之前:开一轮的那条,以及运行中 steer 进来的每一条",
                "`user_prompt`、`delivery`(`turn` / `steer`)",
                Acts(
                    "`decision: \"block\"`:这条消息不处理——开一轮的直接以 `stopped_by_hook` 结束、\
                     不调模型,steer 的不进转录;`updated_input`(JSON 字符串):换掉消息文本",
                ),
            ),
            HookEvent::PreCompact => (
                "每次压缩之前(按阈值自动压、`/compact`、恢复策略要求的压缩)",
                "`tool_input`:`messages_before`、`token_count`、`threshold`、`trigger`(`auto` / `manual`)",
                Acts("`decision: \"block\"` 或 `continue: false`:这次不压缩"),
            ),
            HookEvent::PostCompact => (
                "压缩完成之后",
                "`tool_input`:`strategy`、`messages_before`、`messages_after`、`tokens_before`、`tokens_after`",
                Ignored,
            ),
            HookEvent::PermissionRequested => (
                "权限检查判为\"询问\"、开始等宿主回答之前(无人值守会话不问,直接按拒绝处理)",
                "`tool_name`、`tool_input`、`reason`(询问的文字)、`turn_id`",
                Ignored,
            ),
            HookEvent::PermissionDenied => (
                "一次工具调用被拒:规则拒绝、`PreToolUse` 拦下、没人可问时的询问、宿主答拒绝或没给出决定、询问超时",
                "`tool_name`、`tool_input`、`reason`、`turn_id`",
                Ignored,
            ),
            HookEvent::Setup => (
                "会话开始运行,早于 `SessionStart`",
                "`reason`(`scene=<id> model=<模型>`)",
                Ignored,
            ),
            HookEvent::Notification => (
                "需要人注意时:一个权限询问在等回答;模型调了 `PushNotification`",
                "`tool_name`、`tool_input`(被询问的调用,或 `PushNotification` 的入参)、`reason`、`turn_id`",
                Ignored,
            ),
            HookEvent::SubagentStop => (
                "子 Agent 跑完(成功、出错、被取消);常驻团队成员在被停下时",
                "`session_id` 是子 Agent 的;`agent_type`、`stop_reason`(`end_turn` / `error` / \
                 `cancelled` / `shutdown`)",
                Ignored,
            ),
            HookEvent::SubagentStart => (
                "子 Agent 开始跑之前,每一种起法都发:前台、后台、宿主 `session.startAgent`、\
                 `TeamCreate` 成员、常驻成员",
                "`session_id` 是子 Agent 的;`agent_type`",
                Ignored,
            ),
            HookEvent::TeammateIdle => (
                "团队成员的一轮结束(不论怎么结束),在这一轮的 `TaskCompleted` 之后",
                "`team_name`、`teammate_name`、`turn_id`、`stop_reason`",
                Ignored,
            ),
            HookEvent::TaskCreated => (
                "一个运行中的任务开始:子 Agent、后台 shell 命令、Monitor、团队成员",
                "`reason`(这个任务在 `session.tasks` 里的 JSON)",
                Ignored,
            ),
            HookEvent::TaskCompleted => (
                "一个运行中的任务结束(完成、失败、被停下),与 `TaskCreated` 成对;\
                 另外团队成员的一轮正常结束时也发一次",
                "任务结束:`reason`(这个任务在 `session.tasks` 里的 JSON:`task_id`、`kind`、\
                 `status` 为 `done` / `failed` / `stopped`……);团队成员的一轮:`team_name`、\
                 `teammate_name`、`turn_id`、`stop_reason`",
                Ignored,
            ),
            HookEvent::Elicitation => (
                "MCP 工具的结果里带了 elicitation 链接(`mcp://`、`elicitation://`)",
                "`tool_name`(`mcp_elicitation_<server>`)、`tool_input`:`server_name`、`url`",
                Ignored,
            ),
            HookEvent::ElicitationResult => (
                "宿主把用户对 elicitation 的回答交回来时;还没有宿主侧的回传通路",
                "`tool_name`、`tool_input`:`server_name`、`url`、`response`;`tool_result`",
                Ignored,
            ),
            HookEvent::ConfigChange => (
                "配置在运行中变了;改配置的 RPC 在池一级,高于任何一个会话的 hooks",
                "—",
                Ignored,
            ),
            HookEvent::WorktreeCreate => (
                "`EnterWorktree` 成功之后",
                "`tool_name`、`tool_input`、`turn_id`",
                Ignored,
            ),
            HookEvent::WorktreeRemove => (
                "`ExitWorktree` 成功之后",
                "`tool_name`、`tool_input`、`turn_id`",
                Ignored,
            ),
            HookEvent::InstructionsLoaded => (
                "会话上下文(项目说明)注入转录时,说明非空才发;一个会话一次",
                "`reason`(注入了多少字节)",
                Ignored,
            ),
            HookEvent::CwdChanged => (
                "会话开始运行时报一次工作目录;活着的会话不会换目录",
                "`reason`(`session working directory: <路径>`)",
                Ignored,
            ),
            HookEvent::FileChanged => (
                "根会话的 `instruction_file`,或它载入的某份 `AGENTS.md`,被改、建、删;300 ms 去抖,\
                 一次变化一个事件",
                "`tool_name`(`FileWatcher`)、`tool_input`:`file_path`、`change_type`\
                 (`created` / `modified` / `deleted`)",
                Ignored,
            ),
            HookEvent::PostSampling => (
                "一次模型调用的回复完整收完、写进转录之后(被取消的不发)",
                "`reason`(`stop_reason` 与输出 token 数)",
                Ignored,
            ),
        };
        EventSpec {
            event: self,
            fires,
            payload,
            answer,
        }
    }
}

/// The event table of `docs/hooks.md`.
pub fn render_markdown() -> String {
    let mut out = String::new();
    out.push_str("| 事件 | 何时触发 | payload(四个公共字段之外) | 回答能做什么 |\n");
    out.push_str("|---|---|---|---|\n");
    for event in HookEvent::ALL {
        let spec = event.spec();
        let fires = if event.is_wired() {
            spec.fires.to_string()
        } else {
            format!("**从不触发**:{}", spec.fires)
        };
        let answer = match spec.answer {
            Answer::Ignored => "只通知,回答被丢弃",
            Answer::Acts(what) => what,
        };
        out.push_str(&format!(
            "| `{}` | {} | {} | {} |\n",
            event.name(),
            fires,
            spec.payload,
            answer
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn all_lists_each_event_once() {
        let unique: HashSet<_> = HookEvent::ALL.iter().collect();
        assert_eq!(unique.len(), HookEvent::ALL.len());
    }

    #[test]
    fn the_name_is_what_settings_key_the_event_by() {
        for event in HookEvent::ALL {
            let parsed: HookEvent =
                serde_json::from_value(serde_json::Value::String(event.name())).unwrap();
            assert_eq!(parsed, event);
        }
    }

    #[test]
    fn every_row_says_something_in_every_column() {
        for event in HookEvent::ALL {
            let spec = event.spec();
            assert_eq!(spec.event, event);
            assert!(!spec.fires.trim().is_empty(), "{event:?}");
            assert!(!spec.payload.trim().is_empty(), "{event:?}");
            if let Answer::Acts(what) = spec.answer {
                assert!(!what.trim().is_empty(), "{event:?}");
            }
        }
    }

    #[test]
    fn no_cell_breaks_the_table() {
        let table = render_markdown();
        for line in table.lines().skip(2) {
            assert_eq!(line.matches(" | ").count(), 3, "{line}");
        }
    }
}
