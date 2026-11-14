//! What a permitted call leaves owed once it has run (`Obligations`): a
//! command confined looser than the strict profile is checked for a
//! repository it nested in the project. A foreground command is checked
//! before `PostToolUse` sees its result, a background one when its task ends.
//!
//! A dangerous one is put to the person when someone attends the session and
//! moved aside otherwise; an unanswered question moves it aside too. Either
//! way the result says so, and the host is notified.

use super::tool_exec::{fire_ctx_lifecycle_hook, ToolExecCtx};
use base::event::AgentEvent;
use base::interface::permission::Attendance;
use base::interface::tool_middleware::ToolOutcome;
use base::tool::ToolContext;
use tools::nested_repos::{self, Dangerous};

const KEEP: &str = "keep";
const QUARANTINE: &str = "quarantine";

/// Settle what `obligations` owe for a call that has run; `after` is the
/// context it ran with.
pub(super) async fn fulfil(
    ctx: &ToolExecCtx,
    name: &str,
    input: &serde_json::Value,
    after: ToolContext,
    background: bool,
    outcome: &mut ToolOutcome,
) {
    if background {
        settle_when_its_task_ends(ctx, name, input, after);
        return;
    }
    let lines = settle(ctx, name, input, &after).await;
    if lines.is_empty() {
        return;
    }
    let text = match outcome {
        Ok(answer) => &mut answer.text,
        Err(message) => message,
    };
    for line in lines {
        text.push_str("\n\n");
        text.push_str(&line);
    }
}

/// A background command left its task registered under the call's id; the
/// check waits for that task to end and tells the agent in a notice.
fn settle_when_its_task_ends(
    ctx: &ToolExecCtx,
    name: &str,
    input: &serde_json::Value,
    after: ToolContext,
) {
    let task = after
        .session
        .running_background_tasks()
        .into_iter()
        .find(|t| {
            t.detail
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get("tool_use_id")
                .and_then(|v| v.as_str())
                == Some(after.tool_use_id.as_str())
        });
    let Some(task) = task else {
        return;
    };
    let (ctx, name, input) = (ctx.clone(), name.to_string(), input.clone());
    tokio::spawn(async move {
        task.wait_ended().await;
        let lines = settle(&ctx, &name, &input, &after).await;
        if !lines.is_empty() {
            after.session.post_notice(None, lines.join("\n"));
        }
    });
}

/// One line for each nested repository the call's command left behind.
async fn settle(
    ctx: &ToolExecCtx,
    name: &str,
    input: &serde_json::Value,
    after: &ToolContext,
) -> Vec<String> {
    let scan = nested_repos::scan(after).await;
    let mut lines = scan.notes;
    for repo in scan.dangerous {
        let shown = nested_repos::shown(&after.cwd, &repo.git_dir);
        let keys = repo.keys.join(", ");
        let line = if kept_by_a_person(ctx, name, after, &repo, &shown).await {
            repo.keep(after);
            format!(
                "[`{shown}` is a repository this command nested in the project, whose \
                 configuration names programs ({keys}) that a `git` run in the project runs \
                 outside the sandbox. Kept, as the user asked.]"
            )
        } else {
            match nested_repos::quarantine(after, &repo.git_dir).await {
                Ok(to) => format!(
                    "[`{shown}` is a repository this command nested in the project, whose \
                     configuration names programs ({keys}) that a `git` run in the project \
                     runs outside the sandbox. Moved aside to `{}`; renaming it back restores \
                     the repository.]",
                    nested_repos::shown(&after.cwd, &to)
                ),
                Err(e) => format!(
                    "[`{shown}` is a repository this command nested in the project, whose \
                     configuration names programs ({keys}) that a `git` run in the project \
                     runs outside the sandbox. It could not be moved aside ({e}): do not run \
                     `git` in the project until it is removed.]"
                ),
            }
        };
        notify(ctx, name, input, &line).await;
        lines.push(line);
    }
    lines
}

/// Put the repository to the person attending the session; keep it only on
/// their word.
async fn kept_by_a_person(
    ctx: &ToolExecCtx,
    name: &str,
    after: &ToolContext,
    repo: &Dangerous,
    shown: &str,
) -> bool {
    use base::interface::elicitation::{ElicitKind, ElicitOption, ElicitRequest};
    if after.attendance != Attendance::Attended {
        return false;
    }
    let ask = ctx.elicitation.ask(ElicitRequest {
        id: base::id::Id::new().to_string(),
        kind: ElicitKind::Clarification {
            header: Some("Nested repository".into()),
            tool_use_id: Some(after.tool_use_id.clone()),
        },
        message: format!(
            "`{name}` left a repository at `{shown}` whose configuration names programs ({}). \
             A `git` run in the project would enter it and run them outside the sandbox. Keep \
             it, or move its git directory aside?",
            repo.keys.join(", ")
        ),
        options: vec![
            ElicitOption {
                key: QUARANTINE.into(),
                label: "Move it aside".into(),
            },
            ElicitOption {
                key: KEEP.into(),
                label: "Keep it".into(),
            },
        ],
    });
    let timeout = ctx.config.permission_prompt_timeout_secs;
    let deadline = async move {
        if timeout == 0 {
            std::future::pending::<()>().await
        } else {
            tokio::time::sleep(std::time::Duration::from_secs(timeout)).await
        }
    };
    tokio::select! {
        answer = ask => answer.answer_as::<String>().as_deref() == Some(KEEP),
        _ = deadline => false,
        _ = ctx.cancel.cancelled() => false,
    }
}

async fn notify(ctx: &ToolExecCtx, name: &str, input: &serde_json::Value, line: &str) {
    if let Some(events) = &ctx.events {
        let _ = events.send(AgentEvent::Notification {
            message: line.to_string(),
            status: None,
            turn_id: ctx.turn_id.clone(),
        });
    }
    fire_ctx_lifecycle_hook(ctx, hooks::HookEvent::Notification, |i| {
        i.with_tool(name, input.clone()).with_reason(line)
    })
    .await;
}
