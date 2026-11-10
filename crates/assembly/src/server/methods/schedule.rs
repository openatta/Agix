//! `schedule.*`。

use super::*;
use crate::rpc::methods::schedule::{
    ScheduleCreateParams, ScheduleCreated, ScheduleDeleteParams, ScheduleDeleted, ScheduleList,
    ScheduleListParams,
};
use crate::session_pool::api::NewSchedule;

rpc_methods! {
    "schedule.create" => create(ScheduleCreateParams) -> ScheduleCreated;
    "schedule.list" => list(ScheduleListParams) -> ScheduleList;
    "schedule.delete" => delete(ScheduleDeleteParams) -> ScheduleDeleted;
}

async fn create(
    s: &DaemonServer,
    _: &Call,
    params: ScheduleCreateParams,
) -> Result<ScheduleCreated, PoolError> {
    let job = s
        .pool
        .schedule_add(NewSchedule {
            session_id: params.session_id,
            cron: params.cron,
            prompt: params.prompt,
            recurring: params.recurring,
            durable: params.durable,
        })
        .await?;
    Ok(ScheduleCreated { job })
}

async fn list(
    s: &DaemonServer,
    _: &Call,
    params: ScheduleListParams,
) -> Result<ScheduleList, PoolError> {
    Ok(ScheduleList {
        jobs: s.pool.schedule_list(params.session_id.as_deref()),
    })
}

async fn delete(
    s: &DaemonServer,
    _: &Call,
    params: ScheduleDeleteParams,
) -> Result<ScheduleDeleted, PoolError> {
    Ok(ScheduleDeleted {
        deleted: s.pool.schedule_remove(&params.id),
    })
}
