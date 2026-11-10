//! Tests for the `agent_tool` module, grouped by what they exercise.

use super::*;
use base::interface::model::{MessageRole, ModelMessage};
use std::io::Write;

mod catalog;
mod lifecycle;
mod members;
mod programs;
mod resolve;
mod run;
mod scenes;
mod wiring;

use catalog::*;
use members::*;
use resolve::*;
use run::*;
use wiring::*;
