//! 会话生命周期：`session.create` / `get` / `list` / `close` / `delete` / `fork` /
//! `resume` / `rename` / `setModel` / `setPrompt` / `subscribe` / `unsubscribe` /
//! `interrupt`。

use super::*;
use crate::rpc::methods::session::*;
use crate::rpc::methods::{Patch, WithIgnored};
use crate::rpc::{read_options, RpcError};
use crate::session_pool::api::{
    DeleteReport, ForkReport, ModelChoice, PoolError, PromptChoice, Renamed, ResumeReport,
    SessionCreated, SessionDetail, WatcherId,
};

rpc_methods! {
    "session.create" => create(CreateParams) -> WithIgnored<SessionCreated>;
    "session.get" => get(SessionRef) -> SessionDetail;
    "session.list" => list(ListParams) -> SessionList;
    "session.close" => close(SessionRef) -> Closed;
    "session.delete" => delete(DeleteParams) -> DeleteReport;
    "session.fork" => fork(ForkParams) -> ForkReport;
    "session.resume" => resume(ResumeParams) -> WithIgnored<ResumeReport>;
    "session.rename" => rename(RenameParams) -> Renamed;
    "session.setModel" => set_model(SetModelParams) -> ModelChoice;
    "session.setPrompt" => set_prompt(SetPromptParams) -> PromptChoice;
    "session.subscribe" => subscribe(SubscribeParams) -> SubscribeResult;
    "session.unsubscribe" => unsubscribe(SessionRef) -> Unsubscribed;
    "session.interrupt" => interrupt(SessionRef) -> Interrupted;
}

fn invalid(message: impl Into<String>) -> RpcError {
    PoolError::InvalidRequest(message.into()).into()
}

async fn create(
    s: &DaemonServer,
    cx: &Call,
    p: CreateParams,
) -> Result<WithIgnored<SessionCreated>, RpcError> {
    let (options, ignored_options) = read_options(cx.method, p.options.as_ref())?;
    let result = s
        .pool
        .create_session(p.scene.as_deref(), p.project_root.into(), options.as_ref())
        .await?;
    Ok(WithIgnored {
        result,
        ignored_options,
    })
}

async fn get(s: &DaemonServer, _: &Call, p: SessionRef) -> Result<SessionDetail, PoolError> {
    s.pool.session_detail(&p.session_id).await
}

async fn list(s: &DaemonServer, _: &Call, p: ListParams) -> Result<SessionList, PoolError> {
    let sessions = s
        .pool
        .list_all(p.include_children, p.parent_session_id.as_deref())
        .await;
    Ok(SessionList { sessions })
}

async fn close(s: &DaemonServer, _: &Call, p: SessionRef) -> Result<Closed, PoolError> {
    let sidechains_deleted = s.pool.shutdown_session(&p.session_id).await?;
    Ok(Closed {
        closed: p.session_id,
        sidechains_deleted,
    })
}

async fn delete(s: &DaemonServer, _: &Call, p: DeleteParams) -> Result<DeleteReport, PoolError> {
    s.pool.delete_session(&p.session_id, p.dry_run).await
}

async fn fork(s: &DaemonServer, _: &Call, p: ForkParams) -> Result<ForkReport, PoolError> {
    s.pool.fork_session(&p.session_id, p.at_message).await
}

async fn resume(
    s: &DaemonServer,
    cx: &Call,
    p: ResumeParams,
) -> Result<WithIgnored<ResumeReport>, RpcError> {
    let (options, ignored_options) = read_options(cx.method, p.options.as_ref())?;
    let result = s
        .pool
        .resume_session(&p.session_id, p.create_if_missing, options.as_ref())
        .await?;
    Ok(WithIgnored {
        result,
        ignored_options,
    })
}

async fn rename(s: &DaemonServer, _: &Call, p: RenameParams) -> Result<Renamed, PoolError> {
    s.pool
        .rename_session(&p.session_id, p.name.as_deref())
        .await
}

async fn set_model(s: &DaemonServer, _: &Call, p: SetModelParams) -> Result<ModelChoice, RpcError> {
    let named = |v: &Option<String>| v.clone().filter(|s| !s.trim().is_empty());
    let (model, provider) = (named(&p.model), named(&p.provider));
    if model.is_none() && provider.is_none() {
        return Err(invalid("missing model"));
    }
    Ok(s.pool
        .set_session_model(&p.session_id, model.as_deref(), provider.as_deref())
        .await?)
}

async fn set_prompt(
    s: &DaemonServer,
    _: &Call,
    p: SetPromptParams,
) -> Result<PromptChoice, RpcError> {
    if p.append.is_keep() && p.override_text.is_keep() {
        return Err(invalid("give `append` and/or `override`"));
    }
    let apply = |field: &mut Option<String>, patch: Patch<String>| match patch {
        Patch::Keep => {}
        Patch::Clear => *field = None,
        Patch::Set(text) => *field = Some(text),
    };
    let change = move |prompt: &mut runtime::agent::SessionPrompt| {
        apply(&mut prompt.append, p.append);
        apply(&mut prompt.override_text, p.override_text);
    };
    Ok(s.pool.set_session_prompt(&p.session_id, change).await?)
}

async fn subscribe(
    s: &DaemonServer,
    cx: &Call,
    p: SubscribeParams,
) -> Result<SubscribeResult, PoolError> {
    let watcher = cx.client.clone();
    let subscribed = if p.resume {
        s.pool.subscribe_resuming(&p.session_id, watcher).await?
    } else {
        s.pool.subscribe_session(&p.session_id, watcher).await?
    };
    Ok(watch::subscribe_result(subscribed))
}

async fn unsubscribe(
    s: &DaemonServer,
    cx: &Call,
    p: SessionRef,
) -> Result<Unsubscribed, PoolError> {
    s.pool
        .unsubscribe_session(&p.session_id, WatcherId(cx.client.id().0))
        .await;
    Ok(Unsubscribed {
        session_id: p.session_id,
        subscribed: false,
    })
}

async fn interrupt(s: &DaemonServer, _: &Call, p: SessionRef) -> Result<Interrupted, PoolError> {
    let interrupted = s.pool.interrupt_session(&p.session_id).await?;
    Ok(Interrupted {
        session_id: p.session_id,
        interrupted,
    })
}
