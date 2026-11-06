//! `DeclarativeScene` — a scene described by files instead of Rust source.
//!
//! A scene directory holds `scene.json` (identity, tool surface, budgets,
//! modules, reminders) and `prompt.md` (the system prompt body). A host loads
//! its own from `~/.agix/scenes/<id>/` with [`load_all`]; a plugin package
//! ships one and loads it with [`DeclarativeScene::load_for_plugin`]. Either
//! way the result is an ordinary [`Scene`] that goes into a
//! [`SceneRegistry`](crate::SceneRegistry). The format is specified in
//! `docs/extension_points.md` ("声明式场景").
//!
//! Both files are read and validated once, at load: a field this build does
//! not know or a missing required one is an error naming the file — never a
//! default, and never a re-read under a running session.
//!
//! `prompt.md` may use the placeholders in [`PLACEHOLDERS`], which the scene
//! fills per call. Any other `{{name}}` is left as written for the kernel's
//! `prompt.variable` registrations to expand after assembly, so it cannot be
//! refused here: whether it resolves depends on what the host registered.

use base::interface::prompt::PromptBlock;
use base::interface::scene::{
    ExecutionParams, ReminderContext, Scene, ScenePromptContext, TokenBudget,
};
use base::tool::Tool;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const SCENE_FILE: &str = "scene.json";
pub const PROMPT_FILE: &str = "prompt.md";

pub const DEFAULT_COMPACT_THRESHOLD: usize = 150_000;
pub const DEFAULT_COMPACT_KEEP_RECENT: usize = 20;

const MAX_ID_LEN: usize = 64;

/// The `{{name}}`s the scene fills itself, from the session's prompt context.
pub const PLACEHOLDERS: &[&str] = &["cwd", "os", "shell", "home", "date", "model", "environment"];

