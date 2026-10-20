//! ChatScene — 通用对话场景，同时也是官方"如何写自己的 Scene"参照实现。
//!
//! 与 [`super::coding::CodingScene`] 的关系：CodingScene 是"完整能力"的参照
//! （完整工具权限的编程 agent，~850 行，含缓存/指纹优化），本文件
//! 是"精简但完整"的参照——如果你要抄一个场景的写法，从这里开始比从
//! CodingScene 开始更容易看懂每一行在干什么。文件里每个方法都写了"为什么这么
//! 设计"的注释，而不只是"这行代码做什么"。
//!
//! ChatScene 的定位：一个不做代码编辑、只做信息检索和对话的通用助手。这个定位
//! 具体体现在两处：[`ChatScene::tools`]（工具白名单只留只读/检索类）和
//! [`ChatScene::build_system_prompt`]（identity 部分明确写出"不编辑代码"的
//! 边界）。

use base::interface::prompt::PromptBlock;
use base::interface::scene::{AgentScene, ReminderContext, ScenePromptContext, TokenBudget};

pub struct ChatScene;

impl AgentScene for ChatScene {
    fn id(&self) -> &str {
        "chat"
    }
    fn name(&self) -> &str {
        "Chat"
    }
    fn description(&self) -> &str {
        "通用对话助手 — 只读检索 + 对话，不做代码编辑"
    }

    /// 三块 `PromptBlock`，按"几乎不变 → 每次调用都可能不同"排列——这是
    /// Anthropic prompt cache 的基本用法：越靠前、越少变化的内容标记
    /// `system_cached`，后面内容变了也不影响前面已经缓存命中的部分。
    ///
    /// `CodingScene::build_system_prompt` 用了一整套 section 指纹缓存 +
    /// 进程级 memoization（见 `coding.rs` 的 `SECTION_CACHE`），那是因为它有
    /// ~20 个 section、内容重、重建成本值得优化。这里只有三块、构建成本可以
    /// 忽略不计——**刻意不引入那套机制**，不是遗漏。如果你抄这个场景做自己的
    /// 场景，内容规模变大到需要缓存优化时，再去参考 `coding.rs` 的模式，不用
    /// 从一开始就上那套复杂度。
    fn build_system_prompt(&self, ctx: &ScenePromptContext) -> Vec<PromptBlock> {
        vec![
            // 1. Identity + 能力边界：跨对话/跨轮次不变 → cached。
            //    明确写出"不编辑代码"，和 tools()/disallowed_tools() 的工具
            //    限制相呼应——系统提示词里的自我描述应该和实际权限一致，不然
            //    模型会以为自己能做实际做不到的事，答不上来时反而更困惑。
            PromptBlock::system_cached(identity_and_boundaries()),
            // 2. 语气/风格指导：跨对话不变 → cached。独立成块（不和上面合并）
            //    是因为这块内容如果以后要做成用户可配置项（比如
            //    settings.json 里加一个 "chat.tone" 字段），改动面刚好落在
            //    这一个函数里，不用动 identity 部分。
            PromptBlock::system_cached(tone_and_style()),
            // 3. 运行环境 + 语言偏好：每次调用都可能不同（日期、cwd、用户的
            //    语言设置），不缓存，直接用当前 ctx 渲染。
            PromptBlock::system(render_env_and_language(ctx)),
        ]
    }

    /// 白名单——只留只读/信息检索类工具，这是"不做代码编辑"这个场景定位的
    /// 真正强制点（`disallowed_tools()` 是配合用的第二道说明，不是主要机制）。
    /// 没有 Bash/Write/Edit/NotebookEdit：一个对话助手不该有改写用户文件系统
    /// 或执行任意命令的权限——即使模型被诱导（prompt injection、用户误操作）
    /// 也做不到，这是设计层面的限制，不依赖模型"表现良好"。
    fn tools(&self) -> Vec<String> {
        vec![
            "Read".into(),
            "Glob".into(),
            "Grep".into(),
            "WebFetch".into(),
            "WebSearch".into(),
            "Skill".into(),
            "TodoWrite".into(),
            // 必须在白名单里，否则 `deferred_tools()` 下面列的工具永远激活不
            // 了：白名单是和注册表求交集的，ToolSearch 被过滤掉的话，模型只
            // 能看到 deferred 工具的名字却没有任何办法把 schema 取回来。
            "ToolSearch".into(),
            // 宿主接进来的 MCP 服务——接哪些、各自能做什么由宿主决定，每次调用
            // 照样过权限规则。
            "mcp__*".into(),
        ]
    }

