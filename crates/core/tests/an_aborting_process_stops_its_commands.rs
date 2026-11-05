//! What the panic hook does before an abort: every group a command leads is
//! killed, because nothing else will run to stop it.
//!
//! Its own test binary on purpose: `orphans::kill_all` reaches every group
//! this process has started, and in a shared test binary that would include
//! other tests' commands.

#![cfg(unix)]

use base::interface::exec::local::{orphans, LocalProcess};
use base::interface::exec::{Process, ProcessSpec};
use tokio_util::sync::CancellationToken;

fn group_alive(group: i32) -> bool {
    unsafe { libc::killpg(group, 0) == 0 }
}

#[tokio::test]
async fn kill_all_stops_a_running_command_and_its_children() {
    let mut spec = ProcessSpec::new("sh", std::env::temp_dir());
    spec.args = vec!["-c".into(), "tail -f /dev/null & tail -f /dev/null".into()];
    let handle = LocalProcess::default()
        .spawn(spec, CancellationToken::new())
        .await
        .unwrap();

    let killed = orphans::kill_all();
    assert_eq!(killed.len(), 1, "{killed:?}");
    let group = killed[0];
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while group_alive(group) && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    drop(handle);
    assert!(!group_alive(group), "group {group} outlived kill_all");
}
