//! Tests for the `agent_tool` module, grouped by what they exercise.

use super::*;
use base::interface::model::{MessageRole, ModelMessage};
use base::interface::permission::PermissionPolicy;
use base::tool::InMemoryToolRegistry;
use std::io::Write;

mod catalog;
mod lifecycle;
mod members;
mod parent;
mod permission_facts;
mod programs;
mod resolve;
mod run;
mod scenes;
mod scripted;
mod spawn_paths;
mod wiring;

use catalog::*;
use members::*;
use parent::*;
use resolve::*;
use run::*;
use wiring::*;
