//! 回合状态切片、压缩上报、git 变更识别等小而独立的行为。

use super::support::{recall_test_settings, FixedUsageModel};
use crate::agent::Agent;
use crate::turn::prompt::open_conversation_with_user_blocks;
use crate::turn::request::ModelRequest;
use crate::turn::TurnOutcome;
use base::event::AgentEvent;
use base::interface::model::{MessageRole, ModelContentBlock, ModelMessage};
use base::prompt::PromptBlock;
use base::settings::ThinkingMode;
use std::path::PathBuf;
use std::sync::Arc;

#[test]
fn user_blocks_open_the_conversation_and_the_input_map_follows() {
    let text = |t: &str| ModelMessage {
        role: MessageRole::User,
        content: vec![ModelContentBlock::Text { text: t.into() }],
    };
    let mut request = ModelRequest {
        prompt_blocks: vec![
            PromptBlock::system("rules"),
            PromptBlock::user("skills: a, b").named(base::prompt::names::SKILLS_CATALOG),
        ],
        tool_defs: vec![],
        messages: vec![text("context"), text("hello")],
        params: base::interface::model::StreamParams {
            model: "m".into(),
            max_tokens: 1,
            thinking_mode: ThinkingMode::Off,
            effort: None,
            fallback_model: None,
            cache_edits: vec![],
            origin: None,
            input_map: Some(base::interface::model::InputMap {
                user_message: 1,
                spans: vec![],
            }),
            provider_search: None,
        },
    };
    open_conversation_with_user_blocks(&mut request);

    assert_eq!(request.prompt_blocks.len(), 1);
    assert_eq!(request.prompt_blocks[0].content, "rules");
    assert_eq!(request.messages.len(), 3);
    assert!(matches!(
        &request.messages[0].content[0],
        ModelContentBlock::Text { text } if text == "<system-reminder>\nskills: a, b\n</system-reminder>"
    ));
    assert_eq!(request.params.input_map.unwrap().user_message, 2);
}

/// The session-memory staleness reset in `run_user_turn` recognizes a self-update by comparing `extract_tool_file_paths`
/// against `sm.path()` — this pins down that a `Write` to the exact
/// sidecar path is detected, and an unrelated `Write` isn't mistaken
/// for one.
#[test]
fn extract_tool_file_paths_finds_write_to_exact_path() {
    let sidecar = PathBuf::from("/tmp/sessions/abc/session_memory.md");
    let msg = ModelMessage {
        role: MessageRole::Assistant,
        content: vec![ModelContentBlock::ToolUse {
            id: "1".into(),
            name: "Write".into(),
            input: serde_json::json!({"file_path": sidecar.to_string_lossy(), "content": "x"}),
        }],
    };
    let paths = Agent::extract_tool_file_paths(&[msg]);
    assert!(paths.contains(&sidecar));
}

#[test]
fn extract_tool_file_paths_does_not_match_unrelated_write() {
    let sidecar = PathBuf::from("/tmp/sessions/abc/session_memory.md");
    let msg = ModelMessage {
        role: MessageRole::Assistant,
        content: vec![ModelContentBlock::ToolUse {
            id: "1".into(),
            name: "Write".into(),
            input: serde_json::json!({"file_path": "/tmp/some/other/file.rs", "content": "x"}),
        }],
    };
    let paths = Agent::extract_tool_file_paths(&[msg]);
    assert!(!paths.contains(&sidecar));
}

#[tokio::test]
async fn clearing_old_tool_results_by_age_is_reported_as_a_compaction() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut agent, mut event_rx, _input_tx) = crate::agent::Builder::new()
        .scene(Arc::new(scene::code::spec(
            scene::code::Interaction::Interactive,
        )))
        .model(Arc::new(FixedUsageModel {
            usage: Default::default(),
        }))
        .settings(Arc::new(recall_test_settings(tmp.path())))
        .skip_warmup(true)
        .build()
        .expect("build should succeed");
    agent
        .compaction
        .set_time_based(compaction::time_based_mc::TimeBasedMcConfig {
            max_age: Some(std::time::Duration::from_millis(1)),
            min_messages: 0,
        });
    agent.session.push_message(ModelMessage {
        role: MessageRole::Assistant,
        content: vec![ModelContentBlock::ToolUse {
            id: "r1".into(),
            name: "Read".into(),
            input: serde_json::json!({"file_path": "/x"}),
        }],
    });
    agent.session.push_message(ModelMessage {
        role: MessageRole::User,
        content: vec![ModelContentBlock::ToolResult {
            tool_use_id: "r1".into(),
            content: "old contents ".repeat(200),
            is_error: None,
        }],
    });
    std::thread::sleep(std::time::Duration::from_millis(5));

    agent
        .compact_if_needed(&tokio_util::sync::CancellationToken::new())
        .await;

    let mut frames = Vec::new();
    while let Ok(event) = event_rx.try_recv() {
        if let AgentEvent::CompactAction {
            strategy,
            tokens_before,
            tokens_after,
            ..
        } = event
        {
            frames.push((strategy, tokens_before, tokens_after));
        }
    }
    assert_eq!(frames.len(), 1, "{frames:?}");
    assert_eq!(frames[0].0, "MicroCompact");
    assert!(frames[0].2 < frames[0].1, "{frames:?}");
}

