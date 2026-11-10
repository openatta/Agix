//! 用户回合的主循环 run_user_turn_inner：一次用户输入到停止原因的全部轮次。

use super::attachments::{resolve_attachments, truncate_at_char_boundary};
use super::memory_extraction::extract_memories_after_turn;
use super::process::{hook_permission_mode, PromptScreen};
use super::prompt::user_text_fingerprint;
use super::request::{pair_dangling_tool_uses, sync_mcp_adapters};
use super::round::count_structured_output_calls;
use super::tool_env::engine_git_from;
use super::tool_exec::{execute_tool_with_telemetry, ToolExecCtx};
use super::tool_result::{attach_tool_result_images, PendingToolImage};
use super::{TurnError, TurnOutcome};
use crate::agent::Agent;
use base::interface::event::AgentEvent;
use base::interface::model::{MessageRole, ModelContentBlock, ModelMessage, Usage};
use base::interface::prompt_middleware::PromptDelivery;
use std::collections::HashSet;
use std::sync::{Arc, LazyLock};
use tokio_util::sync::CancellationToken;

/// Empty hashset fallback when frozen context is not yet available.
static EMPTY: LazyLock<HashSet<String>> = LazyLock::new(HashSet::new);

impl Agent {
    pub(super) async fn run_user_turn_inner(
        &mut self,
        mut content: String,
        attachments: Vec<crate::agent::Attachment>,
        cancel: CancellationToken,
        origin: history::entry::MessageOrigin,
    ) -> Result<TurnOutcome, TurnError> {
        // A script's budget is per turn, and this is where a turn begins.
        // Without this the counter only ever climbs, so the quota is a
        // per-session one wearing a per-turn name — and a script bound to a
        // point that fires per tool call goes quiet in the middle of a long
        // session with nothing to say why.
        for turn_start in self.gate.turn_starts.iter() {
            turn_start();
        }

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
            .screen_user_prompt(content, &attachments, PromptDelivery::Turn)
            .await
        {
            PromptScreen::Pass(screened) => content = screened,
            PromptScreen::Blocked(message) => {
                self.record_turn_complete("stopped_by_hook", 0, 0, std::time::Instant::now());
                return Ok(TurnOutcome {
                    message,
                    ..TurnOutcome::stopped("stopped_by_hook")
                });
            }
        }

        // TurnStart — fires after UserPromptSubmit has had its chance to block
        // or rewrite, so a hook here sees the message that will actually be
        // processed. Notification only: the response is discarded.
        self.fire_lifecycle_hook(hooks::HookEvent::TurnStart, |input| {
            input.with_turn_id(self.current_turn_id.clone())
        })
        .await;

        // Same turn_no every `turn_complete` emitted below this point uses
        // (`self.session.turn_count` isn't incremented until the very end of
        // this function, see `increment_turn()`).
        let _ = self
            .telemetry
            .telemetry_handle
            .record(telemetry::TelemetryEvent::turn_start(
                &self.session.session_id,
                self.session.turn_count,
                Some(self.current_turn_id.clone()),
                telemetry::TurnStartPayload {
                    resumed: false,
                    is_retry: false,
                },
            ));
        let _ =
            self.telemetry
                .telemetry_handle
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
        // whatever the *previous* turn injected before this new user
        // message starts a fresh one. No-op if nothing was injected, or on
        // `Permission` implementations without a backing rule engine.
        self.gate.permission.clear_temporary_allows();
        for (tool, pattern) in std::mem::take(&mut self.slash_skill_grants) {
            self.gate
                .permission
                .add_temporary_allow(&tool, pattern.as_deref());
        }

        let _timer = self.telemetry.perf.start_timer("turn", "total");
        let mut api_calls: u32 = 0;
        let mut tool_calls: u32 = 0;
        let mut structured_output_calls: u32 = 0;

        let mut max_tokens_recovery: u32 = 0;
        let mut effective_max_tokens = self.settings.model.max_tokens;
        let mut effective_model = self.settings.model.model_name.clone();
        // What the turn has spent, kept once. The budget reads it through
        // `Spend` and a caller reads it whole; a second accumulator beside it
        // would let the two disagree about the cache.
        let mut turn_usage = Usage::default();
        let start = std::time::Instant::now();

        // The catalog may be shared with other sessions and swapped in place
        // by whoever watches the skill directories, so the diff is against
        // what this session was last shown, not against the catalog as it
        // stood a moment ago.
        let changed = self.tooling.skills.reload_changed();
        if changed > 0 {
            tracing::debug!(count = changed, "Skills reloaded from file changes");
        }
        let skills_now: std::collections::HashSet<String> = self
            .tooling
            .skills
            .list()
            .iter()
            .map(|s| s.name.clone())
            .collect();
        if let Some(skills_before) = self.prompt.announced_skills.replace(skills_now.clone()) {
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
                let _ = self.channels.event_tx.send(AgentEvent::SkillsChanged {
                    added: added.iter().map(|n| (*n).clone()).collect(),
                    removed: removed.iter().map(|n| (*n).clone()).collect(),
                    turn_id: self.current_turn_id.clone(),
                });
            }
        }

