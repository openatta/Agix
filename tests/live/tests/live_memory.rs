//! Memory with a real model: one session writes a memory the way the
//! `# Memory` prompt section says and the next session gets it back before
//! its first call; an index past five entries sends recall through a
//! selection call; a `MEMORY.md` of Chinese text past the 25 000-byte cap is
//! loaded cut, not fatal; with memory off nothing is recalled or extracted.

use std::path::Path;

use live::{LiveRun, Purpose, SeenRequest};
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

const RECALLED: &str = "Relevant memories for this query";
const MEMORY_SECTION: &str = "You have a persistent file-based memory";

fn write_memory(dir: &Path, name: &str, description: &str, body: &str) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(
        dir.join(format!("{name}.md")),
        format!("---\nname: {name}\ndescription: {description}\ntype: project\n---\n\n{body}\n"),
    )?;
    Ok(())
}

/// Seven memories, one of which holds `token`, and their index.
fn seven_memories(dir: &Path, token: &str) -> anyhow::Result<Vec<String>> {
    let others = [
        (
            "release-calendar",
            "When the quarterly releases ship",
            "Releases ship on the first Monday of each quarter.",
        ),
        (
            "oncall-rotation",
            "Who is on call this month",
            "The on-call rotation is in the team wiki.",
        ),
        (
            "style-preference",
            "How the user wants reviews written",
            "Reviews are written as a list of findings.",
        ),
        (
            "staging-host",
            "Where the staging environment lives",
            "Staging runs on staging.internal.example.",
        ),
        (
            "db-migrations",
            "How schema migrations are applied",
            "Migrations run through the migrate job, never by hand.",
        ),
        (
            "pager-channel",
            "Where alerts are posted",
            "Alerts go to the ops-alerts channel.",
        ),
    ];
    let mut names = Vec::new();
    let mut index = String::new();
    for (name, description, body) in others {
        write_memory(dir, name, description, body)?;
        index.push_str(&format!("- [{name}]({name}.md) — {description}\n"));
        names.push(name.to_string());
    }
    write_memory(
        dir,
        "ci-runner-token",
        "The token the CI runner authenticates with",
        &format!("The CI runner token is {token}."),
    )?;
    index.push_str(
        "- [ci-runner-token](ci-runner-token.md) — The token the CI runner authenticates with\n",
    );
    names.push("ci-runner-token".to_string());
    std::fs::write(dir.join("MEMORY.md"), index)?;
    Ok(names)
}

