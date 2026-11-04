//! `PluginScene` — a scene built from a plugin's manifest rather than from
//! Rust source.
//!
//! `SceneRegistry::register` accepts any `Arc<dyn Scene>`, so this
//! needs no new machinery. What `Scene`'s doc means by "code-level,
//! immutable" is the second half: a scene must not change under a running
//! session. A struct whose fields are filled once at load and never written
//! again satisfies that as well as a unit struct does.
//!
//! This is the only place a plugin may shape behavior rather than merely add
//! to it, and the boundary is consent. Rewriting the prompt of a scene the
//! user chose for other reasons is hijacking; owning a scene the user
//! explicitly enters is the plugin doing its job. So a plugin gets its own
//! scene and never anyone else's.

use base::interface::prompt::PromptBlock;
use base::interface::scene::{
    ExecutionParams, ReminderContext, Scene, ScenePromptContext, TokenBudget,
};
use std::sync::Arc;

/// Defaults matching the built-in scenes, used for anything a plugin's
/// `[scene.own.budget]` leaves out.
const DEFAULT_COMPACT_THRESHOLD: usize = 150_000;
const DEFAULT_COMPACT_KEEP_RECENT: usize = 20;

pub struct PluginScene {
    id: String,
    name: String,
    description: String,
    /// The plugin's system prompt, read at load. Reading it per call would
    /// let a plugin change a running session's instructions by editing a
    /// file, which is exactly the mutability the scene contract forbids.
    prompt: String,
    reminder: Option<String>,
    tools: Vec<String>,
    disallowed_tools: Vec<String>,
    deferred_tools: Vec<String>,
    budget: TokenBudget,
    execution: ExecutionParams,
    extra_tools: Vec<Arc<dyn base::tool::Tool>>,
}

impl PluginScene {
    /// Build the scene a plugin declares, or `None` when it declares none.
    ///
    /// A prompt file that can't be read is fatal *for this scene*: a scene
    /// whose system prompt silently defaulted to nothing would be a
    /// personality the user asked for and did not get. The plugin's tools
    /// still load; only the scene is withheld.
    pub fn from_plugin(
        plugin: &plugin::manifest::Plugin,
        extra_tools: Vec<Arc<dyn base::tool::Tool>>,
    ) -> Option<Self> {
        let own = plugin.manifest.scene.own.as_ref()?;
        let prompt_path = plugin.path(&own.prompt);
        let prompt = match std::fs::read_to_string(&prompt_path) {
            Ok(body) => body,
            Err(e) => {
                tracing::warn!(
                    plugin = %plugin.name(),
                    path = %prompt_path.display(),
                    error = %e,
                    "plugin scene declares a system prompt that cannot be read; \
                     not registering the scene"
                );
                return None;
            }
        };

        let reminder = own.reminder.as_ref().and_then(|rel| {
            let path = plugin.path(rel);
            std::fs::read_to_string(&path)
                .map_err(|e| {
                    tracing::warn!(
                        plugin = %plugin.name(),
                        path = %path.display(),
                        error = %e,
                        "plugin scene reminder could not be read; continuing without it"
                    );
                })
                .ok()
        });

        Some(Self {
            id: plugin.scene_id()?,
            name: own.name.clone(),
            description: own.description.clone(),
            prompt,
            reminder,
            tools: own.tools.clone(),
            disallowed_tools: own.disallowed_tools.clone(),
            deferred_tools: own.deferred_tools.clone(),
            budget: TokenBudget {
                compact_threshold: own
                    .budget
                    .compact_threshold
                    .unwrap_or(DEFAULT_COMPACT_THRESHOLD),
                compact_keep_recent: own
                    .budget
                    .compact_keep_recent
                    .unwrap_or(DEFAULT_COMPACT_KEEP_RECENT),
            },
            execution: ExecutionParams {
                max_api_calls_per_turn: own.budget.max_api_calls_per_turn.unwrap_or(u32::MAX),
            },
            extra_tools,
        })
    }
}

