//! Agent tests: shared fixtures here, cases grouped by area in the sibling files.

use super::builder::build_hook_runner;
use super::*;

pub(super) fn test_settings() -> base::settings::LoadedSettings {
    use base::settings::*;
    let mut values = Settings::defaults_for("test");
    values.model.max_tokens = 2000;
    values.model.thinking_mode = ThinkingMode::Auto;
    values.model.effort = None;
    values.telemetry_enabled = false;
    let mut paths = PathSettings::default();
    paths.user_data_dir = "/tmp".into();
    paths.global_data_dir = "/tmp".into();
    paths.workspace = base::settings::Workspace::project_of(std::path::Path::new("/tmp"));
    paths.local_data_dir = "/tmp".into();
    base::settings::LoadedSettings::new(values, paths)
}

mod modules_and_session;
use modules_and_session::*;
mod registry;
mod turn_loop;
use turn_loop::*;
mod events_and_steering;
mod policies;
use events_and_steering::*;
mod build_snapshot;
mod compacting;
mod exec_providers;
mod prompt_middleware;
mod web_tasks;
