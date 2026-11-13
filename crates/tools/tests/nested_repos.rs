//! A repository a command nests in the project is found once the command has
//! run, and moved aside, so the user's own `git` in the project afterwards —
//! outside any sandbox — does not run what its configuration names. The
//! attack is the one `platform`'s ignored
//! `audit2_a_sandboxed_command_cannot_plant_an_embedded_repositorys_configuration`
//! records the backends cannot stop.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use base::interface::tool::{ProgressSender, Tool, ToolContext};
use std::path::Path;
use std::process::Command;

fn git(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .args(["-c", "user.email=t@example.com", "-c", "user.name=T"])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
}

fn project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    assert!(git(dir.path(), &["init", "-q"]).status.success());
    std::fs::write(dir.path().join("a"), "a").unwrap();
    assert!(git(dir.path(), &["add", "a"]).status.success());
    assert!(git(dir.path(), &["commit", "-qm", "first"])
        .status
        .success());
    dir
}

async fn bash(ctx: &ToolContext, command: &str) {
    let result = tools::bash::BashTool
        .call(
            serde_json::json!({ "command": command }),
            ctx.clone(),
            ProgressSender::noop("nested"),
        )
        .await
        .unwrap();
    assert!(!result.is_error, "{:?}", result.content);
}

fn plant(pwned: &Path) -> String {
    format!(
        "mkdir sub && cd sub && git init -q && touch f && git add f && \
         git -c user.email=t@example.com -c user.name=T commit -q -m s && \
         git config core.fsmonitor \"touch '{}'; false\" && cd .. && git add sub",
        pwned.display()
    )
}

#[tokio::test]
async fn a_planted_repository_is_moved_aside_before_the_users_git_enters_it() {
    let project = project();
    let elsewhere = tempfile::tempdir().unwrap();
    let pwned = elsewhere.path().join("pwned");
    let ctx = platform::tool_context_for_test(project.path().canonicalize().unwrap());
    bash(&ctx, &plant(&pwned)).await;

    let status = git(project.path(), &["status"]);
    assert!(status.status.success(), "{status:?}");
    assert!(
        pwned.exists(),
        "precondition: the user's `git status` runs the planted repository's fsmonitor"
    );
    std::fs::remove_file(&pwned).unwrap();

    let scan = tools::nested_repos::scan(&ctx).await;
    assert_eq!(scan.dangerous.len(), 1, "{scan:?}");
    let found = &scan.dangerous[0];
    assert_eq!(found.keys, ["core.fsmonitor"]);
    let moved = tools::nested_repos::quarantine(&found.git_dir)
        .await
        .unwrap();
    assert!(moved
        .file_name()
        .unwrap()
        .to_string_lossy()
        .starts_with(".git.agix-quarantine-"));
    assert!(!project.path().join("sub/.git").exists());

    let status = git(project.path(), &["status"]);
    assert!(status.status.success(), "{status:?}");
    assert!(
        !pwned.exists(),
        "the user's `git status` ran a program the command named"
    );
}

#[tokio::test]
async fn a_repository_someone_kept_is_reported_again_only_once_its_configuration_changes() {
    let project = project();
    let elsewhere = tempfile::tempdir().unwrap();
    let ctx = platform::tool_context_for_test(project.path().canonicalize().unwrap());
    bash(&ctx, &plant(&elsewhere.path().join("pwned"))).await;

    let first = tools::nested_repos::scan(&ctx).await;
    assert_eq!(first.dangerous.len(), 1);
    first.dangerous[0].keep(&ctx);
    assert!(tools::nested_repos::scan(&ctx).await.dangerous.is_empty());

    bash(&ctx, "cd sub && git config diff.external ./d.sh").await;
    let changed = tools::nested_repos::scan(&ctx).await;
    assert_eq!(changed.dangerous.len(), 1);
    assert_eq!(
        changed.dangerous[0].keys,
        ["core.fsmonitor", "diff.external"]
    );
}

#[tokio::test]
async fn harmless_and_unindexed_repositories_are_only_mentioned_and_only_once() {
    let project = project();
    let ctx = platform::tool_context_for_test(project.path().canonicalize().unwrap());
    bash(
        &ctx,
        "mkdir plain && cd plain && git init -q && touch f && git add f && \
         git -c user.email=t@example.com -c user.name=T commit -q -m s && cd .. && \
         git add plain && mkdir -p deep/er && cd deep/er && git init -q",
    )
    .await;

    let scan = tools::nested_repos::scan(&ctx).await;
    assert!(scan.dangerous.is_empty(), "{scan:?}");
    assert_eq!(scan.notes.len(), 2, "{scan:?}");
    assert!(scan.notes.iter().any(|n| n.contains("plain/.git")));
    assert!(scan.notes.iter().any(|n| n.contains("deep/er/.git")));
    assert!(tools::nested_repos::scan(&ctx).await.notes.is_empty());
}

#[tokio::test]
async fn a_declared_submodule_is_left_alone() {
    let project = project();
    let ctx = platform::tool_context_for_test(project.path().canonicalize().unwrap());
    bash(
        &ctx,
        "mkdir lib && cd lib && git init -q && touch f && git add f && \
         git -c user.email=t@example.com -c user.name=T commit -q -m s && \
         git config core.pager cat && cd .. && git add lib && \
         printf '[submodule \"lib\"]\\n\\tpath = lib\\n\\turl = ./lib\\n' > .gitmodules",
    )
    .await;
    let scan = tools::nested_repos::scan(&ctx).await;
    assert!(scan.dangerous.is_empty(), "{scan:?}");
}
