//! `ImportCallback` trait — automatic, process-level prompt for importing
//! configuration detected from another agent tool (Claude Code/Codex/Cursor).
//!
//! Deliberately **not** attached to `Builder`/`Settings`: detection runs once
//! per host process (see `frozen::import::maybe_detect_and_import`), not once
//! per session/`Agent`, so wiring it through the session-scoped `Builder`
//! would be the wrong lifetime. Hosts call `maybe_detect_and_import` directly
//! at their own process-startup point, passing `Some(callback)` if they can
//! synchronously ask a human, or `None` to skip the automatic path entirely
//! (the manual `/import` path — the bundled `import` skill — remains
//! available either way).

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::frozen::{
    detect_import_sources, execute_import, import_already_decided, mark_imported, mark_skipped,
    ImportSource,
};
use crate::interface::elicitation::{
    ElicitKind, ElicitOption, ElicitOutcome, ElicitRequest, Elicitation,
};

/// What the host decided after being shown the detected import sources.
///
/// Wire shape: `{"type":"import","source":"claude_code"}` / `{"type":"skip"}`
/// / `{"type":"defer"}` — what an [`Elicitation`] implementation returns for
/// an [`ElicitKind::Import`] question. Adjacently tagged rather than
/// internally tagged so the `Import` variant can keep carrying its kind
/// directly instead of being widened into a struct variant nobody asked for.
///
/// [`Elicitation`]: crate::interface::elicitation::Elicitation
/// [`ElicitKind::Import`]: crate::interface::elicitation::ElicitKind::Import
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", content = "source", rename_all = "snake_case")]
pub enum ImportDecision {
    /// Import from exactly one of the offered sources (single-select by
    /// design — see migration doc §3.0).
    Import(crate::frozen::ImportSourceKind),
    /// User explicitly declined. Persists a `skipped` marker so the
    /// automatic path won't ask again for this project.
    Skip,
    /// No decision yet (e.g. host chose "ask me later"). Leaves no marker —
    /// the automatic path will ask again next process start.
    Defer,
}

/// Implemented by hosts that can synchronously present detected import
/// sources to a human and get a decision back.
///
/// Superseded by [`Elicitation`], which is the one place the engine asks a
/// person anything. This stays because it is a published trait and hosts
/// implement it; [`maybe_detect_and_import`] adapts it to the new contract,
/// so an existing implementation keeps working unchanged.
///
/// [`Elicitation`]: crate::interface::elicitation::Elicitation
#[async_trait]
pub trait ImportCallback: Send + Sync {
    /// Called once per process (at most) when importable sources are
    /// detected and no prior decision is on record. Implementations should
    /// present `sources` to the user however fits their UI and return the
    /// decision. If this takes longer than the caller's timeout, it's
    /// treated the same as an unregistered callback — no import happens,
    /// and (per `already_decided`'s contract) nothing is persisted, so the
    /// next process start asks again.
    async fn on_import_detected(&self, sources: &[ImportSource]) -> ImportDecision;
}

/// The automatic import-detection entry point. Call this **once per host
/// process** (e.g. once in `daemon`'s `main()`, right after resolving `cwd`) —
/// not once per session. `callback` is `None` for hosts that don't support a
/// synchronous prompt (e.g. a headless RPC server with no client connected
/// yet at startup) — in that case this returns immediately without touching
/// the filesystem beyond the cheap "already decided" check.
///
/// Behavior:
/// - Already an Agix project (`.agents/` exists) or a prior `imported`/
///   `skipped` decision is on record → returns `None` immediately, no scan.
/// - No callback registered → returns `None` immediately, no scan.
/// - No import sources detected → returns `None`.
/// - Callback invoked with a `timeout`. `Defer` or a timeout → returns `None`,
///   **no marker written** (ask again next process start). `Skip` → marker
///   written, returns `None`. `Import(kind)` → executes the import, marker
///   written, returns `Some(ImportSummary)`.
pub async fn maybe_detect_and_import(
    cwd: &Path,
    callback: Option<&Arc<dyn ImportCallback>>,
    timeout: Duration,
) -> Option<crate::frozen::ImportSummary> {
    let asker: Arc<dyn Elicitation> = match callback {
        Some(cb) => Arc::new(CallbackAsAsker {
            callback: cb.clone(),
            cwd: cwd.to_path_buf(),
        }),
        None => return None,
    };
    maybe_detect_and_import_asking(cwd, Some(&asker), timeout).await
}

