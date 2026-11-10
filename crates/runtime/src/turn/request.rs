//! 模型请求：工具定义、请求准备与重试、发送、失败分类、悬空 tool_use 的修复。

use super::prompt::open_conversation_with_user_blocks;
use crate::agent::Agent;
use base::interface::model::{MessageRole, ModelContentBlock, ModelMessage, ModelStream, ToolDef};
use base::prompt::PromptBlock;
use std::collections::HashSet;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub(super) fn estimate_request_overhead(
    counter: &dyn base::interface::token_counter::TokenCounter,
    prompt_blocks: &[PromptBlock],
    tool_defs: &[ToolDef],
) -> usize {
    let mut blocks: Vec<ModelContentBlock> = prompt_blocks
        .iter()
        .map(|b| ModelContentBlock::Text {
            text: b.content.clone(),
        })
        .collect();
    for t in tool_defs {
        blocks.push(ModelContentBlock::Text {
            text: format!(
                "{}{}{}",
                t.name,
                t.description,
                serde_json::to_string(&t.input_schema).unwrap_or_default()
            ),
        });
    }
    // The session's own counter, so this number is identical in kind to
    // `SessionManager::token_count()`: the two get added together.
    counter.count_messages(&[ModelMessage {
        role: MessageRole::User,
        content: blocks,
    }])
}

/// One assembled call to the model.
///
/// The normal path and the two recovery paths differ only in how these four
/// fields are produced, so they build one of these and hand it to
/// [`Agent::send`] rather than each spelling out a `StreamParams` of its own.
/// A turn's model call, assembled.
///
/// The same type interceptors see — one shape rather than a private struct
/// and a public mirror of it that have to be kept agreeing.
pub(super) type ModelRequest = base::interface::model_interceptor::ModelRequestView;

/// Which of the recovery policy's failures a model error is. `rendered` is
/// the error's own text, which `Other` carries.
pub(super) fn model_failure<'a>(
    e: &'a base::interface::model::ModelError,
    rendered: &'a str,
) -> base::interface::recovery_policy::ModelFailure<'a> {
    use base::interface::model::ModelError;
    use base::interface::recovery_policy::ModelFailure;
    match e {
        ModelError::Overloaded => ModelFailure::Overloaded,
        ModelError::Api {
            status: 413,
            message,
        } => ModelFailure::ContextTooLong { message },
        ModelError::Api { status, message }
            if (400..500).contains(status) && says_context_too_long(message) =>
        {
            ModelFailure::ContextTooLong { message }
        }
        ModelError::Internal(message) if says_context_too_long(message) => {
            ModelFailure::ContextTooLong { message }
        }
        _ => ModelFailure::Other { message: rendered },
    }
}

/// Whether a refusal's text says the request was too large for the model.
///
/// No provider gives this a status of its own — Anthropic, OpenAI and the
/// relays in front of them all answer 400 with a type that covers every bad
/// request — so the wording is the signal.
pub(super) fn says_context_too_long(message: &str) -> bool {
    const WORDINGS: &[&str] = &[
        "prompt is too long",
        "prompt too long",
        "input is too long",
        "context_length_exceeded",
        "maximum context length",
        "context length exceeded",
        "exceeds the context window",
        "exceeds the maximum number of tokens",
        "too many tokens",
        "request_too_large",
    ];
    let message = message.to_ascii_lowercase();
    WORDINGS.iter().any(|w| message.contains(w))
}

/// Whether `id` is a `tool_use` of the last reply that nothing after it has
/// answered yet.
pub(super) fn awaits_result(messages: &[ModelMessage], id: &str) -> bool {
    let Some(last_reply) = messages
        .iter()
        .rposition(|m| m.role == MessageRole::Assistant)
    else {
        return false;
    };
    let used = messages[last_reply]
        .content
        .iter()
        .any(|b| matches!(b, ModelContentBlock::ToolUse { id: used, .. } if used == id));
    let answered = messages[last_reply + 1..]
        .iter()
        .flat_map(|m| &m.content)
        .any(
            |b| matches!(b, ModelContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == id),
        );
    used && !answered
}