#[derive(Debug, thiserror::Error)]
pub enum SceneLoadError {
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{}: {source}", path.display())]
    Parse {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("{}: {message}", path.display())]
    Invalid { path: PathBuf, message: String },
    #[error(
        "{}: a scene with id `{id}` is already registered, and a declarative scene \
         never replaces one",
        path.display()
    )]
    IdTaken { path: PathBuf, id: String },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SceneFile {
    id: String,
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    tools: Vec<String>,
    #[serde(default)]
    disallowed_tools: Vec<String>,
    #[serde(default)]
    deferred_tools: Vec<String>,
    #[serde(default)]
    token_budget: BudgetFile,
    #[serde(default)]
    execution: ExecutionFile,
    #[serde(default)]
    requires_project: bool,
    #[serde(default)]
    supports_team: bool,
    #[serde(default)]
    modules: Option<Vec<String>>,
    #[serde(default)]
    disabled_modules: Vec<String>,
    #[serde(default)]
    reminders: Vec<Reminder>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct BudgetFile {
    compact_threshold: Option<usize>,
    compact_keep_recent: Option<usize>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExecutionFile {
    max_api_calls_per_turn: Option<u32>,
}

/// What the per-turn `<system-reminder>` carries, in the order listed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reminder {
    GitStatus,
    Memory,
}

enum IdRule<'a> {
    DirSafe,
    Plugin(&'a str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Placeholder {
    Cwd,
    Os,
    Shell,
    Home,
    Date,
    Model,
    Environment,
}

impl Placeholder {
    fn named(name: &str) -> Option<Self> {
        Some(match name {
            "cwd" => Self::Cwd,
            "os" => Self::Os,
            "shell" => Self::Shell,
            "home" => Self::Home,
            "date" => Self::Date,
            "model" => Self::Model,
            "environment" => Self::Environment,
            _ => return None,
        })
    }

    fn render(self, ctx: &ScenePromptContext) -> String {
        match self {
            Self::Cwd => ctx.cwd.to_string(),
            Self::Os => ctx.os.to_string(),
            Self::Shell => ctx.shell.to_string(),
            Self::Home => ctx.home_dir.to_string(),
            Self::Date => ctx.date.to_string(),
            Self::Model => ctx.model_name.to_string(),
            Self::Environment => environment_block(ctx),
        }
    }
}

#[derive(Debug, Clone)]
enum Piece {
    Text(String),
    Value(Placeholder),
}

/// `prompt.md`, split where it starts to depend on the call: everything
/// before the line holding the first placeholder the scene fills is `fixed`
/// and cacheable, the rest is rendered every time.
#[derive(Debug, Clone)]
struct Prompt {
    raw: String,
    fixed: String,
    varying: Vec<Piece>,
}

impl Prompt {
    fn parse(raw: &str) -> Result<Self, String> {
        if raw.trim().is_empty() {
            return Err("is empty; a scene needs a system prompt".into());
        }
        let Some(first) = tokenize(raw).iter().find_map(|(at, p)| match p {
            Piece::Value(_) => Some(*at),
            Piece::Text(_) => None,
        }) else {
            return Ok(Self {
                raw: raw.to_string(),
                fixed: raw.trim().to_string(),
                varying: Vec::new(),
            });
        };
        let line_start = raw[..first].rfind('\n').map_or(0, |i| i + 1);
        Ok(Self {
            raw: raw.to_string(),
            fixed: raw[..line_start].trim().to_string(),
            varying: tokenize(&raw[line_start..])
                .into_iter()
                .map(|(_, p)| p)
                .collect(),
        })
    }
}

/// Every `{{name}}` whose name is in [`PLACEHOLDERS`] becomes a value; the
/// rest of the text, other `{{...}}` included, stays as written.
fn tokenize(raw: &str) -> Vec<(usize, Piece)> {
    let mut out = Vec::new();
    let mut text_from = 0;
    let mut cursor = 0;
    while let Some(found) = raw[cursor..].find("{{") {
        let open = cursor + found;
        let inner_from = open + 2;
        let Some(len) = raw[inner_from..].find("}}") else {
            break;
        };
        let Some(value) = Placeholder::named(&raw[inner_from..inner_from + len]) else {
            cursor = inner_from;
            continue;
        };
        if text_from < open {
            out.push((text_from, Piece::Text(raw[text_from..open].to_string())));
        }
        out.push((open, Piece::Value(value)));
        cursor = inner_from + len + 2;
        text_from = cursor;
    }
    if text_from < raw.len() {
        out.push((text_from, Piece::Text(raw[text_from..].to_string())));
    }
    out
}

/// The environment facts a scene states when its prompt does not place them
/// itself — also what `{{environment}}` expands to.
pub fn environment_block(ctx: &ScenePromptContext) -> String {
    let mut out = format!(
        "# Environment\nWorking directory: {}\nPlatform: {}\nShell: {}\nToday's date: {}",
        ctx.cwd, ctx.os, ctx.shell, ctx.date
    );
    if let Some(lang) = &ctx.language {
        out.push_str(&format!("\nUser language preference: {lang}"));
    }
    out
}

pub struct DeclarativeScene {
    dir: PathBuf,
    id: String,
    name: String,
    description: String,
    prompt: Prompt,
    tools: Vec<String>,
    disallowed_tools: Vec<String>,
    deferred_tools: Vec<String>,
    budget: TokenBudget,
    execution: ExecutionParams,
    requires_project: bool,
    supports_team: bool,
    modules: Option<Vec<String>>,
    disabled_modules: Vec<String>,
    reminders: Vec<Reminder>,
    extra_tools: Vec<Arc<dyn Tool>>,
}

impl std::fmt::Debug for DeclarativeScene {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeclarativeScene")
            .field("dir", &self.dir)
            .field("id", &self.id)
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl DeclarativeScene {
    /// Load the scene in `dir`. Its id must be a directory-safe name.
    pub fn load(dir: &Path) -> Result<Self, SceneLoadError> {
        Self::read(dir, IdRule::DirSafe)
    }

    /// Load the scene a plugin package ships in `dir`. Its id must be
    /// `plugin:<plugin>`: a plugin owns exactly one scene, under its own
    /// name, and never one a user or another plugin could enter thinking it
    /// was something else.
    pub fn load_for_plugin(dir: &Path, plugin: &str) -> Result<Self, SceneLoadError> {
        Self::read(dir, IdRule::Plugin(plugin))
    }

    /// Build from the two files' contents, for a scene compiled into the
    /// binary. `dir` only names where they came from in error messages.
    pub fn from_sources(
        dir: &Path,
        scene_json: &str,
        prompt_md: &str,
    ) -> Result<Self, SceneLoadError> {
        Self::parse(dir, scene_json, prompt_md, IdRule::DirSafe)
    }

    /// Tools this scene registers into its sessions — a plugin's own
    /// exports, so its scene can name them. See [`Scene::extra_tools`].
    pub fn with_extra_tools(mut self, tools: Vec<Arc<dyn Tool>>) -> Self {
        self.extra_tools = tools;
        self
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// `prompt.md` as written, before any placeholder is filled — what an
    /// installer shows the user.
    pub fn prompt(&self) -> &str {
        &self.prompt.raw
    }

    fn read(dir: &Path, rule: IdRule<'_>) -> Result<Self, SceneLoadError> {
        let read = |name: &str| {
            let path = dir.join(name);
            std::fs::read_to_string(&path).map_err(|source| SceneLoadError::Io { path, source })
        };
        let scene_json = read(SCENE_FILE)?;
        let prompt_md = read(PROMPT_FILE)?;
        Self::parse(dir, &scene_json, &prompt_md, rule)
    }

    fn parse(
        dir: &Path,
        scene_json: &str,
        prompt_md: &str,
        rule: IdRule<'_>,
    ) -> Result<Self, SceneLoadError> {
        let scene_path = dir.join(SCENE_FILE);
        let invalid = |message: String| SceneLoadError::Invalid {
            path: scene_path.clone(),
            message,
        };
        let file: SceneFile =
            serde_json::from_str(scene_json).map_err(|source| SceneLoadError::Parse {
                path: scene_path.clone(),
                source,
            })?;

        check_id(&file.id, &rule).map_err(invalid)?;
        if file.name.trim().is_empty() {
            return Err(invalid("`name` must not be empty".into()));
        }
        check_names("tools", &file.tools, true).map_err(invalid)?;
        check_names("disallowed_tools", &file.disallowed_tools, true).map_err(invalid)?;
        check_names("deferred_tools", &file.deferred_tools, false).map_err(invalid)?;
        check_disjoint(
            ("tools", &file.tools),
            ("disallowed_tools", &file.disallowed_tools),
        )
        .map_err(invalid)?;
        if let Some(modules) = &file.modules {
            check_names("modules", modules, false).map_err(invalid)?;
            check_disjoint(
                ("modules", modules),
                ("disabled_modules", &file.disabled_modules),
            )
            .map_err(invalid)?;
        }
        check_names("disabled_modules", &file.disabled_modules, false).map_err(invalid)?;
        for (i, r) in file.reminders.iter().enumerate() {
            if file.reminders[..i].contains(r) {
                return Err(invalid(format!("`reminders` lists {r:?} twice")));
            }
        }
        if file.execution.max_api_calls_per_turn == Some(0) {
            return Err(invalid(
                "`execution.max_api_calls_per_turn` must be at least 1; leave it out \
                 for no ceiling"
                    .into(),
            ));
        }

        let prompt = Prompt::parse(prompt_md).map_err(|message| SceneLoadError::Invalid {
            path: dir.join(PROMPT_FILE),
            message,
        })?;

        Ok(Self {
            dir: dir.to_path_buf(),
            id: file.id,
            name: file.name,
            description: file.description,
            prompt,
            tools: file.tools,
            disallowed_tools: file.disallowed_tools,
            deferred_tools: file.deferred_tools,
            budget: TokenBudget {
                compact_threshold: file
                    .token_budget
                    .compact_threshold
                    .unwrap_or(DEFAULT_COMPACT_THRESHOLD),
                compact_keep_recent: file
                    .token_budget
                    .compact_keep_recent
                    .unwrap_or(DEFAULT_COMPACT_KEEP_RECENT),
            },
            execution: ExecutionParams {
                max_api_calls_per_turn: file
                    .execution
                    .max_api_calls_per_turn
                    .unwrap_or(ExecutionParams::default().max_api_calls_per_turn),
            },
            requires_project: file.requires_project,
            supports_team: file.supports_team,
            modules: file.modules,
            disabled_modules: file.disabled_modules,
            reminders: file.reminders,
            extra_tools: Vec::new(),
        })
    }
}

/// Every scene under `root`, one per `<root>/<id>/scene.json`, in directory
/// name order.
///
/// A subdirectory with no `scene.json` is not a scene and is passed over:
/// `~/.agix/scenes/<id>/` also holds a scene's settings, plugins and skills,
/// built-in scenes included. One holding a `prompt.md` and no `scene.json`
/// is reported, since that is a scene missing half of itself. A missing
/// `root` holds no scenes.
pub fn load_all(root: &Path) -> Vec<Result<DeclarativeScene, SceneLoadError>> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(source) => {
            return vec![Err(SceneLoadError::Io {
                path: root.to_path_buf(),
                source,
            })]
        }
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| !n.starts_with('.'))
        })
        .collect();
    dirs.sort();

    let mut out = Vec::new();
    for dir in dirs {
        if !dir.join(SCENE_FILE).is_file() {
            if dir.join(PROMPT_FILE).is_file() {
                out.push(Err(SceneLoadError::Invalid {
                    path: dir.join(SCENE_FILE),
                    message: format!("is missing, but {PROMPT_FILE} is there"),
                }));
            }
            continue;
        }
        out.push(DeclarativeScene::load(&dir).and_then(|scene| {
            let dir_name = dir.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if scene.id == dir_name {
                Ok(scene)
            } else {
                Err(SceneLoadError::Invalid {
                    path: dir.join(SCENE_FILE),
                    message: format!(
                        "`id` is {:?} but the directory is named {dir_name:?}; they must match",
                        scene.id
                    ),
                })
            }
        }));
    }
    out
}

