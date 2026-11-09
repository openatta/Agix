//! Slash command system — intercept `/name args` before the LLM.
//!
//! Supports two command types:
//! - **Prompt**: expand skill content, replace user message, continue to LLM
//! - **Local**: answered by the agent itself, skip LLM
//!
//! Architecture:
//! ```text
//! /simplify main.rs
//!   → parse_slash_command → { name: "simplify", args: "main.rs" }
//!   → CommandRegistry::resolve("simplify")
//!   → Command::Prompt(skill_entry) → expand body + replace {args}
//!   → content = expanded prompt → continue to run_user_turn()
//! ```

use base::frozen::SkillEntry;
use base::interface::skill_catalog::SkillCatalog;
use std::collections::HashMap;
use std::sync::Arc;

/// Result of parsing a `/name args` string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashCommand {
    pub name: String,
    pub args: String,
}

/// Parse a slash command from user input.
/// Returns `None` if the input doesn't start with `/`.
pub fn parse_slash_command(input: &str) -> Option<SlashCommand> {
    let trimmed = input.trim();
    if !trimmed.starts_with('/') {
        return None;
    }
    let rest = &trimmed[1..]; // strip leading /
    if rest.is_empty() {
        return None; // bare "/" is not a command
    }
    // Split on first whitespace: name vs args
    let (name, args) = if let Some(space_pos) = rest.find(char::is_whitespace) {
        let (n, a) = rest.split_at(space_pos);
        (n.to_string(), a.trim().to_string())
    } else {
        (rest.to_string(), String::new())
    };
    if name.is_empty() {
        return None;
    }
    Some(SlashCommand { name, args })
}

/// A registered slash command.
#[derive(Clone)]
pub enum Command {
    /// Prompt command: expand skill content and feed to LLM.
    Prompt { entry: Box<SkillEntry> },
    /// MCP prompt command (`/mcp__<server>__<prompt>`): call the owning
    /// server's `prompts/get` and feed the returned messages to the LLM.
    ///
    /// Separate from `Prompt` because expansion is not a local string
    /// substitution — it is an async round trip to the server, which can fail
    /// (server down, prompt handler errors) and whose arguments must be
    /// mapped onto the prompt's *declared* argument list first. The whole
    /// operation lives in `McpManager::invoke_prompt_command`; this variant
    /// only carries what's needed to name and describe it.
    McpPrompt {
        /// Full command name, `mcp__<server>__<prompt>` (no leading `/`).
        command: String,
        description: String,
        /// `name=<value>`-style hint for `/help`, derived from the prompt's
        /// declared arguments.
        argument_hint: String,
    },
    /// Local command: answered by the agent itself, skip LLM.
    Local { description: String },
}

impl Command {
    pub fn description(&self) -> &str {
        match self {
            Command::Prompt { entry } => &entry.description,
            Command::McpPrompt { description, .. } => description,
            Command::Local { description, .. } => description,
        }
    }

    pub fn is_prompt(&self) -> bool {
        matches!(self, Command::Prompt { .. } | Command::McpPrompt { .. })
    }
}

/// Registry of all available slash commands.
///
/// Skill-derived commands are **not** stored here — they are resolved
/// through the live [`SkillCatalog`] on every lookup. The catalog reloads
/// changed files in place (the turn loop calls `reload_changed()` before
/// each turn), so any copy taken at
/// construction time is stale as soon as a user adds, edits or deletes a
/// skill file; a session built at 9am would keep resolving the skill set of
/// 9am for as long as it lived, and the daemon's pool-wide registry — built
/// once and shared by every session — never updated at all outside a
/// plugin refresh.
///
/// Precedence on lookup, highest first: [`registered`](Self::insert_prompt)
/// (MCP-contributed) → live skills → [`builtin`](Default) local commands.
/// This is the order the old insert-everything-into-one-map build sequence
/// produced, preserved deliberately: `Builder::build`/
/// `build_shared_commands` inserted skills over the built-ins and then
/// explicitly registered commands over both.
pub struct CommandRegistry {
    /// Built-in local commands (`/help`, `/clear`, …). Lowest precedence.
    builtin: HashMap<String, Command>,
    /// MCP prompts — everything explicitly registered that has no backing
    /// entry in the skill catalog.
    registered: HashMap<String, Command>,
    skills: Option<Arc<dyn SkillCatalog>>,
}

