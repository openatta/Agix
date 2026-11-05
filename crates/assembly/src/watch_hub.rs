//! `WatchHub` — one process-level file watcher shared by every `(scene,
//! project)` binding, instead of one `notify` thread per session or per
//! binding.
//!
//! The hard invariant this module exists to hold: watched **directories** are
//! `1 + M + P` (global + one per scene + one per project), never
//! `O(会话数)` and never `O(M×P)` — subscriptions to the same path are
//! deduplicated and refcounted so N subscribers of one directory still cost
//! exactly one `notify` registration.

use notify::event::Event;
use notify::{recommended_watcher, RecursiveMode, Watcher};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

/// Which config-layer tier a watched directory belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Tier {
    Global,
    Scene(String),
    Project(PathBuf),
}

/// What kind of resource under that tier's directory changed — carried back
/// in the notification so the subscriber knows what to rebuild without
/// re-deriving it from the path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WatchTarget {
    Settings,
    Skills,
    Agents,
    Plugins,
    RulesHooks,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WatchKey {
    pub tier: Tier,
    pub target: WatchTarget,
}

/// Debounce window: collapses the burst of events one "save" produces
/// (editors writing a temp file then renaming it over the target, `vim`'s
/// `.swp` noise) into a single `changes()` notification. 300ms is chosen to
/// cover that burst without feeling laggy after a settings-panel save — see
/// §4.4's "去抖" note.
const DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(300);

struct Inner {
    watcher: Mutex<Option<notify::RecommendedWatcher>>,
    /// The path actually registered with `notify` for a subscription — may
    /// be a subscriber's requested path, or that path's parent when the
    /// requested path didn't exist yet at subscribe time (see `subscribe`).
    /// Refcounted so the same directory subscribed by multiple keys (or the
    /// same key subscribed twice) is only watched once.
    watched_paths: Mutex<HashMap<PathBuf, usize>>,
    /// Reverse lookup from an actually-watched path to the keys interested
    /// in it. A `notify` event's path is matched against this by prefix
    /// (exact match for a watched file, `starts_with` for a watched
    /// directory or a not-yet-existing path whose parent is watched), which
    /// is also what makes "watch the parent, promote on create" work
    /// without any extra bookkeeping: once the target is created, its path
    /// is a child of the already-watched parent and matches naturally.
    path_keys: Mutex<HashMap<PathBuf, Vec<WatchKey>>>,
    changes_tx: broadcast::Sender<WatchKey>,
    /// One monotonic counter per key, used for the debounce-by-generation
    /// pattern in `schedule_notify`: each event bumps the key's generation
    /// and spawns a checker that fires only if it's still the newest one
    /// after the debounce window — cheaper than cancelling/restarting a
    /// timer task per event.
    debounce_gen: Mutex<HashMap<WatchKey, Arc<AtomicU64>>>,
    /// `notify`'s callback runs on its own OS thread (`drain_events`, spawned
    /// below), not inside a Tokio runtime — `tokio::spawn` from there would
    /// panic with "no reactor running". Captured at construction time (which
    /// must happen inside a runtime) so `schedule_notify` can spawn its
    /// debounce checkers via `Handle::spawn` instead.
    runtime_handle: tokio::runtime::Handle,
}

/// Held by a subscriber; dropping it releases this subscription's share of
/// the underlying `notify` registration. The watched path is only actually
/// unwatched once every token referencing it has been dropped.
pub struct WatchToken {
    key: WatchKey,
    actual_path: PathBuf,
    inner: Arc<Inner>,
}

impl Drop for WatchToken {
    fn drop(&mut self) {
        self.inner.release(&self.key, &self.actual_path);
    }
}

pub struct WatchHub {
    inner: Arc<Inner>,
}