#[tokio::test]
async fn this_turn_starts_at_its_user_message_however_long_the_transcript() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut agent, _event_rx, _input_tx) = crate::agent::Builder::new()
        .scene(Arc::new(scene::code::spec(
            scene::code::Interaction::Interactive,
        )))
        .model(Arc::new(FixedUsageModel {
            usage: Default::default(),
        }))
        .settings(Arc::new(recall_test_settings(tmp.path())))
        .skip_warmup(true)
        .build()
        .expect("build should succeed");
    let write = |path: &str| ModelMessage {
        role: MessageRole::Assistant,
        content: vec![ModelContentBlock::ToolUse {
            id: path.into(),
            name: "Write".into(),
            input: serde_json::json!({"file_path": path, "content": "x"}),
        }],
    };
    for i in 0..60 {
        agent.session.push_message(write(&format!("/old/{i}")));
    }
    std::thread::sleep(std::time::Duration::from_millis(2));
    agent.turn.started_at = Some(std::time::Instant::now());
    agent.session.push_message(write("/new"));

    assert_eq!(
        Agent::extract_tool_file_paths(agent.messages_this_turn()),
        [PathBuf::from("/new")]
    );
}

/// `included_git_mutating_bash_call` gates whether `FrozenContext`
/// pays for a `refresh_git()` round-trip — this pins its heuristic down
/// against the cases it's meant to catch and the ones it must not
/// false-positive on.
fn bash_msg(command: &str) -> ModelMessage {
    ModelMessage {
        role: MessageRole::Assistant,
        content: vec![ModelContentBlock::ToolUse {
            id: "1".into(),
            name: "Bash".into(),
            input: serde_json::json!({"command": command}),
        }],
    }
}

#[test]
fn git_mutating_bash_detects_plain_checkout() {
    assert!(Agent::included_git_mutating_bash_call(&[bash_msg(
        "git checkout -b feature-x"
    )]));
}

#[test]
fn git_mutating_bash_detects_commit_with_global_flags() {
    assert!(Agent::included_git_mutating_bash_call(&[bash_msg(
        "git -C /repo --no-pager commit -m 'msg'"
    )]));
}

#[test]
fn git_mutating_bash_detects_within_a_chained_command() {
    assert!(Agent::included_git_mutating_bash_call(&[bash_msg(
        "cd /repo && git add -A && git commit -m wip"
    )]));
}

#[test]
fn git_mutating_bash_ignores_read_only_git_status() {
    assert!(!Agent::included_git_mutating_bash_call(&[bash_msg(
        "git status --short"
    )]));
}

#[test]
fn git_mutating_bash_ignores_read_only_git_log() {
    assert!(!Agent::included_git_mutating_bash_call(&[bash_msg(
        "git log --oneline -n 5"
    )]));
}

#[test]
fn git_mutating_bash_ignores_non_bash_tools() {
    let msg = ModelMessage {
        role: MessageRole::Assistant,
        content: vec![ModelContentBlock::ToolUse {
            id: "1".into(),
            name: "Read".into(),
            input: serde_json::json!({"file_path": "git commit history.md"}),
        }],
    };
    assert!(!Agent::included_git_mutating_bash_call(&[msg]));
}

#[test]
fn git_mutating_bash_ignores_unrelated_commands() {
    assert!(!Agent::included_git_mutating_bash_call(&[bash_msg(
        "ls -la && cat README.md"
    )]));
}

#[test]
fn turn_outcome_default() {
    let outcome = TurnOutcome::default();
    assert_eq!(outcome.api_calls, 0);
}
