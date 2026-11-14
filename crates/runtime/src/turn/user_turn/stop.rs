//! 模型不再调工具之后：输出上限升级、Stop hook、输出目标续写、排队的用户消息。

use super::end::StopReason;
use super::TurnLoopState;
use crate::agent::Agent;
use crate::turn::process::hook_permission_mode;
use base::interface::model::MessageRole;
use base::interface::model::ModelContentBlock;
use base::interface::model::ModelMessage;
use base::interface::model::Usage;
use std::ops::ControlFlow;
use tokio_util::sync::CancellationToken;

impl Agent {
    pub(super) async fn reply_stopped(
        &mut self,
        state: &mut TurnLoopState,
        result: crate::streaming::StreamResult,
        cancel: &CancellationToken,
    ) -> ControlFlow<StopReason> {
        let stop_reason = result.stop_reason;
        // 6. A reply that stopped early: escalate the output limit, or
        //    send a paused one back to be continued.
        if self.handle_early_stop(
            &stop_reason,
            &mut state.output_limit_escalations,
            &mut state.max_tokens,
        ) {
            return ControlFlow::Continue(());
        }

        // 7. Stop hooks: end the turn, send the model back, or let it stop.
        if let Some(flow) = self.run_stop_hooks(state, cancel).await {
            return flow;
        }

        if self.continue_toward_output_target(result.usage) {
            return ControlFlow::Continue(());
        }

        // The model has answered; a message the user sent meanwhile is
        // answered in the same turn rather than left for one nobody
        // asked to start.
        if self.deliver_queued_user_messages(true, cancel).await {
            return ControlFlow::Continue(());
        }

        ControlFlow::Break(StopReason::Replied(stop_reason))
    }

    /// `None` when no Stop hook objected and the reply may stand.
    async fn run_stop_hooks(
        &mut self,
        state: &mut TurnLoopState,
        cancel: &CancellationToken,
    ) -> Option<ControlFlow<StopReason>> {
        if !self.gate.hooks().has_hooks_for(hooks::HookEvent::Stop) {
            return None;
        }
        let stop_hook_input = hooks::HookInput {
            hook_event_name: "Stop".into(),
            session_id: self.session.session_id.to_string(),
            cwd: self.settings.paths.working_dir().display().to_string(),
            permission_mode: hook_permission_mode(self.session_state.permission_mode()),
            stop_hook_active: Some(state.sent_back_by_stop_hook),
            ..Default::default()
        };
        let hook_result = self
            .gate
            .hooks()
            .run(hooks::HookEvent::Stop, &stop_hook_input, cancel)
            .await;
        if hook_result.discontinued() {
            tracing::info!("Stop hook discontinued the turn");
            // `StopFailure` is the error-shaped sibling of `Stop`
            // (same relationship `PostToolUseFailure` has to
            // `PostToolUse`) and had no trigger. A turn cut short by
            // a hook veto is the case it exists for: `Stop` fired,
            // but the turn did not finish normally.
            self.fire_lifecycle_hook(hooks::HookEvent::StopFailure, |i| {
                i.with_reason("turn discontinued by a Stop hook")
            })
            .await;
            return Some(ControlFlow::Break(StopReason::StopHookDiscontinued(
                hook_result.discontinue_message().map(str::to_string),
            )));
        }
        if let Some(blocked) = hook_result.blocked() {
            let reason = blocked
                .message
                .as_deref()
                .filter(|m| !m.trim().is_empty())
                .unwrap_or("a Stop hook asked for more work before this turn ends");
            tracing::info!(reason, "Stop hook sent the model back to work");
            self.session.push_message(ModelMessage {
                role: MessageRole::User,
                content: vec![ModelContentBlock::Text {
                    text: format!(
                        "<system-reminder>\nA Stop hook did not let this turn end yet:\n\
                         {reason}\n</system-reminder>"
                    ),
                }],
            });
            state.sent_back_by_stop_hook = true;
            return Some(ControlFlow::Continue(()));
        }
        None
    }

    fn continue_toward_output_target(&mut self, usage: Usage) -> bool {
        // 8. Token budget continuation mode.
        // Continue while accumulated output < 90% of target AND not diminishing
        // (≥3 continuations, both this & previous delta < 500). No hard cap.
        let target = self.session_state.ledger().token_target.target;
        if let Some(target) = target {
            let this_delta = usage.output_tokens as u64;
            let progress =
                (self.session_state.ledger().token_target).add_output(target, this_delta);
            if let base::interface::budget_policy::OutputTarget::KeepGoing { nudge } =
                self.policies.budget_policy.on_output_target(&progress)
            {
                let continuation = self
                    .session_state
                    .ledger()
                    .token_target
                    .continued(this_delta);
                self.session.push_message(ModelMessage {
                    role: MessageRole::User,
                    content: vec![ModelContentBlock::Text { text: nudge }],
                });
                tracing::info!(
                    accumulated = progress.accumulated,
                    target,
                    continuation,
                    "Token budget continuation — injecting nudge"
                );
                return true;
            }

            // Budget met (≥90%) or diminishing returns — clear budget state.
            let threshold = (target as f64 * 0.9) as u64;
            let budget_met = progress.accumulated >= threshold;
            tracing::info!(
                accumulated = progress.accumulated,
                target,
                budget_met,
                "Token budget session complete"
            );
            self.session_state.ledger().token_target = Default::default();
        }
        false
    }
}
