//! 提示词装配：MCP 说明、技能清单、指令文件、会话上下文、提示块与 /context。

use super::attachments::truncate_at_char_boundary;
use super::request::{estimate_request_overhead, pair_dangling_tool_uses, ModelRequest};
use super::SESSION_CONTEXT_OPENING;
use crate::agent::Agent;
use base::interface::model::{MessageRole, ModelContentBlock, ModelMessage, ToolDef};
use base::interface::scene::ScenePromptContext;
use base::prompt::PromptBlock;
use std::borrow::Cow;
use std::collections::HashSet;

/// Tokens a request spends before any conversation content: the assembled
/// system prompt plus every tool definition's name, description and JSON
/// schema.
///
/// Both are re-sent on every API call, so they are part of the context
/// budget even though `SessionManager::token_count()` — which counts message
/// content only — cannot see them. Measured with the same tokenizer as
/// everything else by wrapping each piece of text in a `Text` block, so this
/// number is directly comparable to the message total rather than being a
/// second, differently-derived estimate (the exact mistake the token-counting
/// unification fixed elsewhere).
///
/// The per-tool JSON is serialized compactly; the API sends the schema as
/// structured JSON rather than a string, so this is an approximation of the
/// same shape, not of the exact bytes.
/// Locate this turn's user message in the request that is about to go out.
///
/// Searches from the end because a turn's later steps append tool-result user
/// messages after it. Returns `None` when compaction has folded the message
/// away — an absent map says "not present", which is true, where a stale index
/// would say something false.
pub(super) fn resolve_input_map(
    pending: Option<&(u64, Vec<base::interface::model::InputSpan>)>,
    messages: &[ModelMessage],
) -> Option<base::interface::model::InputMap> {
    let (fingerprint, spans) = pending?;
    let user_message = messages.iter().rposition(|m| {
        m.role == MessageRole::User && user_text_fingerprint(m) == Some(*fingerprint)
    })?;
    Some(base::interface::model::InputMap {
        user_message,
        spans: spans.clone(),
    })
}

/// Hash of a message's leading text block, used to find that message again
/// after compaction has reshuffled the list. Only the first block: it is the
/// one holding the prompt, and hashing an attached image would mean hashing
/// megabytes of base64 to identify a message by its text.
pub(super) fn user_text_fingerprint(message: &ModelMessage) -> Option<u64> {
    use std::hash::{Hash, Hasher};
    match message.content.first() {
        Some(ModelContentBlock::Text { text }) => {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            text.hash(&mut hasher);
            Some(hasher.finish())
        }
        _ => None,
    }
}

