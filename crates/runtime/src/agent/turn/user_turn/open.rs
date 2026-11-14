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
        // A script's budget is per turn, and this is where a turn begins.
        // Without this the counter only ever climbs, so the quota is a
        // per-session one wearing a per-turn name — and a script bound to a
        // point that fires per tool call goes quiet in the middle of a long
        // session with nothing to say why.
        self.gate.run_turn_starts();

        // UserPromptSubmit: where a hook sees the message that opens a turn,
        // before any turn setup (skills rescan, FrozenContext,
        // CLAUDE.md injection, ...) has touched anything.
        //
        // Reuses the same `HookResponse` fields `PreToolUse`/`PostToolUse`
        // already use, same soft-fail philosophy (a hook that errors/times
        // out is treated as "ran and said nothing" — `blocked()`/
        // `updated_input()` just find nothing, turn proceeds unchanged; see
        // `HookRunner::run`, not special-cased per event):
        // - `decision: "block"` refuses to process this message at all —
        //   the turn ends immediately with `stopped_by_hook`, before a
        //   single model call, exactly like a `PreToolUse` block ends a
        //   tool call before it runs.
        // - `updated_input` (a JSON string) rewrites the message content
        //   that actually gets processed — the same field name
        //   `PreToolUse` uses for rewriting tool input, repurposed here for
        //   the one thing there is to rewrite: the prompt text itself.
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

        // TurnStart — fires after UserPromptSubmit has had its chance to block
        // or rewrite, so a hook here sees the message that will actually be
        // processed. Notification only: the response is discarded.
        self.fire_turn_hook(hooks::HookEvent::TurnStart, cancel, |input| {
            input.with_turn_id(self.current_turn_id.clone())
        })
        .await;

        // Same turn_no every `turn_complete` emitted below this point uses
        // (`self.session.turn_count` isn't incremented until the very end of
        // this function, see `increment_turn()`).
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

        // A skill's `allowed_tools` (see `SkillTool::call`) pre-approves
        // tools only for the rest of the turn it was invoked in — clear
        // whatever the *previous* turn granted before this new user message
        // starts a fresh one.
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
        // The catalog may be shared with other sessions and swapped in place
        // by whoever watches the skill directories, so the diff is against
        // what this session was last shown, not against the catalog as it
        // stood a moment ago.
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
            // Surface the diff as a system-reminder in the conversation itself,
            // not just a rebuilt static "## Available Skills" list — a model
            // mid-session tends to trust what it already said over a change
            // buried in a large prompt block it re-reads every turn without
            // necessarily re-attending to (observed directly: deepseek-v4-flash
            // kept asserting a just-deleted skill was still available even
            // with a "this list supersedes what you said earlier" disclaimer
            // right above the list; a pointed system-reminder about exactly
            // what changed is a much stronger, harder-to-ignore signal, the
            // same pattern already used for git-status/memory reminders).
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
                // The model is told above; the host is told here. Both need
                // it for the same reason — a skill list they were shown
                // earlier is now wrong — but a host cannot read the
                // conversation, and its slash-command view resolves through
                // the catalog that just changed underneath it.
                self.emit(AgentEvent::SkillsChanged {
                    added: added.iter().map(|n| (*n).clone()).collect(),
                    removed: removed.iter().map(|n| (*n).clone()).collect(),
                    turn_id: self.current_turn_id.clone(),
                });
            }
        }
    }

    async fn refresh_frozen_context(&mut self) {
        // Compute frozen context lazily on first turn. Includes git status,
        // branch, platform, etc.
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

        // `FrozenContext` is a session snapshot on purpose — but the
        // reminder below re-injects `gitStatus` on *every* turn, and a stale
        // status is read by the model as current fact, not as a stale hint.
        // Commit between two turns and it keeps citing files that are no
        // longer dirty. So refresh precisely the fields that get re-shown
        // (see `refresh_git_status`); everything else stays frozen, which is
        // the point of the type. Skipped on the turn that just collected —
        // that snapshot is seconds old.
        if !collected_this_turn {
            if let Some(frozen) = self.prompt.frozen_mut() {
                frozen.refresh_git_status(&engine_git).await;
            }
        }
    }

    fn push_system_reminder(&mut self) {
        // Inject system-reminder: git status + memory summary, via the scene's
        // `reminders` — each scene decides what belongs here.
        // Called once per
        // turn, and the git slice it reads was refreshed just above, so the
        // status shown is this turn's, not the session's opening snapshot.
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
        // Skill discovery prefetch: scan workspace for matching skills as a
        // background task. Discovery runs while the model streams and tools
        // execute; the result is consumed post-tool-execution alongside the
        // memory prefetch.
        let skills = self.tooling.skills().clone();
        let local_dir = self.settings.paths.local_data_dir.clone();
        let invoked_skills: Vec<String> = self.session_state.ledger().invoked_skills.clone();
        // Only scan when the previous turn produced tool calls that may
        // have written files that could reveal new skills — skip on
        // non-write turns. Runs on the first turn too (`last_had_tool_uses`
        // starts `true`).
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
        // Push user message. Recalled memories are injected right behind it,
        // below, so they are part of *this* turn's request.
        //
        // Attachments become additional blocks in this same message, after the
        // text, so the model reads the request and then what it refers to.
        // Images are only reachable this way — there is no other path by which
        // a user-supplied image enters the conversation.
        // `@server:scheme://path` references → MCP `resources/read`, inlined
        // into this same message so the model reads the request and the thing
        // it points at together. The matcher is deliberately narrow (see
        // `mcp::resources` for why: `@` is also emails, decorators, npm
        // scopes), and returns `None` — no work, no allocation — for the
        // overwhelmingly common message that contains no reference, or for
        // any session with no MCP servers connected at all.
        //
        // A reference that can't be resolved comes back as a visible error
        // block, never an error that fails the turn.
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
