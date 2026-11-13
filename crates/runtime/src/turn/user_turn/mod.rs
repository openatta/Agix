//! 用户回合的主循环 run_user_turn_inner：一次用户输入到停止原因的全部轮次。

mod end;
mod open;
mod recall;
mod step;
mod stop;

use crate::agent::Agent;
use crate::turn::TurnError;
use crate::turn::TurnOutcome;
use base::interface::model::Usage;
use std::ops::ControlFlow;
use tokio_util::sync::CancellationToken;

struct TurnLoopState {
    started: std::time::Instant,
    model: String,
    max_tokens: u32,
    output_limit_escalations: u32,
    turn_no: u32,
    api_calls: u32,
    tool_calls: u32,
    structured_output_calls: u32,
    // What the turn has spent, kept once. The budget reads it through
    // `Spend` and a caller reads it whole; a second accumulator beside it
    // would let the two disagree about the cache.
    spent: Usage,
    last_call: Usage,
    had_tool_uses: bool,
    sent_back_by_stop_hook: bool,
    skill_discovery: Option<SkillDiscovery>,
}

type SkillDiscovery =
    tokio_util::task::AbortOnDropHandle<(Vec<base::frozen::SkillEntry>, Vec<String>)>;

impl TurnLoopState {
    fn progress<'a>(&self, stop_reason: &'a str) -> base::interface::turn_policy::TurnProgress<'a> {
        base::interface::turn_policy::TurnProgress {
            api_calls: self.api_calls,
            tool_calls: self.tool_calls,
            structured_output_calls: self.structured_output_calls,
            stop_reason,
        }
    }

    fn new(settings: &base::settings::LoadedSettings) -> Self {
        Self {
            started: std::time::Instant::now(),
            model: settings.values.model.model_name.clone(),
            max_tokens: settings.values.model.max_tokens,
            output_limit_escalations: 0,
            turn_no: 0,
            api_calls: 0,
            tool_calls: 0,
            structured_output_calls: 0,
            spent: Usage::default(),
            last_call: Usage::default(),
            had_tool_uses: false,
            sent_back_by_stop_hook: false,
            skill_discovery: None,
        }
    }
}

impl Agent {
    pub(super) async fn run_user_turn_inner(
        &mut self,
        content: String,
        attachments: Vec<crate::agent::Attachment>,
        cancel: CancellationToken,
        origin: history::entry::MessageOrigin,
    ) -> Result<TurnOutcome, TurnError> {
        let content = match self.open_turn(content, &attachments, &cancel).await {
            ControlFlow::Continue(content) => content,
            ControlFlow::Break(blocked) => return Ok(blocked),
        };

        let _timer = self.telemetry.perf.start_timer("turn", "total");
        let mut state = TurnLoopState::new(&self.settings);

        self.prepare_turn().await;
        let recall = self.start_memory_recall(&content);
        state.skill_discovery = self.start_skill_discovery();
        self.push_user_message(content, &attachments, origin).await;
        self.announce_output_target();
        if let Some(recall) = recall {
            self.inject_recalled_memories(recall).await;
        }

        let stop = loop {
            if let ControlFlow::Break(stop) = self.step(&mut state, &cancel).await? {
                break stop;
            }
        };

        Ok(self.end_turn(stop, &state).await)
    }
}