fn check_id(id: &str, rule: &IdRule<'_>) -> Result<(), String> {
    match rule {
        IdRule::Plugin(plugin) => {
            let expected = format!("plugin:{plugin}");
            if id == expected {
                Ok(())
            } else {
                Err(format!(
                    "`id` is {id:?}; a scene shipped by plugin `{plugin}` must be {expected:?}"
                ))
            }
        }
        IdRule::DirSafe => {
            let valid_char = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
            let ok = id.len() <= MAX_ID_LEN
                && id.chars().next().is_some_and(valid_char)
                && id.chars().all(|c| valid_char(c) || c == '-' || c == '_');
            if ok {
                Ok(())
            } else {
                Err(format!(
                    "`id` {id:?} is not a scene id: 1 to {MAX_ID_LEN} characters of a-z, 0-9, \
                     `-` and `_`, starting with a letter or digit"
                ))
            }
        }
    }
}

fn check_names(field: &str, names: &[String], wildcard: bool) -> Result<(), String> {
    for (i, name) in names.iter().enumerate() {
        if name.trim().is_empty() {
            return Err(format!("`{field}` has an empty entry"));
        }
        let stars = name.matches('*').count();
        if stars > 0 && !(wildcard && stars == 1 && name.ends_with('*')) {
            return Err(if wildcard {
                format!("`{field}` entry {name:?}: `*` is only allowed once, at the end")
            } else {
                format!("`{field}` entry {name:?}: `*` is not allowed here")
            });
        }
        if names[..i].contains(name) {
            return Err(format!("`{field}` lists {name:?} twice"));
        }
    }
    Ok(())
}