impl WatchHub {
    /// Construct the hub and start its one background `notify` watcher
    /// thread. Failure here (permissions, kernel inotify-instance limits,
    /// ...) is the caller's decision to treat as fatal or not — per §4.4,
    /// the documented degrade is "warn and fall back to explicit
    /// `config.reload` only", matching the existing `SkillWatcher::
    /// enable_watching` failure mode, so this returns `Result` rather than
    /// panicking.
    pub fn new() -> Result<Self, String> {
        let runtime_handle = tokio::runtime::Handle::try_current()
            .map_err(|e| format!("WatchHub::new() must run inside a Tokio runtime: {e}"))?;
        let (changes_tx, _rx) = broadcast::channel(256);
        let inner = Arc::new(Inner {
            watcher: Mutex::new(None),
            watched_paths: Mutex::new(HashMap::new()),
            path_keys: Mutex::new(HashMap::new()),
            changes_tx,
            debounce_gen: Mutex::new(HashMap::new()),
            runtime_handle,
        });

        let (tx, rx) = std::sync::mpsc::channel::<notify::Result<Event>>();
        let watcher = recommended_watcher(move |res| {
            let _ = tx.send(res);
        })
        .map_err(|e| format!("notify watcher creation failed: {e}"))?;
        *inner.watcher.lock().unwrap() = Some(watcher);

        let inner_for_thread = inner.clone();
        std::thread::Builder::new()
            .name("watch-hub".into())
            .spawn(move || drain_events(rx, inner_for_thread))
            .map_err(|e| format!("failed to spawn watch-hub thread: {e}"))?;

        Ok(Self { inner })
    }

    /// Subscribe `key` to changes under `path`. Idempotent and refcounted:
    /// subscribing an already-watched path just bumps its refcount and
    /// returns a new token; the underlying `notify` registration happens
    /// once. If `path` doesn't exist yet, its parent is watched instead (and
    /// retried on the next `subscribe` call for that parent — the directory
    /// coming into existence later is picked up by the prefix match in
    /// `drain_events`, not by this call re-checking).
    pub fn subscribe(&self, key: WatchKey, path: &Path) -> WatchToken {
        // Walk up to the nearest existing ancestor — `path`'s immediate
        // parent may not exist either (e.g. subscribing to
        // `<project>/.agents/skills` in a project that has neither
        // directory yet).
        let requested = {
            let mut candidate = path.to_path_buf();
            loop {
                if candidate.exists() {
                    break candidate;
                }
                match candidate.parent() {
                    Some(parent) => candidate = parent.to_path_buf(),
                    None => break path.to_path_buf(),
                }
            }
        };
        // Canonicalize before storing: `notify`'s backend (FSEvents on
        // macOS) reports event paths resolved through symlinks (e.g.
        // `/var/folders/...` -> `/private/var/folders/...`), so comparing
        // against a non-canonical stored path would silently never match.
        let actual_path = std::fs::canonicalize(&requested).unwrap_or(requested);

        let mut watched = self.inner.watched_paths.lock().unwrap();
        let first_registration = !watched.contains_key(&actual_path);
        *watched.entry(actual_path.clone()).or_insert(0) += 1;
        drop(watched);

        self.inner
            .path_keys
            .lock()
            .unwrap()
            .entry(actual_path.clone())
            .or_default()
            .push(key.clone());

        if first_registration && actual_path.exists() {
            if let Some(w) = self.inner.watcher.lock().unwrap().as_mut() {
                if let Err(e) = w.watch(&actual_path, RecursiveMode::Recursive) {
                    tracing::warn!(path = %actual_path.display(), error = %e, "WatchHub: failed to watch path");
                }
            }
        }

        WatchToken {
            key,
            actual_path,
            inner: self.inner.clone(),
        }
    }

    /// Subscribe to change notifications. Each `WatchKey` arrives at most
    /// once per debounce window, coalescing bursts (see `DEBOUNCE`).
    pub fn changes(&self) -> broadcast::Receiver<WatchKey> {
        self.inner.changes_tx.subscribe()
    }

    /// Number of distinct paths currently registered with `notify` — the
    /// quantity §7.5's performance budget bounds at `1 + M + P`. Exposed for
    /// tests to assert the O(1+M+P) invariant directly rather than only
    /// indirectly through behavior.
    pub fn watched_path_count(&self) -> usize {
        self.inner.watched_paths.lock().unwrap().len()
    }
}

