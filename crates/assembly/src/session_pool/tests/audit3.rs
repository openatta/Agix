//! 0.3.12 发布前对抗式审查（第三路：会话池、失败与取消路径）核实为真的缺陷。

use super::mcp_for_project::test_pool_with_store;
use super::*;
use std::sync::mpsc;
use std::sync::Mutex as StdMutex;

async fn pool_with_history() -> (SessionPool, tempfile::TempDir, tempfile::TempDir) {
    let store_dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn history::store::HistoryStore> = Arc::new(
        history::store::JsonlHistoryStore::with_roots(
            store_dir.path(),
            history::path::HistoryRoots::under(store_dir.path()),
        )
        .await
        .unwrap(),
    );
    let (pool, dir) = test_pool_with_store(Some(store)).await;
    (pool, dir, store_dir)
}

async fn new_session(pool: &SessionPool) -> String {
    pool.create_session(None, ProjectSelector::Default, None)
        .await
        .expect("a session")["session_id"]
        .as_str()
        .expect("a session id")
        .to_string()
}

/// Lets the first engine build through and holds the second one — the
/// rebuild — until the test says go.
struct HoldSecondBuild {
    builds: std::sync::atomic::AtomicUsize,
    entered: StdMutex<mpsc::Sender<()>>,
    go: StdMutex<mpsc::Receiver<()>>,
}

/// `shutdown_session` does not take the session lock the rebuild holds, so
/// it removes the entry while `create` is still building the replacement;
/// `create` then inserts into the empty slot as if it were a fresh session.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "audit: confirmed defect, fix pending"]
async fn audit3_close_during_a_rebuild_is_not_undone_by_the_rebuild() {
    let (pool, _dir, _store) = pool_with_history().await;
    let (entered_tx, entered_rx) = mpsc::channel();
    let (go_tx, go_rx) = mpsc::channel();
    let hold = Arc::new(HoldSecondBuild {
        builds: Default::default(),
        entered: StdMutex::new(entered_tx),
        go: StdMutex::new(go_rx),
    });
    let held = hold.clone();
    let pool = Arc::new(pool.with_session_customizer(Arc::new(move |builder, _| {
        if held
            .builds
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            == 1
        {
            let _ = held.entered.lock().unwrap().send(());
            let _ = held.go.lock().unwrap().recv();
        }
        builder
    })));
    let sid = new_session(&pool).await;

    let rebuild = {
        let pool = pool.clone();
        let sid = sid.clone();
        tokio::spawn(async move {
            pool.set_session_prompt(&sid, |p| p.append = Some("Answer briefly.".into()))
                .await
                .is_ok()
        })
    };
    tokio::task::spawn_blocking(move || entered_rx.recv().unwrap())
        .await
        .unwrap();

    pool.shutdown_session(&sid).await.unwrap();
    assert!(!pool.sessions.lock().await.contains_key(&sid));

    go_tx.send(()).unwrap();
    let _ = rebuild.await.unwrap();
    assert!(
        !pool.sessions.lock().await.contains_key(&sid),
        "session.close returned, yet the rebuild in flight put the session back in memory"
    );
}
