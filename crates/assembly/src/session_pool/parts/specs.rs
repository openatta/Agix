//! 每个会话的规格 `SessionSpec` 归 `SpecStore`：重建一个会话时照什么重来。内存里
//! 的那份是准的，日志里的 `agix.session/spec` 条目让它在会话出内存、守护进程重启
//! 之后还能找回来。
//!
//! 锁序：`specs` 和 `kept` 是两把 std 叶子锁，互不嵌套，持锁时不 `.await`。

use crate::session_pool::api::SessionOptions;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use tracing::warn;

const SPEC_NS: &str = "agix.session";
const SPEC_EVENT: &str = "spec";

/// What a rebuild of a session has to reproduce: the options it was created
/// with and what has been changed on it since.
///
/// An `Agent` is rebuilt whenever its binding goes stale, the janitor evicts
/// it, or the daemon restarts; every rebuild reproduces this, not whatever the
/// triggering call happens to pass.
#[derive(Clone, Default)]
pub(in crate::session_pool) struct SessionSpec {
    pub(in crate::session_pool) options: Option<SessionOptions>,
    pub(in crate::session_pool) model: Option<String>,
    pub(in crate::session_pool) provider: Option<String>,
    pub(in crate::session_pool) prompt: Option<runtime::agent::SessionPrompt>,
    /// Shared by every Agent this process builds for the session, so a
    /// rebuild continues a replay where it stood instead of from the first
    /// recorded call. Not persisted: a recording in progress does not outlive
    /// the process that made it.
    pub(in crate::session_pool) recorder: Option<Arc<telemetry::recorder::Recorder>>,
}

impl SessionSpec {
    /// The provider the conversation goes to: chosen since, else asked for
    /// at creation. `None` follows the settings' routing.
    pub(in crate::session_pool) fn provider(&self) -> Option<String> {
        self.provider
            .clone()
            .or_else(|| self.options.as_ref().and_then(|o| o.provider.clone()))
    }

    /// The session's own prompt: set since, else given at creation.
    pub(in crate::session_pool) fn prompt(&self) -> Option<runtime::agent::SessionPrompt> {
        self.prompt
            .clone()
            .or_else(|| self.options.as_ref().and_then(|o| o.prompt.clone()))
            .filter(|p| !p.is_empty())
    }

    /// The conversation's model, by the same precedence as `provider`.
    pub(in crate::session_pool) fn model(&self) -> Option<String> {
        self.model
            .clone()
            .or_else(|| self.options.as_ref().and_then(|o| o.model.clone()))
    }
}

/// The persisted part of a [`SessionSpec`], written to the session's log as
/// an extension entry each time it changes; the last one wins. An extension
/// entry rather than a `Meta` field because the spec changes after `Meta` is
/// written, and readers that do not know the namespace skip it.
#[derive(Default, serde::Serialize, serde::Deserialize)]
pub(in crate::session_pool) struct RecordedSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::session_pool) options: Option<SessionOptions>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::session_pool) prompt: Option<runtime::agent::SessionPrompt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::session_pool) model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::session_pool) provider: Option<String>,
}

pub(in crate::session_pool) struct SpecStore {
    /// Each session's spec, by id. Filled from the session's log the first
    /// time a session not built in this process is brought back.
    specs: Mutex<HashMap<String, SessionSpec>>,
    /// Sessions out of memory whose spec is still in `specs`, oldest first:
    /// those the log cannot give back (a recorder's progress, a session with
    /// no log). At most `cap` of them.
    kept: Mutex<VecDeque<String>>,
    history_store: Option<Arc<dyn history::store::HistoryStore>>,
    cap: usize,
}