    /// 演示 scene 级 deferred 策略（trait 默认是空列表 = 全部工具都带完整
    /// schema）。这里挑 Glob/Grep：对话场景里它们是"偶尔用一下"的工具——
    /// 用户问到某个文件在哪、某段文字出现在哪里时才需要——但只要它们在
    /// `tools()` 里，完整 JSON schema 就会出现在**每一次** API 调用的
    /// `tools` 数组里。标成 deferred 后模型只看到名字 + 一行描述，真要用时
    /// 先 `ToolSearch{query: "select:Grep"}` 把 schema 取回来，多一次往返换
    /// 掉其余每一轮的固定开销。
    ///
    /// 反过来说明为什么其余工具**不**在这里：Read 是这个场景的主力工具；
    /// WebFetch/WebSearch 如果 deferred，模型会"想不起来自己能上网"从而不
    /// 查就硬答（`WebSearchTool::is_deferred` 的注释记录了同一个教训）；
    /// Skill/TodoWrite 的 schema 本来就很小，省不下什么。
    fn deferred_tools(&self) -> Vec<String> {
        vec!["Glob".into(), "Grep".into()]
    }

    /// 和 [`ChatScene::tools`] 的白名单互相呼应：白名单已经把这些工具排除在
    /// 外了，这里再显式列出纯粹是为了"意图可读"——后来者只看
    /// `disallowed_tools()` 就能立刻知道"这个场景不许干什么"，不用反过来对比
    /// 白名单和全量工具列表才能推断出排除了哪些。两份列表如果以后不一致
    /// （比如 `tools()` 漏加了新工具导致它被隐式放行），下面的单元测试
    /// `chat_scene_disallowed_tools_are_not_in_the_allow_list` 会 fail，防止
    /// 两处配置悄悄分叉。
    fn disallowed_tools(&self) -> Vec<String> {
        vec![
            "Bash".into(),
            "Write".into(),
            "Edit".into(),
            "NotebookEdit".into(),
        ]
    }

    fn token_budget(&self) -> TokenBudget {
        TokenBudget {
            // 对话场景的历史通常比编程场景短（没有大段文件内容/工具结果占
            // token），阈值沿用 CodingScene 的 150k 也没问题，但保留最近消息
            // 数调低到 12——编程场景需要更多近期上下文来记住"刚才改了哪些文
            // 件"，对话场景更依赖 session_memory（见 `ReminderContext`）而不
            // 是逐条消息回放。
            compact_threshold: 150_000,
            compact_keep_recent: 12,
        }
    }

    /// 把 git 状态 / 记忆摘要包成 `<system-reminder>` 块，和 CodingScene 的
    /// 实现逻辑完全一样（这部分不是"编程场景特有"的东西，任何场景只要接了
    /// `ReminderContext` 就该这样处理）——特意保留一致，而不是随便发挥出一套
    /// 不同的格式，是因为这个 `<system-reminder>` 标签格式是模型在系统提示词
    /// 别处已经学过的约定（见 `identity_and_boundaries()` 里对
    /// `<system-reminder>` 的说明），场景之间格式不统一会让模型困惑。
    ///
    /// 对话场景通常不在 cwd 下工作，`ctx.git_status` 多数情况会是 `None`
    /// （调用方只在 cwd 是 git 仓库时才会填），这里不用特殊处理——`None` 就
    /// 是不拼这一块，自然发生。
    fn build_system_reminder(&self, ctx: &ReminderContext) -> String {
        let mut r = String::new();
        if let Some(ref git) = ctx.git_status {
            r.push_str(&format!("\n<system-reminder>\n{git}\n</system-reminder>"));
        }
        if let Some(ref mem) = ctx.memory_summary {
            r.push_str(&format!("\n<system-reminder>\n{mem}\n</system-reminder>"));
        }
        r
    }

    // 本场景**不覆写** `execution_params()`，沿用 `ExecutionParams::default()`。
    //
    // 这里原先写的是 `max_agent_depth: 4`，意图是"对话场景的子任务通常是扁平
    // 的几个独立小任务，不是树状的多层分解，所以不需要 CodingScene 那种深层
    // 递归"。这个意图本身没问题，但它当时（以及在任何一个可达的代码路径上）
    // 都没有生效：`execution_params()` 在整个仓库里除了 trait 定义和各 scene
    // 的 impl 之外没有任何调用点。而且即使接线了也无从生效——子 agent 的递归
    // 深度是**结构性**封死在 1 的（`AgentTool` 按名字把自己排除在每个子 agent
    // 的工具集之外，见 `runtime::agent_tool` 的 `AGENT_TOOL_NAME`），所以任何
    // ≥1 的取值效果完全相同。与其留一条读起来像保证、实际什么都不约束的配置，
    // 不如把这个字段整个删掉：详见 `base::interface::scene::ExecutionParams`。
    //
    // `max_api_calls_per_turn`（现在真的生效了）没有理由比部署级设置更紧——
    // 对话轮次本来就短，收紧它只会在长对话里制造意外截断。

