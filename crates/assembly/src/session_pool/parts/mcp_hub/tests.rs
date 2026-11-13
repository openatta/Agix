//! 按 `(project, scene)` 缓存连接的用例：缓存命中、各对互不影响、配置变化只丢掉
//! 由它建出的连接，以及首连卡住时不拖累别的对。

use crate::session_pool::tests::mcp_for_project::test_pool;
use crate::session_pool::*;

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
    assert_eq!(pool.mcp.cache.connected_pairs(), 1);
}

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

/// A pair whose server never finishes its handshake keeps its own first
/// connect waiting and no other pair's.
#[tokio::test(flavor = "multi_thread")]
async fn a_pair_stuck_connecting_holds_up_no_other_pairs_connect() {
    let (pool, dir) = test_pool().await;
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
