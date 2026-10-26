//! In-memory cron scheduler — replaces the OS-crontab-based `ScheduleCronTool`.
//!
//! Three tools: `CronCreate`, `CronDelete`, `CronList`, each acting on the
//! calling session's own jobs. The `CronStore` holds jobs in memory (durable
//! ones also in `~/.agix/scenes/<scope>/scheduled_tasks.json`) and provides
//! `pop_due()` to whatever fires them: the session pool's scheduler, or an
//! engine's own input loop when it runs without a pool.

pub mod create;
pub mod delete;
pub mod list;
pub mod parser;
pub mod store;

pub use create::{CronCreateInput, CronCreateTool};
pub use delete::{CronDeleteInput, CronDeleteTool};
pub use list::CronListTool;
pub use parser::cron_expression_valid;
pub use store::{CronJob, CronStore, NewCronJob, MODEL_JOB_LIFETIME_MS};
