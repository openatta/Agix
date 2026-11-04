//! 会话连续性：`session: shared` 的用例，第二轮真的看得见第一轮。
//!
//! 这个文件补的是整套测试系统里最大的一个空档：`api_runner::run_test_case`
//! 给每一轮一个新的 `session_id`（`test-turn-{index}`），所以在此之前**没有
//! 任何测试跑过跨轮对话**——记忆召回、`already_surfaced` 去重、上下文压缩、
//! session-memory 陈旧判定、压缩后恢复，全都是只在轮与轮之间才存在的行为。
//!
//! 这里用一个假 Model（不打网络、不需要 cassette）把这件事钉死成一个确定性
//! 断言：模型每次被调用时检查"消息历史里有没有出现只在第一轮说过的暗号"，
//! 逐轮隔离时第二轮必然看不到，共享会话时必然看到。

use base::interface::model::{
    Model, ModelError, ModelEvent, ModelMessage, ModelStream, StreamParams, ToolDef, Usage,
};
use base::interface::prompt::PromptBlock;
use base::interface::settings::RecorderMode;
use base::provider::ApiType;
use std::sync::Arc;
use test_runner::api_runner::{self, AgentRunnerConfig};
use test_runner::script::{self};
use tokio_util::sync::CancellationToken;

/// 只在第一轮的用户输入里出现的暗号。
const MARKER: &str = "ORCHID-4271";

/// 假模型：回一句话，说明"这次请求的消息历史里有没有出现暗号"。
/// 不打网络，不需要凭证，行为完全确定。
struct MarkerProbeModel;

#[async_trait::async_trait]
impl Model for MarkerProbeModel {
    fn api_type(&self) -> ApiType {
        ApiType::Anthropic
    }

    async fn stream(
        &self,
        _prompt_blocks: Vec<PromptBlock>,
        _tools: Vec<ToolDef>,
        messages: Vec<ModelMessage>,
        _params: StreamParams,
        _cancel: CancellationToken,
    ) -> Result<ModelStream, ModelError> {
        // 只看**这一轮用户消息之前**的历史——当前轮自己的输入当然带着暗号，
        // 那不说明任何问题；要证明的是"上一轮说过的话还在不在这次请求里"。
        let prior = messages.len().saturating_sub(1);
        let saw_marker = messages[..prior]
            .iter()
            .any(|m| format!("{m:?}").contains(MARKER));
        let text = if saw_marker {
            "SAW_MARKER"
        } else {
            "NO_MARKER"
        };
        Ok(Box::new(futures::stream::iter(vec![
            Ok(ModelEvent::TextDelta {
                text: text.to_string(),
            }),
            Ok(ModelEvent::EndTurn {
                stop_reason: "end_turn".into(),
                usage: Usage::default(),
            }),
        ])))
    }
}

fn two_turn_case() -> script::TestCase {
    // 第一轮带暗号，第二轮不带——第二轮能不能"看见"暗号，完全取决于会话是不是共享的。
    let src = format!(
        "\
>>>>>>>>>>>>>>>>
记住这个暗号：{MARKER}
<<<<<<<<<<<<<<<<
确认收到。

>>>>>>>>>>>>>>>>
我刚才说的暗号是什么？
<<<<<<<<<<<<<<<<
复述上一轮的暗号。
"
    );
    script::parse_test_script(&src, "session_continuity.test").unwrap()
}

fn runner_config(recordings_dir: &std::path::Path, scenario: &str) -> AgentRunnerConfig {
    AgentRunnerConfig {
        model: Arc::new(MarkerProbeModel),
        // Record（而不是 Replay）：这条路径下 recorder 总是调用内层模型，
        // 不查 cassette，也就跟"回放命中/未命中"完全无关——这个测试要验的是
        // 会话语义，不是录制回放。
        recorder_mode: RecorderMode::Record,
        recorder_name: scenario.to_string(),
        case_id: scenario.to_string(),
        recordings_dir: recordings_dir.to_path_buf(),
        telemetry_path: None,
        fixture_dir: None,
        scene: Arc::new(scene::coding::CodingScene),
        recorder: telemetry::recorder::Recorder::new(),
    }
}

/// 两种模式跑同一个用例，断言只有共享会话那次的第二轮看得见第一轮。
///
/// 两次跑放在同一个 `#[test]` 里是有意的：`api_runner` 的两个入口对同一个用例
/// 名用同一个工作目录，并在开头把它删掉重建，并行跑会互相掀桌子。
#[tokio::test(flavor = "multi_thread")]
async fn shared_session_carries_conversation_state_across_turns() {
    let case = two_turn_case();
    let tmp = tempfile::tempdir().unwrap();

    // 1. 默认（逐轮隔离）：第二轮是一个全新会话，看不到第一轮。
    let isolated = api_runner::run_test_case(
        runner_config(&tmp.path().join("isolated"), "isolated"),
        &case,
    )
    .await
    .expect("per-turn run should succeed");
    assert_eq!(isolated.len(), 2);
    assert!(
        isolated[0].text.contains("NO_MARKER"),
        "turn 1 starts empty in both modes, got: {}",
        isolated[0].text
    );
    assert!(
        isolated[1].text.contains("NO_MARKER"),
        "per-turn isolation must NOT carry turn 1's history into turn 2 — this is the \
         long-standing default every recorded cassette depends on; got: {}",
        isolated[1].text
    );

    // 2. 共享会话：第二轮的请求里必须带着第一轮的对话。
    let shared = api_runner::run_test_case_same_session(
        runner_config(&tmp.path().join("shared"), "shared"),
        &case,
        None,
    )
    .await
    .expect("shared-session run should succeed");
    assert_eq!(shared.len(), 2);
    assert!(
        shared[0].text.contains("NO_MARKER"),
        "turn 1 has no prior history even in shared mode, got: {}",
        shared[0].text
    );
    assert!(
        shared[1].text.contains("SAW_MARKER"),
        "shared session must carry turn 1's conversation into turn 2 — if this fails, every \
         cross-turn mechanism (memory recall, already_surfaced dedup, compaction, \
         session-memory staleness) is once again untested; got: {}",
        shared[1].text
    );
}
