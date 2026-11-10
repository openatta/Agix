//! `sandbox.subprocess_env` applied to a command started on this machine.
//!
//! The policy and what it decides are `base`'s
//! ([`SubprocessEnv::resolve`]); this is the one place that reads this
//! process's environment for a child and writes the answer into the command.

use base::interface::exec::{EnvPlan, SubprocessEnv};

/// Set `cmd`'s environment from this process's, as `policy` says.
pub fn apply_subprocess_env(policy: &SubprocessEnv, cmd: &mut tokio::process::Command) {
    match policy.resolve(std::env::vars_os()) {
        EnvPlan::Remove(names) => {
            for name in names {
                cmd.env_remove(name);
            }
        }
        EnvPlan::Replace(vars) => {
            cmd.env_clear();
            cmd.envs(vars);
        }
    }
}
