//! What a session carries from one turn to the next, with a real model:
//! a compaction written by the model keeps the facts the conversation needs,
//! whether asked for or started on its own when a turn outgrows the
//! configured threshold, a fork shares the history before it and nothing after, and a session
//! created with an output schema ends its turn with a result that fits it.

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
