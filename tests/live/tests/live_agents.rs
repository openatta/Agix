//! Sub-agents with a real model: foreground ones on their own models in
//! parallel, a foreground one whose question reaches the host inside its
//! parent's turn, a background one a host talks to while it runs, and a
//! sleep loop that would wait out a background one blind, turned towards
//! `TaskWait`. An agent type's frontmatter bounds its sub-agent (tools,
//! calls, permission mode), a chain of sub-agents ends where the depth limit
//! withholds `Agent`, and `max_concurrent_subagents` refuses one more.

use std::time::Duration;

use live::{LiveRun, Mode};
use serde_json::json;
use serde_json::Value;

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
            "Wait for a message from the operator: run `sleep 1` with Bash, again and \
             again, up to 200 times, until a message arrives. The message contains a word in \
             capitals with a dash and digits. Write exactly that word to word.txt with the \
             Write tool, then answer with the word and stop.",
        ),
    )?;

    let session = run.session().await?;
    let _host = run.watch(&session, true).await?;
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

#[tokio::test(flavor = "multi_thread")]
async fn a_foreground_sub_agents_question_reaches_the_host_inside_its_parents_turn(
) -> anyhow::Result<()> {
    let mut run = LiveRun::start(
        "asker",
        json!({
            "permission_mode": "default",
            "permission_rules": [{ "tool": "Agent", "action": "allow" }]
        }),
        Mode::InProcess,
    )
    .await?;
    let stamp = format!("STAMP-{}", nonce());
    run.write_file(
        "check.sh",
        &format!("echo {stamp} > stamp.txt\necho {stamp}\n"),
    )?;
    let session = run.session().await?;
    let watcher = run.watch(&session, true).await?;

    let events = run
        .turn(
            &session,
            "Delegate this, do not run anything yourself: call the Agent tool once, in the \
             foreground, with prompt \"Run `sh check.sh` with the Bash tool and answer with \
             exactly what it printed.\" Then reply with what the sub-agent reported.",
        )
        .await?;
    assert!(events.text.contains(&stamp), "{}", events.text);
    assert_eq!(
        run.read_file("stamp.txt").map(|s| s.trim().to_string()),
        Some(stamp)
    );

    let tasks = run.tasks(&session).await?;
    let agent = tasks
        .iter()
        .find(|t| t["kind"] == "agent" && t["detail"]["background"] == false)
        .cloned()
        .unwrap_or_default();
    assert_eq!(agent["status"], "done", "{tasks:?}");
    let task_id = agent["task_id"].clone();
    let sidechain = agent["detail"]["agent_session_id"]
        .as_str()
        .unwrap_or_default()
        .to_string();

    let asked: Vec<_> = watcher
        .asked()
        .into_iter()
        .filter(|a| {
            a.prompt["event"]["message"]
                .as_str()
                .is_some_and(|m| m.starts_with("[sub-agent "))
        })
        .collect();
    assert!(!asked.is_empty(), "running a command asks in default mode");
    for question in &asked {
        assert_eq!(
            question.prompt["turn_id"], "asker-t1",
            "asked while the parent's turn ran"
        );
        assert!(question.prompt["event"]["message"]
            .as_str()
            .is_some_and(|m| m.starts_with(&format!("[sub-agent {sidechain}]"))));
        let then = question
            .tasks
            .iter()
            .find(|t| t["task_id"] == task_id)
            .cloned()
            .unwrap_or_default();
        assert_eq!(then["status"], "waiting", "{then}");
        assert!(then["waiting_for"]
            .as_array()
            .is_some_and(|ids| ids.contains(&question.prompt["event"]["prompt_id"])));
    }

    let states: Vec<Value> = watcher
        .events("task_updated")
        .into_iter()
        .filter(|f| f["event"]["task"]["task_id"] == task_id)
        .map(|f| f["event"]["task"]["status"].clone())
        .collect();
    assert_eq!(
        states.last(),
        Some(&json!("running")),
        "back to running once answered: {states:?}"
    );
    let progress = watcher.events("subagent_progress");
    assert!(progress
        .iter()
        .all(|f| f["event"]["task_id"] == task_id && f["turn_id"] == "asker-t1"));
    assert!(progress.iter().any(
        |f| f["event"]["event"]["kind"] == "tool_use" && f["event"]["event"]["name"] == "Bash"
    ));
    drop(watcher);
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sleep_loop_waiting_on_a_background_agent_is_turned_towards_task_wait(
) -> anyhow::Result<()> {
    let mut run = LiveRun::start("waiter", json!({}), Mode::InProcess).await?;
    let token = format!("KESTREL-{}", nonce());
    run.write_file(
        "slow.sh",
        &format!("i=0\nwhile [ $i -lt 6 ]; do i=$((i+1)); sleep 2; done\necho {token}\n"),
    )?;
    run.write_file(
        ".agix/agents/slowpoke.md",
        &agent_type(
            "Runs a slow script",
            run.config().model_at(0),
            "Run `sh slow.sh` with Bash and answer with exactly what it prints.",
        ),
    )?;

    let session = run.session().await?;
    let _host = run.watch(&session, true).await?;
    let events = run
        .turn(
            &session,
            "Start one background sub-agent with the Agent tool: subagent_type slowpoke, \
             background true, description slow, prompt \"Run the script.\". Then wait for it \
             to finish by running exactly this Bash command: \
             `for i in 1 2 3 4 5 6 7 8; do sleep 5; done`. If that command is refused, do what \
             the refusal says instead. Once the sub-agent has finished, reply with exactly what \
             it reported.",
        )
        .await?;
    let refused = events
        .tool_results
        .iter()
        .find(|(n, out, _)| n == "Bash" && out.contains("TaskWait"));
    eprintln!("[live waiter] refusal: {refused:?}");
    assert!(
        refused.is_some(),
        "the sleep loop was refused with a pointer to TaskWait: {:?}",
        events.tool_results
    );
    assert!(
        events.tool_uses.iter().any(|(n, _)| n == "TaskWait"),
        "{:?}",
        events.tool_uses
    );
    assert!(events.text.contains(&token), "{}", events.text);
    run.finish().await;
    Ok(())
}

fn typed(front: &str, body: &str) -> String {
    format!("---\n{front}\n---\n{body}")
}

fn requests_of(run: &LiveRun, marker: &str) -> Vec<live::SeenRequest> {
    run.provider
        .calls()
        .into_iter()
        .filter(|c| c.has_tools() && (c.system_contains(marker) || c.messages_contain(marker)))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn an_agent_types_allowed_tools_and_max_turns_bound_its_sub_agent() -> anyhow::Result<()> {
    let mut run = LiveRun::start("typed", json!({}), Mode::InProcess).await?;
    let model = run.config().model_at(0).to_string();
    for (i, word) in ["one", "two", "three", "four"].iter().enumerate() {
        run.write_file(&format!("{word}.txt"), &format!("word {}\n", i + 1))?;
    }
    run.write_file(
        ".agix/agents/reader.md",
        &typed(
            &format!("description: Reads only\nmodel: {model}\nallowed_tools: [Read]"),
            "READER-ROLE. Write the word WRITTEN to reader.txt with the Write tool if you have \
             it. If you have no tool that writes, answer CANNOT and stop.",
        ),
    )?;
    run.write_file(
        ".agix/agents/brief.md",
        &typed(
            &format!("description: Short on calls\nmodel: {model}\nmax_turns: 2"),
            "BRIEF-ROLE. Read one.txt, two.txt, three.txt and four.txt with the Read tool, \
             exactly one call per message, then answer with their words.",
        ),
    )?;
    let session = run.session().await?;
    let events = run
        .turn(
            &session,
            "Call the Agent tool twice, one after the other and in the foreground: \
             subagent_type reader with prompt \"Go.\", then subagent_type brief with prompt \
             \"Go.\". Do nothing else yourself; then reply with what each reported.",
        )
        .await?;
    assert!(
        events
            .tool_uses
            .iter()
            .filter(|(n, _)| n == "Agent")
            .count()
            >= 2,
        "{:?}",
        events.tool_uses
    );

    assert!(run.read_file("reader.txt").is_none());
    let reader = requests_of(&run, "READER-ROLE");
    assert!(!reader.is_empty(), "no request carried the reader's body");
    for call in &reader {
        let tools = call.tool_names();
        assert!(
            !tools
                .iter()
                .any(|t| t == "Write" || t == "Bash" || t == "Edit"),
            "allowed_tools [Read] offered more: {tools:?}"
        );
    }

    let brief = requests_of(&run, "BRIEF-ROLE");
    eprintln!("[live typed] brief made {} calls", brief.len());
    assert!(
        !brief.is_empty() && brief.len() <= 2,
        "max_turns 2 caps its calls: {}",
        brief.len()
    );
    run.finish().await;
    Ok(())
}

/// A `default` agent type under an `acceptEdits` parent is asked about a read
/// outside the project — and its question waits the session's prompt timeout,
/// not a fixed one.
#[tokio::test(flavor = "multi_thread")]
async fn an_agent_types_permission_mode_governs_what_its_sub_agent_is_asked() -> anyhow::Result<()>
{
    let mut run = LiveRun::builder("typed-mode")
        .settings(json!({
            "permission_mode": "acceptEdits",
            "permission_rules": [{ "tool": "Agent", "action": "allow" }]
        }))
        .options(|o| o.prompt_timeout(Duration::from_secs(3)))
        .start()
        .await?;
    let model = run.config().model_at(0).to_string();
    let outside = run.world.root().join("outside");
    std::fs::create_dir_all(&outside)?;
    let notes = outside.join("notes.txt");
    std::fs::write(&notes, format!("NOTES-{}\n", nonce()))?;
    run.write_file(
        ".agix/agents/careful.md",
        &typed(
            &format!("description: Reads one file\nmodel: {model}\npermission_mode: default"),
            &format!(
                "CAREFUL-ROLE. Read {} with the Read tool and answer with its contents. If \
                 the read is refused, answer REFUSED and stop; do not try another way.",
                notes.display()
            ),
        ),
    )?;
    let session = run.session().await?;
    let watcher = run.watch(&session, false).await?;
    let started = std::time::Instant::now();
    run.turn(
        &session,
        "Call the Agent tool once in the foreground with subagent_type careful and prompt \
         \"Go.\". Do nothing else yourself; reply with what it reported.",
    )
    .await?;
    let asked: Vec<String> = watcher
        .asked()
        .iter()
        .map(|a| {
            a.prompt["event"]["tool_name"]
                .as_str()
                .unwrap_or("?")
                .to_string()
        })
        .collect();
    eprintln!(
        "[live typed-mode] asked {asked:?} in {:?}",
        started.elapsed()
    );
    assert!(
        asked.iter().any(|t| t == "Read"),
        "the `default` sub-agent was not asked about its read: {asked:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(200),
        "an unanswered sub-agent question waited past the session's 3 s: {:?}",
        started.elapsed()
    );
    drop(watcher);
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_chain_of_sub_agents_ends_where_the_depth_limit_withholds_the_agent_tool(
) -> anyhow::Result<()> {
    let mut run = LiveRun::start("chain", json!({}), Mode::InProcess).await?;
    run.write_file(
        ".agix/agents/relay.md",
        &agent_type(
            "Passes the work one level down",
            run.config().model_at(0),
            "RELAY-ROLE. Look at your own tool list. If it has a tool named Agent: run `echo \
             relay >> chain.log` with Bash, then call the Agent tool once, in the foreground, \
             with subagent_type relay and prompt \"Go on.\", and answer with what it answered. \
             If you have no tool named Agent: run `echo bottom >> chain.log` with Bash and \
             answer BOTTOM.",
        ),
    )?;
    let session = run.session().await?;
    let _host = run.watch(&session, true).await?;
    let events = run
        .turn(
            &session,
            "Call the Agent tool once, in the foreground, with subagent_type relay and prompt \
             \"Go on.\". Do nothing else; reply with what it answered.",
        )
        .await?;
    let chain: Vec<String> = run
        .read_file("chain.log")
        .unwrap_or_default()
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    eprintln!("[live chain] {chain:?}");
    assert_eq!(chain, ["relay", "relay", "bottom"], "{}", events.text);
    let relays = requests_of(&run, "RELAY-ROLE");
    assert!(
        relays
            .iter()
            .any(|c| !c.tool_names().iter().any(|t| t == "Agent")),
        "the deepest relay is never offered Agent"
    );
    run.finish().await;
    Ok(())
}

async fn host_agent(run: &mut LiveRun, session: &str) -> anyhow::Result<Option<i64>> {
    let resp = run
        .client
        .call(
            "session.startAgent",
            json!({ "session_id": session, "prompt": "Reply OK.", "description": "host" }),
        )
        .await?;
    Ok(resp.error.map(|e| e.code as i64))
}

#[tokio::test(flavor = "multi_thread")]
async fn max_concurrent_subagents_refuses_one_more_from_the_model_and_the_host(
) -> anyhow::Result<()> {
    let mut run = LiveRun::start(
        "slots",
        json!({ "execution": { "max_concurrent_subagents": 1 } }),
        Mode::InProcess,
    )
    .await?;
    run.write_file(
        "slow.sh",
        "i=0\nwhile [ $i -lt 10 ]; do i=$((i+1)); sleep 2; done\necho slow-done\n",
    )?;
    run.write_file(
        ".agix/agents/slowpoke.md",
        &agent_type(
            "Runs a slow script",
            run.config().model_at(0),
            "Run `sh slow.sh` with Bash and answer with exactly what it prints.",
        ),
    )?;
    let session = run.session().await?;
    let _host = run.watch(&session, true).await?;
    let events = run
        .turn(
            &session,
            "Start a background sub-agent with the Agent tool: subagent_type slowpoke, \
             background true, description first, prompt \"Run it.\". Right after, in your next \
             message, start a second one the same way with description second. If the second \
             is refused, do not retry and do not wait; reply with the refusal.",
        )
        .await?;
    let agents: Vec<&(String, String, bool)> = events
        .tool_results
        .iter()
        .filter(|(n, _, _)| n == "Agent")
        .collect();
    eprintln!("[live slots] {agents:?}");
    assert!(
        agents
            .iter()
            .any(|(_, t, _)| t.contains("max_concurrent_subagents")),
        "the second spawn was refused for the limit: {agents:?}"
    );
    let running = run
        .tasks(&session)
        .await?
        .into_iter()
        .filter(|t| t["kind"] == "agent" && t["status"] == "running")
        .count();
    assert_eq!(running, 1);

    assert_eq!(
        host_agent(&mut run, &session).await?,
        Some(-32019),
        "TASK_LIMIT while the first runs"
    );
    run.wait_tasks_settled(&session, Duration::from_secs(300))
        .await?;
    assert_eq!(
        host_agent(&mut run, &session).await?,
        None,
        "a slot is free again"
    );
    run.wait_tasks_settled(&session, Duration::from_secs(120))
        .await?;
    run.finish().await;
    Ok(())
}
