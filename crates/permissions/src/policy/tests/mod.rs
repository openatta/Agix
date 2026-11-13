//! The policy judged through the requests a session makes of it, and its
//! stages one at a time.

use super::*;
use crate::ruleset::{RuleHit, RuleSet};
use base::context::SessionState;
use base::interface::permission::{
    Answerable, Basis, ModeStance, PermissionFacts, PermissionOutcome, PermissionRequest, Reach,
    Stage, Subject, ToolFacts,
};
use base::paths::SensitivePaths;
use base::permission::{RuleBehavior, RuleSource};
use base::tool::{Tool, ToolContext};
use serde_json::Value;
use std::sync::Arc;

mod answering;
mod judging;
mod requests;

/// The context the runtime runs a call of `session`'s with, and what `tool`
/// says about the call in it.
async fn call_in(
    tool: Option<&Arc<dyn Tool>>,
    input: &Value,
    session: &Arc<SessionState>,
) -> (ToolContext, ToolFacts) {
    let env = session.env();
    let mut ctx = ToolContext::from_engine_ctx_with(
        env.cwd.clone(),
        tokio_util::sync::CancellationToken::new(),
        session
            .exec()
            .unwrap_or_else(base::interface::exec::ExecProviders::in_process),
    );
    ctx.session_id = "s1".into();
    ctx.session = session.clone();
    ctx.permission_mode = session.permission_mode();
    ctx.additional_writable_dirs = env.additional_writable_dirs.clone();
    let facts = match tool {
        Some(tool) => ToolFacts::gather(tool.as_ref(), input, &ctx).await,
        None => ToolFacts::unknown_tool(),
    };
    (ctx, facts)
}
