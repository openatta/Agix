//! 回合收尾：StopReason 统一决定 outcome、turn_complete 遥测与 last_had_tool_uses。

use super::TurnLoopState;
use crate::agent::Agent;
use crate::turn::recall::Extraction;
use crate::turn::TurnOutcome;
use base::interface::model::MessageRole;
use base::interface::model::ModelContentBlock;
use base::interface::model::ModelMessage;
use base::interface::model::Usage;

pub(super) enum StopReason {
    Replied(String),
    Cancelled,
    Policy(String),
    ContextExceeded,
    BudgetExceeded,
    ToolHookDiscontinued,
    StopHookDiscontinued(Option<String>),
}

impl StopReason {
    fn as_str(&self) -> &str {
        match self {
            Self::Replied(reason) | Self::Policy(reason) => reason,
            Self::Cancelled => "cancelled",
            Self::ContextExceeded => "context_exceeded",
            Self::BudgetExceeded => "budget_exceeded",
            Self::ToolHookDiscontinued | Self::StopHookDiscontinued(_) => "stopped_by_hook",
        }
    }

    /// The paths that end on a reply report that call's usage; the others
    /// report zero even after a call.
    fn reports_last_call(&self) -> bool {
        matches!(
            self,
            Self::Replied(_) | Self::ToolHookDiscontinued | Self::StopHookDiscontinued(_)
        )
    }
}

impl Agent {
    pub(super) async fn end_turn(
        &mut self,
        stop: StopReason,
        state: &TurnLoopState,
    ) -> TurnOutcome {
        match &stop {
            StopReason::Replied(stop_reason) => {
                self.complete_replied_turn(stop_reason, state).await;
            }
            _ => self.record_turn_complete(
                stop.as_str(),
                state.api_calls,
                state.tool_calls,
                state.started,
            ),
        }
        self.last_had_tool_uses = state.had_tool_uses;
        let last_call = if stop.reports_last_call() {
            state.last_call
        } else {
            Usage::default()
        };
        let message = match &stop {
            StopReason::StopHookDiscontinued(message) => message.clone(),
            _ => None,
        };
        TurnOutcome::ended(
            stop.as_str(),
            state.api_calls,
            state.tool_calls,
            last_call,
            state.spent,
            message,
        )
    }

    async fn complete_replied_turn(&mut self, stop_reason: &str, state: &TurnLoopState) {
        self.session.increment_turn();
        self.record_turn_complete_in(
            state.turn_no,
            stop_reason,
            state.api_calls,
            state.tool_calls,
            state.started,
        );
        // TurnComplete — the normal end of a turn. Distinct from the
        // `Stop` hook above, which fires whenever the model stops calling
        // tools and can *veto* the stop; this one is a notification that
        // the turn is over, carrying why.
        self.fire_lifecycle_hook(hooks::HookEvent::TurnComplete, |i| {
            i.with_turn_id(self.current_turn_id.clone())
                .with_stop_reason(stop_reason)
        })
        .await;
        self.spawn_memory_extraction();
        self.remind_stale_session_notes().await;
    }

    fn spawn_memory_extraction(&mut self) {
        // Auto-extract durable memories after turn completion.
        // Only extract if the model produced a complete response (not cancelled/max_turns).
        // Gated on `memory_enabled` — see the prefetch block above for why this matters
        // (the flag's whole point is opting out of the memory subsystem entirely, not just
        // the static prompt injection).
        if self.settings.values.memory_enabled && self.memory.extracts_after_turn() {
            let messages = self.session.messages().to_vec();
            // `task_models.memory` lets an app point this background
            // extraction call at a different vendor/model than the main
            // conversation — same TaskRouter mechanism already proven
            // for the "subagent" task type (see
            // `agents::delegate::Delegator::model_for_subagent`). No override
            // configured → falls back to the main model.
            let (model, model_name) = self.model_for_task("memory");
            self.memory.spawn_extraction(Extraction {
                messages,
                model,
                model_name,
                prompt_intro: self.scene.memory_extraction_intro.clone(),
                environment: self.policies.environment().clone(),
                session_id: self.session.session_id.clone(),
            });
        }
    }

    async fn remind_stale_session_notes(&mut self) {
        // Remind the model to update its cross-session notes once they
        // have gone stale.
        if let Some(ref sm) = self.session.session_memory {
            let current_turn = self.session.turn_count;
            // If a Write/Edit this turn targeted the sidecar file itself,
            // the model just updated its notes — reset staleness instead
            // of nagging again next turn. Compares against the exact
            // path we tell the model below, so a literal match is enough
            // (no canonicalization needed).
            let updated_this_turn = Self::extract_tool_file_paths(self.messages_this_turn())
                .iter()
                .any(|p| p == sm.path());
            if updated_this_turn {
                if let Err(e) = sm.mark_extraction_completed(current_turn).await {
                    tracing::warn!(error = %e, "failed to mark session_memory extraction completed");
                }
            } else if sm.is_stale(current_turn) {
                tracing::debug!(
                    last_update_turn = sm.last_update_turn(),
                    current_turn,
                    "session memory is stale; injecting update reminder"
                );
                self.session.push_message(ModelMessage {
                    role: MessageRole::User,
                    content: vec![ModelContentBlock::Text {
                        text: format!(
                            "\
<system-reminder>
Your session notes ({}) have not been updated in several turns.
Consider reviewing and updating them with any persistent facts, user preferences,
or project context that should survive across sessions.
</system-reminder>",
                            sm.path().display()
                        ),
                    }],
                });
            }
        }
    }
}
