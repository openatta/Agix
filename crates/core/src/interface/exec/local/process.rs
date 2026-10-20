//! `LocalProcess` — a child process on this machine.
//!
//! On Unix every spawned program leads a process group of its own, and stopping
//! it signals the whole group: a command is everything it started, not just the
//! one pid `spawn` returned.

use futures::future::BoxFuture;
use futures::stream::BoxStream;
use futures::StreamExt;
use std::process::Stdio;
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

use crate::interface::exec::{
    ExecError, ExitStatus, OutputChunk, OutputStream, Process, ProcessHandle, ProcessSpec,
};

pub struct LocalProcess;

/// How long a stopped group gets to exit on `SIGTERM` before `SIGKILL`.
#[cfg(unix)]
const TERM_GRACE: std::time::Duration = std::time::Duration::from_millis(500);

#[async_trait::async_trait]
impl Process for LocalProcess {
    async fn spawn(
        &self,
        spec: ProcessSpec,
        _cancel: CancellationToken,
    ) -> Result<Box<dyn ProcessHandle>, ExecError> {
        let mut cmd = tokio::process::Command::new(&spec.program);
        cmd.args(&spec.args)
            .current_dir(&spec.cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(if spec.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            });
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        // A handle that is dropped without being waited on must not leave a
        // process behind: cancellation drops the handle, and a leaked child
        // would keep writing to a pipe nobody reads.
        cmd.kill_on_drop(true);
        // Its own group, so that stopping it reaches the background processes
        // it started. The price is that a terminal's Ctrl-C no longer arrives
        // here directly; a host turns Ctrl-C into cancellation, which does.
        #[cfg(unix)]
        cmd.process_group(0);

        let mut child = cmd
            .spawn()
            .map_err(|e| ExecError::failed(format!("{}: {e}", spec.program)))?;

        if let Some(bytes) = spec.stdin {
            if let Some(mut stdin) = child.stdin.take() {
                // Fed from a task of its own and closed when done: a child that
                // prints before it reads would otherwise fill its stdout pipe
                // while this call is still blocked writing, and a child reading
                // to EOF needs the pipe closed.
                tokio::spawn(async move {
                    use tokio::io::AsyncWriteExt;
                    let _ = stdin.write_all(&bytes).await;
                });
            }
        }

        #[cfg(unix)]
        let group = child.id().map(|pid| pid as libc::pid_t);
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        Ok(Box::new(LocalHandle {
            child,
            #[cfg(unix)]
            group,
            output: Some(interleave(stdout, stderr)),
        }))
    }
}

/// Both pipes as one tagged stream.
///
/// `select` rather than chaining, so a command that writes to stderr while
/// stdout is quiet is not held up behind it.
fn interleave(
    stdout: Option<tokio::process::ChildStdout>,
    stderr: Option<tokio::process::ChildStderr>,
) -> BoxStream<'static, Result<OutputChunk, ExecError>> {
    fn chunks<R: AsyncReadExt + Unpin + Send + 'static>(
        reader: Option<R>,
        stream: OutputStream,
    ) -> BoxStream<'static, Result<OutputChunk, ExecError>> {
        let Some(reader) = reader else {
            return futures::stream::empty().boxed();
        };
        // The state goes to `None` after a read error, which ends the stream.
        // Yielding the error and staying alive would hand a consumer that logs
        // and continues an endless supply of the same broken pipe.
        futures::stream::unfold(Some((reader, Vec::new())), move |state| async move {
            let (mut reader, mut buf) = state?;
            buf.resize(8192, 0);
            match reader.read(&mut buf).await {
                Ok(0) => None,
                Ok(n) => {
                    buf.truncate(n);
                    let chunk = OutputChunk {
                        stream,
                        bytes: std::mem::take(&mut buf),
                    };
                    Some((Ok(chunk), Some((reader, buf))))
                }
                Err(e) => Some((Err(ExecError::failed(e)), None)),
            }
        })
        .boxed()
    }
    futures::stream::select(
        chunks(stdout, OutputStream::Stdout),
        chunks(stderr, OutputStream::Stderr),
    )
    .boxed()
}

struct LocalHandle {
    child: tokio::process::Child,
    #[cfg(unix)]
    group: Option<libc::pid_t>,
    output: Option<BoxStream<'static, Result<OutputChunk, ExecError>>>,
}

#[cfg(unix)]
fn signal_group(group: libc::pid_t, signal: libc::c_int) {
    // SAFETY: plain integers in, nothing borrowed; a group that is already
    // gone comes back as ESRCH, which is the outcome being asked for.
    unsafe {
        libc::killpg(group, signal);
    }
}

impl Drop for LocalHandle {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(group) = self.group {
            if matches!(self.child.try_wait(), Ok(None)) {
                signal_group(group, libc::SIGKILL);
            }
        }
    }
}

