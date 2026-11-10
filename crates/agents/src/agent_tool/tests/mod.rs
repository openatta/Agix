//! Tests for the `agent_tool` module, grouped by what they exercise.

use super::*;
use base::interface::model::{MessageRole, ModelMessage};
use base::interface::permission::PermissionPolicy;
use std::io::Write;

mod catalog;
mod lifecycle;
mod members;
mod permission_facts;
mod programs;
mod resolve;
mod run;
mod scenes;
mod spawn_paths;
mod wiring;

use catalog::*;
use members::*;
use resolve::*;
use run::*;
use wiring::*;
