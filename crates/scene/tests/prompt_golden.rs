//! 冻结每个内建场景和一个声明式场景在固定上下文下的系统提示与会话上下文字节。
//! 提示差一个字节，跨会话的提示缓存就失效，所以这里逐字节比对；确实要改提示时
//! 用 `UPDATE_GOLDEN=1 cargo test -p scene --test prompt_golden` 重新生成。

use base::interface::scene::{Scene, ScenePromptContext};
use scene::code::{CodeScene, Interaction};
use scene::declarative::DeclarativeScene;
use std::borrow::Cow;
use std::path::Path;

const GOLDEN: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/golden/scene_prompts.json"
);

const DECLARATIVE_JSON: &str = r#"{
    "id": "review",
    "name": "Review",
    "description": "Reviews a change",
    "tools": ["Read", "Grep", "Glob", "Bash"],
    "deferred_tools": ["Bash"]
}"#;

const DECLARATIVE_PROMPT: &str = "You review code in {{cwd}} on {{os}}.";

fn ctx() -> ScenePromptContext<'static> {
    ScenePromptContext {
        cwd: Cow::Borrowed("/work/project"),
        os: Cow::Borrowed("linux"),
        shell: Cow::Borrowed("bash"),
        home_dir: Cow::Borrowed("/home/me"),
        date: Cow::Borrowed("2026-11-10"),
        model_name: Cow::Borrowed("claude-test"),
        skills_text: Some(Cow::Borrowed("- skillify: turn a session into a skill")),
        mcp_instructions: Some(Cow::Borrowed("## github\nUse the GitHub tools.")),
        session_memory: None,
        is_git: true,
        git_branch: Some(Cow::Borrowed("main")),
        is_worktree: false,
        git_status: Some(Cow::Borrowed("M src/lib.rs")),
        language: None,
        scratchpad_dir: Some(Cow::Borrowed("/tmp/scratch")),
        output_style_content: None,
        available_tools: Some(Cow::Borrowed(
            "Agent,Bash,Edit,Glob,Grep,Read,RunCode,Skill,TaskWait,TeamCreate,ToolSearch,Write",
        )),
        tool_results_ever_cleared: false,
    }
}

fn render(scene: &dyn Scene) -> serde_json::Value {
    let ctx = ctx();
    serde_json::json!({
        "id": scene.id(),
        "system_prompt": scene.build_system_prompt(&ctx),
        "session_context": scene.build_session_context(&ctx),
    })
}

#[test]
fn scene_prompts_match_the_golden_bytes() {
    let declarative = DeclarativeScene::from_sources(
        Path::new("/scenes/review"),
        DECLARATIVE_JSON,
        DECLARATIVE_PROMPT,
    )
    .unwrap();
    let scenes: Vec<Box<dyn Scene>> = vec![
        Box::new(CodeScene::new(Interaction::Interactive)),
        Box::new(CodeScene::new(Interaction::Unattended)),
        Box::new(scene::harnix::HarnixScene),
        Box::new(scene::mini::MiniScene),
        Box::new(declarative),
    ];
    let rendered: Vec<_> = scenes.iter().map(|s| render(s.as_ref())).collect();
    let actual = serde_json::to_string_pretty(&rendered).unwrap() + "\n";

    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(Path::new(GOLDEN).parent().unwrap()).unwrap();
        std::fs::write(GOLDEN, &actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(GOLDEN)
        .unwrap_or_else(|e| panic!("{GOLDEN}: {e}; generate it with UPDATE_GOLDEN=1"));
    assert!(
        actual == expected,
        "scene prompts drifted from {GOLDEN}; if intended, regenerate with UPDATE_GOLDEN=1"
    );
}