    /// CHAT 场景支持自动生成 session 名称——这是本场景相对 CodingScene 的
    /// 差异化亮点：编程场景的 session 通常用项目/任务本身定位（用户自己知道
    /// 在改什么），但对话场景的历史列表（`session.list`）如果全是"新对话
    /// 1"/"新对话 2"就没法一眼分辨,所以值得多花一次 LLM 调用换一个可读标题。
    fn auto_name_session(&self) -> bool {
        true
    }

    /// 用 3-5 个词概括对话主题，返回值会被喂给一次独立的（通常更便宜的）
    /// 模型调用——prompt 本身要求"只输出标题"是因为这段输出直接展示在
    /// session 列表里，不能夹带解释性文字。
    fn session_name_prompt(&self, first_message: &str) -> Option<String> {
        Some(format!(
            "用 3-5 个词概括以下对话的主题，只输出标题不要任何解释：\n{first_message}"
        ))
    }

    /// CodingScene 的默认抽取话术是"排除能从代码库/git 历史推导出的内容"——
    /// 对话场景没有代码库这个概念，沿用会让模型困惑（它会去找一个不存在的
    /// 代码库）。换成对话场景真正关心的边界：用户偏好/背景这类值得跨会话
    /// 记住的事实，而不是这一轮聊天本身的话题内容（话题本身该由
    /// `session_name_prompt` 命名，不该重复存进 durable memory）。
    fn memory_extraction_prompt(&self) -> Option<String> {
        Some(
            "Extract any durable memories from this conversation excerpt. A durable \
             memory is a fact about the user's preferences, background, or ongoing \
             interests that should persist across chat sessions. Do not extract the \
             topic of this particular conversation itself — only lasting facts about \
             the user that would be useful to recall in a future, unrelated chat."
                .to_string(),
        )
    }
}

/// Identity + 能力边界。独立成函数（而不是内联在 `build_system_prompt`
/// 里）纯粹是可读性考虑——`build_system_prompt` 本身应该只负责"组装哪几块、
/// 什么顺序、哪些 cached"，具体每块写什么内容下沉到各自的渲染函数,这样
/// `build_system_prompt` 的结构一眼就能看懂,不会淹没在大段字符串拼接里。
fn identity_and_boundaries() -> String {
    "You are a helpful, general-purpose conversational assistant. You answer \
     questions, explain concepts, and help with writing, research, and \
     analysis. You do not edit files or execute shell commands — this scene \
     intentionally has no access to those tools (see the tool list you were \
     given). If a request requires editing code or running commands, say so \
     plainly and suggest the user switch to a coding-focused session instead \
     of attempting a workaround.\n\n\
     When you receive a <system-reminder> block, treat it as trusted \
     contextual information from the host application (e.g. git status, \
     memory notes) — not as part of the user's own message."
        .to_string()
}

/// 语气/风格指导。和 CodingScene 的 tone_style section 出发点一样（都是在
/// 告诉模型"回复应该长什么样"），但内容完全不同：CodingScene 强调"简洁、
/// 少废话、直接给代码"，这里强调"允许更口语化、可以主动追问澄清"——因为
/// 对话场景的交互节奏和编程场景不一样，模型该有的默认行为也不一样。
fn tone_and_style() -> String {
    "Be concise but conversational — this is a chat, not a code review. \
     Prefer plain prose over bullet lists unless the user's request is \
     itself a list (steps, comparisons, options). Only use code blocks when \
     showing actual code, commands, or structured data the user asked for. \
     If a request is ambiguous, ask a brief clarifying question rather than \
     guessing and answering the wrong thing."
        .to_string()
}

