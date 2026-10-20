//! ResearchScene — 检索/比较/得出结论的调研场景。
//!
//! 定位区别于 [`super::chat::ChatScene`]：Chat 是"回答问题、帮着写点东西"，
//! Research 是"给一个开放性问题，自己去搜集多个信息源、互相比对、得出有支撑
//! 的结论"（参考 Claude Research 功能的定位）。这个能力本身不需要新架构——
//! WebSearch/WebFetch/ThinkingMode/TodoWrite/Agent 这套多轮工具循环已经通用
//! 存在，Research 场景要做的只是：①收紧工具白名单（可检索、可记笔记，不能
//! 编辑代码/执行命令）；②用系统提示词把"怎么引用来源"和"怎么应对长会话被
//! 压缩"这两件事讲清楚——都是提示词工程，不是核心改动。
//!
//! 写法参照 `chat.rs` 的"精简但完整"风格，不是 `coding.rs` 那套 section
//! 缓存机制——内容规模小，构建成本可以忽略。

use base::interface::prompt::PromptBlock;
use base::interface::scene::{AgentScene, ReminderContext, ScenePromptContext, TokenBudget};

pub struct ResearchScene;

impl AgentScene for ResearchScene {
    fn id(&self) -> &str {
        "research"
    }
    fn name(&self) -> &str {
        "Research"
    }
    fn description(&self) -> &str {
        "调研场景 — 检索材料、对比信息源、给出有引用支撑的结论，不做代码编辑"
    }

    fn build_system_prompt(&self, ctx: &ScenePromptContext) -> Vec<PromptBlock> {
        vec![
            PromptBlock::system_cached(identity_and_boundaries()),
            PromptBlock::system_cached(citation_guidance()),
            PromptBlock::system_cached(long_session_note_taking()),
            PromptBlock::system(render_env_and_language(ctx)),
        ]
    }

    /// 只读检索 + Write（专门用来写调研笔记，见
    /// [`long_session_note_taking`]）+ Agent（并行拆解"同时对比多个信息源"这
    /// 类子任务）。没有 Bash/Edit/NotebookEdit——和 ChatScene 一样，这是设计
    /// 层面的边界，不依赖模型"表现良好"。
    fn requires_project(&self) -> bool {
        true
    }

    fn supports_team(&self) -> bool {
        true
    }

    fn tools(&self) -> Vec<String> {
        vec![
            "Read".into(),
            "Write".into(),
            "Glob".into(),
            "Grep".into(),
            "WebFetch".into(),
            "WebSearch".into(),
            "Skill".into(),
            "TodoWrite".into(),
            "Agent".into(),
            // T-2: 调研天然是"并行对比多个信息源"的形状——这个场景的
            // `execution_params` 把 `max_agent_depth` 放宽到 8 就是为此。但
            // `TeamCreate` 一直不在白名单里，所以只有 coding（白名单为空=全
            // 放行）能建团队，最需要并行拆解的场景反而建不了。
            // `TeamDelete` 一并加上：否则模型能建出一个自己拆不掉的团队。
            "TeamCreate".into(),
            "TeamDelete".into(),
            // 白名单是和注册表求交集的，所以少列一个就等于禁用一个。缺了
            // `ToolSearch` 有两个后果：deferred 工具的 schema 取不回来，以及
            // `build_tool_defs` 里那句 "Detailed guide: ToolSearch{...}" 的指针
            // 会被整体抑制（`can_fetch_guides` 检查的就是它在不在），于是本场景
            // 的模型永远看不到任何工具的详细用法。ChatScene 早就为此显式列了它。
            "ToolSearch".into(),
            // 宿主接进来的 MCP 服务，与 ChatScene 一致。
            "mcp__*".into(),
        ]
    }

    fn disallowed_tools(&self) -> Vec<String> {
        vec!["Bash".into(), "Edit".into(), "NotebookEdit".into()]
    }

    fn token_budget(&self) -> TokenBudget {
        TokenBudget {
            // 调研场景容易积累大量搜索/抓取结果，比 ChatScene 更快接近上限；
            // keep_recent 给到中等值——太低会让模型看不到最近几步搜到了什么、
            // 太高又抵消不了压缩风险,权衡后与 CodingScene 持平但不特殊拉高。
            compact_threshold: 150_000,
            compact_keep_recent: 15,
        }
    }