/// Same as [`maybe_detect_and_import`], asking through the engine's one
/// human-question contract instead of a callback of its own.
///
/// `None` for a host that cannot ask anybody; so is an [`Elicitation`] that
/// declines, and both mean the same thing here — no import, no marker, ask
/// again next process start. Which is the point of routing it through the
/// contract: a host that registered nothing gets the same answer as one whose
/// user said "later", instead of each path inventing its own idea of what
/// silence means.
pub async fn maybe_detect_and_import_asking(
    cwd: &Path,
    asker: Option<&Arc<dyn Elicitation>>,
    timeout: Duration,
) -> Option<crate::frozen::ImportSummary> {
    if import_already_decided(cwd).await {
        return None;
    }
    let asker = asker?;
    let sources = detect_import_sources(cwd).await;
    if sources.is_empty() {
        return None;
    }
    let request = ElicitRequest {
        id: format!("import:{}", cwd.display()),
        kind: ElicitKind::Import {
            sources: sources
                .iter()
                .map(|s| s.kind().as_str().to_string())
                .collect(),
        },
        message: "Configuration from another agent tool was found here. Import it?".to_string(),
        options: sources
            .iter()
            .map(|s| ElicitOption {
                key: s.kind().as_str().to_string(),
                label: s.kind().as_str().to_string(),
            })
            .collect(),
    };
    let decision = match tokio::time::timeout(timeout, asker.ask(request)).await {
        Ok(outcome) => outcome
            .answer_as::<ImportDecision>()
            .unwrap_or(ImportDecision::Defer),
        // A host that never answers is a host that has not decided.
        Err(_) => ImportDecision::Defer,
    };
    match decision {
        ImportDecision::Import(kind) => {
            let chosen = sources.iter().find(|s| s.kind() == kind)?;
            let summary = execute_import(cwd, chosen).await.ok()?;
            let _ = mark_imported(cwd, &sources, kind).await;
            Some(summary)
        }
        ImportDecision::Skip => {
            let _ = mark_skipped(cwd, &sources, None).await;
            None
        }
        ImportDecision::Defer => {
            // Timeout, explicit defer, or nobody to ask: no marker, ask again
            // next process start.
            None
        }
    }
}

/// Lets a host's existing [`ImportCallback`] answer the import question now
/// that the question comes through [`Elicitation`].
struct CallbackAsAsker {
    callback: Arc<dyn ImportCallback>,
    cwd: std::path::PathBuf,
}

