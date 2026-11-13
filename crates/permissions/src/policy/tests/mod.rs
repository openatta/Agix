//! The policy judged through the requests a session makes of it, and its
//! stages one at a time.

use super::*;
use crate::ruleset::{RuleHit, RuleSet};
use base::interface::permission::{Answerable, Basis, PermissionOutcome, PermissionRequest, Stage};
use base::paths::SensitivePaths;
use base::permission::{RuleBehavior, RuleSource};
use base::tool::PermissionContentKind;
use serde_json::Value;
use std::sync::Arc;

mod judging;
mod requests;