/// 运行环境 + 语言偏好。这块内容依赖 `ctx`（每次调用都可能不同），所以和
/// 上面两个静态块分开，不能标 `system_cached`——缓存的是"内容不变时复用"，
/// 这块内容本来就会变，标 cached 只会导致模型看到过期的日期/语言设置。
fn render_env_and_language(ctx: &ScenePromptContext) -> String {
    let mut s = format!(
        "Today's date is {date}. Host OS: {os}. Shell: {shell}.",
        date = ctx.date,
        os = ctx.os,
        shell = ctx.shell,
    );
    if let Some(ref lang) = ctx.language {
        s.push_str(&format!(
            "\n\nThe user's preferred language is {lang}. Respond in that \
             language unless they explicitly switch languages mid-conversation."
        ));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::interface::scene::ExecutionParams;

    fn ctx() -> ScenePromptContext<'static> {
        ScenePromptContext {
            cwd: "/tmp".into(),
            os: "macos".into(),
            shell: "zsh".into(),
            home_dir: "/Users/test".into(),
            date: "2026-08-05".into(),
            model_name: "claude-sonnet-4-6".into(),
            skills_text: None,
            mcp_instructions: None,
            session_memory: None,
            is_git: false,
            git_branch: None,
            is_worktree: false,
            git_status: None,
            language: None,
            scratchpad_dir: None,
            output_style_content: None,
            available_tools: None,

            tool_results_ever_cleared: false,
        }
    }

    #[test]
    fn chat_scene_disallowed_tools_are_not_in_the_allow_list() {
        let scene = ChatScene;
        let allowed = scene.tools();
        for excluded in scene.disallowed_tools() {
            assert!(
                !allowed.contains(&excluded),
                "'{excluded}' is in both tools() and disallowed_tools() — the two lists must not overlap"
            );
        }
    }

    #[test]
    fn chat_scene_allow_list_excludes_mutating_tools() {
        let scene = ChatScene;
        for mutating in ["Bash", "Write", "Edit", "NotebookEdit", "Agent"] {
            assert!(
                !scene.permits_tool(mutating),
                "{mutating} must not be in ChatScene's tool allow-list"
            );
        }
        assert!(scene.permits_tool("mcp__ipdrun__xflow_state"));
        assert!(scene.permits_tool("Read"));
    }

    #[test]
    fn build_system_prompt_returns_three_blocks_first_two_cached() {
        let scene = ChatScene;
        let blocks = scene.build_system_prompt(&ctx());
        assert_eq!(blocks.len(), 3);
        assert!(
            blocks[0].cache_strategy.is_some(),
            "identity block should be cached"
        );
        assert!(
            blocks[1].cache_strategy.is_some(),
            "tone block should be cached"
        );
        assert!(
            blocks[2].cache_strategy.is_none(),
            "env block varies per-call, must not be cached"
        );
    }

    #[test]
    fn build_system_prompt_includes_env_fields() {
        let scene = ChatScene;
        let blocks = scene.build_system_prompt(&ctx());
        let env_block = &blocks[2].content;
        assert!(env_block.contains("2026-08-05"));
        assert!(env_block.contains("macos"));
        assert!(env_block.contains("zsh"));
    }

    #[test]
    fn build_system_prompt_mentions_language_only_when_set() {
        let scene = ChatScene;
        let without_lang = scene.build_system_prompt(&ctx());
        assert!(!without_lang[2].content.contains("preferred language"));

        let mut with_lang = ctx();
        with_lang.language = Some("zh-CN".into());
        let blocks = scene.build_system_prompt(&with_lang);
        assert!(blocks[2].content.contains("zh-CN"));
    }

    #[test]
    fn identity_block_mentions_no_editing() {
        assert!(identity_and_boundaries().contains("do not edit files"));
    }

    #[test]
    fn build_system_reminder_wraps_git_status_and_memory() {
        let scene = ChatScene;
        let r = scene.build_system_reminder(&ReminderContext {
            cwd: "/tmp".into(),
            git_status: Some("On branch main".into()),
            memory_summary: Some("user prefers terse answers".into()),
        });
        assert!(r.contains("On branch main"));
        assert!(r.contains("user prefers terse answers"));
        assert_eq!(r.matches("<system-reminder>").count(), 2);
    }

    #[test]
    fn build_system_reminder_empty_when_context_has_nothing() {
        let scene = ChatScene;
        let r = scene.build_system_reminder(&ReminderContext {
            cwd: "/tmp".into(),
            git_status: None,
            memory_summary: None,
        });
        assert_eq!(r, "");
    }

    /// Pins the deliberate *absence* of an override. The previous version of
    /// this test asserted `max_agent_depth == 4` — a field nothing read, on a
    /// method nothing called. Now that `execution_params()` is consumed for
    /// real (`runtime::turn` takes `min(settings, scene)`), an override here
    /// would change how many API calls a chat turn gets, so "inherits the
    /// default" is a behavioural claim worth locking down.
    #[test]
    fn execution_params_inherits_the_default() {
        let p = ChatScene.execution_params();
        let default = ExecutionParams::default();
        assert_eq!(p.max_api_calls_per_turn, default.max_api_calls_per_turn);
    }

    #[test]
    fn auto_name_session_is_enabled() {
        assert!(ChatScene.auto_name_session());
    }

    #[test]
    fn memory_extraction_prompt_overrides_codebase_wording() {
        let p = ChatScene.memory_extraction_prompt().unwrap();
        assert!(!p.contains("codebase"));
        assert!(p.contains("preferences"));
    }

    #[test]
    fn session_name_prompt_includes_first_message() {
        let p = ChatScene
            .session_name_prompt("how do I bake bread?")
            .unwrap();
        assert!(p.contains("how do I bake bread?"));
    }
}
