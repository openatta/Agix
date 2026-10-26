//! Cron job storage — in-memory store with optional file persistence.
//!
//! Every job belongs to the session it runs in (`session_id`). A session pool
//! keeps one store for all its sessions and fires due jobs itself; an engine
//! running on its own keeps one per session and fires them from its input
//! loop. Either way a session sees and changes only its own jobs.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Mutex;

use super::parser::cron_matches;

// ---- Cron types ----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronJob {
    pub id: String,
    pub cron: String,
    pub prompt: String,
    /// true = fire on each cron match; false = fire once then auto-delete
    pub recurring: bool,
    /// true = persisted to disk and survives restarts
    pub durable: bool,
    /// Unix millis when the job was created
    pub created_ms: i64,
    /// Optional agent id (for teammate-owned crons)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// Unix millis of last fire (prevents double-fire within the same minute)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_fired_ms: Option<i64>,
    /// The session the job's prompt is sent to when it fires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Unix millis after which the job's next fire is its last. `None`: never.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_ms: Option<i64>,
}

/// What [`CronStore::add_job`] needs to know about a new job.
#[derive(Debug, Clone, Default)]
pub struct NewCronJob {
    pub cron: String,
    pub prompt: String,
    pub recurring: bool,
    pub durable: bool,
    pub session_id: Option<String>,
    pub expires_ms: Option<i64>,
}

/// How long a recurring job a model creates for itself keeps firing.
pub const MODEL_JOB_LIFETIME_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// Thread-safe cron job store. Shared between cron tools and engine via `Arc`.
pub struct CronStore {
    inner: Mutex<Vec<CronJob>>,
    file_path: Option<PathBuf>,
}

impl CronStore {
    /// Create an empty in-memory store.
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Vec::new()),
            file_path: None,
        }
    }

    /// Create or load from a file path. If the file exists, loads jobs from it.
    pub fn load_or_default(file_path: Option<PathBuf>) -> Self {
        let jobs = file_path
            .as_ref()
            .and_then(|p| Self::load_from_disk(p).ok())
            .unwrap_or_default();
        Self {
            inner: Mutex::new(jobs),
            file_path,
        }
    }

    /// Add a new cron job, belonging to no session. Returns the assigned ID.
    pub fn add(&self, cron: String, prompt: String, recurring: bool, durable: bool) -> String {
        self.add_job(NewCronJob {
            cron,
            prompt,
            recurring,
            durable,
            ..Default::default()
        })
        .id
    }

    /// Add a new cron job. Returns it as stored.
    pub fn add_job(&self, new: NewCronJob) -> CronJob {
        let job = CronJob {
            id: generate_id(),
            cron: new.cron,
            prompt: new.prompt,
            recurring: new.recurring,
            durable: new.durable,
            created_ms: now_ms(),
            agent_id: None,
            last_fired_ms: None,
            session_id: new.session_id,
            expires_ms: new.expires_ms,
        };
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(job.clone());
        if job.durable {
            self.save();
        }
        job
    }

    /// Remove a job by ID. Returns true if found and removed.
    pub fn remove(&self, id: &str) -> bool {
        self.remove_where(|j| j.id == id)
    }

    /// Remove a job by ID if it belongs to `session_id`.
    pub fn remove_for(&self, session_id: &str, id: &str) -> bool {
        self.remove_where(|j| j.id == id && j.session_id.as_deref() == Some(session_id))
    }

    /// Remove every job of `session_id` — the session is gone for good.
    pub fn remove_session(&self, session_id: &str) -> usize {
        let mut removed = 0;
        while self.remove_where(|j| j.session_id.as_deref() == Some(session_id)) {
            removed += 1;
        }
        removed
    }

    fn remove_where(&self, matches: impl Fn(&CronJob) -> bool) -> bool {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some(at) = inner.iter().position(&matches) else {
            return false;
        };
        inner.remove(at);
        drop(inner);
        self.save();
        true
    }

    /// List all active jobs.
    pub fn list(&self) -> Vec<CronJob> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The jobs belonging to `session_id`.
    pub fn list_for(&self, session_id: &str) -> Vec<CronJob> {
        self.list()
            .into_iter()
            .filter(|j| j.session_id.as_deref() == Some(session_id))
            .collect()
    }

    /// Pop all jobs whose cron expression matches the current time.
    ///
    /// Recurring jobs have `last_fired_ms` updated so they won't re-fire
    /// within the same minute. Non-recurring jobs are removed after firing, and
    /// so is a recurring job past its `expires_ms`, after this last fire.
    /// Returns due jobs (oldest first).
    pub fn pop_due(&self) -> Vec<CronJob> {
        self.pop_due_at(time::OffsetDateTime::now_utc())
    }

    /// [`Self::pop_due`] as of `now`.
    pub fn pop_due_at(&self, now: time::OffsetDateTime) -> Vec<CronJob> {
        let now_ms = (now.unix_timestamp_nanos() / 1_000_000) as i64;
        let this_minute = now_ms.div_euclid(60_000);

        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut due: Vec<CronJob> = Vec::new();
        let mut keep: Vec<CronJob> = Vec::new();

        for mut job in inner.drain(..) {
            if cron_matches(&job.cron, &now) {
                let already_fired_this_minute = job
                    .last_fired_ms
                    .is_some_and(|last| last.div_euclid(60_000) == this_minute);
                let expired = job.expires_ms.is_some_and(|at| now_ms >= at);

                if already_fired_this_minute {
                    keep.push(job);
                } else if job.recurring && !expired {
                    job.last_fired_ms = Some(now_ms);
                    due.push(job.clone());
                    keep.push(job);
                } else {
                    due.push(job);
                }
            } else {
                keep.push(job);
            }
        }

        *inner = keep;
        drop(inner); // release lock before save (std::sync::Mutex is not reentrant)
        self.save();
        due
    }

    fn save(&self) {
        let Some(ref path) = self.file_path else {
            return;
        };
        let jobs: Vec<CronJob> = self
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|j| j.durable)
            .cloned()
            .collect();
        if let Ok(json) = serde_json::to_string(&jobs) {
            let _ = std::fs::write(path, &json);
        }
    }

    fn load_from_disk(path: &std::path::Path) -> std::io::Result<Vec<CronJob>> {
        let data = std::fs::read_to_string(path)?;
        serde_json::from_str(&data)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }
}

