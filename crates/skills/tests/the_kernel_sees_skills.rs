//! Skills as the kernel sees them: a `CommandRegistry` over the live
//! [`SkillManager`], and a session assembled with [`SkillsModule`].
//!
//! [`SkillManager`]: skills::manager::SkillManager
//! [`SkillsModule`]: skills::SkillsModule

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use async_trait::async_trait;
use base::frozen::SkillEntry;
use base::interface::model::{Model, ModelMessage};
use base::interface::scene::Scene;
use base::interface::settings::*;
use base::tool::InMemoryToolRegistry;
use runtime::agent::Builder;
use runtime::commands::{Command, CommandRegistry};

struct DummyModel;

#[async_trait]
impl Model for DummyModel {
    fn api_type(&self) -> base::provider::ApiType {
        base::provider::ApiType::Anthropic
    }
    async fn stream(
        &self,
        _prompt_blocks: Vec<base::interface::prompt::PromptBlock>,
        _tools: Vec<base::interface::model::ToolDef>,
        _messages: Vec<ModelMessage>,
        _params: base::interface::model::StreamParams,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError> {
        Err(base::interface::model::ModelError::Internal(
            "DummyModel does not stream".into(),
        ))
    }
}

fn test_settings() -> Settings {
    let dir = std::env::temp_dir().join("agix-skills-module-tests");
    Settings {
        model: ModelSettings {
            api_type: base::provider::ApiType::Anthropic,
            base_url: String::new(),
            auth_token: String::new(),
            model_name: "test".into(),
            max_tokens: 2000,
            thinking_mode: ThinkingMode::Auto,
            effort: None,
            fallback_model: None,
        },
        paths: PathSettings {
            user_data_dir: dir.clone(),
            global_data_dir: dir.clone(),
            local_data_dir: dir,
            scope: "code".into(),
        },
        execution: ExecutionSettings::default(),
        compaction: CompactionConfig::default(),
        sandbox: SandboxConfig::default(),
        plugins: Default::default(),
        instruction_file: None,
        prompt_append: None,
        prompt_override: None,
        recorder: None,
        telemetry_url: None,
        session_dir: None,
        memory_enabled: false,
        disable_skill_shell_execution: true,
        path_style: Default::default(),
        permission_mode: PermissionMode::default(),
        permission_rules: Vec::new(),
        local_permission_rules: Vec::new(),
        allow_client_permission_override: false,
        telemetry_enabled: false,
        hooks_config: None,
        mcp_servers: Default::default(),
        providers: Default::default(),
        default_provider: None,
        task_models: Default::default(),
        language: None,
        feature_flags: Default::default(),
        scripts: Vec::new(),
    }
}

#[test]
fn a_registry_over_the_manager_also_has_builtins() {
    // Regression test: the skill-backed constructor previously started from
    // `Self::new()` (empty), so the production registry (built via this
    // constructor in `Builder::build()`) never actually contained the 5
    // built-in local commands — `/help` et al. silently fell through to
    // the LLM instead of executing.
    let skill_mgr = Arc::new(skills::manager::SkillManager::new());
    let registry = CommandRegistry::from_skill_catalog(skill_mgr);
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
    let skill_mgr = Arc::new(skills::manager::SkillManager::new());
    skill_mgr
        .load_dir_subdirs(dir.path(), skills::manager::SkillSource::Project)
        .unwrap();
    let registry = CommandRegistry::from_skill_catalog(skill_mgr.clone());
    assert!(registry.resolve("late-arrival").is_none());

    write_skill(dir.path(), "late-arrival", "shows up mid-session");
    skill_mgr
        .load_dir_subdirs(dir.path(), skills::manager::SkillSource::Project)
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
    let skill_mgr = Arc::new(skills::manager::SkillManager::new());
    skill_mgr
        .load_dir_subdirs(dir.path(), skills::manager::SkillSource::Project)
        .unwrap();
    skill_mgr
        .enable_watching(&[dir.path().to_path_buf()])
        .unwrap();
    let registry = CommandRegistry::from_skill_catalog(skill_mgr.clone());
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
    let skill_mgr = Arc::new(skills::manager::SkillManager::new());
    skill_mgr.register_bundled(SkillEntry {
        name: "internal-only".into(),
        description: "model-facing".into(),
        user_invocable: false,
        ..Default::default()
    });
    let registry = CommandRegistry::from_skill_catalog(skill_mgr);
    assert!(registry.resolve("internal-only").is_none());
    assert!(!registry.list().iter().any(|(n, _)| n == "internal-only"));
}

/// Precedence, highest first: explicitly registered (plugin/MCP) →
/// skills → built-ins. This is the order the old single-map build
/// sequence produced by insertion (`Default` → skills → plugins), and
/// the split into tiers has to preserve it.
#[test]
fn registered_commands_shadow_skills_which_shadow_builtins() {
    let skill_mgr = Arc::new(skills::manager::SkillManager::new());
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
    let mut registry = CommandRegistry::from_skill_catalog(skill_mgr);
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
    let skill_mgr = Arc::new(skills::manager::SkillManager::new());
    skill_mgr.register_bundled(SkillEntry {
        name: "ship".into(),
        description: "ship it".into(),
        arguments: Some(vec!["issue".into(), "branch".into()]),
        user_invocable: true,
        ..Default::default()
    });
    let registry = CommandRegistry::from_skill_catalog(skill_mgr);
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

/// A shared catalog must be used as-is, with `build()` never
/// falling back to its own disk scan — a skill that only exists in the
/// injected catalog (never written to `settings.paths`' skill
/// directories) must still resolve as a slash command.
#[tokio::test]
async fn build_uses_the_injected_skill_catalog_instead_of_scanning_disk() {
    let settings = Arc::new(test_settings());
    let model: Arc<dyn Model> = Arc::new(DummyModel);
    let scene: Arc<dyn Scene> = Arc::new(scene::code::CodeScene::default());

    let catalog = skills::manager::SkillManager::new();
    catalog.register_bundled(base::frozen::SkillEntry {
        name: "p2-injected-only".into(),
        description: "only exists in the injected catalog".into(),
        source: base::frozen::SkillSource::User,
        path: std::path::PathBuf::from("(test:p2-injected-only)"),
        ..Default::default()
    });
    let catalog = Arc::new(catalog);

    let (agent, _event_rx, _input_tx) = Builder::new()
        .scene(scene)
        .model(model)
        .settings(settings)
        .module(Arc::new(skills::SkillsModule::shared(catalog)))
        .skip_warmup(true)
        .build()
        .expect("build should succeed");

    assert!(
        agent.commands().resolve("p2-injected-only").is_some(),
        "the injected catalog's skill should resolve as a slash command"
    );
}

/// P3-12's acceptance: a fourth skill source, registered from outside the
/// engine, is a source in full — its skills list *and* expand, without
/// `SkillManager` learning anything about where they came from.
#[tokio::test]
async fn a_host_can_register_a_fourth_skill_source() {
    use base::interface::skill_provider::StaticSkills;

    let source = StaticSkills::new(
        "company-bundle",
        vec![base::frozen::SkillEntry {
            name: "file-a-ticket".into(),
            description: "Open a ticket in the company tracker".into(),
            source: base::frozen::SkillSource::Plugin,
            // Deliberately not a readable path: the source owns the body.
            path: std::path::PathBuf::from("(company:file-a-ticket)"),
            ..Default::default()
        }],
    )
    .with_body("file-a-ticket", "Ask for a summary, then POST it.");

    let (agent, _event_rx, _input_tx) = Builder::new()
        .scene(Arc::new(scene::code::CodeScene::default()) as Arc<dyn Scene>)
        .model(Arc::new(DummyModel) as Arc<dyn Model>)
        .settings(Arc::new(test_settings()))
        .module(Arc::new(
            skills::SkillsModule::new().provider(Arc::new(source)),
        ))
        .skip_warmup(true)
        .build()
        .expect("build should succeed");

    let listed = agent.skills().get("file-a-ticket");
    assert!(
        listed.is_some(),
        "the host's source did not reach the catalog: {:?}",
        agent
            .skills()
            .list()
            .iter()
            .map(|s| &s.name)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        agent.skills().body("file-a-ticket").as_deref(),
        Some("Ask for a summary, then POST it."),
        "a skill that lists but cannot be expanded is not a source"
    );
}

/// A host takes `Agent::commands()` before spawning the engine and keeps
/// it for the life of the session: it must stay a live view of the same
/// catalog the turn loop resolves against, including skills that appear
/// after the engine has been moved into its task.
#[tokio::test]
async fn commands_handle_outlives_the_agent_and_stays_live() {
    let dir = tempfile::tempdir().unwrap();
    let skills = Arc::new(skills::manager::SkillManager::new());
    skills
        .load_dir_subdirs(dir.path(), skills::manager::SkillSource::Project)
        .unwrap();

    let (agent, _event_rx, _input_tx) = Builder::new()
        .scene(Arc::new(scene::code::CodeScene::default()) as Arc<dyn Scene>)
        .model(Arc::new(DummyModel))
        .settings(Arc::new(test_settings()))
        .tools(Arc::new(InMemoryToolRegistry::new()))
        .module(Arc::new(skills::SkillsModule::shared(skills.clone())))
        .skip_warmup(true)
        .build()
        .expect("build should succeed");

    let commands = agent.commands();
    drop(agent);
    assert!(commands.resolve("mid-session").is_none());

    let skill_dir = dir.path().join("mid-session");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: mid-session\ndescription: added while the engine runs\n---\n\nbody\n",
    )
    .unwrap();
    skills
        .load_dir_subdirs(dir.path(), skills::manager::SkillSource::Project)
        .unwrap();

    assert!(
        commands.resolve("mid-session").is_some(),
        "the handle a host holds must track the catalog, not a snapshot"
    );
}

#[tokio::test]
async fn the_skill_tool_is_resident_and_needs_the_module() {
    let build = |with_skills: bool| {
        let mut builder = Builder::new()
            .scene(Arc::new(scene::code::CodeScene::default()) as Arc<dyn Scene>)
            .model(Arc::new(DummyModel))
            .settings(Arc::new(test_settings()))
            .tools({
                let t = Arc::new(InMemoryToolRegistry::new());
                tools::register_builtin_tools(&t);
                t
            })
            .skip_warmup(true);
        if with_skills {
            builder = builder.module(Arc::new(skills::SkillsModule::new()));
        }
        builder.build().expect("build should succeed").0
    };
    let agent = build(true);
    let tool = agent
        .tools()
        .get("Skill")
        .expect("the module registers Skill");
    assert!(
        !tool.is_deferred(),
        "Skill is in CodeScene's resident set and must ship its full schema"
    );
    assert!(build(false).tools().get("Skill").is_none());
}
