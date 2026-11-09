//! The workspace tools a declarative scene can offer, with a real model:
//! `EnterWorktree` / `ExitWorktree` on a git project (changes land in the
//! worktree and nowhere else, a removal with changes needs `discard_changes`,
//! a kept worktree stays on disk), `NotebookEdit` on an `.ipynb`, and `Glob` /
//! `Grep` on unicode names, a binary file and a file of several MiB.

use std::path::Path;

use live::LiveRun;
use serde_json::{json, Value};

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

const WORKSPACE: &[&str] = &[
    "Read",
    "Write",
    "Edit",
    "Bash",
    "Glob",
    "Grep",
    "EnterWorktree",
    "ExitWorktree",
    "NotebookEdit",
    "ToolSearch",
];

fn workspace_scene(world: &live::World) -> anyhow::Result<()> {
    world.write_declarative_scene(
        "workspace",
        WORKSPACE,
        "You are an operator's assistant in a code repository. Do exactly the steps you are \
         given, with the tools they name.",
    )?;
    Ok(())
}

async fn start(name: &str, git: bool) -> anyhow::Result<(LiveRun, String)> {
    let project = name.to_string();
    let mut run = LiveRun::builder(name)
        // A declarative scene is unattended: what it may do without asking is
        // what the instance grants.
        .settings(json!({
            "permission_rules": [
                { "tool": "EnterWorktree", "action": "allow" },
                { "tool": "ExitWorktree", "action": "allow" }
            ]
        }))
        .before_start(move |world, _| {
            if git {
                world.git_project(&project)?;
            }
            workspace_scene(world)
        })
        .start()
        .await?;
    let session = run.scene_session("workspace", json!({})).await?;
    Ok((run, session))
}

fn git(project: &Path, home: &Path, args: &[&str]) -> String {
    std::process::Command::new("git")
        .args(args)
        .current_dir(project)
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

fn results_of<'a>(
    events: &'a rpc_client::TurnEvents,
    tool: &str,
) -> Vec<&'a (String, String, bool)> {
    events
        .tool_results
        .iter()
        .filter(|(name, _, _)| name == tool)
        .collect()
}

/// `EnterWorktree` on `slug`, then a file with `token` written inside it:
/// the change is in the worktree and nowhere in the main checkout.
async fn enter_and_write(
    run: &mut LiveRun,
    session: &str,
    slug: &str,
    token: &str,
) -> anyhow::Result<std::path::PathBuf> {
    let events = run
        .turn(
            session,
            &format!(
                "1. Call EnterWorktree with slug {slug}.\n\
                 2. With the Write tool, write feature.txt containing {token} inside that \
                 worktree: use the absolute worktree path EnterWorktree returned, \
                 <that path>/feature.txt.\nReply DONE."
            ),
        )
        .await?;
    let entered = results_of(&events, "EnterWorktree");
    assert!(entered.iter().any(|(_, _, failed)| !failed), "{entered:?}");
    let tree = run.project.join(".agix/worktrees").join(slug);
    assert_eq!(
        std::fs::read_to_string(tree.join("feature.txt"))
            .ok()
            .map(|s| s.trim().to_string()),
        Some(token.to_string())
    );
    assert!(
        run.read_file("feature.txt").is_none(),
        "the main checkout took the change"
    );
    let home = run.world.home();
    let listed = git(&run.project, &home, &["worktree", "list"]);
    assert!(listed.contains(slug), "{listed}");
    let status = git(&run.project, &home, &["status", "--porcelain"]);
    assert!(!status.contains("feature.txt"), "{status}");
    Ok(tree)
}

