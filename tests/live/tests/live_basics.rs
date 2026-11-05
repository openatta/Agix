//! The single-agent surface with a real model: the file and shell tools, a
//! session's own prompt, a resume, plan mode, and a spawned `agixd`.

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
async fn the_file_and_shell_tools_change_the_project() -> anyhow::Result<()> {
    let mut run = LiveRun::start("tools", json!({}), Mode::InProcess).await?;
    let needle = format!("NEEDLE-{}", nonce());
    run.write_file("data/numbers.txt", "17\n25\n8\n50\n")?;
    run.write_file("src/alpha.rs", "fn alpha() {}\n")?;
    run.write_file("src/beta.rs", &format!("// {needle}\nfn beta() {{}}\n"))?;
    run.write_file("config.toml", "name = \"demo\"\nlevel = 1\n")?;

    let session = run.session().await?;
    let events = run
        .turn(
            &session,
            &format!(
                "Do these four things with your tools, then reply DONE:\n\
                 1. Read data/numbers.txt and write their sum (just the number) to sum.txt.\n\
                 2. Search src/ for the text {needle} and write the matching file's path, \
                 relative to the project root, to where.txt.\n\
                 3. In config.toml change `level = 1` to `level = 7` with the Edit tool.\n\
                 4. Run `wc -l < data/numbers.txt | tr -d ' ' > lines.txt` with Bash."
            ),
        )
        .await?;

    assert_eq!(
        run.read_file("sum.txt").map(|s| s.trim().to_string()),
        Some("100".into())
    );
    assert!(
        run.read_file("where.txt")
            .is_some_and(|s| s.trim().trim_start_matches("./") == "src/beta.rs"),
        "{:?}",
        run.read_file("where.txt")
    );
    assert!(run
        .read_file("config.toml")
        .is_some_and(|s| s.contains("level = 7") && s.contains("name = \"demo\"")));
    assert_eq!(
        run.read_file("lines.txt").map(|s| s.trim().to_string()),
        Some("4".into())
    );
    let used: Vec<&str> = events.tool_uses.iter().map(|(n, _)| n.as_str()).collect();
    for tool in ["Read", "Edit", "Bash"] {
        assert!(used.contains(&tool), "{tool} was not used: {used:?}");
    }
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sessions_prompt_and_history_hold_across_a_resume() -> anyhow::Result<()> {
    let mut run = LiveRun::start("resume", json!({}), Mode::InProcess).await?;
    let codeword = format!("ORCHID-{}", nonce());
    let signature = format!("SIG-{}", nonce());

    let session = run.session().await?;
    run.turn(
        &session,
        &format!("Remember this codeword for later: {codeword}. Reply only OK."),
    )
    .await?;

    let set = run
        .client
        .call(
            "session.setPrompt",
            json!({
                "session_id": session,
                "append": format!("End every reply with the exact line {signature}")
            }),
        )
        .await?;
    assert!(set.error.is_none(), "{set:?}");

    run.client
        .call("session.close", json!({ "session_id": session }))
        .await?;
    let resumed = run
        .client
        .call("session.resume", json!({ "session_id": session }))
        .await?;
    assert!(resumed.error.is_none(), "{resumed:?}");

    let events = run
        .turn(
            &session,
            "What was the codeword I gave you? Answer with it alone.",
        )
        .await?;
    assert!(events.text.contains(&codeword), "{}", events.text);
    let last = run.provider.calls().pop().expect("a call");
    assert!(
        last.system_text().contains(&signature),
        "the session's own prompt must survive the resume"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn plan_mode_leaves_the_project_untouched() -> anyhow::Result<()> {
    let mut run = LiveRun::start(
        "plan",
        json!({ "permission_mode": "plan" }),
        Mode::InProcess,
    )
    .await?;
    run.write_file("notes.txt", "keep me\n")?;
    let session = run.session().await?;
    let events = run
        .turn(
            &session,
            "Overwrite notes.txt with the word GONE and create made.txt containing MADE. \
             If you cannot, say why in one sentence.",
        )
        .await?;
    assert_eq!(run.read_file("notes.txt").as_deref(), Some("keep me\n"));
    assert!(run.read_file("made.txt").is_none(), "{}", events.text);
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_spawned_agixd_works_with_the_real_model() -> anyhow::Result<()> {
    let mut run = LiveRun::start("spawned", json!({}), Mode::Spawned).await?;
    let secret = format!("PEBBLE-{}", nonce());
    run.write_file("secret.txt", &format!("{secret}\n"))?;
    let session = run.session().await?;
    let events = run
        .turn(&session, "Read secret.txt and reply with its content only.")
        .await?;
    assert!(events.text.contains(&secret), "{}", events.text);
    assert!(
        run.provider
            .calls()
            .iter()
            .all(|c| c.model() == run.config().model),
        "every request must ask for the configured model"
    );
    run.finish().await;
    Ok(())
}
