//! 0.3.12 发布前对抗式审查（第三路：会话池、失败与取消路径）核实为真的缺陷。

use super::rebuild::builder_with_history;
use super::*;
use std::sync::mpsc;
use std::sync::Mutex as StdMutex;

async fn new_session(pool: &SessionPool) -> String {
    pool.create_session(None, ProjectSelector::Default, None)
        .await
        .expect("a session")
        .session_id
}

/// The rebuild holding `sid`'s lock and one more caller queued on it.
async fn waits_on_session_lock(pool: &SessionPool, sid: &str) -> bool {
    pool.session_locks
        .lock()
        .await
        .get(sid)
        .is_some_and(|lock| Arc::strong_count(lock) >= 3)
}

/// Lets the first engine build through and holds the second one — the
/// rebuild — until the test says go.
struct HoldSecondBuild {
    builds: std::sync::atomic::AtomicUsize,
    entered: StdMutex<mpsc::Sender<()>>,
    go: StdMutex<mpsc::Receiver<()>>,
}

/// A `session.close` issued while a rebuild is building the replacement
/// engine must still leave the session closed once both are done.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn audit3_close_during_a_rebuild_is_not_undone_by_the_rebuild() {
    let (pool, _dir, _store) = builder_with_history().await;
    let (entered_tx, entered_rx) = mpsc::channel();
    let (go_tx, go_rx) = mpsc::channel();
    let hold = Arc::new(HoldSecondBuild {
        builds: Default::default(),
        entered: StdMutex::new(entered_tx),
        go: StdMutex::new(go_rx),
    });
    let held = hold.clone();
    let pool = pool
        .session_customizer(Arc::new(move |builder, _| {
            if held
                .builds
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                == 1
            {
                let _ = held.entered.lock().unwrap().send(());
                let _ = held.go.lock().unwrap().recv();
            }
            builder
        }))
        .build();
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

    let close = {
        let pool = pool.clone();
        let sid = sid.clone();
        tokio::spawn(async move { pool.shutdown_session(&sid).await })
    };
    while !close.is_finished() && !waits_on_session_lock(&pool, &sid).await {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    go_tx.send(()).unwrap();
    let _ = rebuild.await.unwrap();
    close.await.unwrap().unwrap();
    assert!(
        !pool.sessions.lock().await.contains_key(&sid),
        "session.close returned, yet the rebuild in flight put the session back in memory"
    );
}
