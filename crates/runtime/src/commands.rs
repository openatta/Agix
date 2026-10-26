//! Slash command system — intercept `/name args` before the LLM.
//!
//! Supports two command types:
//! - **Prompt**: expand skill content, replace user message, continue to LLM
//! - **Local**: execute handler, return result directly, skip LLM
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
use std::collections::HashMap;
use std::sync::Arc;

// ── Parsed slash command ──

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

// ── Command types ──

/// The result of executing a local command.
#[derive(Debug, Clone)]
pub struct CommandResult {
    /// Text to display to the user (goes via AgentEvent).
    pub text: String,
    /// Whether to continue to the LLM after this command.
    pub should_query: bool,
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
    /// Local command: execute handler, return result, skip LLM.
    Local {
        description: String,
        handler: Arc<dyn Fn(&SlashCommand) -> CommandResult + Send + Sync>,
    },
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

// ── Command registry ──

/// Registry of all available slash commands.
///
/// Skill-derived commands are **not** stored here — they are resolved
/// through the live [`SkillManager`](::skills::manager::SkillManager) on
/// every lookup. That manager reloads changed files in place (the turn loop
/// polls `check_for_changes()` before each turn), so any copy taken at
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
    skills: Option<Arc<::skills::manager::SkillManager>>,
}

