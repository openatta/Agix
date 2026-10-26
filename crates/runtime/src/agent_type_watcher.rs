//! `AgentTypeWatcher` — watches `.agix/agents/*.md` directories for changes
//! and keeps `AgentTool`'s merged agent-type catalog live-reloaded.
//!
//! Mirrors `skills::watcher::SkillWatcher`'s architecture (same `notify`
//! crate, same background-thread-drains-events pattern — see that module's
//! doc comment for the general shape). The reload strategy differs on
//! purpose: Skills patches one entry at a time
//! (`SkillManager::reload_skill`), but here **any** change re-runs the full
//! `merge_agent_types()` over every watched directory. That's because the
//! low-to-high-priority merge (see `merge_agent_types`'s own doc comment)
//! means a single name's effective definition can depend on whether a
//! higher-priority layer also defines it — patching one entry in isolation
//! couldn't correctly fall back to a lower layer's definition after, say, a
//! project-level override file is deleted. The full re-merge is cheap:
//! these directories are typically a handful of small files.

use crate::agent_tool::{merge_agent_types, AgentTypeDefinition, SharedAgentTypeMap};
use notify::event::Event;
use notify::{recommended_watcher, RecursiveMode, Watcher};
use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::sync::Arc;

/// Kept alive only to hold the underlying `notify` watcher (and thus its
/// background thread) open — see `Inner::_agent_type_watcher`'s doc comment
/// for why it's otherwise never read from directly.
pub(crate) struct AgentTypeWatcher {
    #[allow(dead_code)]
    watcher_handle: notify::RecommendedWatcher,
}

impl AgentTypeWatcher {
    /// Start watching `dirs` for `.md` changes. On any change, re-runs
    /// `merge_agent_types(dirs, plugin_types)` and swaps `target`'s content
    /// with the fresh result — `target` is the same `Arc<RwLock<..>>`
    /// `Inner::agent_types` holds, so every clone of `Inner` (and thus every
    /// `AgentTool`/sub-agent-spawn call sharing it) observes the update on
    /// its next lookup, no session rebuild required.
    ///
    /// Returns `Err` if `notify` setup fails (inotify/kqueue limits,
    /// permissions) — callers should treat that as non-fatal (log a warning,
    /// keep the build-time-only catalog), matching how
    /// `SkillManager::enable_watching` failures are handled.
    pub(crate) fn watch(
        dirs: Vec<PathBuf>,
        plugin_types: Vec<AgentTypeDefinition>,
        target: SharedAgentTypeMap,
    ) -> Result<Self, String> {
        let (tx, rx) = std::sync::mpsc::channel();

        let mut watcher_handle = recommended_watcher(move |res: notify::Result<Event>| {
            let _ = tx.send(res);
        })
        .map_err(|e| format!("notify watcher creation failed: {e}"))?;

        for dir in &dirs {
            if dir.exists() {
                watcher_handle
                    .watch(dir, RecursiveMode::Recursive)
                    .map_err(|e| format!("failed to watch '{}': {e}", dir.display()))?;
            } else {
                tracing::warn!(?dir, "Agent-type watch path does not exist, skipping");
            }
        }

        std::thread::Builder::new()
            .name("agent-type-watcher".into())
            .spawn(move || drain_events(rx, &dirs, &plugin_types, &target))
            .expect("agent-type-watcher thread");

        Ok(Self { watcher_handle })
    }
}

fn drain_events(
    rx: Receiver<notify::Result<Event>>,
    dirs: &[PathBuf],
    plugin_types: &[AgentTypeDefinition],
    target: &SharedAgentTypeMap,
) {
    loop {
        match rx.recv() {
            Ok(Ok(event)) => {
                let is_agent_type_change = event
                    .paths
                    .iter()
                    .any(|p| p.extension().and_then(|e| e.to_str()) == Some("md"));
                if is_agent_type_change {
                    let dir_refs: Vec<&std::path::Path> =
                        dirs.iter().map(|p| p.as_path()).collect();
                    let merged = merge_agent_types(&dir_refs, plugin_types);
                    let count = merged.len();
                    *target.write().unwrap() = Arc::new(merged);
                    tracing::debug!(count, "agent types reloaded from file change");
                }
            }
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "Agent-type watcher notify error");
            }
            Err(_) => {
                // Channel closed — sender (and thus the notify watcher) dropped.
                tracing::debug!("Agent-type watcher channel closed, exiting thread");
                break;
            }
        }
    }
}