fn check_disjoint(a: (&str, &[String]), b: (&str, &[String])) -> Result<(), String> {
    match a.1.iter().find(|n| b.1.contains(n)) {
        Some(both) => Err(format!("{both:?} is in both `{}` and `{}`", a.0, b.0)),
        None => Ok(()),
    }
}

impl Scene for DeclarativeScene {
    fn id(&self) -> &str {
        &self.id
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn build_system_prompt(&self, ctx: &ScenePromptContext) -> Vec<PromptBlock> {
        let mut blocks = Vec::new();
        if !self.prompt.fixed.is_empty() {
            blocks.push(PromptBlock::system_cached(self.prompt.fixed.clone()));
        }
        let varying = if self.prompt.varying.is_empty() {
            environment_block(ctx)
        } else {
            let mut out = String::new();
            for piece in &self.prompt.varying {
                match piece {
                    Piece::Text(text) => out.push_str(text),
                    Piece::Value(value) => out.push_str(&value.render(ctx)),
                }
            }
            out.trim_end().to_string()
        };
        blocks.push(PromptBlock::system(varying));
        blocks
    }

    fn tools(&self) -> Vec<String> {
        self.tools.clone()
    }

    /// Unlike the trait's default, `disallowed_tools` still applies under a
    /// non-empty whitelist, so `"tools": ["mcp__*"]` with
    /// `"disallowed_tools": ["mcp__shell__*"]` means what it says.
    fn permits_tool(&self, name: &str) -> bool {
        let covers = |entry: &String| match entry.strip_suffix('*') {
            Some(prefix) => name.starts_with(prefix),
            None => entry == name,
        };
        (self.tools.is_empty() || self.tools.iter().any(covers))
            && !self.disallowed_tools.iter().any(covers)
    }

    fn disallowed_tools(&self) -> Vec<String> {
        self.disallowed_tools.clone()
    }

    fn deferred_tools(&self) -> Vec<String> {
        self.deferred_tools.clone()
    }

    fn extra_tools(&self) -> Vec<Arc<dyn Tool>> {
        self.extra_tools.clone()
    }

    fn token_budget(&self) -> TokenBudget {
        self.budget.clone()
    }

    fn execution_params(&self) -> ExecutionParams {
        self.execution.clone()
    }

    fn requires_project(&self) -> bool {
        self.requires_project
    }

    fn supports_team(&self) -> bool {
        self.supports_team
    }

    fn modules(&self) -> Option<Vec<String>> {
        self.modules.clone()
    }

    fn disabled_modules(&self) -> Vec<String> {
        self.disabled_modules.clone()
    }

    fn build_system_reminder(&self, ctx: &ReminderContext) -> String {
        let mut out = String::new();
        for reminder in &self.reminders {
            let body = match reminder {
                Reminder::GitStatus => ctx.git_status.as_deref(),
                Reminder::Memory => ctx.memory_summary.as_deref(),
            };
            if let Some(body) = body {
                out.push_str(&format!("\n<system-reminder>\n{body}\n</system-reminder>"));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::borrow::Cow;

    const MINIMAL: &str = r#"{"id": "review", "name": "Review"}"#;

    fn ctx() -> ScenePromptContext<'static> {
        ScenePromptContext {
            cwd: Cow::Borrowed("/work"),
            os: Cow::Borrowed("darwin"),
            shell: Cow::Borrowed("zsh"),
            home_dir: Cow::Borrowed("/home/me"),
            date: Cow::Borrowed("2026-11-05"),
            model_name: Cow::Borrowed("claude-test"),
            skills_text: None,
            mcp_instructions: None,
            session_memory: None,
            is_git: false,
            git_branch: None,
            is_worktree: false,
            git_status: None,
            language: None,
            scratchpad_dir: None,
            output_style_content: None,
            available_tools: None,
            tool_results_ever_cleared: false,
        }
    }

    fn parse(scene_json: &str, prompt_md: &str) -> Result<DeclarativeScene, SceneLoadError> {
        DeclarativeScene::from_sources(Path::new("/scenes/review"), scene_json, prompt_md)
    }

    fn error(scene_json: &str, prompt_md: &str) -> String {
        parse(scene_json, prompt_md).unwrap_err().to_string()
    }

    fn write(dir: &Path, scene_json: &str, prompt_md: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(SCENE_FILE), scene_json).unwrap();
        std::fs::write(dir.join(PROMPT_FILE), prompt_md).unwrap();
    }

    #[test]
    fn a_minimal_scene_takes_the_documented_defaults() {
        let s = parse(MINIMAL, "You review code.").unwrap();
        assert_eq!(s.id(), "review");
        assert_eq!(s.name(), "Review");
        assert_eq!(s.description(), "");
        assert!(s.tools().is_empty());
        assert!(s.disallowed_tools().is_empty());
        assert!(s.deferred_tools().is_empty());
        assert_eq!(
            s.token_budget().compact_threshold,
            DEFAULT_COMPACT_THRESHOLD
        );
        assert_eq!(
            s.token_budget().compact_keep_recent,
            DEFAULT_COMPACT_KEEP_RECENT
        );
        assert_eq!(s.execution_params().max_api_calls_per_turn, u32::MAX);
        assert!(!s.requires_project());
        assert!(!s.supports_team());
        assert_eq!(s.modules(), None);
        assert!(s.disabled_modules().is_empty());
        assert!(s.permits_module("agents"));
        assert!(s.permits_tool("Bash"));
    }

    #[test]
    fn every_field_reaches_the_scene() {
        let s = parse(
            r#"{
                "id": "review",
                "name": "Review",
                "description": "Reads, never writes",
                "tools": ["Read", "Grep", "ToolSearch", "mcp__*"],
                "disallowed_tools": ["mcp__shell__*"],
                "deferred_tools": ["Grep"],
                "token_budget": {"compact_threshold": 90000, "compact_keep_recent": 5},
                "execution": {"max_api_calls_per_turn": 12},
                "requires_project": true,
                "supports_team": true,
                "modules": ["agents", "skills"],
                "disabled_modules": ["mcp"],
                "reminders": ["memory", "git_status"]
            }"#,
            "You review code.",
        )
        .unwrap();
        assert_eq!(s.description(), "Reads, never writes");
        assert_eq!(s.tools(), ["Read", "Grep", "ToolSearch", "mcp__*"]);
        assert_eq!(s.deferred_tools(), ["Grep"]);
        assert_eq!(s.token_budget().compact_threshold, 90_000);
        assert_eq!(s.token_budget().compact_keep_recent, 5);
        assert_eq!(s.execution_params().max_api_calls_per_turn, 12);
        assert!(s.requires_project());
        assert!(s.supports_team());
        assert_eq!(s.modules(), Some(vec!["agents".into(), "skills".into()]));
        assert!(s.permits_module("skills"));
        assert!(!s.permits_module("mcp"));
        assert!(!s.permits_module("team"));

        let r = s.build_system_reminder(&ReminderContext {
            cwd: Cow::Borrowed("/work"),
            git_status: Some(Cow::Borrowed("M a.rs")),
            memory_summary: Some(Cow::Borrowed("likes tests")),
        });
        let memory = r.find("likes tests").unwrap();
        let git = r.find("M a.rs").unwrap();
        assert!(memory < git, "reminders come in the order listed: {r}");
    }

    #[test]
    fn the_disallowed_list_narrows_a_whitelist_too() {
        let s = parse(
            r#"{"id": "review", "name": "R", "tools": ["Read", "mcp__*"],
                "disallowed_tools": ["mcp__shell__*"]}"#,
            "p",
        )
        .unwrap();
        assert!(s.permits_tool("Read"));
        assert!(s.permits_tool("mcp__github__diff"));
        assert!(!s.permits_tool("mcp__shell__run"));
        assert!(!s.permits_tool("Bash"));
    }

