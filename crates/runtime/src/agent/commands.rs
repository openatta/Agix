//! Built-in slash commands handled by the agent itself (`/help`, `/skills`, `/clear`, `/cost`).

use super::*;

impl Agent {
    /// `/help` — list all available slash commands.
    pub(crate) fn handle_help_command(&self) -> String {
        let commands = self.commands.list();
        if commands.is_empty() {
            return "No commands registered.".into();
        }
        let mut out = String::from("Available slash commands:\n\n");
        for (name, desc) in &commands {
            out.push_str(&format!("/{} — {}\n", name, desc));
        }
        out.push_str("\nUse /<name> [args] to invoke a command.");
        out
    }

    /// `/skills` — list all available skills.
    pub(crate) fn handle_skills_command(&self) -> String {
        let skills = self.skills.list();
        if skills.is_empty() {
            return "No skills loaded.".into();
        }
        let mut out = String::from("Available skills:\n\n");
        for s in &skills {
            let src = s.source.as_str();
            out.push_str(&format!("/{} — {} [{}]\n", s.name, s.description, src));
        }
        out
    }

    /// `/clear` — clear session message history.
    pub(crate) async fn handle_clear_command(&mut self) {
        if let Err(e) = self.session.clear().await {
            tracing::warn!(error = %e, "failed to record /clear in the transcript");
        }
        self.session_context_injected = false;
        self.invoked_skills.clear();
        self.announced_skills = None;
        self.permission_denial_count
            .store(0, std::sync::atomic::Ordering::Relaxed);
        self.output_token_target = None;
        self.accumulated_output_tokens = 0;
        self.token_budget_continuation_count = 0;
    }

    /// `/cost` — show estimated session API cost.
    pub(crate) fn handle_cost_command(&self) -> String {
        let SessionSpend {
            usage,
            api_calls,
            tool_calls,
        } = self.spent;
        format!(
            "Since this session was loaded: {api_calls} model calls, {tool_calls} tool calls; \
             {} input tokens, {} output tokens, {} written to and {} read from the prompt \
             cache, as the provider reported them. Sub-agents' spending is on their own tasks.",
            usage.input_tokens,
            usage.output_tokens,
            usage.cache_creation_input_tokens,
            usage.cache_read_input_tokens,
        )
    }
}
