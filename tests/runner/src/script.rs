//! TestScript 解析器 — 读取 `.test` 文件。
//!
//! 格式:
//! ```text
//! [第一个 >>>>>>>>>>>>>>>> 之前的内容 = 测试用例元信息]
//!
//! >>>>>>>>>>>>>>>>
//! [第 1 轮输入 — 用户消息]
//! <<<<<<<<<<<<<<<<
//! [第 1 轮预期输出描述 — 给 LLM 比对的自然语言]
//!
//! >>>>>>>>>>>>>>>>
//! [第 2 轮输入]
//! <<<<<<<<<<<<<<<<
//! [第 2 轮预期输出描述]
//! ```

const SEP_IN: &str = ">>>>>>>>>>>>>>>>"; // 16 >
const SEP_OUT: &str = "<<<<<<<<<<<<<<<<"; // 16 <

#[derive(Debug, Clone)]
pub struct TestCase {
    /// 第一个分隔符之前的元信息（用例名称、描述、前置条件）
    pub meta: String,
    /// 文件路径（用于报告）
    pub source_path: String,
    /// 会话模式：每轮独立会话（默认）还是全用例共享一个会话
    pub session_mode: SessionMode,
    /// 用例声明的模板项目 fixture（`fixture: tests/fixtures/template_project`）。
    /// 命令行 `--fixture` 优先。
    pub fixture: Option<String>,
    /// 用例声明的场景（`scene: mini`）。命令行 `--scene` 优先。
    pub scene: Option<String>,
    /// 多轮对话
    pub turns: Vec<Turn>,
}

/// 用例的多轮之间是否共享同一个会话（对话历史）。
///
/// 默认 `PerTurn` —— 这是本 harness 一直以来的行为（`api_runner` 给每轮一个
/// 新的 `session_id`，`cli_runner` 给每轮一个新的 daemon session），已录制的
/// cassette 全部依赖它，不能改默认值。
///
/// 但"默认隔离"意味着**没有任何用例真正跑过跨轮对话**：记忆召回、
/// `already_surfaced` 去重、上下文压缩、session-memory 陈旧判定、压缩后恢复
/// ——这些机制全都只在"轮与轮之间"才有行为可言。一次审计里发现的三个 P0 记忆
/// bug 能在 1737 个测试全绿的情况下溜过去，根因就是这个空档。所以用例可以在
/// `.test` 文件的元信息区显式声明 `session: shared` 来打开共享会话，而不是只
/// 能靠调用方记得加 `--same-session`（用例文件本身才知道它需不需要跨轮状态，
/// 命令行不知道）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SessionMode {
    /// 每轮一个全新会话——轮之间只共享临时工作目录（文件系统状态），没有对话记忆。
    #[default]
    PerTurn,
    /// 整个用例一个会话——对话历史跨轮延续。
    Shared,
}

#[derive(Debug, Clone)]
pub struct Turn {
    /// 轮次编号 (0-based)
    pub index: usize,
    /// 用户输入（发给 Agent 的消息）
    pub input: String,
    /// 预期输出描述（给 LLM 比对的自然语言，已剔除 `@` 开头的结构化断言行）
    pub expected: String,
    /// `@tools: A, B` —— 这些工具本轮必须被调用（确定性断言，不经过 LLM 裁判）。
    pub expect_tools: Vec<String>,
    /// `@no-tools` —— 本轮不得调用任何工具。
    pub expect_no_tools: bool,
    /// `@contains: 文本` —— 本轮回复文本必须包含这些片段（大小写不敏感）。
    pub expect_contains: Vec<String>,
}