impl Default for CronStore {
    fn default() -> Self {
        Self::new()
    }
}

// ---- helpers ----

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn generate_id() -> String {
    let id = uuid::Uuid::new_v4();
    id.to_string()[..8].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cron_store_new_empty() {
        let store = CronStore::new();
        assert!(store.list().is_empty());
    }

    #[test]
    fn cron_add_and_list() {
        let store = CronStore::new();
        let id = store.add("0 9 * * *".into(), "daily check".into(), true, false);
        assert!(!id.is_empty());
        assert_eq!(id.len(), 8);
        let jobs = store.list();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].cron, "0 9 * * *");
        assert_eq!(jobs[0].prompt, "daily check");
    }

    #[test]
    fn cron_add_and_remove() {
        let store = CronStore::new();
        let id = store.add("*/5 * * * *".into(), "ping".into(), true, false);
        assert!(store.remove(&id));
        assert!(store.list().is_empty());
    }

    #[test]
    fn cron_remove_nonexistent() {
        let store = CronStore::new();
        assert!(!store.remove("nonexistent"));
    }

    #[test]
    fn cron_pop_due_non_recurring_is_removed() {
        let store = CronStore::new();
        // Add a job that matches every minute
        store.add("* * * * *".into(), "every-min".into(), false, false);
        let due = store.pop_due();
        assert!(!due.is_empty(), "should fire at least one job");
        // After popping, the non-recurring job should be gone
        assert!(store.list().is_empty());
    }

    #[test]
    fn cron_pop_due_recurring_stays() {
        let store = CronStore::new();
        store.add("* * * * *".into(), "every-min".into(), true, false);
        let due = store.pop_due();
        assert!(!due.is_empty());
        // After popping, the recurring job should still be there
        let remaining = store.list();
        assert_eq!(remaining.len(), 1);
        assert!(remaining[0].last_fired_ms.is_some());
    }

    #[test]
    fn cron_durability_save_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cron_test.json");

        // Create store with path, add a durable job
        let store = CronStore::load_or_default(Some(path.clone()));
        store.add("0 9 * * *".into(), "durable task".into(), true, true);
        drop(store); // drops the Mutex, file is written

        // Load from same path
        let store2 = CronStore::load_or_default(Some(path.clone()));
        let jobs = store2.list();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].prompt, "durable task");
        assert!(jobs[0].durable);
    }

    fn at(rfc3339: &str) -> time::OffsetDateTime {
        time::OffsetDateTime::parse(rfc3339, &time::format_description::well_known::Rfc3339)
            .unwrap()
    }

    #[test]
    fn only_durable_jobs_reach_the_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cron.json");
        let store = CronStore::load_or_default(Some(path.clone()));
        store.add("0 9 * * *".into(), "session only".into(), true, false);
        store.add("0 9 * * *".into(), "durable".into(), true, true);
        let reloaded = CronStore::load_or_default(Some(path));
        let prompts: Vec<String> = reloaded.list().into_iter().map(|j| j.prompt).collect();
        assert_eq!(prompts, vec!["durable".to_string()]);
    }

    #[test]
    fn an_hourly_job_fires_every_hour_not_just_the_first() {
        let store = CronStore::new();
        store.add("0 * * * *".into(), "hourly".into(), true, false);
        assert_eq!(store.pop_due_at(at("2026-10-26T10:00:05Z")).len(), 1);
        assert_eq!(store.pop_due_at(at("2026-10-26T10:00:40Z")).len(), 0);
        assert_eq!(store.pop_due_at(at("2026-10-26T11:00:05Z")).len(), 1);
    }

    #[test]
    fn a_session_sees_and_removes_only_its_own_jobs() {
        let store = CronStore::new();
        let job = |sid: &str| NewCronJob {
            cron: "0 9 * * *".into(),
            prompt: "check".into(),
            recurring: true,
            session_id: Some(sid.into()),
            ..Default::default()
        };
        let a = store.add_job(job("a"));
        store.add_job(job("b"));
        assert_eq!(store.list_for("a").len(), 1);
        assert!(!store.remove_for("b", &a.id));
        assert!(store.remove_for("a", &a.id));
        assert_eq!(store.remove_session("b"), 1);
        assert!(store.list().is_empty());
    }

    #[test]
    fn an_expired_recurring_job_fires_once_more_then_goes() {
        let store = CronStore::new();
        let expires = at("2026-10-26T10:30:00Z").unix_timestamp() * 1000;
        store.add_job(NewCronJob {
            cron: "0 * * * *".into(),
            prompt: "hourly".into(),
            recurring: true,
            expires_ms: Some(expires),
            ..Default::default()
        });
        assert_eq!(store.pop_due_at(at("2026-10-26T10:00:00Z")).len(), 1);
        assert_eq!(store.list().len(), 1);
        assert_eq!(store.pop_due_at(at("2026-10-26T11:00:00Z")).len(), 1);
        assert!(store.list().is_empty());
    }
}
