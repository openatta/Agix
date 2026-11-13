//! 会话目标：`session.goal` / `setGoal` / `updateGoal`。

use super::*;
use crate::rpc::methods::goal::{GoalActionName, GoalResult, SetGoalParams, UpdateGoalParams};
use crate::rpc::methods::SessionRef;
use crate::session_pool::GoalChange;

rpc_methods! {
    "session.goal" => goal(SessionRef) -> GoalResult;
    "session.setGoal" => set_goal(SetGoalParams) -> GoalResult;
    "session.updateGoal" => update_goal(UpdateGoalParams) -> GoalResult;
}

async fn goal(s: &DaemonServer, _: &Call, params: SessionRef) -> Result<GoalResult, PoolError> {
    Ok(GoalResult {
        goal: s.pool.session_goal(&params.session_id).await?,
    })
}

async fn set_goal(
    s: &DaemonServer,
    _: &Call,
    params: SetGoalParams,
) -> Result<GoalResult, PoolError> {
    let change = GoalChange::Set {
        objective: params.objective,
        max_rounds: params.max_rounds,
    };
    Ok(GoalResult {
        goal: s
            .pool
            .change_session_goal(&params.session.session_id, change)
            .await?,
    })
}

async fn update_goal(
    s: &DaemonServer,
    _: &Call,
    params: UpdateGoalParams,
) -> Result<GoalResult, PoolError> {
    let change = match params.action {
        GoalActionName::Pause => GoalChange::Pause,
        GoalActionName::Resume => GoalChange::Resume,
        GoalActionName::Clear => GoalChange::Clear,
        GoalActionName::Edit => GoalChange::Edit {
            objective: params.objective,
            max_rounds: params.max_rounds,
        },
    };
    Ok(GoalResult {
        goal: s
            .pool
            .change_session_goal(&params.session.session_id, change)
            .await?,
    })
}