/// Skill-derived slash commands only exist for `user_invocable` skills.
fn skill_as_command(skill: SkillEntry) -> Option<Command> {
    skill.user_invocable.then(|| Command::Prompt {
        entry: Box::new(skill),
    })
}

impl CommandRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            builtin: HashMap::new(),
            registered: HashMap::new(),
            skills: None,
        }
    }

    /// Build a registry backed by a skill manager (disk + bundled skills), on
    /// top of the 5 built-in local commands (see `Default`). Each
    /// user-invocable skill resolves as a `prompt` command.
    ///
    /// Takes the `Arc` rather than a reference because the manager is kept:
    /// see the type-level comment on why a snapshot is not good enough.
    pub fn from_skill_catalog(catalog: Arc<dyn SkillCatalog>) -> Self {
        Self {
            skills: Some(catalog),
            ..Self::default()
        }
    }

    /// Register every MCP server prompt as a `/mcp__<server>__<prompt>` slash
    /// command — the same treatment `from_skill_catalog` gives skills, so MCP
    /// prompts show up in `/help`, in `commands.list`, and resolve in the
    /// turn loop's slash-command interception.
    ///
    /// Called with `McpServers::prompt_commands()` once the servers are connected
    /// (see `Builder::build`). Re-registering is idempotent: names are stable,
    /// so a later call after `refresh_prompts` overwrites in place.
    pub fn register_mcp_prompts(
        &mut self,
        prompts: &[base::interface::mcp_servers::McpPromptCommand],
    ) {
        for p in prompts {
            self.registered.insert(
                p.command.clone(),
                Command::McpPrompt {
                    command: p.command.clone(),
                    description: p.description.clone(),
                    argument_hint: p.argument_hint.clone(),
                },
            );
        }
    }

    /// Insert a prompt command from a SkillEntry — for commands that have no
    /// entry in the skill catalog (MCP-contributed ones); a skill on disk
    /// needs no registration, it resolves through the manager.
    pub fn insert_prompt(&mut self, entry: SkillEntry) {
        self.registered.insert(
            entry.name.clone(),
            Command::Prompt {
                entry: Box::new(entry),
            },
        );
    }

    pub fn insert_local(&mut self, name: &str, description: &str) {
        self.builtin.insert(
            name.to_string(),
            Command::Local {
                description: description.to_string(),
            },
        );
    }

    /// Look up a command by name.
    ///
    /// Returns an owned `Command`: skill-derived ones are materialized from
    /// the live catalog per call, so there is nothing stable to borrow. The
    /// clone is a `SkillEntry`'s worth of strings, once per slash-command
    /// invocation.
    pub fn resolve(&self, name: &str) -> Option<Command> {
        if let Some(cmd) = self.registered.get(name) {
            return Some(cmd.clone());
        }
        if let Some(cmd) = self
            .skills
            .as_ref()
            .and_then(|s| s.get(name))
            .and_then(skill_as_command)
        {
            return Some(cmd);
        }
        self.builtin.get(name).cloned()
    }

    /// The body a skill-derived command expands to: asked of the live skill
    /// catalog, which knows every source's bodies — bundled, on disk, and the
    /// ones an MCP server's tools generate.
    pub fn skill_body(&self, entry: &SkillEntry) -> Option<String> {
        if let Some(body) = self.skills.as_ref().and_then(|s| s.body(&entry.name)) {
            return Some(body);
        }
        std::fs::read_to_string(&entry.path).ok()
    }

    /// Handle a prompt command: expand the skill body and return the
    /// replacement content.
    pub fn handle_prompt_command(&self, entry: &SkillEntry, cmd: &SlashCommand) -> String {
        expand_skill_for_command(entry, self.skill_body(entry), &cmd.args)
    }

    /// The `argument_hint` to display next to a command in `/help`, if any.
    pub fn argument_hint(&self, name: &str) -> Option<String> {
        match self.resolve(name)? {
            Command::McpPrompt { argument_hint, .. } if !argument_hint.is_empty() => {
                Some(argument_hint)
            }
            Command::Prompt { entry } => entry.argument_hint.clone(),
            _ => None,
        }
    }

    /// Every resolvable command, lowest-precedence tier first so that higher
    /// tiers overwrite by name — the same result `resolve` gives.
    fn merged(&self) -> HashMap<String, Command> {
        let mut out = self.builtin.clone();
        if let Some(skills) = self.skills.as_ref() {
            for skill in skills.list() {
                let name = skill.name.clone();
                if let Some(cmd) = skill_as_command(skill) {
                    out.insert(name, cmd);
                }
            }
        }
        out.extend(self.registered.iter().map(|(k, v)| (k.clone(), v.clone())));
        out
    }

    /// List all commands (for /help).
    pub fn list(&self) -> Vec<(String, String)> {
        let mut entries: Vec<(String, String)> = self
            .merged()
            .into_iter()
            .map(|(name, cmd)| {
                let desc = cmd.description().to_string();
                (name, desc)
            })
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        entries
    }

    /// Number of resolvable commands.
    pub fn len(&self) -> usize {
        self.merged().len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Full command catalog with kind/source metadata (see [`CommandInfo`]) —
    /// unlike `list()` (name+description only, meant for the in-turn
    /// `/help` text), this is for callers outside the LLM loop, e.g.
    /// daemon's `commands.list` RPC.
    pub fn list_detailed(&self) -> Vec<CommandInfo> {
        let mut entries: Vec<CommandInfo> = self
            .merged()
            .into_iter()
            .map(|(name, cmd)| match cmd {
                Command::Local { description, .. } => CommandInfo {
                    name: name.clone(),
                    description: description.clone(),
                    kind: "local",
                    source: "builtin",
                },
                Command::Prompt { entry } => CommandInfo {
                    name: name.clone(),
                    description: entry.description.clone(),
                    kind: "prompt",
                    source: match entry.source {
                        base::frozen::SkillSource::User => "user",
                        base::frozen::SkillSource::Project => "project",
                        base::frozen::SkillSource::Plugin => "plugin",
                    },
                },
                Command::McpPrompt { description, .. } => CommandInfo {
                    name: name.clone(),
                    description: description.clone(),
                    kind: "mcp-prompt",
                    source: "mcp",
                },
            })
            .collect();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        entries
    }
}