impl Inner {
    fn release(&self, key: &WatchKey, actual_path: &Path) {
        if let Some(keys) = self.path_keys.lock().unwrap().get_mut(actual_path) {
            if let Some(pos) = keys.iter().position(|k| k == key) {
                keys.remove(pos);
            }
        }

        let mut watched = self.watched_paths.lock().unwrap();
        let Some(count) = watched.get_mut(actual_path) else {
            return;
        };
        *count -= 1;
        if *count > 0 {
            return;
        }
        watched.remove(actual_path);
        drop(watched);
        self.path_keys.lock().unwrap().remove(actual_path);
        if let Some(w) = self.watcher.lock().unwrap().as_mut() {
            let _ = w.unwatch(actual_path);
        }
    }

    /// Keys whose watched path is, or is an ancestor of, `event_path` — this
    /// is what makes both file watches (`settings.json`) and directory
    /// watches (`skills/`) match with the same logic, and what makes the
    /// "watch parent, promote on create" fallback in `subscribe` work
    /// without extra bookkeeping.
    fn keys_for_event_path(&self, event_path: &Path) -> Vec<WatchKey> {
        self.path_keys
            .lock()
            .unwrap()
            .iter()
            .filter(|(watched, _)| {
                event_path == watched.as_path() || event_path.starts_with(watched)
            })
            .flat_map(|(_, keys)| keys.iter().cloned())
            .collect()
    }

    /// Debounce-by-generation: bump `key`'s generation and spawn a checker
    /// that fires only if, after the debounce window, it's still holding
    /// the newest generation — i.e. no further event for this key arrived
    /// in the meantime. Cheaper than cancelling and restarting a per-key
    /// timer task on every event.
    fn schedule_notify(self: &Arc<Self>, key: WatchKey) {
        let gen_counter = self
            .debounce_gen
            .lock()
            .unwrap()
            .entry(key.clone())
            .or_insert_with(|| Arc::new(AtomicU64::new(0)))
            .clone();
        let my_gen = gen_counter.fetch_add(1, Ordering::SeqCst) + 1;

        let inner = self.clone();
        self.runtime_handle.spawn(async move {
            tokio::time::sleep(DEBOUNCE).await;
            if gen_counter.load(Ordering::SeqCst) == my_gen {
                let _ = inner.changes_tx.send(key);
            }
        });
    }
}