/// Moves the request's `User` prompt blocks into one `<system-reminder>`
/// message at the head of the conversation (see `BlockRole`).
pub(super) fn open_conversation_with_user_blocks(request: &mut ModelRequest) {
    let (opening, system): (Vec<PromptBlock>, Vec<PromptBlock>) =
        std::mem::take(&mut request.prompt_blocks)
            .into_iter()
            .partition(|b| b.role == base::prompt::BlockRole::User);
    request.prompt_blocks = system;
    if opening.is_empty() {
        return;
    }
    let text = opening
        .iter()
        .map(|b| b.content.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    request.messages.insert(
        0,
        ModelMessage {
            role: MessageRole::User,
            content: vec![ModelContentBlock::Text {
                text: format!("<system-reminder>\n{text}\n</system-reminder>"),
            }],
        },
    );
    if let Some(map) = request.params.input_map.as_mut() {
        map.user_message += 1;
    }
}

pub(super) fn build_prompt_context<'a>(
    settings: &'a base::settings::LoadedSettings,
    frozen: Option<&'a base::frozen::FrozenContext>,
    mcp_instructions: Option<&'a str>,
    skills_text: Option<&'a str>,
    effective_model: &str,
    tool_results_ever_cleared: bool,
    env: &dyn base::interface::environment::Environment,
) -> ScenePromptContext<'a> {
    let (is_git, git_branch, is_worktree, git_status, memory_index, output_style, shell, home_dir) =
        if let Some(f) = frozen {
            (
                f.is_git,
                f.git_branch.clone(),
                f.is_worktree,
                f.git_status.clone(),
                f.memory_index.clone().filter(|_| settings.memory_enabled),
                f.output_style.as_ref().map(|os| os.content.clone()),
                f.shell.clone(),
                f.home_dir.clone(),
            )
        } else {
            (false, None, false, None, None, None, None, None)
        };
    ScenePromptContext {
        cwd: Cow::Owned(settings.paths.working_dir().display().to_string()),
        os: Cow::Borrowed(std::env::consts::OS),
        // `FrozenContext::collect` always sets `shell` (falls back to
        // `/bin/bash` itself when `$SHELL` is unset), so this fallback only
        // matters before the first turn's collection has happened.
        shell: Cow::Owned(shell.unwrap_or_else(|| "/bin/bash".to_string())),
        // From the environment snapshot, same as `shell` — a fact about the
        // machine, collected once where a test can replace it.
        home_dir: Cow::Owned(home_dir.unwrap_or_else(|| "/home/user".to_string())),
        date: Cow::Owned(chrono_now(env)),
        // The model actually being called this turn, not the configured
        // default — after an overloaded-fallback switch these diverge, and
        // the prompt should describe reality (knowledge cutoff, model
        // recommendations, etc. downstream all key off this).
        model_name: Cow::Owned(effective_model.to_string()),
        skills_text: skills_text.map(|s| Cow::Owned(s.to_string())),
        mcp_instructions: mcp_instructions.map(|s| Cow::Owned(s.to_string())),
        session_memory: memory_index.map(Cow::Owned),
        is_git,
        git_branch: git_branch.map(Cow::Owned),
        git_status: git_status.map(Cow::Owned),
        is_worktree,
        language: settings.language.clone().map(Cow::Owned),
        // `local_data_dir` is already the `.agix/` dir (flat, no scope
        // segment) — no need to prepend another `.agix/`.
        scratchpad_dir: settings
            .paths
            .local_data_dir
            .join("scratchpad")
            .to_str()
            .map(|s| Cow::Owned(s.to_string())),
        output_style_content: output_style.map(Cow::Owned),
        available_tools: None, // populated by caller if needed
        tool_results_ever_cleared,
    }
}

pub(super) fn chrono_now(env: &dyn base::interface::environment::Environment) -> String {
    let now = env.now();
    format!(
        "{:04}-{:02}-{:02}",
        now.year(),
        u8::from(now.month()),
        now.day()
    )
}

/// Fixed per-entry ceiling on a skill's `description` + `when_to_use`,
/// applied before (and independent of) `build_skills_text`'s listing-wide
/// budget division — a two-layer cap: this 1536-char ceiling always
/// applies, the listing budget then further shrinks entries only when there
/// are enough skills to need it. `when_to_use` is appended (space-separated)
/// to `description`, not shown separately.
pub(super) const SKILL_COMBINED_TEXT_CAP: usize = 1536;

pub(super) fn combined_skill_text_capped(description: &str, when_to_use: Option<&str>) -> String {
    let combined = match when_to_use {
        Some(w) if !w.is_empty() => format!("{description} {w}"),
        _ => description.to_string(),
    };
    if combined.len() <= SKILL_COMBINED_TEXT_CAP {
        combined
    } else {
        truncate_at_char_boundary(&combined, SKILL_COMBINED_TEXT_CAP).to_string()
    }
}

/// Decide which skills keep their full description in the listing versus
/// degrading to name-only, under a shared character budget — pulled out of
/// `Agent::build_skills_text` as a free function so the priority/greedy-fill
/// logic is testable without constructing a real `Agent`. `names`, `counts`,
/// and `entry_lens` must be the same length and index-aligned (one entry
/// per skill, in the order they'll be printed); the returned `Vec<bool>` is
/// aligned the same way. Ties (equal invocation count — the common case:
/// everything at `0` in a fresh session) break on name for determinism,
/// independent of whatever order the caller's data happened to arrive in.
pub(super) fn select_skills_keeping_description(
    names: &[&str],
    counts: &[u32],
    entry_lens: &[usize],
    available_budget: usize,
    per_entry_overhead: usize,
) -> Vec<bool> {
    let mut priority_order: Vec<usize> = (0..names.len()).collect();
    priority_order.sort_by(|&a, &b| {
        counts[b]
            .cmp(&counts[a])
            .then_with(|| names[a].cmp(names[b]))
    });
    let mut keeps_description = vec![false; names.len()];
    let mut spent = 0usize;
    for idx in priority_order {
        let cost = entry_lens[idx] + per_entry_overhead;
        if spent + cost > available_budget {
            break;
        }
        spent += cost;
        keeps_description[idx] = true;
    }
    keeps_description
}