/// Gives every `tool_use` a `tool_result` in the message right after it, and
/// drops every `tool_result` that answers no `tool_use` there.
///
/// The engine never writes an unpaired one, but a transcript written by a
/// process that died mid-step can hold one, and the API rejects every
/// request after it — a session that can never answer again. Results a host
/// injected after the engine's own arrive as later user messages; they are
/// gathered into the first. Applied to the outgoing request only; the
/// transcript keeps what actually happened.
pub(super) fn pair_dangling_tool_uses(messages: Vec<ModelMessage>) -> Vec<ModelMessage> {
    let mut out: Vec<ModelMessage> = Vec::with_capacity(messages.len() + 1);
    let mut messages = messages.into_iter().peekable();
    while let Some(message) = messages.next() {
        if message.role != MessageRole::Assistant {
            push_unless_empty(&mut out, without_tool_results(message));
            continue;
        }
        let used: Vec<String> = message
            .content
            .iter()
            .filter_map(|b| match b {
                ModelContentBlock::ToolUse { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect();
        out.push(message);

        let mut answers: std::collections::HashMap<String, ModelContentBlock> =
            std::collections::HashMap::new();
        let mut replies: Vec<ModelMessage> = Vec::new();
        while let Some(reply) = messages.next_if(|m| m.role == MessageRole::User) {
            let (results, rest): (Vec<_>, Vec<_>) = reply
                .content
                .into_iter()
                .partition(|b| matches!(b, ModelContentBlock::ToolResult { .. }));
            for result in results {
                if let ModelContentBlock::ToolResult { tool_use_id, .. } = &result {
                    if used.contains(tool_use_id) && !answers.contains_key(tool_use_id) {
                        answers.insert(tool_use_id.clone(), result);
                    }
                }
            }
            replies.push(ModelMessage {
                role: MessageRole::User,
                content: rest,
            });
        }
        if used.is_empty() {
            for reply in replies {
                push_unless_empty(&mut out, reply);
            }
            continue;
        }
        let mut paired: Vec<ModelContentBlock> = used
            .iter()
            .map(|id| {
                answers
                    .remove(id)
                    .unwrap_or_else(|| ModelContentBlock::ToolResult {
                        tool_use_id: id.clone(),
                        content: "The tool call did not complete.".into(),
                        is_error: Some(true),
                    })
            })
            .collect();
        let mut replies = replies.into_iter();
        if let Some(first) = replies.next() {
            paired.extend(first.content);
        }
        out.push(ModelMessage {
            role: MessageRole::User,
            content: paired,
        });
        for reply in replies {
            push_unless_empty(&mut out, reply);
        }
    }
    out
}

pub(super) fn without_tool_results(mut message: ModelMessage) -> ModelMessage {
    message
        .content
        .retain(|b| !matches!(b, ModelContentBlock::ToolResult { .. }));
    message
}

pub(super) fn push_unless_empty(out: &mut Vec<ModelMessage>, message: ModelMessage) {
    if !message.content.is_empty() {
        out.push(message);
    }
}

/// Make the dispatch registry's MCP entries match `mcp`'s current adapter
/// list: new tools are added, re-listed ones take the fresh instance, and ones
/// a server stopped listing are withdrawn so they can no longer be called.
/// Adapters the session's tool policy does not permit are never registered
/// — see `tool_policy`.
pub(super) fn sync_mcp_adapters(
    tools: &dyn base::tool::ToolRegistry,
    mcp: &dyn base::interface::mcp_servers::McpServers,
    permits: impl Fn(&str) -> bool,
) {
    let is_mcp = |t: &Arc<dyn base::tool::Tool>| t.spec().origin.is_mcp();
    let current: Vec<_> = mcp
        .tools()
        .into_iter()
        .filter(|a| permits(a.name()))
        .collect();
    let live: HashSet<&str> = current.iter().map(|a| a.name()).collect();
    let registered = tools.list();
    let builtin_names: HashSet<&str> = registered
        .iter()
        .filter(|t| !is_mcp(t))
        .map(|t| t.name())
        .collect();
    for stale in registered
        .iter()
        .filter(|t| is_mcp(t) && !live.contains(t.name()))
    {
        // `remove` withdraws every registration under the name, a builtin's too.
        if !builtin_names.contains(stale.name()) {
            tools.remove(stale.name());
        }
    }
    for adapter in &current {
        if builtin_names.contains(adapter.name()) {
            continue;
        }
        match tools.get(adapter.name()) {
            Some(existing) if Arc::ptr_eq(&existing, adapter) => {}
            _ => {
                tools.replace(adapter.clone());
            }
        }
    }
}

impl Agent {
    pub(super) async fn build_tool_defs(&mut self) -> Vec<base::interface::model::ToolDef> {
        use std::collections::BTreeMap;
        /// The tool through which deferred schemas and `Tool::guide()` usage
        /// guides are fetched. Named here because `build_tool_defs` must not
        /// advertise a guide when the scene has filtered the fetcher out.
        const TOOL_SEARCH_NAME: &str = "ToolSearch";
        let mut pool: BTreeMap<String, Arc<dyn base::tool::Tool>> = BTreeMap::new();
        for t in self.tooling.mcp.tools() {
            pool.insert(t.name().to_string(), t);
        }
        // MCP adapters also sit in `self.tools`, because dispatch looks them up
        // there; only a non-MCP entry can shadow one. Built-ins win.
        for t in self.tooling.tools.list() {
            if t.spec().origin.is_mcp() {
                continue;
            }
            let name = t.name().to_string();
            if pool.contains_key(&name) && self.tooling.mcp_shadow_warned.insert(name.clone()) {
                tracing::warn!(
                    tool = %name,
                    "built-in tool shadows an MCP tool of the same name; the MCP tool is not exposed to the model"
                );
            }
            pool.insert(name, t.clone());
        }
        let selected: Vec<Arc<dyn base::tool::Tool>> = pool
            .into_values()
            .filter(|t| self.tooling.tool_policy.permits(t.name()))
            .collect();

        // A guide is only worth pointing at if the model has a way to fetch
        // it — `ToolSearch` can be filtered out by a scene whitelist, and a
        // pointer to a tool that isn't there is worse than no pointer.
        let can_fetch_guides = selected.iter().any(|t| t.name() == TOOL_SEARCH_NAME);
        let prompt_ctx = base::tool::PromptContext {
            cwd: self.settings.paths.working_dir().to_path_buf(),
            model: self.settings.model.model_name.clone(),
            session_id: self.session.session_id.clone(),
            is_interactive: false,
            all_tool_names: selected.iter().map(|t| t.name().to_string()).collect(),
            allowed_agent_types: vec![],
            path_style: self.config.path_style,
        };

        let mut defs = Vec::with_capacity(selected.len());
        for t in selected {
            let deferred = self.config.deferred_tools.contains(t.name());
            let mut description = t.description().to_string();
            // Surface the existence of `Tool::guide()`, not its body: inlining
            // the guides would add ~85 KB to every request. A deferred tool
            // needs no pointer — fetching its schema returns the guide too.
            if can_fetch_guides && !deferred && t.guide(&prompt_ctx).await.is_some() {
                description.push_str(&format!(
                    "\n\nDetailed guide: ToolSearch{{\"query\":\"select:{}\"}}",
                    t.name()
                ));
            }
            // A deferred tool's entry is its name and description; the stub is
            // still a valid (if empty) schema so a provider that validates the
            // `tools` array accepts it. What an empty schema means is said
            // once, in `ToolSearch`'s description.
            let input_schema = if deferred {
                serde_json::json!({"type": "object", "properties": {}})
            } else {
                base::tool::compact_schema(t.input_schema(self.config.path_style))
            };
            defs.push(base::interface::model::ToolDef {
                name: t.name().to_string(),
                description,
                input_schema,
                source: Some(t.spec().origin.wire()),
            });
        }
        defs
    }

    /// A model this session did not build, made to account for itself.
    ///
    /// `Builder::build` wraps the session's own model once, so everything
    /// that reaches the provider through it is counted. A task model comes
    /// from the router instead — pool-level, shared, built before any session
    /// existed — so a call routed to one is the single kind that would still
    /// arrive nowhere. Wrapping it here costs one allocation per auxiliary
    /// call and closes that.
    pub(super) fn accounted(
        &self,
        model: Arc<dyn base::interface::model::Model>,
    ) -> Arc<dyn base::interface::model::Model> {
        Arc::new(telemetry::model::TelemetryModel::new(
            model,
            self.telemetry.telemetry_handle.clone(),
            self.settings.model.model_name.clone(),
        ))
    }

    /// Assemble a step's request and account for what it costs before any
    /// conversation content (see `request_overhead_tokens`).
    ///
    /// This is also where the pending cache edits are consumed, which is why a
    /// retry after a failed call must go through [`Agent::prepare_retry`]
    /// instead: the edits belong to the call that already spent them.
    pub(super) async fn prepare_request(
        &mut self,
        model: &str,
        max_tokens: u32,
        origin: Option<base::interface::model::CallOrigin>,
    ) -> ModelRequest {
        let (prompt_blocks, tool_defs, messages) = self.build_prompt_for_turn(model).await;
        let input_map = self.resolve_input_map(&messages);
        ModelRequest {
            prompt_blocks,
            tool_defs,
            messages,
            params: base::interface::model::StreamParams {
                model: model.to_string(),
                max_tokens,
                thinking_mode: self.settings.model.thinking_mode.clone(),
                effort: self.settings.model.effort,
                fallback_model: self.settings.model.fallback_model.clone(),
                cache_edits: self.compaction.cached_mc.consume_pending_edits(),
                origin,
                input_map,
                provider_search: None,
            },
        }
    }

    /// Reassemble after a recovery changed the model or the message list.
    ///
    /// The tool table and the messages come from the caller rather than being
    /// rebuilt: the overloaded path retries the *same* conversation against a
    /// different model, while the prompt-too-long path retries a conversation
    /// that compaction just shortened. Only the prompt blocks are rebuilt, so
    /// a fallback model still gets the skills inventory and MCP guidance the
    /// primary one got.
    ///
    /// Neither the overhead estimate nor the cache edits are redone — both
    /// belong to the call that failed.
    pub(super) async fn prepare_retry(
        &self,
        model: &str,
        max_tokens: u32,
        tool_defs: Vec<ToolDef>,
        messages: Vec<ModelMessage>,
        fallback_model: Option<String>,
        origin: Option<base::interface::model::CallOrigin>,
    ) -> ModelRequest {
        let input_map = self.resolve_input_map(&messages);
        ModelRequest {
            prompt_blocks: self.build_prompt_blocks(model).await,
            tool_defs,
            messages,
            params: base::interface::model::StreamParams {
                model: model.to_string(),
                max_tokens,
                thinking_mode: self.settings.model.thinking_mode.clone(),
                effort: self.settings.model.effort,
                fallback_model,
                cache_edits: Vec::new(),
                origin,
                input_map,
                provider_search: None,
            },
        }
    }

    /// The turn's only call into the model.
    ///
    /// Routing all three paths — the normal one and the two recoveries —
    /// through here is what keeps them from diverging: a new field on
    /// `StreamParams` is filled once rather than in three places that have to
    /// be found first.
    pub(super) async fn send(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> Result<ModelStream, base::interface::model::ModelError> {
        // The last point at which the engine's idea of the request and the
        // provider's can still be made to differ. Once per model call, never
        // per chunk — see `base::interface::model_interceptor`.
        let mut request = request;
        base::interface::model_interceptor::intercept_request(
            &self.llm.model_interceptors,
            &mut request,
        );
        open_conversation_with_user_blocks(&mut request);
        self.llm
            .model
            .stream(
                request.prompt_blocks,
                request.tool_defs,
                request.messages,
                request.params,
                cancel,
            )
            .await
    }

    /// Total tokens this session is currently sending per API call: the
    /// conversation plus the fixed per-request overhead.
    ///
    /// `SessionManager::token_count()` counts message content only. That is
    /// the right number for "how much of the context is conversation", and
    /// the wrong one for "are we near the context limit" — the assembled
    /// system prompt (scene blocks, skills inventory, MCP instructions) and
    /// the `tools` array (30+ tools, each with a full JSON schema) are sent
    /// on *every* call and are far from negligible here. Comparing only the
    /// message total against `compact_threshold` therefore under-counted the
    /// real context by a fixed, sizeable margin, and compaction fired later
    /// than it was configured to.
    ///
    /// Zero on the very first turn, before any prompt has been assembled —
    /// which is fine, a session's first turn is never near its budget.
    pub(crate) fn context_tokens(&self) -> usize {
        self.session.token_count() + self.compaction.request_overhead_tokens
    }

    /// What the scene asked for, after the deployment's budget policy has had
    /// its say.
    pub(crate) fn context_budget(&self) -> base::interface::budget_policy::ContextBudget {
        self.policies
            .budget_policy
            .context_budget(&self.scene.token_budget())
    }

    /// Put a model error to the recovery policy.
    ///
    /// The classification is the engine's — which errors *are* an overload and
    /// which *are* a size refusal is a fact about the protocol, not an
    /// opinion. What to do about each is the policy's.
    pub(super) fn classify_failure(
        &self,
        e: &base::interface::model::ModelError,
    ) -> base::interface::recovery_policy::Recovery {
        let rendered = e.to_string();
        self.llm
            .recovery_policy
            .on_failure(&model_failure(e, &rendered))
    }

    /// Ask the recovery policy what a stopped-early response calls for, and
    /// act on the answer.
    ///
    /// Returns true when the caller should run another round. The ladder
    /// itself — 64K first, 8K after, three attempts, the nudge — is
    /// `DefaultRecovery`'s; what stays here is applying the answer to the
    /// loop's own state, which is the part that has to happen in this order.
    /// A continuation adds nothing: the stopped reply is already the
    /// conversation's last message, which is what the provider continues from.
    pub(super) fn handle_early_stop(
        &mut self,
        stop_reason: &str,
        max_tokens_recovery: &mut u32,
        effective_max_tokens: &mut u32,
    ) -> bool {
        use base::interface::recovery_policy::StopRecovery;
        let decision = self.llm.recovery_policy.on_early_stop(
            stop_reason,
            &base::interface::recovery_policy::RecoveryAttempt {
                output_limit_escalations: *max_tokens_recovery,
                current_max_tokens: *effective_max_tokens,
            },
        );
        match decision {
            StopRecovery::Accept => false,
            StopRecovery::Continue => {
                tracing::info!(
                    stop_reason,
                    "the reply stopped early; sending it back to continue"
                );
                true
            }
            StopRecovery::RaiseOutputLimit { max_tokens, nudge } => {
                *max_tokens_recovery += 1;
                *effective_max_tokens = max_tokens;
                if let Some(nudge) = nudge {
                    self.session.push_message(ModelMessage {
                        role: MessageRole::User,
                        content: vec![ModelContentBlock::Text { text: nudge }],
                    });
                }
                tracing::info!(
                    recovery = *max_tokens_recovery,
                    max_tokens = *effective_max_tokens,
                    "output limit raised and the call retried"
                );
                true
            }
        }
    }
}
