//! Demo scene — minimal agent for framework validation.
//!
//! 这是 [`AgentScene`] trait 最小可用实现的范例——只覆盖 6 个必须实现的方法
//! （`id`/`name`/`description`/`build_system_prompt`/`tools`/`token_budget`），
//! **刻意不覆写**任何有默认值的扩展方法（`build_system_reminder` /
//! `disallowed_tools` / `default_skills` / `execution_params` /
//! `auto_name_session` / `session_name_prompt`），全部吃 trait 定义里的默认
//! 实现（空字符串 / 空列表 / `ExecutionParams::default()` / `false` /
//! `None`）。这是有意的教学选择：如果你是第一次实现自己的 Scene，看这个文件
//! 就知道"不覆写某个方法会发生什么"——答案是"什么都不做，走默认值，不会报
//! 错也不会 panic"。想看这些扩展方法被真正用起来是什么样子，去看
//! [`super::coding::CodingScene`]（完整实现）或 [`super::chat::ChatScene`]
//! （精简但完整的参照实现，每个方法都有"为什么这么写"的注释）。

use base::interface::prompt::PromptBlock;
use base::interface::scene::{AgentScene, ScenePromptContext, TokenBudget};

pub struct DemoScene;

impl AgentScene for DemoScene {
    /// 场景唯一 id——daemon `--scene demo` 用它做匹配（见
    /// `daemon::main::resolve_scene`）。必须和 `KNOWN_SCENES`
    /// （`crates/tools/src/bash/sandbox.rs`）里的字符串保持一致，否则沙盒
    /// 规则会漏保护这个场景的 `settings.json`。
    fn id(&self) -> &str {
        "demo"
    }

    /// 人类可读名——目前只在日志/诊断输出里出现，不影响行为。
    fn name(&self) -> &str {
        "Agix Demo"
    }

    /// 一句话描述——同样只用于展示，不影响行为。
    fn description(&self) -> &str {
        "演示场景 — 展示 AgentScene 框架的可扩展性"
    }

    /// 构建系统提示词。这里展示了 `PromptBlock` 两种块的最小用法：
    /// `system_cached`（内容固定、跨轮次不变，可以被 Anthropic prompt cache
    /// 命中）和 `system`（内容依赖 `ctx`，每次调用都可能不同，不缓存）。
    /// 真实场景通常会有更多块（identity、工具使用规范、输出风格……），这里
    /// 只留最小的两块，意在示范"块要怎么分"这个基本原则，不是要还原一个能用
    /// 的完整 prompt。
    fn build_system_prompt(&self, ctx: &ScenePromptContext) -> Vec<PromptBlock> {
        vec![
            PromptBlock::system_cached("You are Agix Demo, a minimal agent for demonstration."),
            PromptBlock::system(format!(
                "Working directory: {}\nDate: {}\nOS: {}",
                ctx.cwd, ctx.date, ctx.os
            )),
        ]
    }

    /// 工具白名单——非空列表，只放行这 4 个只读/信息类工具。如果要做一个
    /// "完全不限制工具"的场景，返回空 `vec![]` 即可（`ChatScene`/
    /// `CodingScene` 的 `tools()` 文档注释里有解释这个"空 = 全部"的约定）。
    fn tools(&self) -> Vec<String> {
        vec!["Read".into(), "Bash".into(), "Glob".into(), "Grep".into()]
    }

    /// Token 预算——数值比 `CodingScene`/`ChatScene` 都小，因为这是个演示
    /// 场景，不指望有长会话。如果要给自己的场景选数值，可以从这两个现有场景
    /// 的数值出发按"会话通常有多长/上下文有多重"来调整，没有一个通用公式。
    fn token_budget(&self) -> TokenBudget {
        TokenBudget {
            compact_threshold: 50_000,
            compact_keep_recent: 10,
        }
    }

    // 以下方法全部使用 trait 默认实现，未覆写：
    //   build_system_reminder  → 默认返回空字符串，不注入 <system-reminder>
    //   disallowed_tools       → 默认空列表，不额外排除任何工具
    //   default_skills         → 默认空列表，不自动加载任何 skill
    //   execution_params       → 默认 ExecutionParams::default()（不设上限，
    //                             由 settings.execution 决定）
    //   auto_name_session      → 默认 false，session 名称不会自动生成
    //   session_name_prompt    → 默认 None（配合上面 auto_name_session=false，
    //                             反正也不会被调用）
    // 如果你的场景需要这些能力中的任何一个，在这里加对应的 fn 覆写即可——
    // 参照 ChatScene 对应方法的写法和注释。
}