impl Scene for PluginScene {
    fn id(&self) -> &str {
        &self.id
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    /// The plugin's prompt, plus the environment facts every scene has to
    /// state. The prompt is cached because it does not vary; the environment
    /// block is not, because the date and working directory do.
    fn build_system_prompt(&self, ctx: &ScenePromptContext) -> Vec<PromptBlock> {
        vec![
            PromptBlock::system_cached(self.prompt.clone()),
            PromptBlock::system(render_environment(ctx)),
        ]
    }

    fn tools(&self) -> Vec<String> {
        self.tools.clone()
    }

    fn disallowed_tools(&self) -> Vec<String> {
        self.disallowed_tools.clone()
    }

    fn deferred_tools(&self) -> Vec<String> {
        self.deferred_tools.clone()
    }

    fn extra_tools(&self) -> Vec<Arc<dyn base::tool::Tool>> {
        self.extra_tools.clone()
    }

    fn token_budget(&self) -> TokenBudget {
        self.budget.clone()
    }

    /// A plugin naming a large ceiling cannot raise the real one: the turn
    /// loop resolves the budget as `min(settings, scene)`, so a scene may
    /// only ever tighten. That is why there is no clamp here — the
    /// combination rule already is one.
    fn execution_params(&self) -> ExecutionParams {
        self.execution.clone()
    }

    fn build_system_reminder(&self, ctx: &ReminderContext) -> String {
        let mut out = String::new();
        if let Some(body) = &self.reminder {
            out.push_str(&format!("\n<system-reminder>\n{body}\n</system-reminder>"));
        }
        if let Some(git) = &ctx.git_status {
            out.push_str(&format!("\n<system-reminder>\n{git}\n</system-reminder>"));
        }
        if let Some(mem) = &ctx.memory_summary {
            out.push_str(&format!("\n<system-reminder>\n{mem}\n</system-reminder>"));
        }
        out
    }
}

fn render_environment(ctx: &ScenePromptContext) -> String {
    let mut out = format!(
        "# Environment\nWorking directory: {}\nPlatform: {}\nToday's date: {}\n",
        ctx.cwd, ctx.os, ctx.date
    );
    if let Some(lang) = &ctx.language {
        out.push_str(&format!("User language preference: {lang}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::borrow::Cow;
    use std::path::Path;

    const HEAD: &str = "[plugin]\nname = \"demo\"\nversion = \"1.0.0\"\napi_version = \"0.1\"\n";

    fn load(root: &Path, body: &str) -> plugin::manifest::Plugin {
        std::fs::write(root.join("plugin.toml"), format!("{HEAD}{body}")).unwrap();
        plugin::manifest::Plugin::load(root, &root.join("plugin.toml")).unwrap()
    }

    fn full_scene_body() -> &'static str {
        r#"
[scene.own]
name = "Demo workflow"
description = "A demo"
prompt = "scene/prompt.md"
reminder = "scene/reminder.md"
tools = ["Read", "Grep"]
disallowed_tools = ["Bash"]
deferred_tools = ["Grep"]

[scene.own.budget]
compact_threshold = 90000
compact_keep_recent = 5
max_api_calls_per_turn = 12
"#
    }

    fn write_scene_files(root: &Path) {
        std::fs::create_dir_all(root.join("scene")).unwrap();
        std::fs::write(root.join("scene/prompt.md"), "You are the demo agent.").unwrap();
        std::fs::write(root.join("scene/reminder.md"), "demo is active").unwrap();
    }

    fn ctx() -> ScenePromptContext<'static> {
        ScenePromptContext {
            cwd: Cow::Borrowed("/work"),
            os: Cow::Borrowed("darwin"),
            shell: Cow::Borrowed("zsh"),
            home_dir: Cow::Borrowed("/home"),
            date: Cow::Borrowed("2026-08-16"),
            model_name: Cow::Borrowed("m"),
            skills_text: None,
            mcp_instructions: None,
            session_memory: None,
            is_git: false,
            git_branch: None,
            is_worktree: false,
            git_status: None,
            language: Some(Cow::Borrowed("zh-CN")),
            scratchpad_dir: None,
            output_style_content: None,
            available_tools: None,
            tool_results_ever_cleared: false,
        }
    }

    #[test]
    fn a_plugin_without_a_scene_declares_none() {
        let dir = tempfile::tempdir().unwrap();
        let p = load(dir.path(), "");
        assert!(PluginScene::from_plugin(&p, Vec::new()).is_none());
    }

    #[test]
    fn the_scene_id_is_namespaced_by_the_plugin() {
        let dir = tempfile::tempdir().unwrap();
        write_scene_files(dir.path());
        let p = load(dir.path(), full_scene_body());
        let s = PluginScene::from_plugin(&p, Vec::new()).unwrap();
        assert_eq!(s.id(), "plugin:demo");
        assert_eq!(s.name(), "Demo workflow");
    }

    #[test]
    fn every_declared_field_reaches_the_scene() {
        let dir = tempfile::tempdir().unwrap();
        write_scene_files(dir.path());
        let p = load(dir.path(), full_scene_body());
        let s = PluginScene::from_plugin(&p, Vec::new()).unwrap();

        assert_eq!(s.tools(), ["Read", "Grep"]);
        assert_eq!(s.disallowed_tools(), ["Bash"]);
        assert_eq!(s.deferred_tools(), ["Grep"]);
        assert_eq!(s.token_budget().compact_threshold, 90_000);
        assert_eq!(s.token_budget().compact_keep_recent, 5);
        assert_eq!(s.execution_params().max_api_calls_per_turn, 12);
    }

    #[test]
    fn an_omitted_budget_falls_back_to_the_built_in_defaults() {
        let dir = tempfile::tempdir().unwrap();
        write_scene_files(dir.path());
        let p = load(
            dir.path(),
            "\n[scene.own]\nname = \"D\"\nprompt = \"scene/prompt.md\"\n",
        );
        let s = PluginScene::from_plugin(&p, Vec::new()).unwrap();
        assert_eq!(
            s.token_budget().compact_threshold,
            DEFAULT_COMPACT_THRESHOLD
        );
        assert_eq!(
            s.token_budget().compact_keep_recent,
            DEFAULT_COMPACT_KEEP_RECENT
        );
        assert_eq!(
            s.execution_params().max_api_calls_per_turn,
            u32::MAX,
            "saying nothing must leave the setting in charge, not impose a ceiling"
        );
    }

    #[test]
    fn the_prompt_is_cached_and_the_environment_block_is_not() {
        let dir = tempfile::tempdir().unwrap();
        write_scene_files(dir.path());
        let p = load(dir.path(), full_scene_body());
        let s = PluginScene::from_plugin(&p, Vec::new()).unwrap();

        let blocks = s.build_system_prompt(&ctx());
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].content, "You are the demo agent.");
        assert!(
            blocks[0].cache_strategy.is_some(),
            "the plugin's prompt does not vary between calls"
        );
        assert!(
            blocks[1].cache_strategy.is_none(),
            "the date and working directory do"
        );
        assert!(blocks[1].content.contains("/work"));
        assert!(blocks[1].content.contains("2026-08-16"));
        assert!(blocks[1].content.contains("zh-CN"));
    }

