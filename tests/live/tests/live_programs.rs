//! `RunCode` with a real model, past the single search `live_basics` covers:
//! a program that fans out to sub-agents and merges what their schemas hand
//! back, a program run in the background whose end reaches the model as a
//! notification, and a deny rule that still holds for a call made from code
//! (under `acceptEdits`: `bypassPermissions` in the daemon skips the rule
//! engine altogether, direct calls included).

use std::time::Duration;

use live::{LiveRun, Mode};
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

fn print_programs(events: &rpc_client::TurnEvents) {
    for ((name, input), (_, result, is_error)) in events.tool_uses.iter().zip(&events.tool_results)
    {
        if name == "RunCode" {
            eprintln!(
                "[live ptc] RunCode{}\n{}\n  => {}",
                if *is_error { " (error)" } else { "" },
                input["code"].as_str().unwrap_or_default(),
                result.chars().take(600).collect::<String>()
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_program_fans_out_to_sub_agents_and_merges_their_results() -> anyhow::Result<()> {
    let mut run = LiveRun::start("ptc-fanout", json!({}), Mode::InProcess).await?;
    let stock = [
        ("north", [3, 9, 4]),
        ("south", [12, 1, 7]),
        ("east", [5, 5, 6]),
        ("west", [20, 2, 8]),
    ];
    let seal = nonce();
    for (depot, counts) in &stock {
        run.write_file(
            &format!("depots/{depot}.txt"),
            &format!(
                "depot: {depot}\nseal: {seal}-{depot}\napples: {}\npears: {}\nplums: {}\n",
                counts[0], counts[1], counts[2]
            ),
        )?;
    }
    let session = run.session().await?;
    let events = run
        .turn(
            &session,
            "Use one RunCode program. In it, start one sub-agent per depot with \
             agent(prompt, {type: \"explore\", schema}) — all four in parallel with Promise.all — \
             for depots/north.txt, depots/south.txt, depots/east.txt and depots/west.txt. Each \
             sub-agent reads its one file and hands back {depot, seal, total}, where total is \
             apples + pears + plums. The schema: {type: \"object\", required: [\"depot\", \"seal\", \
             \"total\"], properties: {depot: {type: \"string\"}, seal: {type: \"string\"}, total: \
             {type: \"integer\"}}}. The program then writes the four results, sorted by depot, as \
             a JSON array to totals.json with tools.Write and returns them. Reply DONE after.",
        )
        .await?;
    print_programs(&events);

    let used: Vec<&str> = events.tool_uses.iter().map(|(n, _)| n.as_str()).collect();
    assert!(used.contains(&"RunCode"), "{used:?}");
    let totals: Vec<Value> = run
        .read_file("totals.json")
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let mut expected: Vec<(&str, i64)> = stock
        .iter()
        .map(|(d, c)| (*d, c.iter().sum::<i64>()))
        .collect();
    expected.sort();
    assert_eq!(totals.len(), 4, "{totals:?}");
    for (got, (depot, total)) in totals.iter().zip(&expected) {
        assert_eq!(got["depot"], *depot, "{totals:?}");
        assert_eq!(got["total"], *total, "{totals:?}");
        assert_eq!(
            got["seal"],
            format!("{seal}-{depot}"),
            "the seal only the file had came back through the sub-agent: {totals:?}"
        );
    }

    let tasks = run.tasks(&session).await?;
    let program = tasks.iter().find(|t| t["kind"] == "code");
    assert!(
        program.is_some_and(|t| t["status"] == "done"),
        "the program ran as a task of kind code: {tasks:?}"
    );
    let agents = tasks.iter().filter(|t| t["kind"] == "agent").count();
    assert!(agents >= 4, "one sub-agent per depot: {tasks:?}");
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_background_program_reports_back_when_it_ends() -> anyhow::Result<()> {
    let mut run = LiveRun::start("ptc-background", json!({}), Mode::InProcess).await?;
    let tag = format!("BATCH-{}", nonce());
    for i in 1..=5 {
        run.write_file(
            &format!("jobs/job{i}.sh"),
            &format!("sleep 1\necho {tag}-{i}\n"),
        )?;
    }
    let session = run.session().await?;
    let watcher = run.watch(&session, false).await?;
    run.turn(
        &session,
        "Start a RunCode program with background: true and description \"run jobs\". The \
         program runs `sh jobs/jobN.sh` for N = 1..5 one after another with tools.Bash, calls \
         log() with each job's stdout, and returns the five stdout lines (trimmed) as an \
         array. Do not wait for it: right after starting it, reply STARTED and end the turn.",
    )
    .await?;

    let tasks = run
        .wait_tasks_settled(&session, Duration::from_secs(300))
        .await?;
    let program = tasks
        .iter()
        .find(|t| t["kind"] == "code")
        .cloned()
        .unwrap_or_default();
    assert_eq!(program["status"], "done", "{tasks:?}");
    assert_eq!(program["detail"]["background"], true, "{program}");
    let task_id = program["task_id"].clone();
    assert!(
        watcher
            .events("task_progress")
            .iter()
            .any(|f| f["event"]["task_id"] == task_id
                && f["event"]["appended"]
                    .as_str()
                    .is_some_and(|a| a.contains(&format!("{tag}-3")))),
        "the host saw log() lines while the program ran"
    );

    let events = run
        .turn(
            &session,
            "Your background program has finished. Write the lines it returned, one per line \
             and in order, to results.txt with the Write tool. Use TaskOutput if you need them.",
        )
        .await?;
    let lines: Vec<String> = run
        .read_file("results.txt")
        .unwrap_or_default()
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    let want: Vec<String> = (1..=5).map(|i| format!("{tag}-{i}")).collect();
    assert_eq!(lines, want, "{}", events.text);
    let task_id = task_id.as_str().unwrap_or("?");
    let told = run.provider.calls().iter().any(|c| {
        let text = c.messages_text();
        text.contains("<task-notification>") && text.contains(task_id)
    });
    assert!(
        told,
        "the program's end reached the model as a notification"
    );
    drop(watcher);
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_deny_rule_holds_for_a_call_made_from_code() -> anyhow::Result<()> {
    let mut run = LiveRun::start(
        "ptc-deny",
        json!({
            "permission_mode": "acceptEdits",
            "permission_rules": [
                { "tool": "Write(**/locked/**)", "action": "deny" },
                { "tool": "Edit(**/locked/**)", "action": "deny" }
            ]
        }),
        Mode::InProcess,
    )
    .await?;
    let mark = format!("STAMP-{}", nonce());
    run.write_file("locked/keep.txt", "keep\n")?;
    let session = run.session().await?;
    let events = run
        .turn(
            &session,
            &format!(
                "Use one RunCode program that writes the text {mark} to each of out/a.txt, \
                 locked/b.txt and out/c.txt with tools.Write, one after another, catching each \
                 failure, and returns which writes succeeded and which failed with the error. \
                 Do not retry a failed write any other way. Reply with the program's result."
            ),
        )
        .await?;
    print_programs(&events);

    assert!(events.tool_uses.iter().any(|(n, _)| n == "RunCode"));
    for path in ["out/a.txt", "out/c.txt"] {
        assert_eq!(
            run.read_file(path).map(|s| s.trim().to_string()),
            Some(mark.clone()),
            "{path}"
        );
    }
    assert!(
        run.read_file("locked/b.txt").is_none(),
        "the deny rule held inside the program"
    );
    assert!(
        events
            .tool_results
            .iter()
            .any(|(n, out, _)| n == "RunCode" && out.contains("denied by rule")),
        "the program saw the refusal: {:?}",
        events.tool_results
    );
    run.finish().await;
    Ok(())
}
