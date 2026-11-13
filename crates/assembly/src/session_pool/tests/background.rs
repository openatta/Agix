//! 池的后台循环（janitor、配置监视器）不让池活过它的最后一个持有者。

use super::mcp_for_project::test_pool;
use super::*;

#[tokio::test]
async fn the_janitor_lets_the_pool_go_and_then_ends() {
    let (pool, _dir) = test_pool().await;
    let janitor = pool.start_janitor();
    let weak = Arc::downgrade(&pool);
    drop(pool);
    assert!(
        weak.upgrade().is_none(),
        "the janitor does not keep the pool alive"
    );

    tokio::time::pause();
    tokio::time::timeout(Duration::from_secs(3600), janitor)
        .await
        .expect("the janitor ends at its next sweep once the pool is gone")
        .expect("the janitor did not panic");
}

#[tokio::test]
async fn the_config_watcher_lets_the_pool_go() {
    let (pool, _dir) = test_pool().await;
    let _watcher = pool.start_config_watcher();
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
    let weak = Arc::downgrade(&pool);
    drop(pool);
    assert!(
        weak.upgrade().is_none(),
        "the config watcher does not keep the pool alive"
    );
}