        // Compute frozen context lazily on first turn. Includes git status,
        // branch, platform, etc.
        let collected_this_turn = self.prompt.frozen.is_none();
        let engine_git = engine_git_from(&self.config, &self.exec, &self.session.session_id);
        if collected_this_turn {
            let cwd = self.settings.paths.project_root();
            let paths = base::paths::ConfigPaths::from_settings(&self.settings.paths);
            self.prompt.frozen = Some(
                base::frozen::FrozenContext::collect(
                    cwd,
                    &paths,
                    &*self.policies.environment,
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
            if let Some(ref mut frozen) = self.prompt.frozen {
                frozen.refresh_git_status(&engine_git).await;
            }
        }

        if !self.prompt.session_context_injected {
            self.inject_session_context().await;
        }

        // Inject system-reminder: git status + memory summary, via the scene's
        // own `build_system_reminder` — each scene decides what belongs here.
        // Called once per
        // turn, and the git slice it reads was refreshed just above, so the
        // status shown is this turn's, not the session's opening snapshot.
        if let Some(ref frozen) = self.prompt.frozen {
            let ctx = base::interface::scene::ReminderContext {
                cwd: std::borrow::Cow::Owned(
                    self.settings
                        .paths
                        .project_root()
                        .to_string_lossy()
                        .into_owned(),
                ),
                git_status: frozen.git_status.as_deref().map(std::borrow::Cow::Borrowed),
                memory_summary: frozen
                    .memory_index
                    .as_deref()
                    .filter(|m| self.settings.memory_enabled && !m.is_empty())
                    .map(std::borrow::Cow::Borrowed),
            };
            let reminder = self.scene.build_system_reminder(&ctx);
            if !reminder.is_empty() {
                self.session.push_message(ModelMessage {
                    role: MessageRole::User,
                    content: vec![ModelContentBlock::Text { text: reminder }],
                });
            }
        }

        // Memory prefetch: fire LLM-based relevant memory selection as a
        // background task. The handle aborts it when dropped, so a recall that
        // misses its budget stops spending on the selection call.
        type Recall = (Vec<String>, Vec<base::interface::memory::DurableMemory>);
        let mut prefetch_handle: Option<tokio_util::task::AbortOnDropHandle<Recall>> =
            if !self.settings.memory_enabled {
                None
            } else {
                let store = self.memory.memory_store.clone();
                let model = self.llm.model.clone();
                let query = content.clone();
                let already_surfaced: HashSet<String> = self
                    .prompt
                    .frozen
                    .as_ref()
                    .map(|f| f.already_surfaced.clone())
                    .unwrap_or_default();
                let recent_tools = self.tooling.tools.names();
                let model_name = self.settings.model.model_name.clone();
                let session_id = self.session.session_id.clone();
                let retriever = Arc::clone(&self.memory.memory_retriever);
                let hooks = Arc::clone(&self.memory.retrieval_hooks);
                Some(tokio_util::task::AbortOnDropHandle::new(tokio::spawn(
                    async move {
                        let memories = tokio::task::spawn_blocking(move || store.load_all())
                            .await
                            .unwrap_or_default();
                        // The retriever reads what was just loaded rather than
                        // the store, whose reads block.
                        let snapshot = base::interface::memory_contracts::InMemoryStorage::new(
                            memories.clone(),
                        );
                        let names = base::interface::memory_contracts::retrieve_with_hooks(
                            retriever.as_ref(),
                            &hooks,
                            &snapshot,
                            model.as_ref(),
                            base::interface::memory_contracts::RetrievalRequest {
                                query,
                                limit: 5,
                                already_surfaced,
                                recent_tools,
                                model_name,
                                session_id: Some(session_id),
                            },
                        )
                        .await;
                        (names, memories)
                    },
                )))
            };
        let prefetch_started_at: std::time::Instant = std::time::Instant::now();

        // Skill discovery prefetch: scan workspace for matching skills as a
        // background task. Discovery runs while the model streams and tools
        // execute; the result is consumed post-tool-execution alongside the
        // memory prefetch.
        let mut skill_prefetch: Option<
            tokio_util::task::AbortOnDropHandle<(Vec<base::frozen::SkillEntry>, Vec<String>)>,
        > = {
            let skills = self.tooling.skills.clone();
            let local_dir = self.settings.paths.local_data_dir.clone();
            let invoked_skills: Vec<String> = self.prompt.invoked_skills.clone();
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
        };

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
        if let Some(resources) = self.tooling.mcp.resolve_resource_refs(&content).await {
            let from = content.len();
            content.push_str(&resources);
            spans.push(base::interface::model::InputSpan {
                source: base::interface::model::input_source::MCP_RESOURCE.into(),
                block: 0,
                range: Some((from, content.len())),
            });
        }

        let mut user_blocks = vec![ModelContentBlock::Text { text: content }];
        user_blocks.extend(resolve_attachments(&attachments).await);
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
        if std::mem::take(&mut self.token_budget.unannounced) {
            if let Some(target) = self.token_budget.target {
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

        // Collected before the first API call, not after tool execution, so a
        // turn the model answers without tools still sees its memories. The
        // wait is dead air before the user's first token, hence the short
        // latency budget. In the common case there is no wait at all —
        // `select_memories_from` returns without a model call when the
        // store holds no more candidates than the requested maximum.
        //
        // `already_surfaced` / `recall_count` are updated here and only here,
        // at the point the memories are actually placed in the transcript —
        // recording a memory as "shown to the model" when it was not
        // permanently suppresses a legitimate future surfacing of it.
        const MEMORY_RECALL_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);
        /// Per-entry cap on inlined memory content. Recall carries at most 5
        /// entries, so the whole injection stays bounded; the cap only exists
        /// so one oversized memory cannot dominate the turn.
        const MEMORY_RECALL_CONTENT_CAP: usize = 1024;
        if let Some(prefetch) = prefetch_handle.take() {
            let (prefetch_names, all) =
                match tokio::time::timeout(MEMORY_RECALL_BUDGET, prefetch).await {
                    Ok(Ok(recall)) => {
                        tracing::debug!(
                            count = recall.0.len(),
                            latency_ms = prefetch_started_at.elapsed().as_millis(),
                            "memory recall completed"
                        );
                        recall
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(error = %e, "memory recall task failed");
                        Default::default()
                    }
                    Err(_) => {
                        tracing::warn!(
                            budget_ms = MEMORY_RECALL_BUDGET.as_millis(),
                            "memory recall exceeded its latency budget"
                        );
                        Default::default()
                    }
                };
            let relevant: Vec<base::interface::memory::DurableMemory> = {
                let surfaced: &std::collections::HashSet<String> = self
                    .prompt
                    .frozen
                    .as_ref()
                    .map(|f| &f.already_surfaced)
                    .unwrap_or(&EMPTY);
                prefetch_names
                    .iter()
                    .filter_map(|name| all.iter().find(|m| &m.name == name).cloned())
                    .filter(|m| !surfaced.contains(&m.name))
                    .take(5)
                    .collect()
            };
            if !relevant.is_empty() {
                // Mark injected memories as surfaced to avoid repeated injection.
                if let Some(ref mut frozen) = self.prompt.frozen {
                    for m in &relevant {
                        frozen.already_surfaced.insert(m.name.clone());
                    }
                }
                {
                    let store = self.memory.memory_store.clone();
                    let names: Vec<String> = relevant.iter().map(|m| m.name.clone()).collect();
                    tokio::task::spawn_blocking(move || {
                        if let Err(e) = store.record_recalled(&names) {
                            tracing::debug!(error = %e, "recording a memory recall failed");
                        }
                    });
                }

                // Carry the memory body, not just its one-line description:
                // the description exists to decide relevance, and a model that
                // only gets the description has to spend a Read round trip to
                // learn anything. At ≤5 entries this is the cheaper option.
                let mut mem_text =
                    String::from("<system-reminder>\nRelevant memories for this query:\n");
                for m in &relevant {
                    mem_text.push_str(&format!("\n## {}\n{}\n", m.name, m.description));
                    let body = m.content.trim();
                    if !body.is_empty() {
                        let shown = truncate_at_char_boundary(body, MEMORY_RECALL_CONTENT_CAP);
                        mem_text.push_str(shown);
                        if shown.len() < body.len() {
                            mem_text.push_str(&format!(
                                "\n… (truncated — read {}.md for the full memory)",
                                m.name
                            ));
                        }
                        mem_text.push('\n');
                    }
                }
                mem_text
                    .push_str("\nUse these memories to inform your response.\n</system-reminder>");
                self.session.push_message(ModelMessage {
                    role: MessageRole::User,
                    content: vec![ModelContentBlock::Text { text: mem_text }],
                });
            }
        }

        let mut had_tool_uses_this_turn = false;
        let mut sent_back_by_stop_hook = false;

        loop {
            // Every step starts from a transcript that is on disk, so a
            // process that dies mid-turn loses at most the step in flight.
            // Here every tool_use already has its tool_result next to it.
            if let Err(e) = self.session.persist().await {
                tracing::warn!(error = %e, "failed to persist session");
            }
            self.record_goal_changes().await;
            if cancel.is_cancelled() {
                self.last_had_tool_uses = had_tool_uses_this_turn;
                self.record_turn_complete("cancelled", api_calls, tool_calls, start);
                return Ok(TurnOutcome::ended_before_reply(
                    "cancelled",
                    api_calls,
                    tool_calls,
                    turn_usage,
                ));
            }
            if let base::interface::turn_policy::TurnStep::Stop { reason } = self
                .policies
                .turn_policy
                .before_model_call(&base::interface::turn_policy::TurnProgress {
                    api_calls,
                    tool_calls,
                    structured_output_calls,
                    stop_reason: "",
                })
            {
                self.last_had_tool_uses = had_tool_uses_this_turn;
                self.record_turn_complete(&reason, api_calls, tool_calls, start);
                return Ok(TurnOutcome::ended_before_reply(
                    reason, api_calls, tool_calls, turn_usage,
                ));
            }

            self.compact_if_needed(&cancel).await;
            if cancel.is_cancelled() {
                continue;
            }

            // A ceiling compaction was unable to get under. Ending the turn is
            // the only honest answer left: the request that would go out next
            // is one the deployment said must never be sent.
            if let Some(cap) = self.context_budget().hard_cap {
                let carried = self.context_tokens();
                if carried > cap {
                    tracing::warn!(carried, cap, "context above the hard cap after compaction");
                    self.last_had_tool_uses = had_tool_uses_this_turn;
                    self.record_turn_complete("context_exceeded", api_calls, tool_calls, start);
                    return Ok(TurnOutcome::ended_before_reply(
                        "context_exceeded",
                        api_calls,
                        tool_calls,
                        turn_usage,
                    ));
                }
            }

            // 2. Assemble the request
            let step = api_calls;
            api_calls += 1;
            let _ = self.channels.event_tx.send(AgentEvent::StepStart {
                step,
                turn_id: self.current_turn_id.clone(),
            });
            let origin = Some(
                base::interface::model::CallOrigin::turn(
                    self.session.session_id.clone(),
                    self.session.turn_count,
                    step,
                )
                .with_lineage(
                    self.session.parent_session_id().map(str::to_string),
                    self.identity.agent_type.clone(),
                ),
            );
            let request = self
                .prepare_request(&effective_model, effective_max_tokens, origin.clone())
                .await;
            // Kept back for the recovery paths, which retry the same tools and
            // (for an overload) the same conversation.
            let tool_defs = request.tool_defs.clone();
            let messages = request.messages.clone();
            // Counted before either is handed on: the call's own accounting
            // needs the size of what went out, and both get moved on the way.
            let sent_message_count = messages.len();
            let sent_tool_count = tool_defs.len();

            // 3. Call the model and 5. stream its reply, running tools as they
            //    arrive. 4. A call that fails before anything of it was kept
            //    goes to the recovery policy, once; the doing stays here,
            //    because compacting, rebuilding a request and sending it are
            //    things only this loop can sequence.
            let call_started = std::time::Instant::now();
            let tools = Arc::clone(&self.tooling.tools);
            let tools_for_safety = Arc::clone(&self.tooling.tools);
            let cwd = self.settings.paths.project_root();
            let local_settings_path = self.settings.paths.local_settings_file();
            let session_id = self.session.session_id.clone();
            let turn_no = self.session.turn_count;
            let th = self.telemetry.telemetry_handle.clone();
            let tid = self.current_turn_id.clone();
            let cancel_for_exec = cancel.clone();
            let agent_depth_for_exec = self.identity.agent_depth;
            let hooks_for_exec = Arc::clone(&self.gate.hooks);
            let discontinued = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let discontinued_for_exec = Arc::clone(&discontinued);
            let events_for_exec = self.channels.event_tx.clone();
            let config_for_exec = Arc::clone(&self.config);
            let exec_providers = self.exec.clone();
            let permission_for_exec = Arc::clone(&self.gate.permission);
            let session_state_for_exec = Arc::clone(&self.session_state);
            let elicitation_for_exec = Arc::clone(&self.gate.elicitation);
            let tool_middleware_for_exec = Arc::clone(&self.tooling.tool_middleware);
            let result_transformers_for_exec = Arc::clone(&self.tooling.result_transformers);
            let tool_images: Arc<std::sync::Mutex<Vec<PendingToolImage>>> =
                Arc::new(std::sync::Mutex::new(Vec::new()));
            let tool_images_for_exec = Arc::clone(&tool_images);
            let execute_tool =
                move |tool_use_id: String, name: String, input: serde_json::Value| {
                    let exec_ctx = ToolExecCtx {
                        tool_use_id,
                        tools: Arc::clone(&tools),
                        cwd: cwd.clone(),
                        local_settings_path: local_settings_path.clone(),
                        session_id: session_id.clone(),
                        turn_no,
                        agent_depth: agent_depth_for_exec,
                        telemetry_handle: th.clone(),
                        turn_id: tid.clone(),
                        cancel: cancel_for_exec.clone(),
                        hooks: Arc::clone(&hooks_for_exec),
                        config: Arc::clone(&config_for_exec),
                        exec: exec_providers.clone(),
                        permission: Arc::clone(&permission_for_exec),
                        session_state: Arc::clone(&session_state_for_exec),
                        elicitation: Arc::clone(&elicitation_for_exec),
                        tool_middleware: Arc::clone(&tool_middleware_for_exec),
                        result_transformers: Arc::clone(&result_transformers_for_exec),
                        discontinued: Arc::clone(&discontinued_for_exec),
                        images: Arc::clone(&tool_images_for_exec),
                        from_code: false,
                        events: Some(events_for_exec.clone()),
                    };
                    async move { execute_tool_with_telemetry(&exec_ctx, &name, input).await }
                };
            let is_concurrency_safe = move |name: &str, input: &serde_json::Value| {
                tools_for_safety
                    .get(name)
                    .map(|t| t.is_concurrency_safe(input))
                    .unwrap_or(false)
            };
            let mut request = request;
            let mut recovered = false;
            let (stream_result, messages_before_step) = loop {
                let messages_before_step = self.session.messages.len();
                let attempt = match self.send(request, cancel.clone()).await {
                    Ok(stream) => {
                        crate::streaming::execute_stream(
                            stream,
                            &mut self.session,
                            &self.channels.event_tx,
                            self.current_turn_id.clone(),
                            execute_tool.clone(),
                            is_concurrency_safe.clone(),
                            cancel.clone(),
                            &self.llm.model_interceptors,
                            self.llm.model.provenance(),
                        )
                        .await
                    }
                    Err(error) => Err(crate::streaming::StreamFailure::refused(error)),
                };
                let failure = match attempt {
                    Ok(result) => break (result, messages_before_step),
                    Err(failure) => failure,
                };
                self.record_api_error(&effective_model, &failure.error, call_started);
                if cancel.is_cancelled() {
                    break (
                        crate::streaming::StreamResult::cancelled(),
                        messages_before_step,
                    );
                }
                let recovery = if recovered || !failure.resendable {
                    base::interface::recovery_policy::Recovery::Fail
                } else {
                    self.classify_failure(&failure.error)
                };
                recovered = true;
                request = match recovery {
                    base::interface::recovery_policy::Recovery::Fail => {
                        return Err(TurnError::Model(format!(
                            "failed to stream model response: {}",
                            failure.error
                        )))
                    }
                    base::interface::recovery_policy::Recovery::RetryWith { model } => {
                        tracing::warn!(
                            from = %effective_model,
                            to = %model,
                            error = %failure.error,
                            "retrying against a different model"
                        );
                        effective_model = model;
                        // `fallback_model: None` — this retry *is* the
                        // fallback, so there is nothing further to fall back to.
                        self.prepare_retry(
                            &effective_model,
                            effective_max_tokens,
                            tool_defs.clone(),
                            messages.clone(),
                            None,
                            origin.clone(),
                        )
                        .await
                    }
                    base::interface::recovery_policy::Recovery::CompactAndRetry => {
                        self.compact_for_retry(&failure.error, &cancel).await?;
                        if cancel.is_cancelled() {
                            break (
                                crate::streaming::StreamResult::cancelled(),
                                messages_before_step,
                            );
                        }
                        // The retry keeps skills and MCP instructions: they
                        // cost a few KB, the history is what blew the budget,
                        // and the compaction freed far more.
                        self.prepare_retry(
                            &effective_model,
                            effective_max_tokens,
                            tool_defs.clone(),
                            pair_dangling_tool_uses(self.session.messages().to_vec()),
                            self.settings.model.fallback_model.clone(),
                            origin.clone(),
                        )
                        .await
                    }
                };
                let _ = self.channels.event_tx.send(AgentEvent::StepStart {
                    step,
                    turn_id: self.current_turn_id.clone(),
                });
            };

            // Any images the round's tools returned ride out-of-band (see
            // `ToolExecCtx::images`) and get appended to the tool-result message
            // `execute_stream` just pushed — after every `tool_result` block, as
            // the API requires.
            let pending_images =
                std::mem::take(&mut *tool_images.lock().unwrap_or_else(|e| e.into_inner()));
            if !pending_images.is_empty() {
                attach_tool_result_images(&mut self.session.messages, pending_images);
            }

            let has_tool_uses = stream_result.has_tool_uses;
            had_tool_uses_this_turn = had_tool_uses_this_turn || has_tool_uses;
            tool_calls += stream_result.tool_calls;
            let stop_reason = stream_result.stop_reason;
            let usage = stream_result.usage;
            self.turn.last_call_usage = Some(usage);
            self.turn.truncated |= stream_result.truncated;
            if let Err(e) = self.session.persist().await {
                tracing::warn!(error = %e, "failed to persist session");
            }
            let _ = self.channels.event_tx.send(AgentEvent::StepEnd {
                step,
                stop_reason: stop_reason.clone(),
                usage,
                tool_calls: stream_result.tool_calls,
                turn_id: self.current_turn_id.clone(),
                seq: self.session.messages.len() as u64,
            });

            // One call, accounted for. Every other number the engine reports is
            // per turn, and a turn is several calls — so this is the event a
            // cost can be computed from, and the only one that says which model
            // the spending went to when a turn switched models partway.
            let _ = self
                .telemetry
                .telemetry_handle
                .record(telemetry::TelemetryEvent::api_request(
                    &self.session.session_id,
                    self.session.turn_count,
                    Some(self.current_turn_id.clone()),
                    telemetry::ApiRequestPayload {
                        model: effective_model.clone(),
                        input_tokens: usage.input_tokens as u64,
                        output_tokens: usage.output_tokens as u64,
                        cache_creation_tokens: usage.cache_creation_input_tokens as u64,
                        cache_read_tokens: usage.cache_read_input_tokens as u64,
                        latency_ms: call_started.elapsed().as_millis() as u64,
                        stop_reason: stop_reason.clone(),
                        input_message_count: sent_message_count,
                        tool_count: sent_tool_count,
                        default_model: effective_model == self.settings.model.model_name,
                        // A turn's own call. The auxiliary ones are labelled
                        // by `telemetry::model::TelemetryModel`, which is
                        // where they are visible at all.
                        purpose: None,
                        truncated: stream_result.truncated,
                    },
                ));
            // Added here rather than beside the budget's own counter further
            // down: a turn can end between the two, and a call that happened
            // has to be counted whether or not the turn went on to check a
            // budget.
            turn_usage = turn_usage.plus(usage);
            if stream_result.cancelled {
                continue;
            }

            // `PostSampling`: one model response has been fully streamed and
            // is in the transcript. Purely observational: like the other lifecycle hooks, the
            // response is not modifiable from here (there is no defined
            // recovery path for a veto at this point).
            {
                let sr = stop_reason.clone();
                let out_tokens = usage.output_tokens;
                self.fire_lifecycle_hook(hooks::HookEvent::PostSampling, |i| {
                    i.with_reason(format!(
                        "model response complete (stop_reason={sr}, output_tokens={out_tokens})"
                    ))
                })
                .await;
            }

            // A git-mutating Bash call this round means the session-start
            // `FrozenContext` git snapshot (branch/status/log) is now stale
            // — refresh just that slice.
            if has_tool_uses && Self::included_git_mutating_bash_call(self.this_step()) {
                if let Some(ref mut frozen) = self.prompt.frozen {
                    frozen
                        .refresh_git(&engine_git_from(
                            &self.config,
                            &self.exec,
                            &self.session.session_id,
                        ))
                        .await;
                }
            }
            if has_tool_uses {
                self.notice_repeated_calls();
            }

            // A PostToolUse hook can request the turn end right here — every
            // tool_use emitted in this round already has its tool_result in
            // the session (execute_stream writes the reply together with all
            // of its results once the round is over), so there's no risk of
            // a dangling tool_use block without a matching result. Mirrors the Stop hook's
            // discontinue path below, just triggered from a different point.
            if discontinued.load(std::sync::atomic::Ordering::Relaxed) {
                tracing::info!("PostToolUse hook discontinued the turn");
                self.record_turn_complete("stopped_by_hook", api_calls, tool_calls, start);
                self.last_had_tool_uses = had_tool_uses_this_turn;
                return Ok(TurnOutcome::ended(
                    "stopped_by_hook",
                    api_calls,
                    tool_calls,
                    usage,
                    turn_usage,
                    None,
                ));
            }

            // Structured output retry limit.
            structured_output_calls += self
                .session
                .messages
                .get(messages_before_step..)
                .map_or(0, count_structured_output_calls);
            if let base::interface::turn_policy::TurnStep::Stop { reason } = self
                .policies
                .turn_policy
                .after_model_call(&base::interface::turn_policy::TurnProgress {
                    api_calls,
                    tool_calls,
                    structured_output_calls,
                    stop_reason: &stop_reason,
                })
            {
                tracing::warn!(
                    structured_output_calls,
                    %reason,
                    "turn policy ended the turn"
                );
                self.last_had_tool_uses = had_tool_uses_this_turn;
                self.record_turn_complete(&reason, api_calls, tool_calls, start);
                return Ok(TurnOutcome::ended_before_reply(
                    reason, api_calls, tool_calls, turn_usage,
                ));
            }

            // Cumulative token budget, checked against the real usage the
            // provider reported for this call — no per-model price table,
            // no estimate. At 90% inject a warning; at 100% abort.
            //
            // The engine hands over all four figures and does not decide which
            // of them count as spent; that is the policy's judgement, and the
            // telemetry below records the reading the engine's own policy uses.
            {
                let spend = base::interface::budget_policy::Spend::from(turn_usage);
                let total_tokens_used = spend.total_tokens();
                match self.policies.budget_policy.on_usage(&spend) {
                    base::interface::budget_policy::Spending::WithinBudget => {}
                    base::interface::budget_policy::Spending::Warn { reminder, limit } => {
                        tracing::warn!(
                            total_tokens_used,
                            budget = limit,
                            "approaching token budget limit; injecting continue reminder"
                        );
                        let _ = self.telemetry.telemetry_handle.record(
                            telemetry::TelemetryEvent::budget_enforced(
                                &self.session.session_id,
                                self.session.turn_count,
                                Some(self.current_turn_id.clone()),
                                telemetry::BudgetEnforcedPayload {
                                    action: telemetry::BudgetEnforcedAction::WarningInjected,
                                    total_tokens_used,
                                    budget: limit,
                                },
                            ),
                        );
                        self.session.push_message(ModelMessage {
                            role: MessageRole::User,
                            content: vec![ModelContentBlock::Text { text: reminder }],
                        });
                    }
                    base::interface::budget_policy::Spending::Exhausted { limit } => {
                        tracing::warn!(total_tokens_used, budget = limit, "token budget exceeded");
                        let _ = self.telemetry.telemetry_handle.record(
                            telemetry::TelemetryEvent::budget_enforced(
                                &self.session.session_id,
                                self.session.turn_count,
                                Some(self.current_turn_id.clone()),
                                telemetry::BudgetEnforcedPayload {
                                    action: telemetry::BudgetEnforcedAction::TurnStopped,
                                    total_tokens_used,
                                    budget: limit,
                                },
                            ),
                        );
                        self.last_had_tool_uses = had_tool_uses_this_turn;
                        self.record_turn_complete("budget_exceeded", api_calls, tool_calls, start);
                        return Ok(TurnOutcome::ended_before_reply(
                            "budget_exceeded",
                            api_calls,
                            tool_calls,
                            turn_usage,
                        ));
                    }
                }
            }

            // If tools were executed during streaming, continue to next API call.
            if has_tool_uses {
                // Collect async skill discovery prefetch (fired at turn start).
                if let Some(handle) = skill_prefetch.take() {
                    // Time-bounded wait: if discovery hasn't completed after
                    // tool execution, skip it this turn (it'll run next turn).
                    if let Ok(Ok((_discovered, new_names))) =
                        tokio::time::timeout(std::time::Duration::from_millis(500), handle).await
                    {
                        if !new_names.is_empty() {
                            let skills_text = format!(
                                "<system-reminder>\nSkills discovered in workspace: {}. Use /<skill-name> to invoke.\n</system-reminder>",
                                new_names.join(", ")
                            );
                            self.session.push_message(ModelMessage {
                                role: MessageRole::User,
                                content: vec![ModelContentBlock::Text { text: skills_text }],
                            });
                            for name in &new_names {
                                if !self.prompt.invoked_skills.contains(name) {
                                    self.prompt.invoked_skills.push(name.clone());
                                }
                            }
                        }
                    }
                }

                // Activate conditional skills whose `paths` patterns match
                // files accessed by Read/Write/Edit tool operations this turn.
                {
                    let file_paths = Self::extract_tool_file_paths(self.messages_this_turn());
                    if !file_paths.is_empty() {
                        let activated = self.tooling.skills.activate_for_paths(&file_paths);
                        if !activated.is_empty() {
                            let names: Vec<&str> =
                                activated.iter().map(|s| s.name.as_str()).collect();
                            let skills_text = format!(
                                "<system-reminder>\nConditional skills activated for \
                                 current context: {}. Use /<skill-name> to invoke.\n\
                                 </system-reminder>",
                                names.join(", "),
                            );
                            self.session.push_message(ModelMessage {
                                role: MessageRole::User,
                                content: vec![ModelContentBlock::Text { text: skills_text }],
                            });
                            for s in &activated {
                                if !self.prompt.invoked_skills.contains(&s.name) {
                                    self.prompt.invoked_skills.push(s.name.clone());
                                }
                            }
                        }
                    }
                }

                self.tooling.mcp.refresh_if_changed().await;
                sync_mcp_adapters(
                    self.tooling.tools.as_ref(),
                    self.tooling.mcp.as_ref(),
                    |n| self.tooling.tool_policy.permits(n),
                );
                if !cancel.is_cancelled() {
                    self.deliver_queued_user_messages(false).await;
                }
                continue;
            }

            // 6. A reply that stopped early: escalate the output limit, or
            //    send a paused one back to be continued.
            if self.handle_early_stop(
                &stop_reason,
                &mut max_tokens_recovery,
                &mut effective_max_tokens,
            ) {
                continue;
            }

            // 7. Stop hooks: end the turn, send the model back, or let it stop.
            if self.gate.hooks.has_hooks_for(hooks::HookEvent::Stop) {
                let stop_hook_input = hooks::HookInput {
                    hook_event_name: "Stop".into(),
                    session_id: self.session.session_id.to_string(),
                    cwd: self.settings.paths.project_root().display().to_string(),
                    permission_mode: hook_permission_mode(self.session_state.permission_mode()),
                    stop_hook_active: Some(sent_back_by_stop_hook),
                    ..Default::default()
                };
                let hook_result = self
                    .gate
                    .hooks
                    .run(hooks::HookEvent::Stop, &stop_hook_input)
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
                    self.record_turn_complete("stopped_by_hook", api_calls, tool_calls, start);
                    self.last_had_tool_uses = had_tool_uses_this_turn;
                    return Ok(TurnOutcome::ended(
                        "stopped_by_hook",
                        api_calls,
                        tool_calls,
                        usage,
                        turn_usage,
                        hook_result.discontinue_message().map(str::to_string),
                    ));
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
                    sent_back_by_stop_hook = true;
                    continue;
                }
            }

            // 8. Token budget continuation mode.
            // Continue while accumulated output < 90% of target AND not diminishing
            // (≥3 continuations, both this & previous delta < 500). No hard cap.
            if let Some(target) = self.token_budget.target {
                self.token_budget.accumulated = self
                    .token_budget
                    .accumulated
                    .saturating_add(usage.output_tokens as u64);

                let this_delta = usage.output_tokens as u64;
                if let base::interface::budget_policy::OutputTarget::KeepGoing { nudge } = self
                    .policies
                    .budget_policy
                    .on_output_target(&base::interface::budget_policy::OutputProgress {
                        accumulated: self.token_budget.accumulated,
                        target,
                        continuations: self.token_budget.continuations,
                        this_delta,
                        last_delta: self.token_budget.last_delta,
                    })
                {
                    self.token_budget.continuations += 1;
                    self.token_budget.last_delta = this_delta;
                    self.session.push_message(ModelMessage {
                        role: MessageRole::User,
                        content: vec![ModelContentBlock::Text { text: nudge }],
                    });
                    tracing::info!(
                        accumulated = self.token_budget.accumulated,
                        target,
                        continuation = self.token_budget.continuations,
                        "Token budget continuation — injecting nudge"
                    );
                    continue;
                }

                // Budget met (≥90%) or diminishing returns — clear budget state.
                let threshold = (target as f64 * 0.9) as u64;
                let budget_met = self.token_budget.accumulated >= threshold;
                tracing::info!(
                    accumulated = self.token_budget.accumulated,
                    target,
                    budget_met,
                    "Token budget session complete"
                );
                self.token_budget.target = None;
                self.token_budget.accumulated = 0;
                self.token_budget.continuations = 0;
                self.token_budget.last_delta = 0;
            }

            // The model has answered; a message the user sent meanwhile is
            // answered in the same turn rather than left for one nobody
            // asked to start.
            if self.deliver_queued_user_messages(true).await {
                continue;
            }

            // No tools → turn complete
            self.session.increment_turn();
            self.record_turn_complete_in(turn_no, &stop_reason, api_calls, tool_calls, start);
            // TurnComplete — the normal end of a turn. Distinct from the
            // `Stop` hook above, which fires whenever the model stops calling
            // tools and can *veto* the stop; this one is a notification that
            // the turn is over, carrying why.
            self.fire_lifecycle_hook(hooks::HookEvent::TurnComplete, |i| {
                i.with_turn_id(self.current_turn_id.clone())
                    .with_stop_reason(stop_reason.clone())
            })
            .await;
            // Auto-extract durable memories after turn completion.
            // Only extract if the model produced a complete response (not cancelled/max_turns).
            // Gated on `memory_enabled` — see the prefetch block above for why this matters
            // (the flag's whole point is opting out of the memory subsystem entirely, not just
            // the static prompt injection).
            if self.settings.memory_enabled {
                let session_messages = self.session.messages().to_vec();
                let store = self.memory.memory_store.clone();
                // `task_models.memory` lets an app point this background
                // extraction call at a different vendor/model than the main
                // conversation — same TaskRouter mechanism already proven
                // for the "subagent" task type (see
                // `AgentTool::Inner::model_for_subagent`). No override
                // configured → falls back to the main model.
                let (model, model_name) = match &self.llm.task_router {
                    Some(router) => (
                        self.accounted(router.model_for("memory")),
                        router
                            .model_name_for("memory")
                            .unwrap_or(&self.settings.model.model_name)
                            .to_string(),
                    ),
                    None => (
                        self.llm.model.clone(),
                        self.settings.model.model_name.clone(),
                    ),
                };
                let prompt_intro = self.scene.memory_extraction_prompt();
                let environment = self.policies.environment.clone();
                let session_id = self.session.session_id.clone();
                while self.memory.extractions.try_join_next().is_some() {}
                self.memory.extractions.spawn(async move {
                    extract_memories_after_turn(
                        store,
                        &session_messages,
                        model.as_ref(),
                        &model_name,
                        prompt_intro.as_deref(),
                        &*environment,
                        &session_id,
                    )
                    .await;
                });
            }

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

            self.last_had_tool_uses = had_tool_uses_this_turn;
            return Ok(TurnOutcome::ended(
                stop_reason,
                api_calls,
                tool_calls,
                usage,
                turn_usage,
                None,
            ));
        }
    }
}