impl Agent {
    /// Build MCP server instructions text for system prompt injection.
    /// Per-server and total length are already bounded by
    /// `McpManager::server_instructions()` — see that method's doc comment.
    pub(super) fn build_mcp_instructions(&self) -> String {
        let instructions = self.tooling.mcp.instructions();
        if instructions.is_empty() {
            return String::new();
        }
        let mut text = String::from(
            "# MCP Server Instructions\n\n\
             The following MCP servers have provided instructions for how to \
             use their tools and resources:\n\n",
        );
        for instr in &instructions {
            text.push_str(&format!("## {}\n{}\n\n", instr.name, instr.instructions));
        }
        text
    }

    /// Build skills text for system prompt injection.
    ///
    /// The listing always names every skill. Under budget pressure the
    /// *description* is what gives way — and it gives way whole-entry, not
    /// shortened everywhere at once: the least-invoked skills lose their
    /// description first (degrading to name-only), while the most-invoked
    /// skills keep their full (1536-char-capped) text — not every
    /// description shrunk proportionally regardless of use, which would mean
    /// a single heavily-used skill degraded exactly as much as one never
    /// invoked.
    pub(super) fn build_skills_text(&self) -> String {
        let skills = self.tooling.skills.list();
        // Filter out skills with disable_model_invocation: true
        let llm_skills: Vec<_> = skills
            .iter()
            .filter(|s| !s.disable_model_invocation)
            .collect();
        if llm_skills.is_empty() {
            return String::new();
        }

        // Budget-aware skill listing: 1% of context window tokens * 4
        // chars/token, fallback 8000 chars.
        let context_window = self.context_budget().compact_threshold;
        let budget_chars = if context_window > 0 {
            ((context_window as f64 * 0.01) * 4.0) as usize
        } else {
            8000
        };
        const HEADER_CHARS: usize = 22;
        const PER_ENTRY_OVERHEAD: usize = 6;

        // Fixed per-entry ceiling on `description` + `when_to_use` combined
        // — applies regardless of listing budget (see `combined_skill_text_capped`).
        let combined: Vec<String> = llm_skills
            .iter()
            .map(|s| combined_skill_text_capped(&s.description, s.when_to_use.as_deref()))
            .collect();

        let available_budget = budget_chars.saturating_sub(HEADER_CHARS);
        let names: Vec<&str> = llm_skills.iter().map(|s| s.name.as_str()).collect();
        let counts: Vec<u32> = names
            .iter()
            .map(|n| self.tooling.skills.invocation_count(n))
            .collect();
        let entry_lens: Vec<usize> = combined.iter().map(|c| c.len()).collect();
        let keeps_description = select_skills_keeping_description(
            &names,
            &counts,
            &entry_lens,
            available_budget,
            PER_ENTRY_OVERHEAD,
        );

        let mut text = String::from(
            "## Available Skills\n\n\
             Current as of this turn; it overrides anything said earlier about \
             which skills exist.\n\n",
        );
        for (i, s) in llm_skills.iter().enumerate() {
            if !keeps_description[i] {
                text.push_str(&format!("- **{}**\n", s.name));
                continue;
            }
            let desc = &combined[i];
            if let Some(when) = &s.argument_hint {
                text.push_str(&format!("- **{}**: {} (args: {})\n", s.name, desc, when));
            } else {
                text.push_str(&format!("- **{}**: {}\n", s.name, desc));
            }
        }
        text
    }