    /// Editing the prompt file must not change a session that is already
    /// running under this scene — the scene contract is that it does not
    /// move underneath anyone.
    #[test]
    fn the_prompt_is_read_once_at_load() {
        let dir = tempfile::tempdir().unwrap();
        write_scene_files(dir.path());
        let p = load(dir.path(), full_scene_body());
        let s = PluginScene::from_plugin(&p, Vec::new()).unwrap();

        std::fs::write(dir.path().join("scene/prompt.md"), "REWRITTEN").unwrap();
        assert_eq!(
            s.build_system_prompt(&ctx())[0].content,
            "You are the demo agent."
        );
    }

    /// A personality the user asked for and silently did not get is worse
    /// than a scene that refuses to register.
    #[test]
    fn an_unreadable_prompt_withholds_the_scene() {
        let dir = tempfile::tempdir().unwrap();
        let p = load(
            dir.path(),
            "\n[scene.own]\nname = \"D\"\nprompt = \"missing.md\"\n",
        );
        assert!(PluginScene::from_plugin(&p, Vec::new()).is_none());
    }

    /// The reminder is optional, so losing it degrades rather than refuses.
    #[test]
    fn an_unreadable_reminder_only_costs_the_reminder() {
        let dir = tempfile::tempdir().unwrap();
        write_scene_files(dir.path());
        std::fs::remove_file(dir.path().join("scene/reminder.md")).unwrap();
        let p = load(dir.path(), full_scene_body());
        let s = PluginScene::from_plugin(&p, Vec::new()).unwrap();
        assert!(s
            .build_system_reminder(&ReminderContext {
                cwd: Cow::Borrowed("/work"),
                git_status: None,
                memory_summary: None,
            })
            .is_empty());
    }

    #[test]
    fn the_reminder_joins_git_status_and_memory() {
        let dir = tempfile::tempdir().unwrap();
        write_scene_files(dir.path());
        let p = load(dir.path(), full_scene_body());
        let s = PluginScene::from_plugin(&p, Vec::new()).unwrap();

        let out = s.build_system_reminder(&ReminderContext {
            cwd: Cow::Borrowed("/work"),
            git_status: Some(Cow::Borrowed("M src/main.rs")),
            memory_summary: Some(Cow::Borrowed("remembered a thing")),
        });
        assert!(out.contains("demo is active"));
        assert!(out.contains("M src/main.rs"));
        assert!(out.contains("remembered a thing"));
    }
}

#[cfg(test)]
mod delegation_tests {
    use super::*;
    use crate::InstalledPlugins;
    use runtime::plugin_host::PluginHost;

    /// The two halves of delegation are produced independently — the scene
    /// id comes from `Plugin::scene_id`, the agent's target from a string a
    /// plugin author typed — and they only meet at spawn time, where a
    /// mismatch degrades silently into "unrecognised scene, inherit the
    /// parent's". This is the check that they line up.
    #[test]
    fn a_plugin_agent_can_name_the_scene_its_own_plugin_owns() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("scene")).unwrap();
        std::fs::write(dir.path().join("scene/prompt.md"), "scene prompt").unwrap();
        std::fs::write(dir.path().join("agent.md"), "agent prompt").unwrap();
        std::fs::write(
            dir.path().join("plugin.toml"),
            r#"
[plugin]
name = "demo"
version = "1.0.0"
api_version = "0.1"

[scene.own]
name = "Demo"
prompt = "scene/prompt.md"

[[agent]]
name = "worker"
description = "runs inside the plugin's own scene"
prompt = "agent.md"
scene = "plugin:demo"
"#,
        )
        .unwrap();
        let p =
            plugin::manifest::Plugin::load(dir.path(), &dir.path().join("plugin.toml")).unwrap();

        let scene = PluginScene::from_plugin(&p, Vec::new()).expect("the plugin owns a scene");
        let types = InstalledPlugins::new(vec![p]).agent_types();

        assert_eq!(
            types[0].scene.as_deref(),
            Some(scene.id()),
            "an agent targeting its own plugin's scene must name the id that scene registers under"
        );
    }
}
