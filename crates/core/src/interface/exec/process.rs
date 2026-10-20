//! `Process` — running a program somewhere.

use futures::future::BoxFuture;
use futures::stream::BoxStream;
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;

use super::ExecError;

/// What to run. A shell command is one of these by the time it gets here:
/// `bash -c <command>`. There is deliberately no separate shell contract —
/// two execution contracts would mean two places a sandbox has to be attached,
/// and therefore two places it can be missed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    /// Added to whatever the provider's own environment is. Not a replacement:
    /// a remote provider's environment is not this machine's to dictate.
    pub env: Vec<(String, String)>,
    pub stdin: Option<Vec<u8>>,
}

impl ProcessSpec {
    pub fn new(program: impl Into<String>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            cwd: cwd.into(),
            env: Vec::new(),
            stdin: None,
        }
    }

    pub fn arg(mut self, a: impl Into<String>) -> Self {
        self.args.push(a.into());
        self
    }

    pub fn args<I: IntoIterator<Item = S>, S: Into<String>>(mut self, it: I) -> Self {
        self.args.extend(it.into_iter().map(Into::into));
        self
    }
}

/// How long output may keep arriving once the process itself has exited.
///
/// A background process the command started inherits its pipes, and they stay
/// open as long as it runs. Reading on to EOF would wait for that process, not
/// for the command; past this a reader stops and treats what it has as the
/// output.
pub const OUTPUT_GRACE_AFTER_EXIT: std::time::Duration = std::time::Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputStream {
    Stdout,
    Stderr,
}

/// A piece of output, tagged with where it came from.
///
/// One stream rather than two, because a remote provider has one channel and
/// splitting it would make ordering a provider's problem to reinvent. A
/// consumer that wants the two separated accumulates two buffers; a consumer
/// that wants them interleaved already has them in order.
///
/// Bytes rather than lines: line splitting is a consumer's concern, and a
/// provider that split for them would be deciding what a line is on output
/// that may not have any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputChunk {
    pub stream: OutputStream,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitStatus {
    /// `None` when the process was killed by a signal.
    pub code: Option<i32>,
    pub success: bool,
}

/// A process that has started.
pub trait ProcessHandle: Send {
    /// The output, incrementally. `None` after the first call — the stream
    /// owns the pipes.
    ///
    /// **Streaming is a requirement, not an optimization.** A long-running
    /// command's output reaches the user while it runs; a provider that
    /// returned everything at the end would remove that silently.
    fn output(&mut self) -> Option<BoxStream<'static, Result<OutputChunk, ExecError>>>;

    /// Stop it and whatever it started that is still running. Best effort,
    /// and idempotent; also valid after `wait`, for the descendants that
    /// outlived it.
    fn kill(&mut self) -> BoxFuture<'_, ()>;

    /// Wait for it to finish.
    fn wait(&mut self) -> BoxFuture<'_, Result<ExitStatus, ExecError>>;
}

/// Where a program runs.
#[async_trait::async_trait]
pub trait Process: Send + Sync {
    async fn spawn(
        &self,
        spec: ProcessSpec,
        cancel: CancellationToken,
    ) -> Result<Box<dyn ProcessHandle>, ExecError>;
}
