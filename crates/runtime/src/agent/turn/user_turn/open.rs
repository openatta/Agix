//! 开回合：UserPromptSubmit 与 TurnStart、回合开场的上下文、用户消息入列。

use super::SkillDiscovery;
use crate::agent::Agent;
use crate::turn::attachments::resolve_attachments;
use crate::turn::process::PromptScreen;
use crate::turn::prompt::user_text_fingerprint;
use crate::turn::tool_env::engine_exec_from;
use crate::turn::TurnOutcome;
use base::event::AgentEvent;
use base::interface::model::MessageRole;
use base::interface::model::ModelContentBlock;
use base::interface::model::ModelMessage;
use base::interface::prompt_middleware::PromptDelivery;
use std::ops::ControlFlow;
use tokio_util::sync::CancellationToken;

impl Agent {
    pub(super) async fn open_turn(
        &mut self,
        mut content: String,
        attachments: &[crate::agent::Attachment],
        cancel: &CancellationToken,
    ) -> ControlFlow<TurnOutcome, String> {
        // A script's budget is per turn; without this reset the counter only
        // climbs and a script bound to a per-tool-call point goes silent
        // mid-session.
        self.gate.run_turn_starts();

        // UserPromptSubmit runs before any turn setup has touched anything. A
        // hook that errors or times out counts as having said nothing.
        match self
            .screen_user_prompt(content, attachments, PromptDelivery::Turn, cancel)
            .await
        {
            PromptScreen::Pass(screened) => content = screened,
            PromptScreen::Blocked(message) => {
                self.record_turn_complete("stopped_by_hook", 0, 0, std::time::Instant::now());
                return ControlFlow::Break(TurnOutcome {
                    message,
                    ..TurnOutcome::stopped("stopped_by_hook")
                });
            }
        }

        // After UserPromptSubmit, so the hook sees the message that will be
        // processed. Notification only: the response is discarded.
        self.fire_turn_hook(hooks::HookEvent::TurnStart, cancel, |input| {
            input.with_turn_id(self.current_turn_id.clone())
        })
        .await;
        // Here rather than only before the first tool runs: taking it there
        // puts a `git` call between a stream's concurrent tool starts.
        tools::nested_repos::take_baseline(&self.skill_invocation_context(cancel.clone())).await;

        // `turn_count` is incremented only at the end of the turn, so this
        // is the turn_no every later `turn_complete` carries.
        self.telemetry.record(telemetry::TelemetryEvent::turn_start(
            &self.session.session_id,
            self.session.turn_count,
            Some(self.current_turn_id.clone()),
            telemetry::TurnStartPayload {
                resumed: false,
                is_retry: false,
            },
        ));
        self.telemetry
            .record(telemetry::TelemetryEvent::user_prompt_submit(
                &self.session.session_id,
                self.session.turn_count,
                Some(self.current_turn_id.clone()),
                telemetry::UserPromptSubmitPayload {
                    char_count: u32::try_from(content.chars().count()).unwrap_or(u32::MAX),
                    attachment_count: u32::try_from(attachments.len()).unwrap_or(u32::MAX),
                },
            ));

        // A skill's `allowed_tools` last for the turn it was invoked in.
        self.gate.begin_turn().await;

        ControlFlow::Continue(content)
    }

    pub(super) async fn prepare_turn(&mut self) {
        self.announce_skill_changes();
        self.refresh_frozen_context().await;

        if !self.prompt.context_injected() {
            self.inject_session_context().await;
        }
        self.push_system_reminder();
    }

    fn announce_skill_changes(&mut self) {
        // The catalog is shared and swapped in place, so diff against what
        // this session was last shown, not against the catalog a moment ago.
        let changed = self.tooling.skills().reload_changed();
        if changed > 0 {
            tracing::debug!(count = changed, "Skills reloaded from file changes");
        }
        let skills_now: std::collections::HashSet<String> = self
            .tooling
            .skills()
            .list()
            .iter()
            .map(|s| s.name.clone())
            .collect();
        if let Some(skills_before) = self.prompt.replace_announced(skills_now.clone()) {
            // A reminder in the conversation, not only a rebuilt skills block:
            // a model keeps asserting a just-deleted skill is available
            // (seen with deepseek-v4-flash) even when the list says otherwise.
            let mut removed: Vec<&String> = skills_before.difference(&skills_now).collect();
            let mut added: Vec<&String> = skills_now.difference(&skills_before).collect();
            removed.sort();
            added.sort();
            if !removed.is_empty() || !added.is_empty() {
                let mut note = String::from(
                    "<system-reminder>\nSkill availability just changed. This is \
                     authoritative — if you or the user said anything about these \
                     skills' availability earlier in this conversation, that is now \
                     outdated:\n",
                );
                for name in &removed {
                    note.push_str(&format!(
                        "- \"{name}\" was just removed — it no longer exists. Do not \
                         call it, and if asked whether it's available, say no.\n"
                    ));
                }
                for name in &added {
                    note.push_str(&format!(
                        "- \"{name}\" was just added — it's now available.\n"
                    ));
                }
                note.push_str("</system-reminder>");
                self.session.push_message(ModelMessage {
                    role: MessageRole::User,
                    content: vec![ModelContentBlock::Text { text: note }],
                });
                // The host cannot read the conversation, and its slash-command
                // view resolves through the catalog that just changed.
                self.emit(AgentEvent::SkillsChanged {
                    added: added.iter().map(|n| (*n).clone()).collect(),
                    removed: removed.iter().map(|n| (*n).clone()).collect(),
                    turn_id: self.current_turn_id.clone(),
                });
            }
        }
    }

