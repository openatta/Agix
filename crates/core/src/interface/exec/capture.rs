//! Running a process through the execution contract and keeping all of it.
//!
//! `Process` hands back a stream because a long command's output has to reach
//! the user while it is still running. Most callers do not want that: they run
//! `git`, wait, and parse what it printed. Those call sites drain the stream
//! through here.

use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use super::{ExecError, ExitStatus, OutputStream, Process, ProcessSpec};

#[derive(Debug)]
pub struct Captured {
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

pub async fn capture(
    process: &dyn Process,
    spec: ProcessSpec,
    cancel: CancellationToken,
) -> Result<Captured, ExecError> {
    let mut child = process.spawn(spec, cancel).await?;
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
    use crate::interface::exec::in_process::{ScriptedProcess, ScriptedRun};

    #[tokio::test]
    async fn both_streams_and_the_status_are_kept_apart() {
        let process = ScriptedProcess::new().with(
            "git status",
            ScriptedRun {
                stdout: "out".into(),
                stderr: "err".into(),
                code: 2,
            },
        );
        let out = capture(
            &process,
            ProcessSpec::new("git", "/").arg("status"),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(out.stdout_lossy(), "out");
        assert_eq!(out.stderr_lossy(), "err");
        assert_eq!(out.status.code, Some(2));
        assert!(!out.status.success);
    }

    #[tokio::test]
    async fn a_spawn_failure_is_returned_as_is() {
        let e = capture(
            &ScriptedProcess::new(),
            ProcessSpec::new("git", "/"),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(matches!(e, ExecError::Failed(_)), "{e:?}");
    }
}
