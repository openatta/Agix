//! `base::interface::exec::capture` against real processes on this machine.

use base::interface::exec::capture::capture;
use base::interface::exec::{ExecError, ProcessSpec};
use platform::LocalProcess;
use tokio_util::sync::CancellationToken;

fn sh(command: &str) -> ProcessSpec {
    ProcessSpec::new("bash", std::env::temp_dir()).args(["-c".to_string(), command.to_string()])
}

#[tokio::test]
async fn both_pipes_and_the_status_survive_the_round_trip() {
    let out = capture(
        &LocalProcess::default(),
        sh("echo out; echo err >&2; exit 2"),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(out.stdout_lossy().trim(), "out");
    assert_eq!(out.stderr_lossy().trim(), "err");
    assert_eq!(out.status.code, Some(2));
}

/// The reason the stream is drained before the wait.
#[tokio::test]
async fn a_child_that_outruns_its_pipe_buffer_still_finishes() {
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        capture(
            &LocalProcess::default(),
            sh("head -c 400000 /dev/zero | tr '\\0' 'x'"),
            CancellationToken::new(),
        ),
    )
    .await
    .expect("waiting before draining would hang here")
    .unwrap();
    assert_eq!(out.stdout.len(), 400_000);
}

#[tokio::test]
async fn a_missing_program_is_the_targets_failure_not_an_unreachable_provider() {
    let e = capture(
        &LocalProcess::default(),
        ProcessSpec::new("agix-no-such-program", std::env::temp_dir()),
        CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert!(matches!(e, ExecError::Failed(_)), "{e:?}");
}