impl Turn {
    /// 检查本轮的 `@tools`/`@no-tools`/`@contains` 断言，返回失败描述列表。
    ///
    /// 存在的理由：在此之前，`.test` 用例的唯一"断言"方式是把要求写进自然
    /// 语言预期描述里，交给 LLM 裁判（`comparator.rs`）判断——而且只有加了
    /// `--compare` 才会跑。"模型有没有调用 Write"、"回复里有没有出现那个数字"
    /// 都是确定性事实，不需要（也不该）花一次模型调用去模糊判断。
    pub fn check_expectations(
        &self,
        text: &str,
        tool_uses: &[(String, serde_json::Value)],
    ) -> Vec<String> {
        let mut failures = Vec::new();
        let called: Vec<&str> = tool_uses.iter().map(|(n, _)| n.as_str()).collect();
        let haystack = text.to_lowercase();
        for want in &self.expect_contains {
            if !haystack.contains(&want.to_lowercase()) {
                failures.push(format!(
                    "turn {}: @contains expected `{want}` in the reply text, got: {}",
                    self.index,
                    if text.trim().is_empty() {
                        "(empty)"
                    } else {
                        text.trim()
                    }
                ));
            }
        }
        if self.expect_no_tools && !called.is_empty() {
            failures.push(format!(
                "turn {}: @no-tools, but these tools were called: {}",
                self.index,
                called.join(", ")
            ));
        }
        for want in &self.expect_tools {
            if !called.iter().any(|c| c.eq_ignore_ascii_case(want)) {
                failures.push(format!(
                    "turn {}: @tools expected `{want}` to be called, actual calls: {}",
                    self.index,
                    if called.is_empty() {
                        "(none)".into()
                    } else {
                        called.join(", ")
                    }
                ));
            }
        }
        failures
    }
}

/// 解析 `.test` 文件。
pub fn parse_test_file(path: &std::path::Path) -> anyhow::Result<TestCase> {
    let content = std::fs::read_to_string(path)?;
    parse_test_script(&content, &path.display().to_string())
}

/// 解析 `.test` 脚本内容。
pub fn parse_test_script(content: &str, source_path: &str) -> anyhow::Result<TestCase> {
    let content = content.trim();
    if content.is_empty() {
        anyhow::bail!("empty test script");
    }

    // 找到第一个 >>>>>>>>>>>>>>>> 的位置
    let first_sep = content.find(SEP_IN);
    let (meta, body) = match first_sep {
        Some(pos) => {
            let m = content[..pos].trim().to_string();
            let b = &content[pos + SEP_IN.len()..];
            (m, b)
        }
        None => {
            // 没有分隔符 → 整个文件是单轮输入（无预期输出）
            return Ok(TestCase {
                meta: String::new(),
                source_path: source_path.to_string(),
                session_mode: SessionMode::default(),
                fixture: None,
                scene: None,
                turns: vec![Turn {
                    index: 0,
                    input: content.to_string(),
                    expected: String::new(),
                    expect_tools: vec![],
                    expect_no_tools: false,
                    expect_contains: vec![],
                }],
            });
        }
    };

    let session_mode = parse_session_mode(&meta)?;
    let fixture = parse_meta_field(&meta, "fixture");
    let scene = parse_meta_field(&meta, "scene");

    // 解析轮次
    let turns = parse_turns(body)?;
    if turns.is_empty() {
        anyhow::bail!("no turns found in test script (missing >>>>>>>>>>>>>>>> markers)");
    }

    Ok(TestCase {
        meta,
        source_path: source_path.to_string(),
        session_mode,
        fixture,
        scene,
        turns,
    })
}

/// A `key: value` line in the meta block, if the case declares one.
///
/// Prerequisites used to live only in the prose header (`# 前置: --fixture …
/// --scene mini`), which meant every runner invocation had to remember to pass
/// them by hand — and `tests/run_api.sh` never did. A case that needs MCP, or a
/// scene other than the default, was therefore recorded without them: the
/// recording looked fine and exercised none of what the case was written for.
/// Declaring it here makes the runner configure itself from the case.
fn parse_meta_field(meta: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}:");
    meta.lines()
        .filter_map(|line| {
            let line = line.trim().trim_start_matches('#').trim();
            line.strip_prefix(&prefix).map(|v| v.trim().to_string())
        })
        .rfind(|v: &String| !v.is_empty())
}

/// 从元信息区（第一个 `>>>>` 之前）读 `session:` 声明。
///
/// 允许的写法（大小写不敏感，可带注释前缀 `#`）：
/// ```text
/// # session: shared      → 整个用例共用一个会话，对话历史跨轮延续
/// # session: per-turn    → 每轮独立会话（默认，可以显式写出来当文档）
/// ```
/// 没写就是 `per-turn`——现存全部用例（以及它们的 cassette）都靠这个默认值。
fn parse_session_mode(meta: &str) -> anyhow::Result<SessionMode> {
    let mut found = None;
    for line in meta.lines() {
        let line = line.trim().trim_start_matches('#').trim();
        let Some(value) = line.strip_prefix("session:") else {
            continue;
        };
        let value = value.trim().to_ascii_lowercase();
        let mode = match value.as_str() {
            "shared" => SessionMode::Shared,
            "per-turn" | "per_turn" | "isolated" => SessionMode::PerTurn,
            other => anyhow::bail!(
                "unknown `session: {other}` in test meta — expected `shared` or `per-turn`"
            ),
        };
        found = Some(mode);
    }
    Ok(found.unwrap_or_default())
}