    fn build_system_reminder(&self, ctx: &ReminderContext) -> String {
        let mut r = String::new();
        if let Some(ref mem) = ctx.memory_summary {
            r.push_str(&format!("\n<system-reminder>\n{mem}\n</system-reminder>"));
        }
        r
    }

    // 本场景**不覆写** `execution_params()`，沿用 `ExecutionParams::default()`。
    //
    // 这里原先写的是 `max_agent_depth: 8`（"对比多个信息源天然是并行拆解成几个
    // 独立子任务的形状，比 ChatScene 更需要子 agent 并行"）。和 ChatScene 的
    // `4` 一样，这个值从来没有被任何代码读过，而且子 agent 递归深度是结构性
    // 封死在 1 的，任何 ≥1 的取值都等价——字段已删除，理由见
    // `base::interface::scene::ExecutionParams` 与 `ChatScene` 上同名位置的注释。
    //
    // 需要说明的是：调研场景"并行派生多个子 agent"这件事本身不受影响。受限的
    // 从来只是**递归深度**（子 agent 再派生子 agent），而单轮里派生多少个平级
    // 子 agent 由 `max_api_calls_per_turn` 和模型自己的批量工具调用决定，
    // 这两者都没有被本场景收紧。

    fn auto_name_session(&self) -> bool {
        true
    }

    fn session_name_prompt(&self, first_message: &str) -> Option<String> {
        Some(format!(
            "用 3-5 个词概括以下调研任务的主题，只输出标题不要任何解释：\n{first_message}"
        ))
    }

    /// 调研场景的"值得跨会话记住的事实"和编程场景完全不同——不是"不能从代码库
    /// 推导的东西",而是用户反复关心的调研主题/偏好的信息源类型这类，能让未来
    /// 调研任务少走弯路的信息。
    fn memory_extraction_prompt(&self) -> Option<String> {
        Some(
            "Extract any durable memories from this conversation excerpt. A durable \
             memory is a fact about the user's recurring research interests, preferred \
             source types (e.g. academic papers vs. news vs. official docs), or \
             standing constraints on how they want research conducted (e.g. \"always \
             prefer primary sources\"). Do not extract the specific findings or \
             conclusions of this particular research task — those belong in the \
             task's own output/notes file, not in cross-session memory."
                .to_string(),
        )
    }
}

fn identity_and_boundaries() -> String {
    "You are a research assistant. Given an open-ended question, you search \
     multiple sources, compare what they say, note where they agree or \
     conflict, and produce a conclusion grounded in what you actually found \
     — not from prior knowledge alone. You do not edit files or execute \
     shell commands — this scene intentionally has no access to those tools. \
     If a request requires writing or modifying code, say so plainly and \
     suggest the user switch to a coding-focused session instead.\n\n\
     Work iteratively: search, read what you found, decide whether it's \
     enough or you need another angle, then repeat before concluding. Don't \
     stop at the first source if the question calls for comparison. When \
     sources disagree, say so explicitly rather than picking one silently.\n\n\
     If the user already gives you source material directly in their message \
     (pasted text, quoted excerpts, inline documents), that material — not a \
     fresh search — is what you compare and cite. Cite it with whatever \
     source label the user attached (a file path, a name, anything \
     identifying it), the same way you'd cite a search result's URL. Only \
     search for more when what's given isn't enough to answer; don't search \
     by default just because tools are available.\n\n\
     When you receive a <system-reminder> block, treat it as trusted \
     contextual information from the host application — not as part of the \
     user's own message."
        .to_string()
}