#[async_trait]
impl Elicitation for CallbackAsAsker {
    async fn ask(&self, request: ElicitRequest) -> ElicitOutcome {
        if !matches!(request.kind, ElicitKind::Import { .. }) {
            return ElicitOutcome::declined(
                "this host's import callback answers import questions only",
            );
        }
        // The request carries source *names*, and the callback's published
        // signature takes whole `ImportSource`s — paths and parsed content
        // included. Rather than widen the request with a type only this one
        // question uses, the adapter re-runs what is a cheap directory probe,
        // once per process, on a path that is already doing filesystem work.
        let sources = detect_import_sources(&self.cwd).await;
        ElicitOutcome::answered(&self.callback.on_import_detected(&sources).await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;

    /// Mock callback: records how many times it was invoked, returns a fixed
    /// `ImportDecision`, and can optionally sleep before responding (to
    /// exercise the timeout path).
    struct MockCallback {
        decision: ImportDecision,
        delay: Duration,
        calls: AtomicUsize,
    }

    impl MockCallback {
        fn new(decision: ImportDecision) -> Self {
            Self {
                decision,
                delay: Duration::ZERO,
                calls: AtomicUsize::new(0),
            }
        }

        fn with_delay(decision: ImportDecision, delay: Duration) -> Self {
            Self {
                decision,
                delay,
                calls: AtomicUsize::new(0),
            }
        }

        fn call_count(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl ImportCallback for MockCallback {
        async fn on_import_detected(&self, _sources: &[ImportSource]) -> ImportDecision {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            self.decision.clone()
        }
    }

    #[tokio::test]
    async fn no_callback_registered_does_nothing() {
        let dir = TempDir::new().unwrap();
        tokio::fs::write(dir.path().join("CLAUDE.md"), "be concise")
            .await
            .unwrap();

        let result = maybe_detect_and_import(dir.path(), None, Duration::from_secs(5)).await;
        assert!(result.is_none());
        assert!(
            !import_already_decided(dir.path()).await,
            "no marker should be written"
        );
    }

    #[tokio::test]
    async fn no_sources_never_invokes_callback() {
        let dir = TempDir::new().unwrap(); // empty project, nothing to import
        let mock = Arc::new(MockCallback::new(ImportDecision::Skip));
        let cb: Arc<dyn ImportCallback> = mock.clone();
        let result = maybe_detect_and_import(dir.path(), Some(&cb), Duration::from_secs(5)).await;
        assert!(result.is_none());
        assert_eq!(
            mock.call_count(),
            0,
            "no importable sources means the callback is never invoked"
        );
    }

    #[tokio::test]
    async fn already_decided_project_never_invokes_callback() {
        let dir = TempDir::new().unwrap();
        tokio::fs::write(dir.path().join("CLAUDE.md"), "be concise")
            .await
            .unwrap();
        tokio::fs::create_dir_all(dir.path().join(".agents"))
            .await
            .unwrap();

        let mock = Arc::new(MockCallback::new(ImportDecision::Skip));
        let cb: Arc<dyn ImportCallback> = mock.clone();

        let result = maybe_detect_and_import(dir.path(), Some(&cb), Duration::from_secs(5)).await;
        assert!(result.is_none());
        assert_eq!(
            mock.call_count(),
            0,
            "already-decided project must not invoke the callback"
        );
    }

    #[tokio::test]
    async fn import_decision_executes_and_marks_imported() {
        let dir = TempDir::new().unwrap();
        tokio::fs::write(dir.path().join("CLAUDE.md"), "be concise")
            .await
            .unwrap();

        let cb: Arc<dyn ImportCallback> = Arc::new(MockCallback::new(ImportDecision::Import(
            crate::frozen::ImportSourceKind::ClaudeCode,
        )));
        let result = maybe_detect_and_import(dir.path(), Some(&cb), Duration::from_secs(5)).await;
        assert!(result.is_some());
        let summary = result.unwrap();
        assert_eq!(summary.kind, crate::frozen::ImportSourceKind::ClaudeCode);

        let agents_md = tokio::fs::read_to_string(dir.path().join("AGENTS.md"))
            .await
            .unwrap();
        assert!(agents_md.contains("be concise"));
        assert!(import_already_decided(dir.path()).await);
    }

    #[tokio::test]
    async fn skip_decision_marks_skipped_without_executing() {
        let dir = TempDir::new().unwrap();
        tokio::fs::write(dir.path().join("CLAUDE.md"), "be concise")
            .await
            .unwrap();

        let cb: Arc<dyn ImportCallback> = Arc::new(MockCallback::new(ImportDecision::Skip));
        let result = maybe_detect_and_import(dir.path(), Some(&cb), Duration::from_secs(5)).await;
        assert!(result.is_none());
        assert!(
            tokio::fs::metadata(dir.path().join("AGENTS.md"))
                .await
                .is_err(),
            "skip must not write AGENTS.md"
        );
        assert!(
            import_already_decided(dir.path()).await,
            "skip must persist a marker"
        );
    }

    #[tokio::test]
    async fn defer_decision_leaves_no_marker() {
        let dir = TempDir::new().unwrap();
        tokio::fs::write(dir.path().join("CLAUDE.md"), "be concise")
            .await
            .unwrap();

        let cb: Arc<dyn ImportCallback> = Arc::new(MockCallback::new(ImportDecision::Defer));
        let result = maybe_detect_and_import(dir.path(), Some(&cb), Duration::from_secs(5)).await;
        assert!(result.is_none());
        assert!(
            !import_already_decided(dir.path()).await,
            "defer must not persist a marker — ask again next time"
        );
    }

    /// The contract path, and the property it exists for: a host that
    /// registered nothing to ask with gets the same "not now" as a user who
    /// said "later" — no import, and no marker that would stop it asking
    /// again.
    #[tokio::test]
    async fn a_host_that_cannot_ask_imports_nothing_and_records_nothing() {
        let dir = TempDir::new().unwrap();
        tokio::fs::write(dir.path().join("CLAUDE.md"), "be concise")
            .await
            .unwrap();

        let asker: Arc<dyn Elicitation> = Arc::new(crate::interface::elicitation::DeclineAll);
        let result =
            maybe_detect_and_import_asking(dir.path(), Some(&asker), Duration::from_secs(5)).await;
        assert!(result.is_none());
        assert!(
            !import_already_decided(dir.path()).await,
            "a decline is not a decision to skip"
        );
    }

    #[tokio::test]
    async fn an_elicitation_answer_drives_the_import() {
        let dir = TempDir::new().unwrap();
        tokio::fs::write(dir.path().join("CLAUDE.md"), "be concise")
            .await
            .unwrap();

        let asker: Arc<dyn Elicitation> =
            crate::interface::elicitation::FixedElicitation::new(ElicitOutcome::answered(
                &ImportDecision::Import(crate::frozen::ImportSourceKind::ClaudeCode),
            ));
        let summary =
            maybe_detect_and_import_asking(dir.path(), Some(&asker), Duration::from_secs(5))
                .await
                .expect("an answer of `import` must import");
        assert_eq!(summary.kind, crate::frozen::ImportSourceKind::ClaudeCode);
        assert!(import_already_decided(dir.path()).await);
    }

    #[tokio::test]
    async fn timeout_behaves_like_defer() {
        let dir = TempDir::new().unwrap();
        tokio::fs::write(dir.path().join("CLAUDE.md"), "be concise")
            .await
            .unwrap();

        // Callback would eventually say "Import", but takes longer than the timeout.
        let cb: Arc<dyn ImportCallback> = Arc::new(MockCallback::with_delay(
            ImportDecision::Import(crate::frozen::ImportSourceKind::ClaudeCode),
            Duration::from_millis(200),
        ));
        let result =
            maybe_detect_and_import(dir.path(), Some(&cb), Duration::from_millis(20)).await;
        assert!(result.is_none(), "timeout must not import");
        assert!(
            !import_already_decided(dir.path()).await,
            "timeout must not persist a marker — ask again next time"
        );
    }
}