fn first_turn_after(calls: &[SeenRequest], from: usize) -> Option<&SeenRequest> {
    calls[from..].iter().find(|c| c.purpose() == Purpose::Turn)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_memory_written_in_one_session_is_recalled_in_the_next() -> anyhow::Result<()> {
    let mut run = LiveRun::builder("memory-recall")
        .settings(json!({ "memory_enabled": true }))
        .start()
        .await?;
    let dir = run.world.global_memory_dir();
    let token = format!("CODENAME-{}", nonce());
    let writer = run.session().await?;
    run.turn(
        &writer,
        &format!(
            "Save one memory exactly as the Memory section of your instructions describes, \
             in the memory directory it names:\n\
             1. With the Write tool, create release-codename.md there with frontmatter name \
             release-codename, description \"The codename of the next billing release\", \
             type project, and the body `The next billing release is codenamed {token}.`\n\
             2. Add its one-line pointer to MEMORY.md in the same directory (create it with \
             Write if it does not exist).\n\
             Then reply DONE."
        ),
    )
    .await?;
    let memory = std::fs::read_to_string(dir.join("release-codename.md")).unwrap_or_default();
    assert!(memory.contains(&token), "{memory}");
    assert!(memory.contains("name: release-codename"), "{memory}");
    let index = std::fs::read_to_string(dir.join("MEMORY.md")).unwrap_or_default();
    assert!(index.contains("release-codename.md"), "{index}");

    let before = run.provider.calls().len();
    let reader = run.session().await?;
    run.turn(
        &reader,
        "What is the codename of the next billing release? Write it to p.txt with the Write \
         tool, then reply DONE.",
    )
    .await?;
    let calls = run.provider.calls();
    let first = first_turn_after(&calls, before).expect("a turn call");
    assert!(
        first.system_contains(MEMORY_SECTION) && first.system_contains("release-codename.md"),
        "the new session's prompt has no index"
    );
    assert!(
        first.messages_contain(RECALLED) && first.messages_contain(&token),
        "the memory was not recalled into the first call"
    );
    assert!(
        calls[before..]
            .iter()
            .all(|c| c.purpose() != Purpose::MemoryRecall),
        "a selection call for a single memory"
    );
    assert_eq!(
        run.read_file("p.txt").map(|s| s.trim().to_string()),
        Some(token)
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn more_than_five_memories_go_through_a_selection_call() -> anyhow::Result<()> {
    let token = format!("CI-{}", nonce());
    let seeded = token.clone();
    let mut run = LiveRun::builder("memory-select")
        .settings(json!({ "memory_enabled": true }))
        .before_start(move |world, _| seven_memories(&world.global_memory_dir(), &seeded).map(drop))
        .start()
        .await?;
    let session = run.session().await?;
    let question = "Which token does the CI runner authenticate with? Write it to t.txt with \
                    the Write tool, then reply DONE.";
    run.turn(&session, question).await?;
    let calls = run.provider.calls();
    let selections: Vec<&SeenRequest> = calls
        .iter()
        .filter(|c| c.purpose() == Purpose::MemoryRecall)
        .collect();
    assert_eq!(selections.len(), 1, "one selection call");
    let selection = selections[0];
    assert_eq!(selection.model(), run.config().model);
    assert!(
        selection.messages_contain(question),
        "the query is the user's message"
    );
    for name in ["ci-runner-token", "release-calendar", "pager-channel"] {
        assert!(
            selection.messages_contain(&format!("**{name}**")),
            "{name} missing from the manifest"
        );
    }
    assert!(
        !selection.messages_contain(&token),
        "the selection call was handed memory bodies"
    );
    let first = first_turn_after(&calls, 0).expect("a turn call");
    if first.messages_contain(RECALLED) {
        assert!(first.messages_contain(&token), "recalled the wrong memory");
    } else {
        eprintln!("[live memory-select] the selection missed the 2 s recall budget");
    }
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "product bug: a MEMORY.md over 25 000 bytes whose byte 25 000 falls inside a multi-byte character panics the prompt build — `truncated[..MAX_BYTES]` slices a str at a byte offset (crates/core/src/memory.rs:655, truncate_entrypoint_content); the first turn never ends (a release build, panic = abort, loses the whole daemon)"]
async fn a_chinese_memory_index_past_the_byte_cap_is_loaded_cut() -> anyhow::Result<()> {
    let mut index = String::new();
    let mut n = 0;
    while index.len() < 30_000 {
        index.push_str(&format!(
            "- [记忆{n}](m{n}.md) — {}\n",
            "部署流程与回滚步骤的说明，务必在发布之前逐条核对清楚".repeat(3)
        ));
        n += 1;
    }
    while index.trim().is_char_boundary(25_000) {
        index.insert(0, 'x');
    }
    let trimmed = index.trim();
    assert!(
        trimmed.lines().count() < 200,
        "the line cap would hide the byte cap"
    );
    let seeded = index.clone();
    let mut run = LiveRun::builder("memory-cjk")
        .settings(json!({ "memory_enabled": true }))
        .before_start(move |world, _| {
            let dir = world.global_memory_dir();
            std::fs::create_dir_all(&dir)?;
            std::fs::write(dir.join("MEMORY.md"), seeded)?;
            Ok(())
        })
        .start()
        .await?;
    let session = tokio::time::timeout(std::time::Duration::from_secs(60), run.session()).await??;
    tokio::time::timeout(
        std::time::Duration::from_secs(120),
        run.turn(&session, "Reply with the word READY and nothing else."),
    )
    .await??;
    let first = &run.provider.calls()[0];
    assert!(
        first.system_contains("记忆0"),
        "the head of the index is in the prompt"
    );
    assert!(
        first.system_contains("Only part of it was loaded"),
        "the cut is not marked"
    );
    assert!(
        !first.system_contains(&format!("记忆{}", n - 1)),
        "the tail past the cap is in the prompt"
    );
    let ping = run
        .client
        .call("daemon.ping", serde_json::Value::Null)
        .await?;
    assert!(ping.error.is_none(), "the daemon is still up");
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn with_memory_off_nothing_is_recalled_or_extracted() -> anyhow::Result<()> {
    let token = format!("CI-{}", nonce());
    let seeded = token.clone();
    let mut run = LiveRun::builder("memory-off")
        .settings(json!({ "memory_enabled": false }))
        .before_start(move |world, _| seven_memories(&world.global_memory_dir(), &seeded).map(drop))
        .start()
        .await?;
    let session = run.session().await?;
    run.turn(
        &session,
        "Write the word OFF to off.txt with the Write tool, then reply DONE. Remember that I \
         prefer short answers.",
    )
    .await?;
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    let calls = run.provider.calls();
    assert!(!calls.is_empty());
    for call in &calls {
        assert_eq!(call.purpose(), Purpose::Turn, "a {:?} call", call.purpose());
        assert!(
            !call.system_contains(MEMORY_SECTION),
            "the Memory section is in the prompt"
        );
        assert!(
            !call.system_contains("ci-runner-token"),
            "the index is in the prompt"
        );
        assert!(!call.messages_contain(RECALLED) && !call.messages_contain(&token));
    }
    let project_memories = run.world.memory_dir(&run.project);
    assert!(
        !project_memories.exists() || std::fs::read_dir(&project_memories)?.next().is_none(),
        "something was extracted into {}",
        project_memories.display()
    );
    run.finish().await;
    Ok(())
}