    #[test]
    fn no_reminders_unless_asked_for() {
        let s = parse(MINIMAL, "p").unwrap();
        let r = s.build_system_reminder(&ReminderContext {
            cwd: Cow::Borrowed("/work"),
            git_status: Some(Cow::Borrowed("M a.rs")),
            memory_summary: Some(Cow::Borrowed("likes tests")),
        });
        assert_eq!(r, "");
    }

    #[test]
    fn an_unknown_field_is_an_error_not_ignored() {
        let e = error(r#"{"id": "review", "name": "R", "tool": ["Read"]}"#, "p");
        assert!(e.contains("unknown field `tool`"), "{e}");
        assert!(e.contains("scene.json"), "{e}");
        let e = error(
            r#"{"id": "review", "name": "R", "token_budget": {"threshold": 1}}"#,
            "p",
        );
        assert!(e.contains("unknown field `threshold`"), "{e}");
    }

    #[test]
    fn a_missing_required_field_is_an_error() {
        let e = error(r#"{"id": "review"}"#, "p");
        assert!(e.contains("missing field `name`"), "{e}");
        let e = error(r#"{"name": "R"}"#, "p");
        assert!(e.contains("missing field `id`"), "{e}");
    }

    #[test]
    fn an_id_that_is_not_directory_safe_is_refused() {
        for id in ["", "Review", "../up", "a/b", "plugin:x", "-lead", "a b"] {
            let e = error(&format!(r#"{{"id": {id:?}, "name": "R"}}"#), "p");
            assert!(e.contains("is not a scene id"), "{id:?}: {e}");
        }
        let long = "a".repeat(MAX_ID_LEN + 1);
        assert!(error(&format!(r#"{{"id": "{long}", "name": "R"}}"#), "p")
            .contains("is not a scene id"));
        parse(r#"{"id": "code-review_2", "name": "R"}"#, "p").unwrap();
    }

    #[test]
    fn contradictory_or_malformed_lists_are_refused() {
        let cases = [
            (
                r#""tools": ["Read"], "disallowed_tools": ["Read"]"#,
                "in both",
            ),
            (
                r#""modules": ["mcp"], "disabled_modules": ["mcp"]"#,
                "in both",
            ),
            (r#""tools": ["Read", "Read"]"#, "twice"),
            (r#""tools": [""]"#, "empty entry"),
            (r#""tools": ["*cp"]"#, "only allowed once, at the end"),
            (r#""deferred_tools": ["mcp__*"]"#, "not allowed here"),
            (r#""modules": ["*"]"#, "not allowed here"),
            (r#""reminders": ["memory", "memory"]"#, "twice"),
            (r#""reminders": ["weather"]"#, "unknown variant"),
            (
                r#""execution": {"max_api_calls_per_turn": 0}"#,
                "at least 1",
            ),
            (r#""name": " ""#, "must not be empty"),
        ];
        for (fields, expected) in cases {
            let json = if fields.starts_with(r#""name""#) {
                format!(r#"{{"id": "review", {fields}}}"#)
            } else {
                format!(r#"{{"id": "review", "name": "R", {fields}}}"#)
            };
            let e = error(&json, "p");
            assert!(e.contains(expected), "{fields}: {e}");
        }
    }

    #[test]
    fn an_empty_prompt_is_refused() {
        let e = error(MINIMAL, "  \n");
        assert!(e.contains("prompt.md") && e.contains("is empty"), "{e}");
    }

    #[test]
    fn a_plain_prompt_is_cached_and_followed_by_the_environment() {
        let s = parse(MINIMAL, "\nYou review code.\n\n").unwrap();
        let blocks = s.build_system_prompt(&ctx());
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].content, "You review code.");
        assert!(blocks[0].cache_strategy.is_some());
        assert!(blocks[1].cache_strategy.is_none());
        assert!(blocks[1].content.starts_with("# Environment"));
        assert!(blocks[1].content.contains("/work"));
        assert!(blocks[1].content.contains("2026-11-05"));
    }

    #[test]
    fn placeholders_split_the_prompt_at_the_line_where_they_start() {
        let s = parse(
            MINIMAL,
            "You review code.\nBe terse.\n\nCwd: {{cwd}} on {{os}}/{{shell}}\nHome {{home}}, \
             {{date}}, {{model}}\n{{environment}}\n",
        )
        .unwrap();
        let blocks = s.build_system_prompt(&ctx());
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].content, "You review code.\nBe terse.");
        assert!(blocks[0].cache_strategy.is_some());
        assert_eq!(
            blocks[1].content,
            format!(
                "Cwd: /work on darwin/zsh\nHome /home/me, 2026-11-05, claude-test\n{}",
                environment_block(&ctx())
            )
        );
        assert!(blocks[1].cache_strategy.is_none());
    }

    #[test]
    fn a_prompt_that_starts_with_a_placeholder_has_no_fixed_part() {
        let s = parse(MINIMAL, "Today is {{date}}.").unwrap();
        let blocks = s.build_system_prompt(&ctx());
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].content, "Today is 2026-11-05.");
    }

    #[test]
    fn a_name_the_scene_does_not_fill_is_left_for_prompt_variables() {
        let s = parse(MINIMAL, "Team {{team_name}}.\nIn {{cwd}}, see {{ticket}}.").unwrap();
        let blocks = s.build_system_prompt(&ctx());
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].content, "Team {{team_name}}.");
        assert_eq!(blocks[1].content, "In /work, see {{ticket}}.");

        let s = parse(MINIMAL, "Templates look like {{ user.name }} or {{Name}}.").unwrap();
        let blocks = s.build_system_prompt(&ctx());
        assert_eq!(
            blocks[0].content,
            "Templates look like {{ user.name }} or {{Name}}."
        );
        assert!(
            blocks[1].content.starts_with("# Environment"),
            "a prompt the scene fills nothing in gets the environment appended"
        );
    }

    #[test]
    fn a_plugin_scene_must_carry_its_plugins_id() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), r#"{"id": "plugin:demo", "name": "D"}"#, "p");
        let s = DeclarativeScene::load_for_plugin(dir.path(), "demo").unwrap();
        assert_eq!(s.id(), "plugin:demo");

        let e = DeclarativeScene::load_for_plugin(dir.path(), "other")
            .unwrap_err()
            .to_string();
        assert!(e.contains(r#"must be "plugin:other""#), "{e}");
        let e = DeclarativeScene::load(dir.path()).unwrap_err().to_string();
        assert!(e.contains("is not a scene id"), "{e}");
    }

    #[test]
    fn the_files_are_read_once_at_load() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), MINIMAL, "original");
        let s = DeclarativeScene::load(dir.path()).unwrap();
        std::fs::write(dir.path().join(PROMPT_FILE), "rewritten").unwrap();
        assert_eq!(s.build_system_prompt(&ctx())[0].content, "original");
        assert_eq!(s.prompt(), "original");
    }

    #[test]
    fn a_missing_prompt_file_is_an_error_naming_it() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(SCENE_FILE), MINIMAL).unwrap();
        let e = DeclarativeScene::load(dir.path()).unwrap_err().to_string();
        assert!(e.contains("prompt.md"), "{e}");
    }

    #[test]
    fn load_all_reads_scene_dirs_and_passes_over_the_rest() {
        let root = tempfile::tempdir().unwrap();
        write(&root.path().join("review"), MINIMAL, "p");
        write(
            &root.path().join("triage"),
            r#"{"id": "triage", "name": "T"}"#,
            "p",
        );
        std::fs::create_dir_all(root.path().join("code")).unwrap();
        std::fs::write(root.path().join("code/settings.json"), "{}").unwrap();
        std::fs::create_dir_all(root.path().join(".hidden")).unwrap();
        std::fs::write(root.path().join("notes.txt"), "").unwrap();

        let ids: Vec<String> = load_all(root.path())
            .into_iter()
            .map(|r| r.unwrap().id().to_string())
            .collect();
        assert_eq!(ids, ["review", "triage"]);
    }

    #[test]
    fn load_all_reports_a_broken_scene_and_still_loads_the_others() {
        let root = tempfile::tempdir().unwrap();
        write(&root.path().join("review"), MINIMAL, "p");
        write(
            &root.path().join("renamed"),
            r#"{"id": "original", "name": "R"}"#,
            "p",
        );
        std::fs::create_dir_all(root.path().join("half")).unwrap();
        std::fs::write(root.path().join("half").join(PROMPT_FILE), "p").unwrap();

        let results = load_all(root.path());
        assert_eq!(results.len(), 3);
        let errors: Vec<String> = results
            .iter()
            .filter_map(|r| r.as_ref().err().map(|e| e.to_string()))
            .collect();
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(errors
            .iter()
            .any(|e| e.contains("half") && e.contains("is missing")));
        assert!(errors
            .iter()
            .any(|e| e.contains("\"original\"") && e.contains("\"renamed\"")));
        assert!(results
            .iter()
            .any(|r| r.as_ref().is_ok_and(|s| s.id() == "review")));
    }

    #[test]
    fn load_all_of_a_missing_root_is_empty() {
        let root = tempfile::tempdir().unwrap();
        assert!(load_all(&root.path().join("absent")).is_empty());
    }
}
