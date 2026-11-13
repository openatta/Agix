//! 会话日志：`session.history` / `appendEntry` / `entries` / `export`。

use super::*;
use crate::rpc::methods::history::{AppendEntryParams, EntriesParams, ExportParams, HistoryParams};
use crate::session_pool::api::{EntryList, EntryView, ExportSummary, HistoryPage};

rpc_methods! {
    "session.history" => history(HistoryParams) -> HistoryPage;
    "session.appendEntry" => append_entry(AppendEntryParams) -> EntryView;
    "session.entries" => entries(EntriesParams) -> EntryList;
    "session.export" => export(ExportParams) -> ExportSummary;
}

/// Page size when the caller does not say: big enough for a typical
/// conversation in one round trip, small enough that a forgotten `limit`
/// cannot turn one RPC into a multi-megabyte line.
const HISTORY_DEFAULT_LIMIT: usize = 100;

/// A request for more is clamped, not refused: the page echoes the `limit`
/// applied plus `total` and `has_more`, so a caller can tell and page on.
const HISTORY_MAX_LIMIT: usize = 500;

async fn history(
    s: &DaemonServer,
    _: &Call,
    params: HistoryParams,
) -> Result<HistoryPage, PoolError> {
    let offset = params.offset.min(usize::MAX as u64) as usize;
    let limit = params
        .limit
        .map(|n| n.min(HISTORY_MAX_LIMIT as u64) as usize)
        .unwrap_or(HISTORY_DEFAULT_LIMIT);
    s.pool
        .session_history(&params.session_id, offset, limit)
        .await
}

async fn append_entry(
    s: &DaemonServer,
    _: &Call,
    params: AppendEntryParams,
) -> Result<EntryView, PoolError> {
    s.pool
        .append_entry(
            &params.session_id,
            &params.ns,
            &params.event,
            params.payload,
        )
        .await
}

async fn entries(
    s: &DaemonServer,
    _: &Call,
    params: EntriesParams,
) -> Result<EntryList, PoolError> {
    s.pool
        .list_entries(&params.session_id, params.namespaces.as_deref())
        .await
}

async fn export(
    s: &DaemonServer,
    _: &Call,
    params: ExportParams,
) -> Result<ExportSummary, PoolError> {
    let include = match params.include {
        None => crate::export::Include::default(),
        Some(names) => crate::export::Include::parse(&names).map_err(PoolError::InvalidRequest)?,
    };
    s.pool
        .export_session(crate::export::ExportRequest {
            session_id: params.session_id,
            path: params.path,
            format: params.format,
            include,
        })
        .await
}