fn assert_removed(run: &LiveRun, tree: &Path, slug: &str) {
    let home = run.world.home();
    assert!(!tree.exists(), "the worktree directory is gone");
    let branch = format!("agix/worktree-{slug}");
    let branches = git(&run.project, &home, &["branch", "--list", &branch]);
    assert!(branches.trim().is_empty(), "{branches}");
    assert!(!git(&run.project, &home, &["worktree", "list"]).contains(slug));
    assert!(run.read_file("feature.txt").is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_worktree_takes_the_changes_and_leaves_the_main_checkout_alone() -> anyhow::Result<()> {
    let (mut run, session) = start("worktree", true).await?;
    let token = format!("FEATURE-{}", nonce());
    let tree = enter_and_write(&mut run, &session, "feat-a", &token).await?;
    let events = run
        .turn(
            &session,
            "Call ExitWorktree with action remove and discard_changes true. Reply DONE.",
        )
        .await?;
    let exits = results_of(&events, "ExitWorktree");
    assert!(
        exits.first().is_some_and(|(_, _, failed)| !failed),
        "{exits:?}"
    );
    assert_removed(&run, &tree, "feat-a");
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn removing_a_worktree_with_changes_is_refused_until_discard_is_given() -> anyhow::Result<()>
{
    let (mut run, session) = start("worktree-dirty", true).await?;
    let token = format!("FEATURE-{}", nonce());
    let tree = enter_and_write(&mut run, &session, "feat-d", &token).await?;
    let events = run
        .turn(
            &session,
            "Call ExitWorktree with action remove and no discard_changes. If that is refused, \
             call ExitWorktree again with action remove and discard_changes true. Reply DONE.",
        )
        .await?;
    let exits = results_of(&events, "ExitWorktree");
    eprintln!("[live worktree-dirty] exits: {exits:?}");
    assert!(
        exits
            .first()
            .is_some_and(|(_, text, failed)| *failed && text.contains("uncommitted")),
        "a removal with changes in it must be refused first: {exits:?}"
    );
    assert!(
        exits.last().is_some_and(|(_, _, failed)| !failed),
        "{exits:?}"
    );
    assert_removed(&run, &tree, "feat-d");
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn keeping_a_worktree_leaves_it_on_disk_and_the_session_can_enter_another(
) -> anyhow::Result<()> {
    let (mut run, session) = start("worktree-keep", true).await?;
    let token = format!("KEPT-{}", nonce());
    let events = run
        .turn(
            &session,
            &format!(
                "Do these steps in order:\n\
                 1. Call EnterWorktree with slug keep-a.\n\
                 2. With the Write tool, write kept.txt containing {token} inside that worktree \
                 (<the absolute worktree path>/kept.txt).\n\
                 3. Call ExitWorktree with action keep.\n\
                 4. Call EnterWorktree with slug next-b. Do not retry it if it fails.\n\
                 Reply with what step 4 returned."
            ),
        )
        .await?;
    let kept = run.project.join(".agix/worktrees/keep-a");
    assert_eq!(
        std::fs::read_to_string(kept.join("kept.txt"))
            .ok()
            .map(|s| s.trim().to_string()),
        Some(token)
    );
    let entered = results_of(&events, "EnterWorktree");
    eprintln!("[live worktree-keep] enters: {entered:?}");
    assert!(
        entered.len() >= 2 && !entered[1].2,
        "once exited, the session can enter another worktree: {entered:?}"
    );
    assert!(run.project.join(".agix/worktrees/next-b").exists());
    assert!(kept.exists(), "keep leaves the worktree on disk");
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_worktree_outside_a_git_repository_is_refused() -> anyhow::Result<()> {
    let (mut run, session) = start("no-git", false).await?;
    let events = run
        .turn(
            &session,
            "Call EnterWorktree with slug try-it. If it fails, do not try anything else; reply \
             with the error.",
        )
        .await?;
    let entered = results_of(&events, "EnterWorktree");
    assert!(
        !entered.is_empty() && entered.iter().all(|(_, _, failed)| *failed),
        "{entered:?}"
    );
    assert!(!run.project.join(".agix/worktrees").exists());
    run.finish().await;
    Ok(())
}

fn notebook() -> Value {
    let cell = |kind: &str, source: &str| {
        let mut c = json!({ "cell_type": kind, "metadata": {}, "source": [source] });
        if kind == "code" {
            c["outputs"] = json!([]);
            c["execution_count"] = Value::Null;
        }
        c
    };
    json!({
        "cells": [cell("markdown", "# Title"), cell("code", "x = 1"), cell("code", "print('old')")],
        "metadata": { "kernelspec": { "name": "python3", "display_name": "Python 3" } },
        "nbformat": 4,
        "nbformat_minor": 5
    })
}

fn source(cell: &Value) -> String {
    match &cell["source"] {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts.iter().filter_map(|p| p.as_str()).collect(),
        _ => String::new(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn notebook_edit_replaces_inserts_and_deletes_cells_and_refuses_a_missing_one(
) -> anyhow::Result<()> {
    let (mut run, session) = start("notebook", false).await?;
    let id = nonce();
    let (replaced, inserted) = (format!("REPLACED-{id}"), format!("INSERTED-{id}"));
    let path = run.write_file(
        "analysis.ipynb",
        &serde_json::to_string_pretty(&notebook())?,
    )?;
    let events = run
        .turn(
            &session,
            &format!(
                "Edit the notebook {} with the NotebookEdit tool, one call at a time, cell \
                 indexes counted from 0:\n\
                 1. mode edit, cell_index 2, new_source `print(\"{replaced}\")`.\n\
                 2. mode insert, cell_index 1, cell_type markdown, new_source `{inserted}`.\n\
                 3. mode delete, cell_index 0.\n\
                 4. mode edit, cell_index 9, new_source `nope`. It should fail; do not retry it \
                 and do not change the notebook any other way.\nReply DONE.",
                path.display()
            ),
        )
        .await?;
    let edits = results_of(&events, "NotebookEdit");
    eprintln!("[live notebook] {edits:?}");
    assert!(edits.len() >= 4, "{edits:?}");
    assert!(
        edits
            .iter()
            .any(|(_, text, failed)| *failed && text.contains("out of range")),
        "{edits:?}"
    );
    let book: Value = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
    let cells = book["cells"].as_array().cloned().unwrap_or_default();
    let shape: Vec<(String, String)> = cells
        .iter()
        .map(|c| {
            (
                c["cell_type"].as_str().unwrap_or_default().to_string(),
                source(c),
            )
        })
        .collect();
    assert_eq!(
        shape,
        vec![
            ("markdown".to_string(), inserted.clone()),
            ("code".to_string(), "x = 1".to_string()),
            ("code".to_string(), format!("print(\"{replaced}\")")),
        ],
        "{book}"
    );
    assert_eq!(book["nbformat"], 4, "{book}");
    assert_eq!(book["metadata"]["kernelspec"]["name"], "python3", "{book}");
    assert!(
        cells[1..].iter().all(|c| c["outputs"].is_array()),
        "code cells stay valid: {book}"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn glob_and_grep_find_unicode_names_and_leave_binary_files_out() -> anyhow::Result<()> {
    let (mut run, session) = start("search", false).await?;
    let id = nonce();
    let (uni, bin) = (format!("UNI-{id}"), format!("BINMARK-{id}"));
    let name = format!("说明 ü-{id}.md");
    run.write_file(&format!("docs/{name}"), &format!("# 说明\n{uni}\n"))?;
    run.write_file("notes/plain.txt", &format!("{bin}\n"))?;
    let mut blob = vec![0u8, 1, 2, 3];
    blob.extend_from_slice(bin.as_bytes());
    blob.extend_from_slice(&[0u8; 64]);
    std::fs::create_dir_all(run.project.join("bin"))?;
    std::fs::write(run.project.join("bin/blob.dat"), &blob)?;

    let events = run
        .turn(
            &session,
            &format!(
                "Without Bash, one call at a time:\n\
                 1. Glob with pattern **/*.md.\n\
                 2. Grep for {uni} with output_mode files_with_matches.\n\
                 3. Grep for {bin} with output_mode files_with_matches.\n\
                 Reply with the paths each one found."
            ),
        )
        .await?;
    let globbed = results_of(&events, "Glob");
    assert!(
        globbed
            .iter()
            .any(|(_, t, failed)| !failed && t.contains(&name)),
        "{globbed:?}"
    );
    let grepped = results_of(&events, "Grep");
    eprintln!("[live search] {grepped:?}");
    assert!(
        grepped.iter().any(|(_, t, _)| t.contains(&name)),
        "the unicode name came back whole: {grepped:?}"
    );
    let binary = grepped
        .iter()
        .find(|(_, t, _)| t.contains("plain.txt"))
        .expect("the text file with the binary file's mark");
    assert!(!binary.1.contains("blob.dat"), "{binary:?}");
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn grep_finds_a_match_at_the_end_of_a_large_file() -> anyhow::Result<()> {
    let (mut run, session) = start("search-big", false).await?;
    let deep = format!("DEEP-{}", nonce());
    let mut log: String = (0..120_000)
        .map(|i| format!("{i:08} routine reading, all values nominal\n"))
        .collect();
    log.push_str(&format!("{deep}\n"));
    run.write_file("big/huge.log", &log)?;
    let events = run
        .turn(
            &session,
            &format!(
                "Grep for {deep} with output_mode files_with_matches, without Bash. Reply with \
                 what it found."
            ),
        )
        .await?;
    let grepped = results_of(&events, "Grep");
    assert!(
        grepped.iter().any(|(_, t, _)| t.contains("huge.log")),
        "a {} MB file with the match: {grepped:?}",
        log.len() / 1_000_000
    );
    run.finish().await;
    Ok(())
}