fn drain_events(rx: std::sync::mpsc::Receiver<notify::Result<Event>>, inner: Arc<Inner>) {
    loop {
        match rx.recv() {
            Ok(Ok(event)) => {
                for path in &event.paths {
                    for key in inner.keys_for_event_path(path) {
                        inner.schedule_notify(key);
                    }
                }
            }
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "WatchHub: notify error");
            }
            Err(_) => {
                tracing::debug!("WatchHub: channel closed, exiting thread");
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// How long to wait for a notification that should arrive.
    ///
    /// A ceiling, not a measurement. Every use below is a *positive* wait —
    /// the `timeout` returns the moment `recv()` resolves, so on a healthy
    /// machine these tests still finish in milliseconds and only a genuine
    /// break pays the budget. What sets the floor is not in this process:
    /// filesystem events cross the kernel and a debounce window, and under a
    /// loaded `cargo test --workspace` that can take far longer than the
    /// event itself.
    ///
    /// This is not the usual "the test failed so raise the timeout". These
    /// tests are worth having because they can fail when a notification never
    /// arrives; none of them is worth anything for its ability to fail when
    /// one arrives *late*. The one place timing is the subject — the burst
    /// below, which asserts a second notification does **not** arrive — keeps
    /// its own short budget, because there the wait is the assertion.
    fn settle() -> Duration {
        DEBOUNCE + Duration::from_secs(15)
    }

    #[tokio::test]
    async fn subscribing_the_same_path_twice_only_watches_it_once() {
        let hub = WatchHub::new().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let key_a = WatchKey {
            tier: Tier::Global,
            target: WatchTarget::Settings,
        };
        let key_b = WatchKey {
            tier: Tier::Scene("coding".into()),
            target: WatchTarget::Settings,
        };

        let _t1 = hub.subscribe(key_a, dir.path());
        assert_eq!(hub.watched_path_count(), 1);
        let _t2 = hub.subscribe(key_b, dir.path());
        assert_eq!(
            hub.watched_path_count(),
            1,
            "second subscriber to the same path must not add a second notify registration"
        );
    }

    #[tokio::test]
    async fn dropping_all_tokens_for_a_path_unwatches_it() {
        let hub = WatchHub::new().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let key = WatchKey {
            tier: Tier::Global,
            target: WatchTarget::Skills,
        };

        let t1 = hub.subscribe(key.clone(), dir.path());
        let t2 = hub.subscribe(key, dir.path());
        assert_eq!(hub.watched_path_count(), 1);

        drop(t1);
        assert_eq!(
            hub.watched_path_count(),
            1,
            "one remaining token must keep the watch alive"
        );

        drop(t2);
        assert_eq!(
            hub.watched_path_count(),
            0,
            "last token dropped must unwatch the path"
        );
    }

    #[tokio::test]
    async fn a_file_change_notifies_the_subscribed_key() {
        let hub = WatchHub::new().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("settings.json");
        std::fs::write(&file, "{}").unwrap();

        let key = WatchKey {
            tier: Tier::Global,
            target: WatchTarget::Settings,
        };
        let mut rx = hub.changes();
        let _token = hub.subscribe(key.clone(), &file);

        std::fs::write(&file, "{\"changed\":true}").unwrap();

        let got = tokio::time::timeout(settle(), rx.recv()).await;
        assert_eq!(got.unwrap().unwrap(), key);
    }

    #[tokio::test]
    async fn a_burst_of_writes_collapses_into_one_notification() {
        let hub = WatchHub::new().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("settings.json");
        std::fs::write(&file, "{}").unwrap();

        let key = WatchKey {
            tier: Tier::Global,
            target: WatchTarget::Settings,
        };
        let mut rx = hub.changes();
        let _token = hub.subscribe(key, &file);

        for i in 0..5 {
            std::fs::write(&file, format!("{{\"n\":{i}}}")).unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let first = tokio::time::timeout(settle(), rx.recv()).await;
        assert!(first.is_ok(), "expected exactly one debounced notification");
        let second = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await;
        assert!(
            second.is_err(),
            "burst must collapse to a single notification, got a second one"
        );
    }

    #[tokio::test]
    async fn watching_a_not_yet_existing_path_falls_back_to_its_parent_and_promotes_on_create() {
        let hub = WatchHub::new().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("agents").join("settings.json");

        let key = WatchKey {
            tier: Tier::Project(dir.path().to_path_buf()),
            target: WatchTarget::Agents,
        };
        let mut rx = hub.changes();
        let _token = hub.subscribe(key.clone(), &missing);
        assert_eq!(
            hub.watched_path_count(),
            1,
            "must fall back to watching the existing parent"
        );

        std::fs::create_dir_all(missing.parent().unwrap()).unwrap();
        std::fs::write(&missing, "{}").unwrap();

        let got = tokio::time::timeout(settle(), rx.recv()).await;
        assert_eq!(
            got.unwrap().unwrap(),
            key,
            "creating the target under the watched parent must still notify"
        );
    }

    #[tokio::test]
    async fn distinct_keys_on_different_paths_notify_independently() {
        let hub = WatchHub::new().unwrap();
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        let file_a = dir_a.path().join("settings.json");
        let file_b = dir_b.path().join("settings.json");
        std::fs::write(&file_a, "{}").unwrap();
        std::fs::write(&file_b, "{}").unwrap();

        let key_a = WatchKey {
            tier: Tier::Scene("coding".into()),
            target: WatchTarget::Settings,
        };
        let key_b = WatchKey {
            tier: Tier::Scene("chat".into()),
            target: WatchTarget::Settings,
        };
        let mut rx = hub.changes();
        let _t_a = hub.subscribe(key_a.clone(), &file_a);
        let _t_b = hub.subscribe(key_b.clone(), &file_b);
        assert_eq!(hub.watched_path_count(), 2);

        std::fs::write(&file_a, "{\"changed\":true}").unwrap();
        let got = tokio::time::timeout(settle(), rx.recv()).await;
        assert_eq!(
            got.unwrap().unwrap(),
            key_a,
            "only the changed path's key should fire"
        );
    }
}
