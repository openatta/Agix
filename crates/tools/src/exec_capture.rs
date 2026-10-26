//! Running a process through the execution contract and keeping all of it.
//!
//! `Process` hands back a stream because a long command's output has to reach
//! the user while it is still running. Most tools do not want that: they run
//! `git`, wait, and parse what it printed. Those call sites drain the stream
//! themselves, and this is the one place that does it.

use base::interface::exec::{ExecError, ExecProviders, ExitStatus, OutputStream, ProcessSpec};
use futures::StreamExt;
use tokio_util::sync::CancellationToken;

#[derive(Debug)]
pub(crate) struct Captured {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Captured {
    pub fn stdout_lossy(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.stdout)
    }

    pub fn stderr_lossy(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.stderr)
    }
}

pub(crate) async fn capture(
    exec: &ExecProviders,
    spec: ProcessSpec,
    cancel: CancellationToken,
) -> Result<Captured, ExecError> {
    let mut child = exec.process.spawn(spec, cancel).await?;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    // Drained before waiting: a child that fills its output pipe blocks until
    // someone reads it, so a `wait` first would deadlock on anything verbose.
    if let Some(mut chunks) = child.output() {
        while let Some(chunk) = chunks.next().await {
            let chunk = chunk?;
            match chunk.stream {
                OutputStream::Stdout => stdout.extend_from_slice(&chunk.bytes),
                OutputStream::Stderr => stderr.extend_from_slice(&chunk.bytes),
            }
        }
    }
    let status = child.wait().await?;
    Ok(Captured {
        status,
        stdout,
        stderr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(command: &str) -> ProcessSpec {
        ProcessSpec::new("bash", std::env::temp_dir()).args(["-c".to_string(), command.to_string()])
    }

    #[tokio::test]
    async fn both_pipes_and_the_status_survive_the_round_trip() {
        let out = capture(
            &ExecProviders::local(),
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
                &ExecProviders::local(),
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
            &ExecProviders::local(),
            ProcessSpec::new("agix-no-such-program", std::env::temp_dir()),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(matches!(e, ExecError::Failed(_)), "{e:?}");
    }
}