    /// Build the instruction-file context injected once per session as a
    /// `<system-reminder>` (the `# claudeMd` block).
    ///
    /// `FrozenContext::collect` walks the directory tree upward and gathers
    /// every `AGENTS.md` it finds into `memory_blocks`
    /// (`collect_memory_files_with`), so a session rooted in a sub-package of
    /// a monorepo also sees the repository-root conventions.
    ///
    /// **Precedence: root-first, nearest-last, nearest wins.** Files are
    /// emitted farthest-ancestor first and the most specific file last
    /// (`collect_memory_files_with` already returns them in that order), so
    /// the closest file is what the model reads most recently, and the header
    /// states the rule explicitly rather than relying on position alone. The
    /// session's own `instruction_file` — which is usually the nearest
    /// `AGENTS.md` and therefore already in `memory_blocks` — is appended
    /// last if its content isn't already present. Deduplication is by trimmed
    /// content, not path, since `instruction_file` may be an alias or symlink
    /// of a file the walk already canonicalized under a different name.
    pub(super) fn build_instruction_context(&self) -> String {
        let mut sections: Vec<(String, String)> = Vec::new();
        if let Some(ref frozen) = self.prompt.frozen {
            for entry in &frozen.memory_blocks {
                if entry.content.trim().is_empty() {
                    continue;
                }
                sections.push((entry.path.display().to_string(), entry.content.clone()));
            }
        }
        if let Some(ref own) = self.prompt.claude_md_content {
            let already = sections.iter().any(|(_, c)| c.trim() == own.trim());
            if !already && !own.trim().is_empty() {
                sections.push((
                    self.settings
                        .instruction_file
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "instruction file".to_string()),
                    own.clone(),
                ));
            }
        }