/// 一个预期输出块里剥出来的结构化断言（`@` 开头的行）。
#[derive(Debug, Default)]
struct Expectations {
    prose: String,
    tools: Vec<String>,
    no_tools: bool,
    contains: Vec<String>,
}

/// 从预期输出块里剥出 `@` 开头的结构化断言行，剩下的才是给 LLM 裁判的自然语言。
fn split_expectations(raw: &str) -> Expectations {
    let mut prose = Vec::new();
    let mut e = Expectations::default();
    for line in raw.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("@tools:") {
            e.tools.extend(
                rest.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty()),
            );
        } else if let Some(rest) = t.strip_prefix("@contains:") {
            // 整行当一个片段（不按逗号拆——要断言的文本里本来就可能有逗号）。
            let want = rest.trim();
            if !want.is_empty() {
                e.contains.push(want.to_string());
            }
        } else if t == "@no-tools" || t == "@no_tools" {
            e.no_tools = true;
        } else {
            prose.push(line);
        }
    }
    e.prose = prose.join("\n").trim().to_string();
    e
}

fn parse_turns(body: &str) -> anyhow::Result<Vec<Turn>> {
    let mut turns = Vec::new();
    let body = body.trim();

    // 按 >>>>>>>>>>>>>>>> 分割
    let blocks: Vec<&str> = body.split(SEP_IN).collect();
    for (i, block) in blocks.into_iter().enumerate() {
        let block = block.trim();
        if block.is_empty() {
            continue;
        }

        // 按 <<<<<<<<<<<<<<<< 分割输入和预期输出
        let (input, expected) = match block.split_once(SEP_OUT) {
            Some((inp, exp)) => (inp.trim().to_string(), exp.trim().to_string()),
            None => (block.to_string(), String::new()),
        };

        let e = split_expectations(&expected);
        turns.push(Turn {
            index: i,
            input,
            expected: e.prose,
            expect_tools: e.tools,
            expect_no_tools: e.no_tools,
            expect_contains: e.contains,
        });
    }

    Ok(turns)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_single_turn() {
        let script = "\
# Test case
This is a test.

>>>>>>>>>>>>>>>>
Hello, create a file.
<<<<<<<<<<<<<<<<
Should create a file and report success.
";
        let tc = parse_test_script(script, "test.test").unwrap();
        assert_eq!(tc.meta, "# Test case\nThis is a test.");
        assert_eq!(tc.turns.len(), 1);
        assert_eq!(tc.turns[0].input, "Hello, create a file.");
        assert_eq!(
            tc.turns[0].expected,
            "Should create a file and report success."
        );
    }

    #[test]
    fn parse_multi_turn() {
        let script = "\
Multi-turn test

>>>>>>>>>>>>>>>>
Turn 1 input.
<<<<<<<<<<<<<<<<
Turn 1 expected.

>>>>>>>>>>>>>>>>
Turn 2 input.
<<<<<<<<<<<<<<<<
Turn 2 expected.
";
        let tc = parse_test_script(script, "test.test").unwrap();
        assert_eq!(tc.turns.len(), 2);
        assert_eq!(tc.turns[0].input, "Turn 1 input.");
        assert_eq!(tc.turns[1].input, "Turn 2 input.");
    }

    #[test]
    fn parse_no_expected() {
        let script = "\
>>>>>>>>>>>>>>>>
Just input, no expected output marker.
";
        let tc = parse_test_script(script, "test.test").unwrap();
        assert_eq!(tc.turns.len(), 1);
        assert_eq!(tc.turns[0].input, "Just input, no expected output marker.");
        assert!(tc.turns[0].expected.is_empty());
    }

    /// The prose header said `# 前置: --fixture … --scene mini` and nothing
    /// read it, so `run_api.sh` recorded every such case without either. A
    /// declaration the runner actually consults is the difference between a
    /// recording that exercises MCP and one that silently does not.
    #[test]
    fn a_case_declares_its_own_fixture_and_scene() {
        let script = "# 前置: --fixture tests/fixtures/template_project\n\
                      # fixture: tests/fixtures/template_project\n\
                      # scene: mini\n\n>>>>>>>>>>>>>>>>\nhi\n";
        let tc = parse_test_script(script, "t.test").unwrap();
        assert_eq!(
            tc.fixture.as_deref(),
            Some("tests/fixtures/template_project")
        );
        assert_eq!(tc.scene.as_deref(), Some("mini"));
    }

    /// The prose line alone must not be mistaken for a declaration — it is
    /// free text and says `--fixture`, not `fixture:`.
    #[test]
    fn a_case_that_declares_nothing_leaves_both_absent() {
        let tc = parse_test_script(
            "# 前置: 无（--scene mini）\n\n>>>>>>>>>>>>>>>>\nhi\n",
            "t.test",
        )
        .unwrap();
        assert_eq!(tc.fixture, None);
        assert_eq!(tc.scene, None);
    }

    #[test]
    fn session_mode_defaults_to_per_turn() {
        // Every existing case (and every cassette recorded against one) relies
        // on this default — a shared session changes the message history, which
        // changes the request, which a replay reports as a divergence.
        let script = "no directive here\n\n>>>>>>>>>>>>>>>>\nhi\n";
        let tc = parse_test_script(script, "t.test").unwrap();
        assert_eq!(tc.session_mode, SessionMode::PerTurn);
    }

    #[test]
    fn session_shared_directive_opts_into_one_session_for_the_whole_case() {
        let script = "\
# 用例说明
# session: shared

>>>>>>>>>>>>>>>>
turn 1
<<<<<<<<<<<<<<<<
expected 1
>>>>>>>>>>>>>>>>
turn 2
<<<<<<<<<<<<<<<<
expected 2
";
        let tc = parse_test_script(script, "t.test").unwrap();
        assert_eq!(tc.session_mode, SessionMode::Shared);
        assert_eq!(tc.turns.len(), 2);
    }

    #[test]
    fn session_per_turn_can_be_stated_explicitly() {
        let script = "session: per-turn\n\n>>>>>>>>>>>>>>>>\nhi\n";
        assert_eq!(
            parse_test_script(script, "t.test").unwrap().session_mode,
            SessionMode::PerTurn
        );
    }

    #[test]
    fn unknown_session_value_is_an_error_not_a_silent_default() {
        // A typo'd `session: shred` silently running per-turn would reproduce
        // exactly the gap this directive exists to close.
        let script = "# session: shred\n\n>>>>>>>>>>>>>>>>\nhi\n";
        let err = parse_test_script(script, "t.test").unwrap_err().to_string();
        assert!(err.contains("unknown `session:"), "got: {err}");
    }

    #[test]
    fn tool_expectations_are_split_out_of_the_prose() {
        let script = "\
>>>>>>>>>>>>>>>>
写个文件
<<<<<<<<<<<<<<<<
模型应该创建一个笔记文件。
@tools: Write, Read
";
        let tc = parse_test_script(script, "t.test").unwrap();
        let turn = &tc.turns[0];
        assert_eq!(turn.expected, "模型应该创建一个笔记文件。");
        assert_eq!(
            turn.expect_tools,
            vec!["Write".to_string(), "Read".to_string()]
        );
        assert!(!turn.expect_no_tools);
        assert!(turn
            .check_expectations(
                "",
                &[
                    ("Write".into(), serde_json::json!({})),
                    ("Read".into(), serde_json::json!({})),
                ]
            )
            .is_empty());
        let failures = turn.check_expectations("", &[("Write".into(), serde_json::json!({}))]);
        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains("Read"), "got: {failures:?}");
    }

    #[test]
    fn no_tools_expectation_fails_when_a_tool_was_called() {
        let script = "\
>>>>>>>>>>>>>>>>
只回答，别动手
<<<<<<<<<<<<<<<<
直接回答即可。
@no-tools
";
        let turn = &parse_test_script(script, "t.test").unwrap().turns[0];
        assert!(turn.expect_no_tools);
        assert!(turn.check_expectations("", &[]).is_empty());
        let failures = turn.check_expectations("", &[("Bash".into(), serde_json::json!({}))]);
        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains("Bash"), "got: {failures:?}");
    }

    #[test]
    fn parse_empty_meta() {
        let script = "\
>>>>>>>>>>>>>>>>
Input only.
<<<<<<<<<<<<<<<<
Expected.
";
        let tc = parse_test_script(script, "test.test").unwrap();
        assert!(tc.meta.is_empty());
        assert_eq!(tc.turns.len(), 1);
    }
}
