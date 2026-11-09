//! What a session carries from one turn to the next, with a real model:
//! a compaction written by the model keeps the facts the conversation needs,
//! whether asked for or started on its own when a turn outgrows the
//! configured threshold, a fork shares the history before it and nothing after, and a session
//! created with an output schema ends its turn with a result that fits it. After a compaction
//! the model is reminded that plan mode is on and a background task still runs; a
//! compaction with nothing to drop leaves the context as it was; reactive compaction clears
//! the oldest reads of a long turn.

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
async fn a_compaction_summary_keeps_what_the_conversation_needs() -> anyhow::Result<()> {
    let mut run = LiveRun::start("compact", json!({}), Mode::InProcess).await?;
    let codeword = format!("HERON-{}", nonce());
    let filler: String = (0..400)
        .map(|i| format!("line {i}: routine status, nothing to report\n"))
        .collect();
    run.write_file("status.log", &filler)?;
    let session = run.session().await?;
    let watcher = run.watch(&session, false).await?;

    run.turn(
        &session,
        &format!(
            "The release codeword is {codeword}; I will ask for it later. For now, read \
             status.log with the Read tool and tell me in one sentence whether it reports \
             any problem."
        ),
    )
    .await?;
    run.turn(&session, "/compact").await?;
    let compacted = watcher
        .wait_for("a compact frame", Duration::from_secs(30), |e| {
            e["kind"] == "compact"
        })
        .await?;
    eprintln!("[live compact] {}", compacted["event"]);
    assert_eq!(
        compacted["event"]["strategy"], "FullCompact",
        "the model wrote the summary: {compacted}"
    );

    let events = run
        .turn(
            &session,
            "What is the release codeword? Answer with it alone, without using any tool.",
        )
        .await?;
    assert!(events.text.contains(&codeword), "{}", events.text);
    drop(watcher);
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fork_shares_the_history_before_it_and_nothing_after() -> anyhow::Result<()> {
    let mut run = LiveRun::start("fork", json!({}), Mode::InProcess).await?;
    let first = format!("AMBER-{}", nonce());
    let second = format!("COBALT-{}", nonce());
    let session = run.session().await?;
    run.turn(
        &session,
        &format!("Remember the codeword {first}. Reply only OK."),
    )
    .await?;

    let forked = run
        .client
        .call("session.fork", json!({ "session_id": session }))
        .await?;
    assert!(forked.error.is_none(), "{forked:?}");
    let fork = forked.result.unwrap_or_default()["session_id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(!fork.is_empty() && fork != session);

    run.turn(
        &session,
        &format!("Remember a second codeword too: {second}. Reply only OK."),
    )
    .await?;

    let ask = "List every codeword I have given you in this conversation, one per line, \
               and nothing else. Do not use any tool.";
    let on_fork = run.turn(&fork, ask).await?;
    assert!(on_fork.text.contains(&first), "{}", on_fork.text);
    assert!(
        !on_fork.text.contains(&second),
        "the fork never saw what came after it: {}",
        on_fork.text
    );
    let on_source = run.turn(&session, ask).await?;
    assert!(
        on_source.text.contains(&first) && on_source.text.contains(&second),
        "{}",
        on_source.text
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_with_an_output_schema_ends_with_a_result_that_fits_it() -> anyhow::Result<()> {
    let mut run = LiveRun::start("schema", json!({}), Mode::InProcess).await?;
    let reference = format!("INV-{}", nonce());
    run.write_file(
        "invoice.txt",
        &format!(
            "Invoice {reference}\n\nwidget   2 x 12.50\ngadget   1 x 30.00\nbolt    10 x  0.40\n\
             \nAll prices in EUR.\n"
        ),
    )?;
    let session = run
        .session_with(json!({
            "options": {
                "output_schema": {
                    "type": "object",
                    "required": ["reference", "total", "currency", "lines"],
                    "properties": {
                        "reference": { "type": "string" },
                        "total": { "type": "number" },
                        "currency": { "type": "string", "enum": ["EUR", "USD"] },
                        "lines": { "type": "integer" }
                    }
                }
            }
        }))
        .await?;
    let events = run
        .turn(
            &session,
            "Read invoice.txt and hand in its reference, the invoice total, the currency \
             and the number of item lines with the StructuredOutput tool.",
        )
        .await?;
    let output = events
        .turn_complete_event
        .as_ref()
        .map(|e| e["structured_output"].clone())
        .unwrap_or_default();
    eprintln!("[live schema] {output}");
    assert_eq!(output["reference"], reference.as_str(), "{output}");
    assert_eq!(output["currency"], "EUR", "{output}");
    assert_eq!(output["lines"], 3, "{output}");
    let total = output["total"].as_f64().unwrap_or_default();
    assert!((total - 59.0).abs() < 0.001, "{output}");
    assert!(
        events
            .tool_uses
            .iter()
            .any(|(n, _)| n == "StructuredOutput"),
        "{:?}",
        events.tool_uses
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_that_outgrows_the_threshold_compacts_on_its_own_and_carries_on(
) -> anyhow::Result<()> {
    let mut run = LiveRun::start(
        "autocompact",
        json!({ "compaction": { "threshold_tokens": 22000, "keep_recent": 2 } }),
        Mode::InProcess,
    )
    .await?;
    let codeword = format!("OSPREY-{}", nonce());
    let stem = nonce();
    let badges: Vec<String> = ["ALDER", "BIRCH", "CEDAR", "DOGWOOD"]
        .iter()
        .enumerate()
        .map(|(i, w)| format!("{w}-{}{stem}", i * 37 + 11))
        .collect();
    for (i, badge) in badges.iter().enumerate() {
        let line = |l: usize| {
            format!(
                "part {} line {l}: routine reading, all values nominal\n",
                i + 1
            )
        };
        let mut body: String = (0..300).map(line).collect();
        body.push_str(&format!("badge: {badge}\n"));
        body.extend((300..600).map(line));
        run.write_file(&format!("part{}.log", i + 1), &body)?;
    }
    let session = run.session().await?;
    let watcher = run.watch(&session, false).await?;

    run.turn(
        &session,
        &format!("The release codeword is {codeword}; I will ask for it later. Reply only OK."),
    )
    .await?;
    let events = run
        .turn(
            &session,
            "Read part1.log, part2.log, part3.log and part4.log with the Read tool, one at a \
             time and in that order, each one whole: no offset, no limit, and no other tool \
             to search them. Somewhere in each file is a line `badge: ...`. After reading each \
             file, write its badge in your reply before reading the next one. Finish with the \
             four badges, one per line.",
        )
        .await?;
    let compactions = watcher.events("compact");
    eprintln!(
        "[live autocompact] {:?}",
        compactions
            .iter()
            .map(|f| f["event"]["strategy"].clone())
            .collect::<Vec<_>>()
    );
    assert!(
        compactions
            .iter()
            .any(|f| f["event"]["strategy"] == "FullCompact"),
        "the turn compacted with a model-written summary on its own: {compactions:?}"
    );
    for badge in &badges {
        assert!(events.text.contains(badge), "{badge} in {}", events.text);
    }

    let recalled = run
        .turn(
            &session,
            "What is the release codeword? Answer with it alone, without using any tool.",
        )
        .await?;
    assert!(recalled.text.contains(&codeword), "{}", recalled.text);
    drop(watcher);
    run.finish().await;
    Ok(())
}

const PLAN_REMINDER: &str = "Plan mode is still active";
const FILES_REMINDER: &str = "The following files were read before context compaction";

#[tokio::test(flavor = "multi_thread")]
async fn after_a_compaction_the_model_is_reminded_of_plan_mode_and_a_running_task(
) -> anyhow::Result<()> {
    let mut run = LiveRun::start("reminded", json!({}), Mode::InProcess).await?;
    let session = run.session().await?;
    let watcher = run.watch(&session, false).await?;
    run.turn(
        &session,
        "Start a Monitor (load it with ToolSearch if needed) with command `sleep 600` and \
         description watch-dog. Do not stop it. Reply STARTED.",
    )
    .await?;
    let monitor = run
        .tasks(&session)
        .await?
        .into_iter()
        .find(|t| t["kind"] == "monitor" && t["status"] == "running")
        .ok_or_else(|| anyhow::anyhow!("no running monitor"))?;
    let task_id = monitor["task_id"].as_str().unwrap_or_default().to_string();
    run.turn(
        &session,
        "Call EnterPlanMode (load it with ToolSearch if needed), then reply PLANNING.",
    )
    .await?;
    run.turn(&session, "/compact").await?;
    let compacted = watcher
        .wait_for("a compact frame", Duration::from_secs(30), |e| {
            e["kind"] == "compact"
        })
        .await?;
    eprintln!("[live reminded] {}", compacted["event"]);

    run.turn(
        &session,
        "Which mode are you in, and is any background task still running? Answer without \
         using any tool.",
    )
    .await?;
    let after = run
        .provider
        .calls()
        .into_iter()
        .find(|c| c.messages_contain("Which mode are you in"))
        .ok_or_else(|| anyhow::anyhow!("no request after the compaction"))?;
    assert!(
        after.messages_contain(PLAN_REMINDER),
        "no plan-mode reminder after the compaction"
    );
    assert!(
        after.messages_contain(&format!("task:{task_id}")),
        "no reminder of the running monitor {task_id}"
    );
    run.client
        .call(
            "session.stopTask",
            json!({ "session_id": session, "task_id": task_id }),
        )
        .await?;
    drop(watcher);
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "product bug: an automatic compaction whose cascade returns NoOp (context over the threshold, nothing old enough to drop) still splices the post-compact recovery in and reports a compact frame (runtime/src/turn.rs:2426 only short-circuits NoOp for a manual trigger), so every step re-attaches the files read and the context grows"]
async fn a_compaction_with_nothing_to_drop_does_not_grow_the_context() -> anyhow::Result<()> {
    let mut run = LiveRun::start(
        "noop-compact",
        json!({ "compaction": { "threshold_tokens": 2000, "keep_recent": 50 } }),
        Mode::InProcess,
    )
    .await?;
    run.write_file("notes.txt", "build: green\nowner: ops\n")?;
    let session = run.session().await?;
    let watcher = run.watch(&session, false).await?;
    run.turn(
        &session,
        "Read notes.txt with the Read tool. Then run `echo one`, `echo two` and `echo three` \
         with Bash, one command per message. Reply DONE.",
    )
    .await?;
    let compactions: Vec<serde_json::Value> = watcher
        .events("compact")
        .into_iter()
        .map(|f| f["event"].clone())
        .collect();
    eprintln!("[live noop-compact] {compactions:?}");
    for c in &compactions {
        assert_ne!(c["strategy"], "NoOp", "a compaction that did nothing: {c}");
        assert!(
            c["messages_after"].as_u64() <= c["messages_before"].as_u64(),
            "a compaction grew the conversation: {c}"
        );
    }
    let reattached = run
        .provider
        .calls()
        .last()
        .map(|c| c.messages_text().matches(FILES_REMINDER).count())
        .unwrap_or(0);
    assert!(
        reattached <= 1,
        "notes.txt was re-attached {reattached} times"
    );
    drop(watcher);
    run.finish().await;
    Ok(())
}

/// The tool results a request carried back to the model, joined.
fn tool_results_sent(request: &live::SeenRequest) -> String {
    request.body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m["content"].as_array())
        .flatten()
        .filter(|b| b["type"] == "tool_result")
        .map(|b| match &b["content"] {
            serde_json::Value::String(text) => text.clone(),
            other => other.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A turn long enough for reactive compaction to clear the oldest reads:
/// eight files, one round each, with the trigger set so the context crosses
/// it from the start.
async fn reactive_turn(
    name: &str,
) -> anyhow::Result<(LiveRun, Vec<serde_json::Value>, Vec<String>)> {
    let mut run = LiveRun::start(
        name,
        json!({
            "feature_flags": { "reactive_compact": true },
            "compaction": { "threshold_tokens": 60000, "keep_recent": 2 }
        }),
        Mode::InProcess,
    )
    .await?;
    let stem = nonce();
    let badges: Vec<String> = (1..=8).map(|i| format!("BADGE{i}-{stem}")).collect();
    for (i, badge) in badges.iter().enumerate() {
        let mut body: String = (0..80)
            .map(|l| {
                format!(
                    "part {} line {l}: routine reading, all values nominal\n",
                    i + 1
                )
            })
            .collect();
        body.push_str(&format!("badge: {badge}\n"));
        run.write_file(&format!("part{}.log", i + 1), &body)?;
    }
    let session = run.session().await?;
    let watcher = run.watch(&session, false).await?;
    let events = run
        .turn(
            &session,
            "Read part1.log to part8.log with the Read tool, one file per message and in order, \
             each whole. Each has a line `badge: ...`; after reading each file, write its badge \
             in your reply before reading the next one. Finish with the eight badges, one per \
             line.",
        )
        .await?;
    for badge in &badges {
        assert!(events.text.contains(badge), "{badge} in {}", events.text);
    }
    let compactions = watcher
        .events("compact")
        .into_iter()
        .map(|f| f["event"].clone())
        .collect();
    drop(watcher);
    Ok((run, compactions, badges))
}

#[tokio::test(flavor = "multi_thread")]
async fn reactive_compaction_clears_the_oldest_reads_in_a_long_turn() -> anyhow::Result<()> {
    let (run, compactions, badges) = reactive_turn("reactive").await?;
    eprintln!("[live reactive] compact frames {compactions:?}");
    let calls = run.provider.calls();
    let first = &badges[0];
    let turn: Vec<&live::SeenRequest> = calls.iter().filter(|c| c.has_tools()).collect();
    let carried = turn
        .iter()
        .position(|c| tool_results_sent(c).contains(first))
        .ok_or_else(|| anyhow::anyhow!("part1.log's badge never reached the model"))?;
    let cleared = turn[carried..]
        .iter()
        .position(|c| {
            let sent = tool_results_sent(c);
            !sent.contains(first) && sent.contains(&badges[5])
        })
        .map(|at| at + carried);
    eprintln!("[live reactive] part1 carried from call {carried}, cleared at {cleared:?}");
    assert!(
        cleared.is_some(),
        "part1.log's read stayed whole all turn (carried from call {carried})"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "product bug: a reactive micro-compaction emits no compact frame (runtime/src/turn.rs:2270 only logs it), so the host cannot see why earlier tool results vanished — the protocol says the compact frame is the only place the reason is told (daemon_rpc_protocol.md §7.1)"]
async fn reactive_compaction_tells_the_host_it_happened() -> anyhow::Result<()> {
    let (run, compactions, _) = reactive_turn("reactive-frame").await?;
    assert!(!compactions.is_empty(), "no compact frame");
    run.finish().await;
    Ok(())
}