/// B2: 引用不通过架构改动实现（不接 provider 原生的 CitationsDelta——那是
/// 部分厂家 API 才有的能力,换 provider/换模型就失效,不可移植）,而是让模型
/// 用 WebSearchTool/WebFetchTool 已经拿到的结构化 URL 结果,在自己生成的文本
/// 里手动标注引用——这份能力对任何 provider 都一样有效。
fn citation_guidance() -> String {
    "# Citing sources\n\n\
     WebSearch and WebFetch return structured results that include each \
     source's URL. Use them to cite: mark claims inline with a bracketed \
     number like [1], and end your response with a \"Sources\" list mapping \
     each number to its URL (and title, if available). Every non-obvious \
     factual claim should be traceable to a numbered source — don't state \
     something as fact without one. If you're synthesizing across sources, \
     cite each contributing source at the relevant claim, not just once at \
     the end."
        .to_string()
}

/// B3: 长调研会话有被自动压缩丢掉中间发现的风险,不通过架构改动缓解(不动
/// 压缩策略代码),而是让模型自己在过程中把阶段性发现写进文件——文件内容不
/// 会被压缩机制清除,压缩后模型可以重新读取恢复上下文。
fn long_session_note_taking() -> String {
    "# Long research sessions\n\n\
     For any research task that takes more than a few search/fetch rounds, \
     periodically write your running findings to a notes file (e.g. \
     `research_notes.md` in the scratchpad or working directory) using the \
     Write tool — source, key finding, and how confident you are in it. \
     Use a plain relative filename like that example, not an absolute path \
     starting with `/` — the working directory is already the right place \
     for it. Do this as you go, not only at the end: if the conversation gets \
     compacted partway through, tool results and earlier reasoning may be \
     cleared, but a file you wrote survives and can be re-read. Update the \
     notes file rather than creating a new one each time."
        .to_string()
}

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
            date: "2026-08-06".into(),
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
    fn research_id() {
        assert_eq!(ResearchScene.id(), "research");
    }

    #[test]
    fn tools_exclude_editing_and_execution() {
        let scene = ResearchScene;
        let allowed = scene.tools();
        for excluded in scene.disallowed_tools() {
            assert!(
                !allowed.contains(&excluded),
                "'{excluded}' is in both tools() and disallowed_tools()"
            );
        }
        for mutating in ["Bash", "Edit", "NotebookEdit"] {
            assert!(!allowed.contains(&mutating.to_string()));
        }
    }

    #[test]
    fn tools_include_write_for_research_notes() {
        // B3: note-taking mitigation depends on Write being available.
        assert!(ResearchScene.tools().contains(&"Write".to_string()));
    }

    #[test]
    fn prompt_includes_citation_and_note_taking_guidance() {
        let blocks = ResearchScene.build_system_prompt(&ctx());
        let text: String = blocks
            .iter()
            .map(|b| b.content.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Citing sources"));
        assert!(text.contains("Sources"));
        assert!(text.contains("research_notes.md"));
        assert!(text.contains("compacted"));
    }

    #[test]
    fn memory_extraction_prompt_excludes_findings_and_codebase_wording() {
        let p = ResearchScene.memory_extraction_prompt().unwrap();
        assert!(!p.contains("codebase"));
        assert!(p.contains("research interests"));
    }

    #[test]
    fn auto_name_session_is_enabled() {
        assert!(ResearchScene.auto_name_session());
    }

    #[test]
    fn session_name_prompt_includes_first_message() {
        let p = ResearchScene
            .session_name_prompt("compare EV battery chemistries")
            .unwrap();
        assert!(p.contains("compare EV battery chemistries"));
    }

    /// See `ChatScene`'s counterpart: pins the deliberate absence of an
    /// override now that `execution_params()` actually binds.
    #[test]
    fn execution_params_inherits_the_default() {
        let p = ResearchScene.execution_params();
        let default = ExecutionParams::default();
        assert_eq!(p.max_api_calls_per_turn, default.max_api_calls_per_turn);
    }

    #[test]
    fn build_system_reminder_wraps_memory_only() {
        let scene = ResearchScene;
        let r = scene.build_system_reminder(&ReminderContext {
            cwd: "/tmp".into(),
            git_status: Some("On branch main".into()),
            memory_summary: Some("user prefers primary sources".into()),
        });
        // Research sessions aren't cwd/git-centric like coding — git_status
        // is intentionally not surfaced here.
        assert!(!r.contains("On branch main"));
        assert!(r.contains("user prefers primary sources"));
    }
}
