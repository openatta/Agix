//! `mcp_for_project_tests` 原样搬来的用例。

use super::*;

struct AllowAllTestPermission;
#[async_trait::async_trait]
impl base::interface::permission::PermissionPolicy for AllowAllTestPermission {
    async fn decide(
        &self,
        _req: &base::interface::permission::PermissionRequest<'_>,
    ) -> base::interface::permission::PermissionOutcome {
        base::interface::permission::PermissionOutcome::Permit {
            basis: base::interface::permission::Basis::Bypass,
            profile: Default::default(),
            obligations: Default::default(),
        }
    }
}

/// A `SessionPool` with no MCP servers configured anywhere — every
/// project's `mcp_for_project` connect resolves to an empty
/// `McpManager` immediately (no subprocess, no network), which is all
/// these tests need: they're about the *cache* around the connect, not
/// the connect itself (that's `mcp_toy_server_smoke.rs`'s job).
pub(in crate::session_pool) async fn test_pool() -> (Arc<SessionPool>, tempfile::TempDir) {
    test_pool_with_store(None).await
}

pub(in crate::session_pool) async fn test_pool_with_store(
    history_store: Option<Arc<dyn history::store::HistoryStore>>,
) -> (Arc<SessionPool>, tempfile::TempDir) {
    let (builder, dir) = test_pool_builder(history_store);
    (builder.build(), dir)
}

pub(in crate::session_pool) fn test_pool_builder(
    history_store: Option<Arc<dyn history::store::HistoryStore>>,
) -> (SessionPoolBuilder, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let scene: Arc<SceneSpec> = Arc::new(scene::code::spec(scene::code::Interaction::Interactive));
    let permission =
        Some(Arc::new(AllowAllTestPermission)
            as Arc<dyn base::interface::permission::PermissionPolicy>);
    let paths: Arc<dyn crate::config::DaemonPaths> = Arc::new(
        crate::config::StaticDaemonPaths::new(dir.path().to_path_buf()),
    );
    let client: Arc<dyn AnthropicClient> = Arc::new(model::mock::MockAnthropicClient::new());
    let mut settings = base::settings::LoadedSettings::new(
        Settings::defaults_for("claude-sonnet-4-6"),
        base::settings::PathSettings::default(),
    );
    settings.paths.user_data_dir = dir.path().join("user");
    settings.paths.global_data_dir = dir.path().join("global");

    let builder = SessionPool::builder(
        8,
        3600,
        client,
        Arc::new(settings),
        scene,
        permission,
        cwd,
        history_store,
        paths,
    );
    (builder, dir)
}

/// The watcher budget `crate::watch_hub` states: watched paths are
/// `1 + M + P` — one global tier, one per scene, one per project — and
/// not a multiple of the number of bindings.
///
/// What this pins is the sharing: the tiers several projects have in
/// common are registered once, so a binding only ever costs the paths
/// that are its own. That a binding brings no `notify` streams of its
/// own besides the hub's is not visible from here — the backend
/// offers no count — and is held by construction instead: the catalogs a
/// binding builds are the `unwatched` ones.
#[tokio::test]
async fn bindings_do_not_multiply_watchers() {
    let (pool, dir) = test_pool().await;
    let watched = |pool: &SessionPool| {
        pool.bindings
            .watched_path_count()
            .expect("a hub in a test process")
    };

    // The tiers of the pool's own project: whatever of them exist on
    // disk are registered once each.
    pool.binding_for(Some(pool.cwd()), "code").await;
    let baseline = watched(&pool);

    // Three more projects, each with a settings file, a skills directory
    // and an agents directory of its own — nine more paths that exist,
    // and the shared global and scene tiers again.
    for name in ["one", "two", "three"] {
        let root = dir.path().join(name);
        std::fs::create_dir_all(root.join(".agix").join("agents")).unwrap();
        std::fs::create_dir_all(root.join(".agents").join("skills")).unwrap();
        std::fs::write(root.join(".agix").join("settings.json"), "{}").unwrap();
        pool.binding_for(Some(&root), "code").await;
    }

    let grown = watched(&pool) - baseline;
    assert_eq!(
        grown, 9,
        "three projects should add their own three paths each and re-register no shared one"
    );
}

fn is_stale(pool: &SessionPool, binding: &ProjectBinding) -> bool {
    !pool.bindings.is_current(&binding.stamp)
}

/// A tier change that lands while a binding is being built — after the
/// build read the disk, before it reached the cache — leaves that build
/// stale and out of the cache, so the next session on the pair reads the
/// disk again.
#[tokio::test]
async fn an_invalidation_during_a_build_leaves_that_build_stale_and_uncached() {
    use crate::watch_hub::Tier;
    let (pool, _dir) = test_pool().await;
    let key = (Some(pool.cwd().to_path_buf()), "mini".to_string());

    let stamp = pool.bindings.begin_build(&key);
    let mut binding = pool
        .bindings
        .binding_from((*pool.bindings.pool_settings().await).clone());
    pool.invalidate_bindings(Some(&Tier::Project(pool.cwd().to_path_buf())))
        .await;
    binding.stamp = stamp;
    let built = pool.bindings.finish_build(Arc::new(binding)).await;

    assert!(is_stale(&pool, &built));
    let next = pool.binding_for(Some(pool.cwd()), "mini").await;
    assert!(!Arc::ptr_eq(&built, &next));
    assert!(!is_stale(&pool, &next));
}

