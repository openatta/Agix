//! `with_session_lock_tests` 原样搬来的用例。

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

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

async fn test_pool() -> (Arc<SessionPool>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let scene: Arc<dyn Scene> = Arc::new(scene::code::CodeScene::default());
    let permission: Option<Arc<dyn Permission>> = Some(Arc::new(AllowAllTestPermission));
    let paths: Arc<dyn crate::config::DaemonPaths> = Arc::new(
        crate::config::StaticDaemonPaths::new(dir.path().to_path_buf()),
    );
    let client: Arc<dyn AnthropicClient> = Arc::new(model::mock::MockAnthropicClient::new());

    let pool = Arc::new(SessionPool::new(
        8,
        3600,
        client,
        Arc::new(Settings::defaults_for("claude-sonnet-4-6")),
        scene,
        permission,
        cwd,
        None,
        paths,
    ));
    (pool, dir)
}

/// `session.close`'s cascade delete and a concurrent
/// `session.resume(..., create_if_missing: true)` on the same sidechain id
/// must not interleave. This drives many concurrent critical sections for
/// the *same* id and asserts they never overlap.
#[tokio::test]
async fn concurrent_calls_for_the_same_id_never_overlap() {
    let (pool, _dir) = test_pool().await;
    let concurrent = Arc::new(AtomicUsize::new(0));
    let max_concurrent = Arc::new(AtomicUsize::new(0));

    let mut tasks = Vec::new();
    for _ in 0..8 {
        let pool = pool.clone();
        let concurrent = concurrent.clone();
        let max_concurrent = max_concurrent.clone();
        tasks.push(tokio::spawn(async move {
            pool.with_session_lock("shared-id", async {
                let now = concurrent.fetch_add(1, Ordering::SeqCst) + 1;
                max_concurrent.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(5)).await;
                concurrent.fetch_sub(1, Ordering::SeqCst);
            })
            .await;
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }

    assert_eq!(
        max_concurrent.load(Ordering::SeqCst),
        1,
        "critical sections for the same session_id must never overlap"
    );
}

/// The flip side: locking must be per-id, not a single pool-wide mutex —
/// otherwise every unrelated `session.close`/`.resume` pair would
/// serialize against each other too.
#[tokio::test]
async fn distinct_ids_run_concurrently() {
    let (pool, _dir) = test_pool().await;
    let concurrent = Arc::new(AtomicUsize::new(0));
    let max_concurrent = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(tokio::sync::Barrier::new(4));

    let mut tasks = Vec::new();
    for i in 0..4 {
        let pool = pool.clone();
        let concurrent = concurrent.clone();
        let max_concurrent = max_concurrent.clone();
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            pool.with_session_lock(&format!("id-{i}"), async {
                barrier.wait().await;
                let now = concurrent.fetch_add(1, Ordering::SeqCst) + 1;
                max_concurrent.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(20)).await;
                concurrent.fetch_sub(1, Ordering::SeqCst);
            })
            .await;
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }

    assert!(
        max_concurrent.load(Ordering::SeqCst) > 1,
        "distinct session ids must not serialize against each other"
    );
}

#[tokio::test]
async fn registry_entry_is_cleaned_up_after_use() {
    let (pool, _dir) = test_pool().await;
    pool.with_session_lock("temp-id", async {}).await;
    assert!(
        pool.session_locks.lock().await.is_empty(),
        "a lock nothing else references must not linger in the registry forever"
    );
}