impl SpecStore {
    pub(in crate::session_pool) fn new(
        history_store: Option<Arc<dyn history::store::HistoryStore>>,
        cap: usize,
    ) -> Self {
        Self {
            specs: Mutex::new(HashMap::new()),
            kept: Mutex::new(VecDeque::new()),
            history_store,
            cap,
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, SessionSpec>> {
        self.specs
            .lock()
            .expect("specs mutex is never held across a panic")
    }

    fn lock_kept(&self) -> MutexGuard<'_, VecDeque<String>> {
        self.kept
            .lock()
            .expect("kept_specs is never held across a panic")
    }

    pub(in crate::session_pool) fn get(&self, session_id: &str) -> Option<SessionSpec> {
        self.lock().get(session_id).cloned()
    }

    /// The spec of a session being brought up: the one this process already
    /// holds, else the one `log` recorded (for a resume), else a new one
    /// from `options`. The session is in memory again, so its spec is no
    /// longer one of the kept.
    pub(in crate::session_pool) async fn spec_for(
        &self,
        session_id: &str,
        log: Option<&[history::entry::EnvelopedEntry]>,
        options: Option<&SessionOptions>,
    ) -> SessionSpec {
        let spec = self.resolve(session_id, log, options).await;
        self.lock_kept().retain(|k| k != session_id);
        spec
    }

    async fn resolve(
        &self,
        session_id: &str,
        log: Option<&[history::entry::EnvelopedEntry]>,
        options: Option<&SessionOptions>,
    ) -> SessionSpec {
        if let Some(spec) = self.get(session_id) {
            return spec;
        }
        let resume = log.is_some();
        let mut spec = log.map(recorded_in).unwrap_or_default();
        // A log written before specs were recorded has none; the options of
        // the call that brought the session back are the best there is, and
        // are recorded so the next resume finds them too.
        let adopted = resume && spec.options.is_none() && options.is_some();
        if spec.options.is_none() {
            spec.options = options.cloned();
        }
        // `recorder` is never recorded, so the call bringing the session back
        // is the only place it can come from.
        if let Some(recorder) = options.and_then(|o| o.recorder.clone()) {
            spec.options.get_or_insert_with(Default::default).recorder = Some(recorder);
        }
        if spec.options.as_ref().is_some_and(|o| o.recorder.is_some()) {
            spec.recorder = Some(telemetry::recorder::Recorder::new());
        }
        let spec = self
            .lock()
            .entry(session_id.to_string())
            .or_insert(spec)
            .clone();
        if adopted {
            self.persist(session_id, &spec).await;
        }
        spec
    }

    /// Have the spec of `session_id` in memory, from its log if this process
    /// holds none.
    pub(in crate::session_pool) async fn load(&self, session_id: &str) {
        if self.lock().contains_key(session_id) {
            return;
        }
        let recorded = self.recorded(session_id).await;
        self.lock()
            .entry(session_id.to_string())
            .or_insert(recorded);
    }

    /// The spec the log of `session_id` recorded last; an empty one when it
    /// recorded none or cannot be read.
    pub(in crate::session_pool) async fn recorded(&self, session_id: &str) -> SessionSpec {
        let entries = match (
            self.history_store.as_ref(),
            base::session::SessionId::parse(session_id),
        ) {
            (Some(store), Ok(sid)) => store.load(sid).await.unwrap_or_default(),
            _ => Vec::new(),
        };
        recorded_in(&entries)
    }

    pub(in crate::session_pool) async fn persist(&self, session_id: &str, spec: &SessionSpec) {
        let Some(store) = self.history_store.as_ref() else {
            return;
        };
        let Ok(sid) = base::session::SessionId::parse(session_id) else {
            return;
        };
        let recorded = RecordedSpec {
            options: spec.options.clone().map(|mut o| {
                o.recorder = None;
                o
            }),
            model: spec.model.clone(),
            provider: spec.provider.clone(),
            prompt: spec.prompt.clone(),
        };
        let Ok(payload) = serde_json::to_value(&recorded) else {
            return;
        };
        let entry = history::entry::LogEntry::Extension {
            ns: SPEC_NS.to_string(),
            event: SPEC_EVENT.to_string(),
            payload,
        };
        if let Err(e) = store.append(sid, entry).await {
            warn!(%session_id, error = %e, "failed to record the session spec");
        }
    }

    /// Change this session's spec in memory only, returning it as it was and
    /// as it is now.
    pub(in crate::session_pool) fn update(
        &self,
        session_id: &str,
        change: impl FnOnce(&mut SessionSpec),
    ) -> (Option<SessionSpec>, SessionSpec) {
        let mut specs = self.lock();
        let previous = specs.get(session_id).cloned();
        let spec = specs.entry(session_id.to_string()).or_default();
        change(spec);
        (previous, spec.clone())
    }

    /// Put back what [`Self::update`] returned as the spec before.
    pub(in crate::session_pool) fn restore(&self, session_id: &str, previous: Option<SessionSpec>) {
        let mut specs = self.lock();
        match previous {
            Some(previous) => specs.insert(session_id.to_string(), previous),
            None => specs.remove(session_id),
        };
    }

    /// Drop the spec of `sid`, now out of memory, when its log gives it
    /// back; one the log cannot — a recorder's progress, a session with no
    /// log — is kept, for at most `cap` sessions.
    pub(in crate::session_pool) fn release(&self, sid: &str) {
        let logged = self.history_store.is_some() && base::session::SessionId::parse(sid).is_ok();
        let records = self
            .lock()
            .get(sid)
            .is_some_and(|spec| spec.recorder.is_some());
        if logged && !records {
            self.forget(sid);
            return;
        }
        let oldest = {
            let mut kept = self.lock_kept();
            kept.retain(|k| k != sid);
            kept.push_back(sid.to_string());
            if kept.len() > self.cap {
                kept.pop_front()
            } else {
                None
            }
        };
        if let Some(oldest) = oldest {
            self.forget(&oldest);
        }
    }

    /// Forget a session's spec — it is deleted or closed, and an id is never
    /// reused.
    pub(in crate::session_pool) fn forget(&self, session_id: &str) {
        self.lock().remove(session_id);
    }

    #[cfg(test)]
    pub(in crate::session_pool) fn contains(&self, session_id: &str) -> bool {
        self.lock().contains_key(session_id)
    }

    #[cfg(test)]
    pub(in crate::session_pool) fn with_mut<T>(
        &self,
        session_id: &str,
        f: impl FnOnce(&mut SessionSpec) -> T,
    ) -> T {
        f(self.lock().entry(session_id.to_string()).or_default())
    }

    #[cfg(test)]
    pub(in crate::session_pool) fn kept_len(&self) -> usize {
        self.lock_kept().len()
    }
}

/// The spec `entries`, a session's log, recorded last; an empty one when they
/// record none.
fn recorded_in(entries: &[history::entry::EnvelopedEntry]) -> SessionSpec {
    let recorded = entries
        .iter()
        .rev()
        .find_map(|env| match &env.entry {
            history::entry::LogEntry::Extension { ns, event, payload }
                if ns == SPEC_NS && event == SPEC_EVENT =>
            {
                serde_json::from_value::<RecordedSpec>(payload.clone()).ok()
            }
            _ => None,
        })
        .unwrap_or_default();
    SessionSpec {
        options: recorded.options,
        model: recorded.model,
        provider: recorded.provider,
        prompt: recorded.prompt,
        recorder: None,
    }
}
