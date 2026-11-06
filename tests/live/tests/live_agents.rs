//! Sub-agents with a real model: foreground ones on their own models in
//! parallel, and a background one a host talks to while it runs.

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

fn agent_type(description: &str, model: &str, body: &str) -> String {
    format!("---\ndescription: {description}\nmodel: {model}\n---\n{body}")
}

#[tokio::test(flavor = "multi_thread")]
async fn foreground_sub_agents_run_on_the_models_their_types_name() -> anyhow::Result<()> {
    let mut run = LiveRun::start("readers", json!({}), Mode::InProcess).await?;
    let (left_model, right_model) = (
        run.config().model_at(0).to_string(),
        run.config().model_at(1).to_string(),
    );
    let (left, right) = (format!("LEFT-{}", nonce()), format!("RIGHT-{}", nonce()));
    run.write_file("left.txt", &format!("{left}\n"))?;
    run.write_file("right.txt", &format!("{right}\n"))?;
    let body = "Read the file you are told to read and answer with its content only.";
    run.write_file(
        ".agix/agents/left-reader.md",
        &agent_type("Reads one file", &left_model, body),
    )?;
    run.write_file(
        ".agix/agents/right-reader.md",
        &agent_type("Reads one file", &right_model, body),
    )?;

    let session = run.session().await?;
    let events = run
        .turn(
            &session,
            "In a single message, call the Agent tool twice: subagent_type left-reader with \
             prompt \"Read left.txt\", and subagent_type right-reader with prompt \"Read \
             right.txt\". Do not read the files yourself. Then reply with both answers.",
        )
        .await?;

    let agents = events
        .tool_uses
        .iter()
        .filter(|(n, _)| n == "Agent")
        .count();
    assert!(agents >= 2, "{:?}", events.tool_uses);
    assert!(
        events.text.contains(&left) && events.text.contains(&right),
        "{}",
        events.text
    );
    let calls = run.provider.calls();
    for model in [&left_model, &right_model] {
        assert!(
            calls.iter().any(|c| c.model() == model),
            "no request asked for {model}"
        );
    }
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_background_agent_takes_a_message_and_reports_back() -> anyhow::Result<()> {
    let mut run = LiveRun::start("listener", json!({}), Mode::InProcess).await?;
    let word = format!("LANTERN-{}", nonce());
    run.write_file(
        ".agix/agents/listener.md",
        &agent_type(
            "Waits for an operator message",
            run.config().model_at(0),
            "Wait for a message from the operator: run `sleep 5` with Bash, again and \
             again, up to 40 times, until a message arrives. The message contains a word in \
             capitals with a dash and digits. Write exactly that word to word.txt with the \
             Write tool, then answer with the word and stop.",
        ),
    )?;

    let session = run.session().await?;
    run.turn(
        &session,
        "Start one background sub-agent with the Agent tool: subagent_type listener, \
         background true, description listener, prompt \"Wait for the operator.\". \
         Then reply STARTED without waiting for it.",
    )
    .await?;

    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    let task_id = loop {
        let tasks = run.tasks(&session).await?;
        if let Some(t) = tasks
            .iter()
            .find(|t| t["detail"]["agent_type"] == "listener" && t["status"] == "running")
        {
            break t["task_id"].as_str().unwrap_or_default().to_string();
        }
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "the listener never started: {tasks:?}"
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    };
    tokio::time::sleep(Duration::from_secs(8)).await;
    let sent = run
        .client
        .call(
            "session.messageTask",
            json!({
                "session_id": session,
                "task_id": task_id,
                "message": format!("Operator here. The word is {word}.")
            }),
        )
        .await?;
    assert_eq!(sent.result.unwrap_or_default()["delivered"], json!(true));

    let tasks = run
        .wait_tasks_settled(&session, Duration::from_secs(300))
        .await?;
    assert!(
        tasks
            .iter()
            .any(|t| t["task_id"] == task_id.as_str() && t["status"] == "done"),
        "{tasks:?}"
    );
    assert_eq!(
        run.read_file("word.txt").map(|s| s.trim().to_string()),
        Some(word.clone())
    );

    let events = run
        .turn(
            &session,
            "Has the listener finished? Reply with the word it reported, nothing else.",
        )
        .await?;
    assert!(events.text.contains(&word), "{}", events.text);
    let last = run
        .provider
        .calls()
        .into_iter()
        .rfind(|c| c.messages_text().contains("Has the listener finished"));
    assert!(
        last.is_some_and(|c| c.messages_text().contains("<task-notification>")),
        "the finished listener must reach the coordinator as a notification"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_started_agent_hands_in_what_its_schema_asks_for() -> anyhow::Result<()> {
    let mut run = LiveRun::start("hosted", json!({}), Mode::InProcess).await?;
    let code = format!("KESTREL-{}", nonce());
    run.write_file("ticket.txt", &format!("ticket code: {code}\npriority: 4\n"))?;
    let session = run.session().await?;

    let started = run
        .client
        .call(
            "session.startAgent",
            json!({
                "session_id": session,
                "prompt": "Read ticket.txt, then hand in its code and priority with the \
                           StructuredOutput tool.",
                "description": "ticket",
                "tag": "live:ticket",
                "output_schema": {
                    "type": "object",
                    "required": ["code", "priority"],
                    "properties": {
                        "code": { "type": "string" },
                        "priority": { "type": "integer" }
                    }
                }
            }),
        )
        .await?;
    assert!(started.error.is_none(), "{started:?}");
    let task_id = started.result.unwrap_or_default()["task_id"]
        .as_str()
        .unwrap_or_default()
        .to_string();

    let tasks = run
        .wait_tasks_settled(&session, Duration::from_secs(300))
        .await?;
    let task = tasks
        .iter()
        .find(|t| t["task_id"] == task_id.as_str())
        .cloned()
        .unwrap_or_default();
    assert_eq!(task["status"], "done", "{task}");
    assert_eq!(task["detail"]["tag"], "live:ticket", "{task}");
    assert_eq!(task["structured_output"]["code"], code.as_str(), "{task}");
    assert_eq!(task["structured_output"]["priority"], 4, "{task}");

    let (calls, input, output) = run.provider.spend();
    assert!(calls >= 2, "it read the file, then handed in");
    assert_eq!(task["usage"]["output_tokens"], output, "{task}");
    assert_eq!(task["usage"]["input_tokens"], input, "{task}");
    run.finish().await;
    Ok(())
}