    async fn refresh_frozen_context(&mut self) {
        let collected_this_turn = self.prompt.frozen().is_none();
        let engine_git = engine_exec_from(
            &self.config,
            &self.exec,
            &self.session.session_id,
            self.settings.trust(),
        );
        if collected_this_turn {
            self.prompt.set_frozen(
                crate::turn::collect_frozen(
                    &self.settings.paths,
                    self.policies.environment().as_ref(),
                    &engine_git,
                )
                .await,
            );
        }

        // `FrozenContext` is a snapshot on purpose, but the reminder re-shows
        // `gitStatus` every turn and the model reads a stale one as fact.
        // Refresh only that; skipped on the turn that just collected.
        if !collected_this_turn {
            if let Some(frozen) = self.prompt.frozen_mut() {
                frozen.refresh_git_status(&engine_git).await;
            }
        }
    }

    fn push_system_reminder(&mut self) {
        if let Some(frozen) = self.prompt.frozen() {
            let mut ctx = base::interface::scene::ReminderContext::new(
                self.settings
                    .paths
                    .working_dir()
                    .to_string_lossy()
                    .into_owned(),
            );
            ctx.git_status = frozen.git_status.as_deref().map(std::borrow::Cow::Borrowed);
            ctx.memory_summary = frozen
                .memory_index
                .as_deref()
                .filter(|m| self.settings.values.memory_enabled && !m.is_empty())
                .map(std::borrow::Cow::Borrowed);
            let reminder = self.scene.reminder(&ctx);
            if !reminder.is_empty() {
                self.session.push_message(ModelMessage {
                    role: MessageRole::User,
                    content: vec![ModelContentBlock::Text { text: reminder }],
                });
            }
        }
    }

    pub(super) fn start_skill_discovery(&self) -> Option<SkillDiscovery> {
        let skills = self.tooling.skills().clone();
        let local_dir = self.settings.paths.local_data_dir.clone();
        let invoked_skills: Vec<String> = self.session_state.ledger().invoked_skills.clone();
        // Only a previous turn with tool calls can have written new skills;
        // `last_had_tool_uses` starts `true` so the first turn scans too.
        let should_run = self.last_had_tool_uses;
        if should_run {
            Some(tokio_util::task::AbortOnDropHandle::new(
                tokio::task::spawn_blocking(move || {
                    let discovered = skills.discover_for_paths(&[local_dir]);
                    let new_names: Vec<String> = discovered
                        .iter()
                        .filter(|s| !invoked_skills.contains(&s.name))
                        .map(|s| s.name.clone())
                        .collect();
                    (discovered, new_names)
                }),
            ))
        } else {
            None
        }
    }

    pub(super) async fn push_user_message(
        &mut self,
        mut content: String,
        attachments: &[crate::agent::Attachment],
        origin: history::entry::MessageOrigin,
    ) {
        // Images reach the conversation only as attachment blocks. The
        // `@server:scheme://path` matcher is deliberately narrow (`@` is also
        // emails, decorators, npm scopes); an unresolvable reference becomes
        // a visible error block, never a failed turn.
        let mut spans = vec![base::interface::model::InputSpan {
            source: base::interface::model::input_source::USER_PROMPT.into(),
            block: 0,
            range: Some((0, content.len())),
        }];
        if let Some(resources) = self.tooling.mcp().resolve_resource_refs(&content).await {
            let from = content.len();
            content.push_str(&resources);
            spans.push(base::interface::model::InputSpan {
                source: base::interface::model::input_source::MCP_RESOURCE.into(),
                block: 0,
                range: Some((from, content.len())),
            });
        }

        let mut user_blocks = vec![ModelContentBlock::Text { text: content }];
        let files = base::paths::PromptFiles::for_project(
            &base::paths::ConfigPaths::from_settings(&self.settings.paths),
        );
        user_blocks.extend(resolve_attachments(attachments, &files).await);
        for block in 1..user_blocks.len() {
            spans.push(base::interface::model::InputSpan {
                source: base::interface::model::input_source::ATTACHMENT.into(),
                block,
                range: None,
            });
        }
        let message = ModelMessage {
            role: MessageRole::User,
            content: user_blocks,
        };
        self.deliver_notices();
        self.turn.pending_input = user_text_fingerprint(&message).map(|fp| (fp, spans));
        self.turn.started_at = Some(std::time::Instant::now());
        self.session.push_message_from(message, origin);
    }

    pub(super) fn announce_output_target(&mut self) {
        let unannounced = self.session_state.ledger().token_target.take_unannounced();
        if let Some(target) = unannounced {
            self.session.push_message(ModelMessage {
                role: MessageRole::User,
                content: vec![ModelContentBlock::Text {
                    text: format!(
                        "<system-reminder>\nThe user set an output token target of {target}. \
                         Your output token count is shown as you go and work continues \
                         automatically until the target is met. The target is a hard \
                         ceiling; let its size set how thorough to be.\n</system-reminder>"
                    ),
                }],
            });
        }
    }
}
