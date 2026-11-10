//! The crate's integration tests, compiled into one test binary; each module
//! is one file of this directory. The two files that touch process-wide state
//! are test binaries of their own.

mod capture;
mod commands_have_no_terminal;
mod engine_git_in_sandbox;
mod frozen_git;
