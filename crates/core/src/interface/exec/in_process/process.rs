//! Processes that were decided in advance.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use futures::stream::BoxStream;
use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::interface::exec::{
    ExecError, ExitStatus, OutputChunk, OutputStream, Process, ProcessHandle, ProcessSpec,
};

/// What a scripted command answers with.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScriptedRun {
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
}

impl ScriptedRun {
    pub fn ok(stdout: impl Into<String>) -> Self {
        Self {
            stdout: stdout.into(),
            stderr: String::new(),
            code: 0,
        }
    }

    pub fn failing(code: i32, stderr: impl Into<String>) -> Self {
        Self {
            stdout: String::new(),
            stderr: stderr.into(),
            code,
        }
    }
}

/// A process provider whose answers are written down beforehand.
///
/// Matched on the whole argv joined by spaces, which is crude and deliberate:
/// a matcher with globs or regexes is a small language, and a test that needs
/// one is usually testing the matcher.
///
/// A command with no entry fails rather than succeeding quietly — an
/// unscripted command in a test is a gap in the test, and returning empty
/// success would hide it.
#[derive(Clone, Default)]
pub struct ScriptedProcess {
    runs: Arc<Mutex<BTreeMap<String, ScriptedRun>>>,
    seen: Arc<Mutex<Vec<String>>>,
}

impl ScriptedProcess {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(self, argv: impl Into<String>, run: ScriptedRun) -> Self {
        self.runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(argv.into(), run);
        self
    }

    /// Every command that was asked for, in order — including the ones with
    /// no entry.
    pub fn seen(&self) -> Vec<String> {
        self.seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn key(spec: &ProcessSpec) -> String {
        std::iter::once(spec.program.clone())
            .chain(spec.args.iter().cloned())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[async_trait::async_trait]
impl Process for ScriptedProcess {
    async fn spawn(
        &self,
        spec: ProcessSpec,
        _cancel: CancellationToken,
    ) -> Result<Box<dyn ProcessHandle>, ExecError> {
        let key = Self::key(&spec);
        self.seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(key.clone());
        let run = self
            .runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
            .cloned();
        let Some(run) = run else {
            return Err(ExecError::failed(format!("nothing scripted for `{key}`")));
        };
        Ok(Box::new(ScriptedHandle {
            run: Some(run),
            taken: false,
        }))
    }
}

struct ScriptedHandle {
    run: Option<ScriptedRun>,
    taken: bool,
}

impl ProcessHandle for ScriptedHandle {
    fn output(&mut self) -> Option<BoxStream<'static, Result<OutputChunk, ExecError>>> {
        if self.taken {
            return None;
        }
        self.taken = true;
        let run = self.run.clone().unwrap_or_default();
        let mut chunks = Vec::new();
        if !run.stdout.is_empty() {
            chunks.push(Ok(OutputChunk {
                stream: OutputStream::Stdout,
                bytes: run.stdout.into_bytes(),
            }));
        }
        if !run.stderr.is_empty() {
            chunks.push(Ok(OutputChunk {
                stream: OutputStream::Stderr,
                bytes: run.stderr.into_bytes(),
            }));
        }
        Some(futures::stream::iter(chunks).boxed())
    }

    fn kill(&mut self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }

    fn wait(&mut self) -> BoxFuture<'_, Result<ExitStatus, ExecError>> {
        let code = self.run.as_ref().map(|r| r.code).unwrap_or(0);
        Box::pin(async move {
            Ok(ExitStatus {
                code: Some(code),
                success: code == 0,
            })
        })
    }
}
