//! `mcp_for_project_tests` 原样搬来的用例。

use super::*;

struct AllowAllTestPermission;
#[async_trait::async_trait]
impl base::interface::permission::Permission for AllowAllTestPermission {
    async fn check(
        &self,
        _: &str,
        _: &serde_json::Value,
        _: &std::path::Path,
        _: &str,
    ) -> base::interface::permission::PermissionOutcome {
        base::interface::permission::PermissionOutcome::Permit
    }
}

/// A `SessionPool` with no MCP servers configured anywhere — every
/// project's `mcp_for_project` connect resolves to an empty
/// `McpManager` immediately (no subprocess, no network), which is all
/// these tests need: they're about the *cache* around the connect, not
/// the connect itself (that's `mcp_toy_server_smoke.rs`'s job).
pub(super) async fn test_pool() -> (SessionPool, tempfile::TempDir) {
    test_pool_with_store(None).await
}

pub(super) async fn test_pool_with_store(
    history_store: Option<Arc<dyn history::store::HistoryStore>>,
) -> (SessionPool, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let scene: Arc<dyn Scene> = Arc::new(scene::code::CodeScene::default());
    let permission: Option<Arc<dyn Permission>> = Some(Arc::new(AllowAllTestPermission));
    let paths: Arc<dyn crate::config::DaemonPaths> = Arc::new(
        crate::config::StaticDaemonPaths::new(dir.path().to_path_buf()),
    );
    let client: Arc<dyn AnthropicClient> = Arc::new(model::mock::MockAnthropicClient::new());

    let pool = SessionPool::new(
        8,
        3600,
        client,
        Arc::new(Settings::defaults_for("claude-sonnet-4-6")),
        scene,
        permission,
        cwd,
        history_store,
        paths,
    );
    (pool, dir)
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
        pool.watch
            .as_ref()
            .expect("a hub in a test process")
            .watched_path_count()
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

#[cfg(feature = "mcp")]
#[tokio::test]
async fn cache_hit_returns_the_same_arc_without_reconnecting() {
    let (pool, _dir) = test_pool().await;
    let first = pool.mcp_for_project(Some(pool.cwd()), "code").await;
    let second = pool.mcp_for_project(Some(pool.cwd()), "code").await;
    assert!(
        Arc::ptr_eq(&first, &second),
        "a cache hit must return the exact cached Arc, not a freshly connected one"
    );
}

#[cfg(feature = "mcp")]
#[tokio::test]
async fn different_projects_get_independent_cache_entries() {
    let (pool, dir) = test_pool().await;
    let other_project = dir.path().join("other");
    std::fs::create_dir_all(&other_project).unwrap();

    let default_manager = pool.mcp_for_project(Some(pool.cwd()), "code").await;
    let other_manager = pool.mcp_for_project(Some(&other_project), "code").await;
    assert!(
        !Arc::ptr_eq(&default_manager, &other_manager),
        "a project seen for the first time must get its own cache miss and connect, \
             not silently inherit another project's connections"
    );

    let other_again = pool.mcp_for_project(Some(&other_project), "code").await;
    assert!(
        Arc::ptr_eq(&other_manager, &other_again),
        "the second lookup for the same non-default project must be a cache hit"
    );
}

#[cfg(feature = "mcp")]
#[tokio::test]
async fn no_project_key_is_cached_independently_of_the_default_project() {
    let (pool, _dir) = test_pool().await;
    let none_manager = pool.mcp_for_project(None, "code").await;
    let default_manager = pool.mcp_for_project(Some(pool.cwd()), "code").await;
    assert!(
        !Arc::ptr_eq(&none_manager, &default_manager),
        "the no-project tier (`None`) must not share a cache slot with the default project"
    );
}

/// A changed `Authorization` has to reach the next connection; reusing the
/// cached one would keep sending the old token until a daemon restart.
#[cfg(feature = "mcp")]
#[tokio::test]
async fn a_settings_change_drops_exactly_the_connections_built_from_it() {
    use crate::watch_hub::Tier;
    let (pool, dir) = test_pool().await;
    let other = dir.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    let default_coding = pool.mcp_for_project(Some(pool.cwd()), "code").await;
    let default_harnix = pool.mcp_for_project(Some(pool.cwd()), "harnix").await;
    let other_coding = pool.mcp_for_project(Some(&other), "code").await;

    pool.forget_mcp_built_from(&Tier::Project(other.clone()), None)
        .await;
    assert!(!Arc::ptr_eq(
        &other_coding,
        &pool.mcp_for_project(Some(&other), "code").await
    ));
    assert!(Arc::ptr_eq(
        &default_coding,
        &pool.mcp_for_project(Some(pool.cwd()), "code").await
    ));

    pool.forget_mcp_built_from(&Tier::Scene("harnix".into()), None)
        .await;
    assert!(!Arc::ptr_eq(
        &default_harnix,
        &pool.mcp_for_project(Some(pool.cwd()), "harnix").await
    ));
    assert!(Arc::ptr_eq(
        &default_coding,
        &pool.mcp_for_project(Some(pool.cwd()), "code").await
    ));

    let kept = (Some(pool.cwd().to_path_buf()), "code".to_string());
    pool.forget_mcp_built_from(&Tier::Global, Some(&kept)).await;
    assert!(Arc::ptr_eq(
        &default_coding,
        &pool.mcp_for_project(Some(pool.cwd()), "code").await
    ));
    assert_eq!(pool.mcp_by_project.connected_pairs(), 1);
}

#[cfg(feature = "mcp")]
#[tokio::test]
async fn lost_change_notifications_drop_every_other_pairs_connections() {
    let (pool, dir) = test_pool().await;
    let other = dir.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    let other_coding = pool.mcp_for_project(Some(&other), "code").await;
    let default_harnix = pool.mcp_for_project(Some(pool.cwd()), "harnix").await;

    pool.reload_after_lost_changes().await;

    assert!(!Arc::ptr_eq(
        &other_coding,
        &pool.mcp_for_project(Some(&other), "code").await
    ));
    assert!(!Arc::ptr_eq(
        &default_harnix,
        &pool.mcp_for_project(Some(pool.cwd()), "harnix").await
    ));
}

fn is_stale(binding: &ProjectBinding) -> bool {
    binding.stale.load(std::sync::atomic::Ordering::Relaxed)
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
    assert!(is_stale(&other_coding));
    assert!(!is_stale(&own_coding) && !is_stale(&own_harnix));
    assert!(Arc::ptr_eq(
        &own_coding,
        &pool.binding_for(Some(pool.cwd()), "code").await
    ));

    pool.invalidate_bindings(Some(&Tier::Scene("harnix".into())))
        .await;
    assert!(is_stale(&own_harnix));
    assert!(!is_stale(&own_coding));

    pool.invalidate_bindings(Some(&Tier::Global)).await;
    assert!(is_stale(&own_coding));
    assert!(pool.bindings.lock().await.is_empty());
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

    pool.refresh_catalogs(
        &Tier::Project(pool.cwd().to_path_buf()),
        WatchTarget::Skills,
    )
    .await;

    assert!(binding.modules.skills().get("late").is_some());
    assert!(!is_stale(&binding));
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
    assert!(is_stale(&before));
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
    assert!(is_stale(&before));
    assert!(after.modules.has_agent_type("late-reviewer"));
    assert!(after.appeared().is_none());
    assert!(Arc::ptr_eq(
        &after,
        &pool.binding_for(Some(pool.cwd()), "mini").await
    ));
}

/// A pair whose server never finishes its handshake keeps its own first
/// connect waiting and no other pair's.
#[cfg(feature = "mcp")]
#[tokio::test(flavor = "multi_thread")]
async fn a_pair_stuck_connecting_holds_up_no_other_pairs_connect() {
    let (pool, dir) = test_pool().await;
    let pool = Arc::new(pool);
    let stuck = dir.path().join("stuck");
    let asked = dir.path().join("asked");
    std::fs::create_dir_all(stuck.join(".agix")).unwrap();
    std::fs::write(
        stuck.join(".agix").join("settings.json"),
        serde_json::json!({
            "mcp_servers": {
                "mute": {
                    "type": "stdio",
                    "command": "/bin/sh",
                    "args": ["-c", format!("touch '{}'; while read l; do :; done", asked.display())],
                    "startup_timeout_ms": 20_000,
                }
            }
        })
        .to_string(),
    )
    .unwrap();
    let other = dir.path().join("other");
    std::fs::create_dir_all(&other).unwrap();

    let mut connecting = tokio::spawn({
        let pool = pool.clone();
        async move { pool.mcp_for_project(Some(&stuck), "code").await }
    });
    while !asked.exists() {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let unaffected = pool.mcp_for_project(Some(&other), "code");
    tokio::pin!(unaffected);
    tokio::select! {
        biased;
        manager = &mut unaffected => {
            assert_eq!(manager.server_count(), 0);
            assert!(!connecting.is_finished(), "the other pair waited for the stuck connect");
        }
        _ = &mut connecting => panic!("the other pair waited for the stuck connect"),
    }
    connecting.abort();
}

/// Two first callers on one pair get the one manager its one connect made.
#[cfg(feature = "mcp")]
#[tokio::test(flavor = "multi_thread")]
async fn two_first_callers_on_one_pair_share_its_connect() {
    let (pool, dir) = test_pool().await;
    let other = dir.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    let (a, b) = tokio::join!(
        pool.mcp_for_project(Some(&other), "code"),
        pool.mcp_for_project(Some(&other), "code"),
    );
    assert!(Arc::ptr_eq(&a, &b));
}