/// Metadata about one registered command, for callers that need to display
/// or enumerate the command catalog without executing anything (e.g. an
/// application layer rendering a command palette via daemon's
/// `commands.list` RPC — see `CommandRegistry::list_detailed`).
#[derive(Debug, Clone, serde::Serialize)]
pub struct CommandInfo {
    pub name: String,
    pub description: String,
    /// `"prompt"` (expands a skill body, continues to the LLM) or `"local"`
    /// (executes immediately, no LLM turn).
    pub kind: &'static str,
    /// Where the command came from: `"builtin"`, `"user"`, `"project"`, or
    /// `"plugin"`.
    pub source: &'static str,
}

impl Default for CommandRegistry {
    fn default() -> Self {
        let mut registry = Self::new();
        registry.insert_local("help", "List all available slash commands");
        registry.insert_local("skills", "List all available skills");
        registry.insert_local("clear", "Clear the current session context");
        registry.insert_local("compact", "Trigger context compaction now");
        registry.insert_local("cost", "Show session API cost");
        registry
    }
}

/// Expand a skill entry for a slash command invocation: substitute `{args}`
/// into `body` (see [`CommandRegistry::skill_body`]) and wrap it in the
/// invocation header. With no body, the entry's own name and description stand
/// in for it.
pub fn expand_skill_for_command(entry: &SkillEntry, body: Option<String>, args: &str) -> String {
    let body = body.unwrap_or_else(|| format!("# {}\n\n{}", entry.name, entry.description));

    // Expand variables: {args}, $ARGUMENTS, $name (named `arguments:`), etc.
    let expanded = base::frozen::skill::expand_skill_vars_named(
        &body,
        args,
        entry.arguments.as_deref().unwrap_or(&[]),
    );

    // Wrap with invocation header (command-message/command-name XML tags).
    format!(
        "\n<command-message>{name} is running...</command-message>\n\
         <command-name>{name}{args_suffix}</command-name>\n\
         {expanded}",
        name = entry.name,
        args_suffix = if args.is_empty() {
            String::new()
        } else {
            format!(" {}", args)
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_command() {
        let result = parse_slash_command("/simplify").unwrap();
        assert_eq!(result.name, "simplify");
        assert_eq!(result.args, "");
    }

    #[test]
    fn parse_command_with_args() {
        let result = parse_slash_command("/simplify src/main.rs").unwrap();
        assert_eq!(result.name, "simplify");
        assert_eq!(result.args, "src/main.rs");
    }

    #[test]
    fn parse_command_with_multi_word_args() {
        let result = parse_slash_command("/debug the auth null pointer").unwrap();
        assert_eq!(result.name, "debug");
        assert_eq!(result.args, "the auth null pointer");
    }

    #[test]
    fn parse_rejects_non_slash() {
        assert!(parse_slash_command("hello").is_none());
        assert!(parse_slash_command("").is_none());
    }

    #[test]
    fn parse_rejects_bare_slash() {
        assert!(parse_slash_command("/").is_none());
    }

    #[test]
    fn registry_default_has_builtins() {
        let registry = CommandRegistry::default();
        assert!(registry.resolve("help").is_some());
        assert!(registry.resolve("skills").is_some());
        assert!(registry.resolve("clear").is_some());
        assert!(registry.resolve("compact").is_some());
        assert!(registry.resolve("cost").is_some());
    }

    #[test]
    fn resolve_unknown_returns_none() {
        let registry = CommandRegistry::default();
        assert!(registry.resolve("nonexistent").is_none());
    }

    #[test]
    fn list_detailed_reports_kind_and_source() {
        let mut registry = CommandRegistry::default();
        registry.insert_prompt(SkillEntry {
            name: "review".into(),
            description: "Review a diff".into(),
            source: base::frozen::SkillSource::Plugin,
            path: std::path::PathBuf::from("(plugin:code-review:review)"),
            user_invocable: true,
            ..Default::default()
        });
        let entries = registry.list_detailed();

        let help = entries.iter().find(|e| e.name == "help").unwrap();
        assert_eq!(help.kind, "local");
        assert_eq!(help.source, "builtin");

        let review = entries.iter().find(|e| e.name == "review").unwrap();
        assert_eq!(review.kind, "prompt");
        assert_eq!(review.source, "plugin");

        // Sorted by name.
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
    }

    fn mcp_prompt_entry() -> base::interface::mcp_servers::McpPromptCommand {
        base::interface::mcp_servers::McpPromptCommand {
            command: "mcp__github__review_pr".into(),
            description: "Review a pull request".into(),
            argument_hint: "repo=<value> [note=<value>]".into(),
        }
    }

    #[test]
    fn mcp_prompts_register_as_slash_commands() {
        let mut registry = CommandRegistry::default();
        registry.register_mcp_prompts(&[mcp_prompt_entry()]);

        let cmd = registry
            .resolve("mcp__github__review_pr")
            .expect("MCP prompt should resolve as a slash command");
        assert!(cmd.is_prompt());
        assert_eq!(cmd.description(), "Review a pull request");
        assert_eq!(
            registry.argument_hint("mcp__github__review_pr"),
            Some("repo=<value> [note=<value>]".to_string())
        );
        // Discoverable via /help ...
        assert!(registry
            .list()
            .iter()
            .any(|(n, _)| *n == "mcp__github__review_pr"));
        // ... and via the daemon's commands.list RPC, tagged as MCP.
        let info = registry
            .list_detailed()
            .into_iter()
            .find(|c| c.name == "mcp__github__review_pr")
            .unwrap();
        assert_eq!(info.kind, "mcp-prompt");
        assert_eq!(info.source, "mcp");
    }

    #[test]
    fn registering_mcp_prompts_is_idempotent() {
        let mut registry = CommandRegistry::default();
        let before = registry.len();
        registry.register_mcp_prompts(&[mcp_prompt_entry()]);
        registry.register_mcp_prompts(&[mcp_prompt_entry()]);
        assert_eq!(registry.len(), before + 1);
    }

    #[test]
    fn prompt_command_expands_bundled_skill() {
        let entry = SkillEntry {
            name: "test-skill".into(),
            description: "A test skill".into(),
            source: base::frozen::SkillSource::User,
            path: std::path::PathBuf::from("(bundled:test-skill)"),
            ..Default::default()
        };
        let expanded = CommandRegistry::default()
            .handle_prompt_command(&entry, &parse_slash_command("/test-skill hello").unwrap());
        assert!(expanded.contains("test-skill"));
        assert!(!expanded.is_empty());
    }
}