/// Skill-derived slash commands only exist for `user_invocable` skills.
fn skill_as_command(skill: &::skills::manager::SkillInfo) -> Option<Command> {
    if !skill.user_invocable {
        return None;
    }
    Some(Command::Prompt {
        entry: Box::new(SkillEntry {
            name: skill.name.clone(),
            description: skill.description.clone(),
            when_to_use: skill.when_to_use.clone(),
            source: match skill.source {
                ::skills::manager::SkillSource::User => base::frozen::SkillSource::User,
                ::skills::manager::SkillSource::Project => base::frozen::SkillSource::Project,
                ::skills::manager::SkillSource::Plugin => base::frozen::SkillSource::Plugin,
            },
            path: skill.path.clone(),
            argument_hint: skill.argument_hint.clone(),
            allowed_tools: skill.allowed_tools.clone(),
            disallowed_tools: skill.disallowed_tools.clone(),
            arguments: skill.arguments.clone(),
            model: skill.model.clone(),
            effort: skill.effort.clone(),
            context: skill.context.clone(),
            agent: skill.agent.clone(),
            background: skill.background,
            disable_model_invocation: skill.disable_model_invocation,
            user_invocable: skill.user_invocable,
            paths: skill.paths.clone(),
            version: skill.version.clone(),
            // `SkillInfo` has no counterpart for these two — they are dropped
            // by `SkillEntry -> SkillInfo` on load, so the catalog cannot
            // give them back. Fields listed exhaustively (no
            // `..Default::default()`) so that adding one to `SkillEntry`
            // fails the build here instead of silently arriving as `None`,
            // which is how `arguments` went missing and quietly disabled
            // named-argument expansion for every slash command.
            files: None,
            hooks: None,
        }),
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
    pub fn from_skill_manager(skill_mgr: Arc<::skills::manager::SkillManager>) -> Self {
        Self {
            skills: Some(skill_mgr),
            ..Self::default()
        }
    }

    /// Register every MCP server prompt as a `/mcp__<server>__<prompt>` slash
    /// command — the same treatment `from_skill_manager` gives skills, so MCP
    /// prompts show up in `/help`, in `commands.list`, and resolve in the
    /// turn loop's slash-command interception.
    ///
    /// Called with `McpManager::all_prompts()` once the servers are connected
    /// (see `Builder::build`). Re-registering is idempotent: names are stable,
    /// so a later call after `refresh_prompts` overwrites in place.
    pub fn register_mcp_prompts(&mut self, prompts: &[::mcp::manager::McpPromptEntry]) {
        for p in prompts {
            let command = p.command_name();
            self.registered.insert(
                command.clone(),
                Command::McpPrompt {
                    command,
                    description: if p.description.is_empty() {
                        format!("MCP prompt `{}` from server `{}`", p.name, p.server)
                    } else {
                        p.description.clone()
                    },
                    argument_hint: p.argument_hint(),
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

    /// Insert a local command with a handler.
    pub fn insert_local(
        &mut self,
        name: &str,
        description: &str,
        handler: Arc<dyn Fn(&SlashCommand) -> CommandResult + Send + Sync>,
    ) {
        self.builtin.insert(
            name.to_string(),
            Command::Local {
                description: description.to_string(),
                handler,
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
            .as_ref()
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
        if let Some(body) = self
            .skills
            .as_ref()
            .and_then(|s| s.get_skill_content(&entry.name))
        {
            return Some(body);
        }
        if entry.path.to_string_lossy().starts_with("(bundled:") {
            return ::skills::bundled::bundled_body(&entry.name);
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
                if let Some(cmd) = skill_as_command(&skill) {
                    out.insert(skill.name.clone(), cmd);
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
        // Register built-in local commands
        registry.insert_local(
            "help",
            "List all available slash commands",
            Arc::new(|_cmd| {
                // Handler is filled by the caller with registry reference
                CommandResult {
                    text: "Use /help to see available commands".into(),
                    should_query: false,
                }
            }),
        );
        registry.insert_local(
            "skills",
            "List all available skills",
            Arc::new(|_cmd| CommandResult {
                text: "Use /skills to see available skills".into(),
                should_query: false,
            }),
        );
        registry.insert_local(
            "clear",
            "Clear the current session context",
            Arc::new(|_cmd| CommandResult {
                text: "Session cleared. All messages have been removed.".into(),
                should_query: false,
            }),
        );
        registry.insert_local(
            "compact",
            "Trigger context compaction now",
            Arc::new(|_cmd| CommandResult {
                text: "Compaction triggered. Context has been summarized.".into(),
                should_query: true,
            }),
        );
        registry.insert_local(
            "cost",
            "Show session API cost",
            Arc::new(|_cmd| CommandResult {
                text: "Cost tracking: use /cost for details".into(),
                should_query: false,
            }),
        );
        registry
    }
}

// ── Skill expansion ──

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

/// Handle a local command: execute the handler and return the result.
pub fn handle_local_command(cmd: &Command, sc: &SlashCommand) -> CommandResult {
    match cmd {
        Command::Local { handler, .. } => handler(sc),
        _ => CommandResult {
            text: "Internal error: expected local command".into(),
            should_query: false,
        },
    }
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
    fn from_skill_manager_also_has_builtins() {
        // Regression test: `from_skill_manager` previously started from
        // `Self::new()` (empty), so the production registry (built via this
        // constructor in `Builder::build()`) never actually contained the 5
        // built-in local commands — `/help` et al. silently fell through to
        // the LLM instead of executing.
        let skill_mgr = Arc::new(::skills::manager::SkillManager::new());
        let registry = CommandRegistry::from_skill_manager(skill_mgr);
        assert!(registry.resolve("help").is_some());
        assert!(registry.resolve("skills").is_some());
        assert!(registry.resolve("clear").is_some());
        assert!(registry.resolve("compact").is_some());
        assert!(registry.resolve("cost").is_some());
    }

    fn write_skill(dir: &std::path::Path, name: &str, description: &str) {
        let skill_dir = dir.join(name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\n\nbody\n"),
        )
        .unwrap();
    }

    /// The registry must reflect skills added to its manager *after* it was
    /// built. It used to copy `skill_mgr.list()` into a `HashMap` once, so a
    /// session (or, worse, the daemon's pool-wide catalog, built at startup
    /// and shared by every session) kept resolving the skill set it saw at
    /// construction for the rest of its life — a skill file created during
    /// the session was reloaded into the `SkillManager` by the turn loop and
    /// still could not be invoked as a slash command.
    #[test]
    fn registry_sees_skills_added_after_construction() {
        let dir = tempfile::tempdir().unwrap();
        let skill_mgr = Arc::new(::skills::manager::SkillManager::new());
        skill_mgr
            .load_dir_subdirs(dir.path(), ::skills::manager::SkillSource::Project)
            .unwrap();
        let registry = CommandRegistry::from_skill_manager(skill_mgr.clone());
        assert!(registry.resolve("late-arrival").is_none());

        write_skill(dir.path(), "late-arrival", "shows up mid-session");
        skill_mgr
            .load_dir_subdirs(dir.path(), ::skills::manager::SkillSource::Project)
            .unwrap();

        let cmd = registry
            .resolve("late-arrival")
            .expect("a skill loaded after the registry was built must still resolve");
        assert!(cmd.is_prompt());
        assert_eq!(cmd.description(), "shows up mid-session");
        assert!(registry.list().iter().any(|(n, _)| n == "late-arrival"));
        assert!(registry
            .list_detailed()
            .iter()
            .any(|c| c.name == "late-arrival" && c.source == "project"));
    }

    /// The other half of liveness: a deleted skill must stop resolving.
    /// Deletion is what the watcher reports through `check_for_changes`, so
    /// this goes through the real reload path rather than a second
    /// `load_dir_subdirs`.
    #[test]
    fn registry_drops_skills_removed_after_construction() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "doomed", "about to be deleted");
        let skill_mgr = Arc::new(::skills::manager::SkillManager::new());
        skill_mgr
            .load_dir_subdirs(dir.path(), ::skills::manager::SkillSource::Project)
            .unwrap();
        skill_mgr
            .enable_watching(&[dir.path().to_path_buf()])
            .unwrap();
        let registry = CommandRegistry::from_skill_manager(skill_mgr.clone());
        assert!(registry.resolve("doomed").is_some());

        std::fs::remove_dir_all(dir.path().join("doomed")).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline && registry.resolve("doomed").is_some() {
            skill_mgr.check_for_changes();
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(
            registry.resolve("doomed").is_none(),
            "a deleted skill must stop resolving as a slash command"
        );
        assert!(!registry.list().iter().any(|(n, _)| n == "doomed"));
    }

    /// Skills are not user-facing commands unless they say so — the filter
    /// moved from build time to lookup time and has to survive the move.
    #[test]
    fn non_user_invocable_skills_are_not_commands() {
        let skill_mgr = Arc::new(::skills::manager::SkillManager::new());
        skill_mgr.register_bundled(SkillEntry {
            name: "internal-only".into(),
            description: "model-facing".into(),
            user_invocable: false,
            ..Default::default()
        });
        let registry = CommandRegistry::from_skill_manager(skill_mgr);
        assert!(registry.resolve("internal-only").is_none());
        assert!(!registry.list().iter().any(|(n, _)| n == "internal-only"));
    }

    /// Precedence, highest first: explicitly registered (plugin/MCP) →
    /// skills → built-ins. This is the order the old single-map build
    /// sequence produced by insertion (`Default` → skills → plugins), and
    /// the split into tiers has to preserve it.
    #[test]
    fn registered_commands_shadow_skills_which_shadow_builtins() {
        let skill_mgr = Arc::new(::skills::manager::SkillManager::new());
        skill_mgr.register_bundled(SkillEntry {
            name: "help".into(),
            description: "skill help".into(),
            user_invocable: true,
            ..Default::default()
        });
        skill_mgr.register_bundled(SkillEntry {
            name: "review".into(),
            description: "skill review".into(),
            user_invocable: true,
            ..Default::default()
        });
        let mut registry = CommandRegistry::from_skill_manager(skill_mgr);
        registry.insert_prompt(SkillEntry {
            name: "review".into(),
            description: "plugin review".into(),
            user_invocable: true,
            ..Default::default()
        });

        assert_eq!(
            registry.resolve("help").unwrap().description(),
            "skill help"
        );
        assert_eq!(
            registry.resolve("review").unwrap().description(),
            "plugin review"
        );
        // Shadowed names collapse to one entry, not two.
        assert_eq!(
            registry
                .list()
                .iter()
                .filter(|(n, _)| n == "review")
                .count(),
            1
        );
    }

    /// `arguments:` was silently dropped by the old skill → command copy
    /// (`..Default::default()` after listing only some fields), so `$name`
    /// substitution in a skill body never worked through a slash command.
    #[test]
    fn skill_derived_commands_carry_named_arguments() {
        let skill_mgr = Arc::new(::skills::manager::SkillManager::new());
        skill_mgr.register_bundled(SkillEntry {
            name: "ship".into(),
            description: "ship it".into(),
            arguments: Some(vec!["issue".into(), "branch".into()]),
            user_invocable: true,
            ..Default::default()
        });
        let registry = CommandRegistry::from_skill_manager(skill_mgr);
        match registry.resolve("ship").unwrap() {
            Command::Prompt { entry } => {
                assert_eq!(
                    entry.arguments.as_deref(),
                    Some(&["issue".to_string(), "branch".to_string()][..])
                );
            }
            _ => panic!("expected a prompt command"),
        }
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

    fn mcp_prompt_entry() -> ::mcp::manager::McpPromptEntry {
        ::mcp::manager::McpPromptEntry {
            server: "github".into(),
            name: "review_pr".into(),
            description: "Review a pull request".into(),
            arguments: vec![
                ::mcp::client::McpPromptArg {
                    name: "repo".into(),
                    description: None,
                    required: Some(true),
                },
                ::mcp::client::McpPromptArg {
                    name: "note".into(),
                    description: None,
                    required: Some(false),
                },
            ],
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