/// A project's settings changing is that project's business: the other
/// projects' bindings, and the sessions built on them, stay as they are.
#[tokio::test]
async fn a_tier_change_invalidates_only_the_bindings_merged_from_it() {
    use crate::watch_hub::Tier;
    let (pool, dir) = test_pool().await;
    let other = dir.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    let own_coding = pool.binding_for(Some(pool.cwd()), "code").await;
    let own_harnix = pool.binding_for(Some(pool.cwd()), "harnix").await;
    let other_coding = pool.binding_for(Some(&other), "code").await;

    pool.invalidate_bindings(Some(&Tier::Project(other.clone())))
        .await;
    assert!(is_stale(&pool, &other_coding));
    assert!(!is_stale(&pool, &own_coding) && !is_stale(&pool, &own_harnix));
    assert!(Arc::ptr_eq(
        &own_coding,
        &pool.binding_for(Some(pool.cwd()), "code").await
    ));

    pool.invalidate_bindings(Some(&Tier::Scene("harnix".into())))
        .await;
    assert!(is_stale(&pool, &own_harnix));
    assert!(!is_stale(&pool, &own_coding));

    pool.invalidate_bindings(Some(&Tier::Global)).await;
    assert!(is_stale(&pool, &own_coding));
    assert!(pool.bindings.is_empty().await);
}

/// A skill file added after a binding was built shows up in the catalog
/// its sessions already hold, and nothing is marked for rebuilding.
#[cfg(feature = "skills")]
#[tokio::test]
async fn a_skill_change_reloads_the_shared_catalog_in_place() {
    use crate::watch_hub::{Tier, WatchTarget};
    let (pool, _dir) = test_pool().await;
    // Not the pool's startup scene: `test_pool`'s startup scene tier is
    // the tempdir root, which also holds the project.
    let skills = pool.cwd().join(".agents").join("skills");
    std::fs::create_dir_all(&skills).unwrap();
    let binding = pool.binding_for(Some(pool.cwd()), "mini").await;
    let skill_dir = skills.join("late");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: late\ndescription: added after the binding was built\n---\nbody\n",
    )
    .unwrap();
    assert!(binding.modules.skills().get("late").is_none());

    pool.bindings
        .refresh_catalogs(
            &Tier::Project(pool.cwd().to_path_buf()),
            WatchTarget::Skills,
        )
        .await;

    assert!(binding.modules.skills().get("late").is_some());
    assert!(!is_stale(&pool, &binding));
    assert!(Arc::ptr_eq(
        &binding,
        &pool.binding_for(Some(pool.cwd()), "mini").await
    ));
}

/// A project with no skills directory when its binding was built gets its
/// first skill the next time the binding is asked for, and the directory
/// is watched from then on.
#[cfg(feature = "skills")]
#[tokio::test]
async fn a_skills_directory_created_later_is_picked_up_and_watched() {
    let (pool, _dir) = test_pool().await;
    let skills = pool.cwd().join(".agents").join("skills");
    assert!(!skills.exists());
    let before = pool.binding_for(Some(pool.cwd()), "mini").await;

    std::fs::create_dir_all(skills.join("first")).unwrap();
    std::fs::write(
        skills.join("first").join("SKILL.md"),
        "---\nname: first\ndescription: the project's first skill\n---\nbody\n",
    )
    .unwrap();

    let after = pool.binding_for(Some(pool.cwd()), "mini").await;
    assert!(is_stale(&pool, &before));
    assert!(after.modules.skills().get("first").is_some());
    assert!(after.appeared().is_none());
    assert!(Arc::ptr_eq(
        &after,
        &pool.binding_for(Some(pool.cwd()), "mini").await
    ));
}

/// A project with no `.agix/agents/` when its binding was built gets its
/// first agent type the next time the binding is asked for, and the
/// directory is watched from then on.
#[cfg(feature = "agents")]
#[tokio::test]
async fn an_agents_directory_created_later_is_picked_up_and_watched() {
    let (pool, _dir) = test_pool().await;
    let agents = pool.cwd().join(".agix").join("agents");
    assert!(!agents.exists());
    let before = pool.binding_for(Some(pool.cwd()), "mini").await;
    assert!(!before.modules.has_agent_type("late-reviewer"));

    std::fs::create_dir_all(&agents).unwrap();
    std::fs::write(
        agents.join("late-reviewer.md"),
        "---\ndescription: the project's first agent type\n---\nbody\n",
    )
    .unwrap();

    let after = pool.binding_for(Some(pool.cwd()), "mini").await;
    assert!(is_stale(&pool, &before));
    assert!(after.modules.has_agent_type("late-reviewer"));
    assert!(after.appeared().is_none());
    assert!(Arc::ptr_eq(
        &after,
        &pool.binding_for(Some(pool.cwd()), "mini").await
    ));
}