        match sections.len() {
            0 => String::new(),
            // A single file needs no precedence preamble and no path header —
            // keep the payload byte-identical to what it was before parent
            // files were merged in, which is the overwhelmingly common
            // non-monorepo case.
            1 => sections.pop().map(|(_, c)| c).unwrap_or_default(),
            _ => {
                let mut out = String::from(
                    "Instructions from multiple AGENTS.md files apply, listed from the \
                     outermost directory to the innermost. Where two files conflict, the \
                     later (more specific, closer to the working directory) one wins.\n",
                );
                for (path, content) in sections {
                    out.push_str(&format!("\n## {path}\n\n{}\n", content.trim_end()));
                }
                out
            }
        }
    }

    /// The conversation's opening `<system-reminder>`: the scene's session
    /// facts, the project instructions and today's date, as one user message.
    pub(super) async fn inject_session_context(&mut self) {
        let model = self.settings.model.model_name.clone();
        let facts = {
            let ctx = build_prompt_context(
                &self.settings,
                self.prompt.frozen.as_ref(),
                None,
                None,
                &model,
                self.compaction.tool_results_ever_cleared,
                &*self.policies.environment,
            );
            self.scene.session_context(&ctx)
        };
        let instructions = self.build_instruction_context();
        let today = chrono_now(&*self.policies.environment);

        let mut text = String::from(SESSION_CONTEXT_OPENING);
        if !facts.is_empty() {
            text.push_str(&format!("# environment\n{facts}\n\n"));
        }
        if !instructions.is_empty() {
            text.push_str(&format!("# claudeMd\n{instructions}\n\n"));
        }
        text.push_str(&format!(
            "# currentDate\n\
             Today's date is {today}.\n\n\
             IMPORTANT: this context may or may not be relevant to your tasks. \
             You should not respond to this context unless it is highly relevant \
             to your task.\n\
             </system-reminder>"
        ));
        self.session.push_message(ModelMessage {
            role: MessageRole::User,
            content: vec![ModelContentBlock::Text { text }],
        });
        self.prompt.session_context_injected = true;

        if !instructions.is_empty() {
            let bytes = instructions.len();
            self.fire_lifecycle_hook(hooks::HookEvent::InstructionsLoaded, |i| {
                i.with_reason(format!("{bytes} bytes of project instructions injected"))
            })
            .await;
        }
        if self.identity.agent_depth == 0 {
            let loaded: Vec<std::path::PathBuf> = self
                .prompt
                .frozen
                .iter()
                .flat_map(|f| f.memory_blocks.iter().map(|e| e.path.clone()))
                .collect();
            if let Err(e) = self.gate.hooks.watch_files(&loaded) {
                tracing::warn!(error = %e, "could not watch the instruction files for FileChanged");
            }
        }
    }

    /// Assemble the system-prompt blocks for one API call.
    ///
    /// Shared by the normal path *and* both recovery paths (overloaded →
    /// fallback model, prompt-too-long → compact and retry), so a recovery
    /// call keeps the skill inventory and MCP guidance. Dropping them would
    /// not help a prompt-too-long: skills text is budgeted to ~1% of the
    /// compact threshold and MCP instructions are a few KB.
    pub(super) async fn build_prompt_blocks(&self, effective_model: &str) -> Vec<PromptBlock> {
        let mcp_instructions = self.build_mcp_instructions();
        let mcp_ref: Option<&str> = if mcp_instructions.is_empty() {
            None
        } else {
            Some(&mcp_instructions)
        };
        let skills_text = self.build_skills_text();
        let skills_ref: Option<&str> = if skills_text.is_empty() {
            None
        } else {
            Some(&skills_text)
        };
        let mut seen_names = HashSet::new();
        let tool_names: String = self
            .tooling
            .tools
            .list()
            .iter()
            .map(|t| t.name().to_string())
            .chain(
                self.tooling
                    .mcp
                    .tools()
                    .into_iter()
                    .map(|t| t.name().to_string()),
            )
            .filter(|n| seen_names.insert(n.clone()))
            .collect::<Vec<_>>()
            .join(",");
        let tools_ref: Option<Cow<'_, str>> = if tool_names.is_empty() {
            None
        } else {
            Some(Cow::Owned(tool_names))
        };
        let mut ctx = build_prompt_context(
            &self.settings,
            self.prompt.frozen.as_ref(),
            mcp_ref,
            skills_ref,
            effective_model,
            self.compaction.tool_results_ever_cleared,
            &*self.policies.environment,
        );
        ctx.available_tools = tools_ref;
        let rules = self.prompt.rules.rules();
        let blocks = self.prompt.prompt_assembler.assemble(
            &base::interface::prompt_assembler::AssemblyRequest {
                registry: self.prompt.prompt_registry.as_ref(),
                scene: self.scene.as_ref(),
                settings: &self.settings,
                memory_store: &self.memory.memory_store,
                ctx: &ctx,
                rules: &rules,
                skills_text: skills_ref,
                mcp_instructions: mcp_ref,
            },
        );
        // Passes that have to await something — a script carrier, most of
        // all. Kept out of `assemble_prompt_with` so assembly stays a
        // synchronous function for every caller that registers none.
        base::interface::prompt_assembly::run_async_assembly_hooks(
            blocks,
            &self.prompt.prompt_registry.async_assembly_hooks(),
            &ctx,
        )
        .await
    }

    /// Locate this turn's user message in the request that is about to go out.
    ///
    /// Searches from the end because a turn's later steps append tool-result
    /// user messages after it. Returns `None` when compaction has folded the
    /// message away — an absent map says "not present", which is true, where a
    /// stale index would say something false.
    pub(super) fn resolve_input_map(
        &self,
        messages: &[ModelMessage],
    ) -> Option<base::interface::model::InputMap> {
        resolve_input_map(self.turn.pending_input.as_ref(), messages)
    }

    pub(super) async fn build_prompt_for_turn(
        &mut self,
        effective_model: &str,
    ) -> (Vec<PromptBlock>, Vec<ToolDef>, Vec<ModelMessage>) {
        let prompt_blocks = self.build_prompt_blocks(effective_model).await;
        let tool_defs = self.build_tool_defs().await;
        // Remember what this request costs *before* any conversation content,
        // so the next turn's compaction check can include it — see
        // `request_overhead_tokens` and `context_tokens`.
        self.compaction.request_overhead_tokens =
            estimate_request_overhead(&**self.session.token_counter(), &prompt_blocks, &tool_defs);
        let messages = pair_dangling_tool_uses(self.session.messages().to_vec());
        (prompt_blocks, tool_defs, messages)
    }

    /// Answer a host's look at, or change to, the context. A change is made
    /// here and written to the log as an `agix.context` entry, so a resume
    /// reproduces it — by this engine, the session log's one writer.
    pub(super) async fn handle_context(
        &mut self,
        request: crate::agent::ContextRequest,
    ) -> Result<crate::agent::ContextAnswer, crate::agent::ContextError> {
        use crate::agent::{ContextAnswer, ContextError, ContextRequest};
        let count = self.session.messages().len();
        let unchanged = |total: Option<usize>| match total {
            Some(total) if total != count => Err(ContextError::Refused(format!(
                "the context has {count} messages, not {total}: it changed since it was read"
            ))),
            _ => Ok(()),
        };
        let (answer, edits) = match request {
            ContextRequest::Inspect => {
                return Ok(ContextAnswer::Inspected(self.inspect_context().await))
            }
            ContextRequest::Drop { at, total } => {
                unchanged(total)?;
                for &i in &at {
                    match self.session.messages().get(i) {
                        None => {
                            return Err(ContextError::Refused(format!(
                                "there is no message {i}; the context has {count}"
                            )))
                        }
                        Some(m) if m.role != base::interface::model::MessageRole::User => {
                            return Err(ContextError::Refused(format!(
                                "message {i} is the model's; only user messages and tool results can be dropped"
                            )))
                        }
                        Some(_) => {}
                    }
                }
                for &i in &at {
                    self.session
                        .drop_message(i)
                        .map_err(ContextError::Refused)?;
                }
                let edits = at
                    .iter()
                    .map(|i| ("drop", serde_json::json!({ "at": i })))
                    .collect::<Vec<_>>();
                (ContextAnswer::Dropped(at), edits)
            }
            ContextRequest::Edit { at, text, total } => {
                unchanged(total)?;
                self.session
                    .edit_message(at, text.clone())
                    .map_err(ContextError::Refused)?;
                (
                    ContextAnswer::Edited(at),
                    vec![("edit", serde_json::json!({ "at": at, "text": text }))],
                )
            }
        };
        let at_ms =
            u64::try_from(self.policies.environment.now().unix_timestamp_nanos() / 1_000_000)
                .unwrap_or(0);
        for (event, mut payload) in edits {
            payload["at_ms"] = serde_json::json!(at_ms);
            self.session
                .record_extension(history::transcript::CONTEXT_NS, event, payload)
                .await
                .map_err(|e| ContextError::NotRecorded(e.to_string()))?;
        }
        Ok(answer)
    }

    /// What the next model call would carry if a step started now with no new
    /// input: the system blocks and who contributed each, the tool table, the
    /// messages, and roughly what each costs. Assembled the way a step
    /// assembles its request, minus what only a real call should do — it
    /// records no request overhead and spends no pending cache edits.
    ///
    /// A real next turn differs by what it adds: the user's message, any
    /// memories recalled for it, anything a hook injects on submit.
    pub(super) async fn inspect_context(&mut self) -> crate::agent::ContextReport {
        use crate::agent::{ContextReport, MessagesReport, SystemBlockReport, ToolReport};
        let counter = self.session.token_counter().clone();
        let tokens_of = |text: &str| {
            counter.count_messages(&[ModelMessage {
                role: base::interface::model::MessageRole::User,
                content: vec![ModelContentBlock::Text {
                    text: text.to_string(),
                }],
            }])
        };
        let model = self.settings.model.model_name.clone();
        let blocks = self.build_prompt_blocks(&model).await;
        let tools = self.build_tool_defs().await;
        let messages = pair_dangling_tool_uses(self.session.messages().to_vec());

        let system: Vec<SystemBlockReport> = blocks
            .iter()
            .map(|b| SystemBlockReport {
                name: b.name.clone(),
                role: b.role,
                origin: b.origin.clone(),
                tokens: tokens_of(&b.content),
                text: b.content.clone(),
            })
            .collect();
        let tools: Vec<ToolReport> = tools
            .iter()
            .map(|t| {
                let schema = t.input_schema.to_string();
                ToolReport {
                    name: t.name.clone(),
                    source: t.source.clone(),
                    schema_bytes: schema.len(),
                    tokens: tokens_of(&t.description) + tokens_of(&schema),
                }
            })
            .collect();
        let message_tokens = counter.count_messages(&messages);
        let total = system.iter().map(|b| b.tokens).sum::<usize>()
            + tools.iter().map(|t| t.tokens).sum::<usize>()
            + message_tokens;
        let budget = self.context_budget();
        ContextReport {
            basis: "next_step_without_new_input".to_string(),
            model,
            max_tokens: self.settings.model.max_tokens,
            system,
            tools,
            messages: MessagesReport {
                count: messages.len(),
                tokens: message_tokens,
            },
            total_tokens: total,
            compact_threshold: budget.compact_threshold,
            hard_cap: budget.hard_cap,
        }
    }
}
