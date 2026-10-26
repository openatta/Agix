//! **D **：用户 prompt 的轻量意图分类，按类别注入条件 system prompt 片段。
//!
//! 触发点：每一轮 turn 开始时取最后一条 User Message 的文本，分类后把对应
//! fragment 作为新 system block 追加（不进 cache，因为每 turn 可能不同）。
//!
//! 设计取舍（当前）：
//! - **纯启发式**：关键词匹配，<1ms
//! - **召回 > 准确**：误判一个 vibe 当 code 没事，反之让 agix 多 recon 也没事。
//! - **fragment 短**：每条 < 300 字符，避免 prompt 膨胀。
//!
//! 当前类别：
//! - `vibe` — 主观判断 / 性能 / 风格 / 项目特定开放问题。让 agix 先做轻 recon
//! - `research` — 研究 / 调查 / 找文件 / 跨多文件追踪。鼓励起 Agent 子代理。
//! - `terse` — 一句话 / 一个数字 / yes-no。强提醒 agix 简洁到底。
//! - 其他 → 不注入（默认 BEHAVIOR_BLOCK 已涵盖）。

/// 启发式分类。返回类别 name 或 None。
pub fn classify_intent(prompt: &str) -> Option<&'static str> {
    let p = prompt.trim();
    if p.is_empty() {
        return None;
    }
    let pl = p.to_lowercase();

    // **terse** —— 用户希望极简回答（"reply with just"/"yes/no"/"one word"/etc）
    let terse_markers = [
        "reply with just",
        "just the number",
        "one word",
        "one line",
        "yes or no",
        "yes/no",
        "no prose",
        "just print",
        "only the",
        "without explanation",
    ];
    if terse_markers.iter().any(|m| pl.contains(m)) {
        return Some("terse");
    }

    // **vibe** —— 主观 / 性能 / 风格 / 设计判断
    let vibe_markers = [
        "feels wrong",
        "feels off",
        "what's the best",
        "what is the best",
        "best way to",
        "how should i",
        "how should we",
        "make this faster",
        "make it faster",
        "tidy up",
        "is this idiomatic",
        "why is this",
        "what do you think",
        "any concerns",
        "any issues",
        "code smell",
        "thoughts on",
        "your opinion",
        "should we",
        "should i ",
    ];
    if vibe_markers.iter().any(|m| pl.contains(m)) {
        return Some("vibe");
    }

    // **research** —— 跨文件 / 找定义 / 研究
    let research_markers = [
        "find every",
        "find all",
        "search the codebase",
        "look across",
        "trace through",
        "where is",
        "where are",
        "what files",
        "which files",
        "list all",
        "investigate",
        "analyze the",
        "understand the codebase",
    ];
    if research_markers.iter().any(|m| pl.contains(m)) {
        return Some("research");
    }

    None
}

/// 类别 → 短 system fragment。返回 None 表示不注入。
pub fn intent_fragment(class: &str) -> Option<&'static str> {
    match class {
        "vibe" => Some(
            "# Intent: opinion / judgement\n\
             - The user wants your take, not a generic answer. Glance at 1-3 \
             relevant files (Read / Glob) so the recommendation is grounded \
             in *this* project, then give a direct opinionated answer.\n\
             - Don't hedge. Pick a recommendation. Briefly note the tradeoff.",
        ),
        "research" => Some(
            "# Intent: research / investigation\n\
             - Multi-file investigation: prefer spawning an Agent subagent \
             (it isolates context and returns a summary) over many sequential \
             Read/Grep calls.\n\
             - Output: bounded report (table or punch-list), not a long prose dump.",
        ),
        "terse" => Some(
            "# Intent: terse answer\n\
             - The user explicitly asked for minimal output. Reply with \
             *only* the answer — no preamble, no commentary, no tool-call \
             narration. Sub-10 words ideal.",
        ),
        _ => None,
    }
}
