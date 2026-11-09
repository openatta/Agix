//! An unattended session with a real model, as a host that runs one headless
//! sees it: the `harnix` scene under the default permission policy, with no
//! one to answer a prompt. Work inside the project gets done — files edited,
//! commands run in the OS sandbox — and a write outside it is refused rather
//! than put to a person, and the model is told so instead of left waiting. A
//! shell command that writes outside the roots — a redirect, a copy — is
//! stopped the same way.

use std::time::Duration;

use live::{LiveRun, Mode};
use serde_json::json;

fn nonce() -> String {
    format!(
        "{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
            % 0xffff_ffff_ffff
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unattended_session_works_inside_its_roots_and_is_refused_outside() -> anyhow::Result<()>
{
    let mut run = LiveRun::start(
        "unattended",
        json!({ "permission_mode": "default" }),
        Mode::InProcess,
    )
    .await?;
    let token = format!("TOKEN-{}", nonce());
    run.write_file("app/version.txt", "1.0.0\n")?;
    run.write_file(
        "app/build.sh",
        &format!("echo built {token} > app/build.log\n"),
    )?;
    let outside = run.world.root().join("elsewhere");
    std::fs::create_dir_all(&outside)?;
    let stray = outside.join("stray.txt");

    run.client
        .call("scene.activate", json!({ "scene": "harnix" }))
        .await?;
    let session = run.session_with(json!({ "scene": "harnix" })).await?;
    let watcher = run.watch(&session, false).await?;
    let events = tokio::time::timeout(
        Duration::from_secs(600),
        run.turn(
            &session,
            &format!(
                "Do these three steps in order, then reply with a one-line report per step:\n\
                 1. Change app/version.txt to 1.0.1 with the Edit tool.\n\
                 2. Run `sh app/build.sh` with the Bash tool.\n\
                 3. Write the word STRAY to {} with the Write tool. If that is refused, \
                 do not try any other way; say so in the report.",
                stray.display()
            ),
        ),
    )
    .await??;

    assert_eq!(
        run.read_file("app/version.txt")
            .map(|s| s.trim().to_string()),
        Some("1.0.1".into())
    );
    assert!(
        run.read_file("app/build.log")
            .is_some_and(|s| s.contains(&token)),
        "Bash ran in the sandbox and wrote inside the project"
    );
    assert!(
        !stray.exists(),
        "a write outside the instance's roots must not happen unattended"
    );
    let refused = events
        .tool_results
        .iter()
        .any(|(name, _, is_error)| name == "Write" && *is_error);
    assert!(
        refused,
        "the model was told the write was refused: {:?}",
        events.tool_results
    );
    assert!(
        watcher.events("prompt").is_empty(),
        "nobody is there to ask, so nothing was asked"
    );

    for call in run.provider.calls() {
        let tools = call.tool_names();
        let deferred = call.messages_text();
        for absent in ["AskUserQuestion", "EnterPlanMode", "ExitPlanMode"] {
            assert!(
                !tools.iter().any(|t| t == absent) && !deferred.contains(absent),
                "{absent} waits on a reply and is not offered unattended, not even by name: {tools:?}"
            );
        }
    }
    drop(watcher);
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_shell_command_writing_outside_the_roots_is_refused_unattended() -> anyhow::Result<()> {
    let mut run = LiveRun::start(
        "unattended-shell",
        json!({ "permission_mode": "default" }),
        Mode::InProcess,
    )
    .await?;
    // Not under the world's root: that is in the user's temp directory,
    // which the macOS sandbox leaves writable for `mktemp`.
    let elsewhere = tempfile::Builder::new()
        .prefix("agix-outside-")
        .tempdir_in("/private/tmp")?;
    let outside = elsewhere.path().to_path_buf();
    let (stray, copied) = (outside.join("stray.txt"), outside.join("copied.txt"));
    run.write_file("inside.txt", "inside\n")?;
    run.client
        .call("scene.activate", json!({ "scene": "harnix" }))
        .await?;
    let session = run.session_with(json!({ "scene": "harnix" })).await?;
    let watcher = run.watch(&session, false).await?;
    let events = tokio::time::timeout(
        Duration::from_secs(600),
        run.turn(
            &session,
            &format!(
                "Run these two commands with the Bash tool, one at a time, exactly as written. \
                 If one fails, do not try any other way; report its error and go on.\n\
                 1. `echo STRAY > {}`\n\
                 2. `cp inside.txt {}`\n\
                 Reply with a one-line report per command.",
                stray.display(),
                copied.display()
            ),
        ),
    )
    .await??;
    assert!(
        events.tool_uses.iter().filter(|(n, _)| n == "Bash").count() >= 2,
        "{:?}",
        events.tool_uses
    );
    assert!(!stray.exists(), "a redirect wrote outside the roots");
    assert!(!copied.exists(), "a copy wrote outside the roots");
    assert!(
        watcher.events("prompt").is_empty(),
        "nobody is there to ask, so nothing was asked"
    );
    drop(watcher);
    run.finish().await;
    Ok(())
}
