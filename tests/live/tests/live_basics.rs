//! The single-agent surface with a real model: the file and shell tools, web
//! search, a monitor whose events reach the model while it works, a session's
//! own prompt, a resume, plan mode, and a spawned `agixd`.

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
                 1. Read data/numbers.txt with the Read tool and write their sum (just the number) to sum.txt.\n\
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

#[tokio::test(flavor = "multi_thread")]
async fn web_search_is_the_engines_tool_searching_through_the_sessions_model() -> anyhow::Result<()>
{
    let mut run = LiveRun::start("search", json!({}), Mode::InProcess).await?;
    let session = run.session().await?;
    let events = run
        .turn(
            &session,
            "Use the WebSearch tool to find the Rust 1.80.0 release announcement, then reply \
             with its release date in the form YYYY-MM-DD and nothing else.",
        )
        .await?;

    assert!(
        events.tool_uses.iter().any(|(name, _)| name == "WebSearch"),
        "the host saw the search as a tool call: {:?}",
        events.tool_uses
    );
    let (_, found, failed) = events
        .tool_results
        .iter()
        .find(|(name, _, _)| name == "WebSearch")
        .cloned()
        .unwrap_or_default();
    assert!(!failed && found.contains("http"), "{found}");
    assert!(events.text.contains("2024-07-25"), "{}", events.text);

    let calls = run.provider.calls();
    let is_provider_search = |tool: &serde_json::Value| {
        tool["type"]
            .as_str()
            .is_some_and(|t| t.starts_with("web_search_"))
    };
    let (searches, turn_calls): (Vec<_>, Vec<_>) = calls.iter().partition(|c| {
        c.body["tools"]
            .as_array()
            .is_some_and(|tools| tools.iter().any(is_provider_search))
    });
    assert!(!searches.is_empty(), "the search went to the provider");
    for search in &searches {
        assert_eq!(search.body["tools"].as_array().map(Vec::len), Some(1));
        assert_eq!(
            search.model(),
            run.config().model,
            "the session's own model"
        );
    }
    for call in &turn_calls {
        let names: Vec<&str> = call.body["tools"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|t| t["name"].as_str())
            .collect();
        assert!(names.contains(&"WebSearch") && !names.contains(&"web_search"));
    }
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_monitors_events_reach_the_model_while_it_works() -> anyhow::Result<()> {
    let mut run = LiveRun::start("watch", json!({}), Mode::InProcess).await?;
    let code = format!("E-{}", nonce());
    run.write_file("app.log", "")?;
    run.write_file(
        "deploy.sh",
        &format!(
            "sleep 1\necho 'INFO deploy started' >> app.log\nsleep 1\n\
             echo 'ERROR {code} payment gateway timeout' >> app.log\nsleep 1\n\
             echo 'INFO deploy finished' >> app.log\nsleep 1\necho deployed\n"
        ),
    )?;
    let session = run.session().await?;
    let watcher = run.watch(&session, false).await?;

    let events = run
        .turn(
            &session,
            "1. Start a Monitor with persistent true, description \"errors in app.log\" and \
             command `tail -n 0 -F app.log | grep --line-buffered ERROR`.\n\
             2. Then run `sh deploy.sh` with the Bash tool; it takes a few seconds.\n\
             3. When the monitor reports an ERROR line, write its error code (the word that \
             starts with E-) to incident.txt with the Write tool.\n\
             4. Stop the monitor with TaskStop, then reply with the error code.",
        )
        .await?;
    let monitor_at = events.tool_uses.iter().position(|(n, _)| n == "Monitor");
    let deploy_at = events.tool_uses.iter().position(|(n, input)| {
        n == "Bash"
            && input["command"]
                .as_str()
                .is_some_and(|c| c.contains("deploy.sh"))
    });
    assert!(
        monitor_at.is_some() && monitor_at < deploy_at,
        "the monitor returned at once and the work went on: {:?}",
        events.tool_uses
    );
    assert_eq!(
        run.read_file("incident.txt").map(|s| s.trim().to_string()),
        Some(code.clone())
    );
    assert!(events.text.contains(&code), "{}", events.text);

    let told = run.provider.calls().iter().position(|c| {
        let text = c.messages_text();
        text.contains("<kind>monitor</kind>") && text.contains(&code)
    });
    assert!(
        told.is_some(),
        "the event reached the model as a notification"
    );

    let tasks = run.tasks(&session).await?;
    let monitor = tasks
        .iter()
        .find(|t| t["kind"] == "monitor")
        .cloned()
        .unwrap_or_default();
    assert_eq!(monitor["status"], "stopped", "TaskStop ended it: {tasks:?}");
    let task_id = monitor["task_id"].clone();
    assert!(
        watcher
            .events("task_progress")
            .iter()
            .any(|f| f["event"]["task_id"] == task_id
                && f["event"]["appended"]
                    .as_str()
                    .is_some_and(|a| a.contains(&code))),
        "the host saw the error line as it came"
    );
    drop(watcher);
    run.finish().await;
    Ok(())
}
