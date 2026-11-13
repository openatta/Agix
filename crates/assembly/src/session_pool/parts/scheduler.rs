//! 定时任务的调度者 `Scheduler`：持有所有会话的定时任务表，每分钟把到期的任务
//! 交给 [`TurnLauncher`]。它不认识会话，一个任务怎么变成会话的一轮由门面决定。
//!
//! 不持锁：任务表 `CronStore` 自己管并发。

use crate::session_pool::api::{NewSchedule, PoolError};
use std::sync::{Arc, Weak};
use std::time::Duration;
use tools::cron::{CronJob, CronStore, NewCronJob};

/// Runs a due job as a turn of its session.
#[async_trait::async_trait]
pub(in crate::session_pool) trait TurnLauncher: Send + Sync {
    async fn launch_scheduled(&self, job: CronJob);
}

pub(in crate::session_pool) struct Scheduler {
    store: Arc<CronStore>,
}

impl Scheduler {
    pub(in crate::session_pool) fn load(path: std::path::PathBuf) -> Self {
        Self {
            store: Arc::new(CronStore::load_or_default(Some(path))),
        }
    }

    /// The table every session engine's cron tools write to.
    pub(in crate::session_pool) fn store(&self) -> Arc<CronStore> {
        self.store.clone()
    }

    /// `new` as a job, once its fields are there and its cron expression
    /// parses. Whether the session exists is the caller's to check.
    pub(in crate::session_pool) fn prepare(new: NewSchedule) -> Result<NewCronJob, PoolError> {
        let text = |value: &str| value.trim().to_string();
        let (session_id, cron, prompt) =
            (text(&new.session_id), text(&new.cron), text(&new.prompt));
        if session_id.is_empty() || cron.is_empty() || prompt.is_empty() {
            return Err(PoolError::InvalidRequest(
                "schedule.create needs session_id, cron and prompt".to_string(),
            ));
        }
        if !tools::cron::cron_expression_valid(&cron) {
            return Err(PoolError::InvalidRequest(format!(
                "invalid cron expression `{cron}`: expected 5 fields, M H DoM Mon DoW"
            )));
        }
        Ok(NewCronJob {
            cron,
            prompt,
            recurring: new.recurring,
            durable: new.durable,
            session_id: Some(session_id),
            expires_ms: None,
        })
    }

    pub(in crate::session_pool) fn add(&self, job: NewCronJob) -> CronJob {
        self.store.add_job(job)
    }

    pub(in crate::session_pool) fn list(&self, session_id: Option<&str>) -> Vec<CronJob> {
        match session_id {
            Some(sid) => self.store.list_for(sid),
            None => self.store.list(),
        }
    }

    pub(in crate::session_pool) fn remove(&self, id: &str) -> bool {
        self.store.remove(id)
    }

    pub(in crate::session_pool) fn remove_for(&self, session_id: &str, id: &str) -> bool {
        self.store.remove_for(session_id, id)
    }

    pub(in crate::session_pool) fn remove_session(&self, session_id: &str) {
        self.store.remove_session(session_id);
    }

    /// Launch every job due at `now` and return once they have all ended.
    pub(in crate::session_pool) async fn run_due(
        &self,
        launcher: &dyn TurnLauncher,
        now: time::OffsetDateTime,
    ) {
        run_due(&self.store, launcher, now).await;
    }

    /// Check at the top of every minute, a 5-field cron expression's
    /// resolution, for as long as `launcher` lives.
    pub(in crate::session_pool) fn start(&self, launcher: Weak<dyn TurnLauncher>) {
        let store = self.store.clone();
        tokio::spawn(async move {
            loop {
                let into_minute = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() % 60_000)
                    .unwrap_or(0) as u64;
                tokio::time::sleep(Duration::from_millis(60_000 - into_minute + 50)).await;
                let Some(launcher) = launcher.upgrade() else {
                    return;
                };
                let store = store.clone();
                tokio::spawn(async move {
                    run_due(&store, &*launcher, time::OffsetDateTime::now_utc()).await;
                });
            }
        });
    }
}

async fn run_due(store: &CronStore, launcher: &dyn TurnLauncher, now: time::OffsetDateTime) {
    let turns: Vec<_> = store
        .pop_due_at(now)
        .into_iter()
        .map(|job| launcher.launch_scheduled(job))
        .collect();
    futures::future::join_all(turns).await;
}