impl ProcessHandle for LocalHandle {
    fn output(&mut self) -> Option<BoxStream<'static, Result<OutputChunk, ExecError>>> {
        self.output.take()
    }

    fn kill(&mut self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            #[cfg(unix)]
            if let Some(group) = self.group {
                signal_group(group, libc::SIGTERM);
                let _ = tokio::time::timeout(TERM_GRACE, self.child.wait()).await;
                signal_group(group, libc::SIGKILL);
            }
            let _ = self.child.kill().await;
        })
    }

    fn wait(&mut self) -> BoxFuture<'_, Result<ExitStatus, ExecError>> {
        Box::pin(async move {
            let s = self.child.wait().await.map_err(ExecError::failed)?;
            Ok(ExitStatus {
                code: s.code(),
                success: s.success(),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn run(spec: ProcessSpec) -> (String, String, ExitStatus) {
        let mut h = LocalProcess
            .spawn(spec, CancellationToken::new())
            .await
            .unwrap();
        let mut out = h.output().expect("first call yields the stream");
        let (mut o, mut e) = (Vec::new(), Vec::new());
        while let Some(chunk) = out.next().await {
            let c = chunk.unwrap();
            match c.stream {
                OutputStream::Stdout => o.extend(c.bytes),
                OutputStream::Stderr => e.extend(c.bytes),
            }
        }
        let status = h.wait().await.unwrap();
        (
            String::from_utf8_lossy(&o).into_owned(),
            String::from_utf8_lossy(&e).into_owned(),
            status,
        )
    }

    fn sh(command: &str) -> ProcessSpec {
        ProcessSpec::new("bash", std::env::temp_dir()).args(["-c".to_string(), command.to_string()])
    }

    #[tokio::test]
    async fn both_pipes_arrive_tagged_and_the_exit_status_comes_back() {
        let (out, err, status) = run(sh("echo hi; echo bad >&2; exit 3")).await;
        assert_eq!(out.trim(), "hi");
        assert_eq!(err.trim(), "bad");
        assert_eq!(status.code, Some(3));
        assert!(!status.success);
    }

    #[tokio::test]
    async fn the_stream_is_taken_once() {
        let mut h = LocalProcess
            .spawn(sh("true"), CancellationToken::new())
            .await
            .unwrap();
        assert!(h.output().is_some());
        assert!(h.output().is_none(), "the stream owns the pipes");
        let _ = h.wait().await;
    }

    #[tokio::test]
    async fn stdin_reaches_the_child_and_then_ends() {
        let mut spec = sh("cat");
        spec.stdin = Some(b"fed in".to_vec());
        let (out, _, status) = run(spec).await;
        assert_eq!(out, "fed in");
        assert!(
            status.success,
            "a child reading to EOF has to see the pipe close"
        );
    }

    /// Output arrives while the process is still running, which is the
    /// property a whole-output provider would silently remove.
    #[tokio::test]
    async fn output_arrives_before_the_process_exits() {
        let mut h = LocalProcess
            .spawn(
                sh("echo first; sleep 5; echo never"),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let mut out = h.output().unwrap();
        let first = tokio::time::timeout(std::time::Duration::from_secs(2), out.next())
            .await
            .expect("the first chunk must not wait for the exit")
            .unwrap()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&first.bytes).trim(), "first");
        h.kill().await;
    }

    #[cfg(unix)]
    async fn gone_within(pid: libc::pid_t, limit: std::time::Duration) -> bool {
        let deadline = std::time::Instant::now() + limit;
        while std::time::Instant::now() < deadline {
            // SAFETY: signal 0 only asks whether the pid exists.
            if unsafe { libc::kill(pid, 0) } != 0 {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        false
    }

    #[cfg(unix)]
    async fn first_line(out: &mut BoxStream<'static, Result<OutputChunk, ExecError>>) -> String {
        let chunk = tokio::time::timeout(std::time::Duration::from_secs(5), out.next())
            .await
            .expect("the background pid is printed straight away")
            .unwrap()
            .unwrap();
        String::from_utf8_lossy(&chunk.bytes).trim().to_string()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn kill_takes_the_background_children_with_it() {
        let mut h = LocalProcess
            .spawn(sh("sleep 300 & echo $!; wait"), CancellationToken::new())
            .await
            .unwrap();
        let mut out = h.output().unwrap();
        let pid: libc::pid_t = first_line(&mut out).await.parse().unwrap();
        h.kill().await;
        assert!(
            gone_within(pid, std::time::Duration::from_secs(3)).await,
            "the background sleep outlived the kill"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn kill_after_exit_still_reaches_what_was_left_running() {
        let mut h = LocalProcess
            .spawn(sh("sleep 300 & echo $!"), CancellationToken::new())
            .await
            .unwrap();
        let mut out = h.output().unwrap();
        let pid: libc::pid_t = first_line(&mut out).await.parse().unwrap();
        assert!(h.wait().await.unwrap().success);
        h.kill().await;
        assert!(
            gone_within(pid, std::time::Duration::from_secs(3)).await,
            "the orphaned sleep survived a kill issued after its parent exited"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_a_running_handle_stops_the_whole_group() {
        let mut h = LocalProcess
            .spawn(sh("sleep 300 & echo $!; wait"), CancellationToken::new())
            .await
            .unwrap();
        let mut out = h.output().unwrap();
        let pid: libc::pid_t = first_line(&mut out).await.parse().unwrap();
        drop(h);
        assert!(gone_within(pid, std::time::Duration::from_secs(3)).await);
    }
}
