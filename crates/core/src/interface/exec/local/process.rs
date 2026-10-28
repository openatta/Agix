//! `LocalProcess` — a child process on this machine.
//!
//! On Unix every spawned program leads a process group of its own, and stopping
//! it signals the whole group: a command is everything it started, not just the
//! one pid `spawn` returned. A [`GroupLedger`] remembers those groups past the
//! handle, so whoever owns a session can stop what its commands left running.

use futures::future::BoxFuture;
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use futures::stream::BoxStream;
use futures::StreamExt;
use std::process::Stdio;
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

use super::env::SubprocessEnv;
use crate::interface::exec::{
    ExecError, ExitStatus, OutputChunk, OutputStream, Process, ProcessHandle, ProcessSpec,
};

#[derive(Debug, Clone, Default)]
pub struct LocalProcess {
    env: SubprocessEnv,
    ledger: Option<Arc<GroupLedger>>,
}

impl LocalProcess {
    pub fn new(env: SubprocessEnv) -> Self {
        Self { env, ledger: None }
    }

    pub fn with_ledger(env: SubprocessEnv, ledger: Arc<GroupLedger>) -> Self {
        Self {
            env,
            ledger: Some(ledger),
        }
    }
}

#[derive(Debug, Default)]
pub struct GroupLedger {
    groups: Mutex<BTreeSet<i32>>,
}

impl GroupLedger {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Forgets the groups that have emptied first: a group id is reused once
    /// its group is gone, and a stale entry would one day signal a stranger.
    pub fn record(&self, group: i32) {
        let mut groups = self.groups.lock().unwrap_or_else(|e| e.into_inner());
        groups.retain(|g| group_alive(*g));
        groups.insert(group);
    }

    pub fn groups(&self) -> Vec<i32> {
        let mut groups = self.groups.lock().unwrap_or_else(|e| e.into_inner());
        groups.retain(|g| group_alive(*g));
        groups.iter().copied().collect()
    }

    pub async fn reap(&self, grace: std::time::Duration) {
        let targets: Vec<i32> = {
            let mut groups = self.groups.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::take(&mut *groups)
                .into_iter()
                .filter(|g| group_alive(*g))
                .collect()
        };
        if targets.is_empty() {
            return;
        }
        #[cfg(unix)]
        {
            for g in &targets {
                signal_group(*g, libc::SIGTERM);
            }
            let deadline = tokio::time::Instant::now() + grace;
            while tokio::time::Instant::now() < deadline
                && targets.iter().any(|g| group_alive(*g))
            {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            for g in &targets {
                if group_alive(*g) {
                    signal_group(*g, libc::SIGKILL);
                }
            }
        }
        #[cfg(not(unix))]
        let _ = grace;
    }
}

#[cfg(unix)]
fn group_alive(group: i32) -> bool {
    // SAFETY: signal 0 only asks whether the group exists and is ours to signal.
    unsafe { libc::killpg(group, 0) == 0 }
}

#[cfg(not(unix))]
fn group_alive(_group: i32) -> bool {
    false
}

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
        self.env.apply(&mut cmd);
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        // A handle that is dropped without being waited on must not leave a
        // process behind: cancellation drops the handle, and a leaked child
        // would keep writing to a pipe nobody reads.
        cmd.kill_on_drop(true);
        // Its own group, so that stopping it reaches the background processes
        // it started. The price is that a terminal's Ctrl-C does not reach it
        // directly; a host turns Ctrl-C into cancellation, which does.
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
        #[cfg(unix)]
        if let (Some(ledger), Some(group)) = (&self.ledger, group) {
            ledger.record(group);
        }
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
        run_with(LocalProcess::default(), spec).await
    }

    async fn run_with(process: LocalProcess, spec: ProcessSpec) -> (String, String, ExitStatus) {
        let mut h = process.spawn(spec, CancellationToken::new()).await.unwrap();
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
        let mut h = LocalProcess::default()
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
        let mut h = LocalProcess::default()
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

    #[tokio::test]
    async fn the_environment_policy_reaches_the_child() {
        // `cargo test` runs the test binary with this set; under anything
        // else there is nothing to observe.
        if std::env::var_os("CARGO_MANIFEST_DIR").is_none() {
            return;
        }
        let probe = || sh("printenv CARGO_MANIFEST_DIR; printenv PATH >/dev/null && echo has-path");
        let (inherited, _, _) = run(probe()).await;
        assert!(inherited.contains(env!("CARGO_MANIFEST_DIR")));

        let allowlist = super::super::env::SubprocessEnv {
            mode: super::super::env::SubprocessEnvMode::Allowlist,
            allow: vec![],
        };
        let (out, _, _) = run_with(LocalProcess::new(allowlist), probe()).await;
        assert_eq!(out.trim(), "has-path");
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
        let mut h = LocalProcess::default()
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
        let mut h = LocalProcess::default()
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
    async fn the_ledger_reaps_what_a_finished_command_left_running() {
        let ledger = GroupLedger::new();
        let process = LocalProcess::with_ledger(SubprocessEnv::default(), ledger.clone());
        let mut h = process
            .spawn(sh("sleep 300 >/dev/null 2>&1 & echo $!"), CancellationToken::new())
            .await
            .unwrap();
        let mut out = h.output().unwrap();
        let pid: libc::pid_t = first_line(&mut out).await.parse().unwrap();
        assert!(h.wait().await.unwrap().success);
        drop(out);
        drop(h);
        assert!(
            !gone_within(pid, std::time::Duration::from_millis(200)).await,
            "dropping a finished handle leaves the background sleep alone"
        );
        assert_eq!(ledger.groups().len(), 1, "the group is still on record");

        ledger.reap(std::time::Duration::from_millis(500)).await;
        let reaped = gone_within(pid, std::time::Duration::from_secs(3)).await;
        if !reaped {
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        assert!(reaped, "reap stops the background sleep");
        assert!(ledger.groups().is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_emptied_group_drops_off_the_ledger() {
        let ledger = GroupLedger::new();
        let process = LocalProcess::with_ledger(SubprocessEnv::default(), ledger.clone());
        let (_, _, status) = run_with(process, sh("true")).await;
        assert!(status.success);
        assert!(ledger.groups().is_empty(), "{:?}", ledger.groups());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_a_running_handle_stops_the_whole_group() {
        let mut h = LocalProcess::default()
            .spawn(sh("sleep 300 & echo $!; wait"), CancellationToken::new())
            .await
            .unwrap();
        let mut out = h.output().unwrap();
        let pid: libc::pid_t = first_line(&mut out).await.parse().unwrap();
        drop(h);
        assert!(gone_within(pid, std::time::Duration::from_secs(3)).await);
    }
}
