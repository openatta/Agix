//! A `TeamCreate` team with a real model: a later stage works from what an
//! earlier stage saved to the team's shared memory; two members of one stage
//! pass a word through their mailboxes, the stage reports `team_progress`,
//! and the team is listed and then deleted.

use live::{LiveRun, Mode};
use serde_json::json;

#[tokio::test(flavor = "multi_thread")]
async fn a_later_stage_works_from_what_an_earlier_stage_remembered() -> anyhow::Result<()> {
    let mut run = LiveRun::start(
        "relay",
        json!({ "execution": { "team_enabled": true } }),
        Mode::InProcess,
    )
    .await?;
    let secret = format!(
        "QUARTZ-{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
            % 0xffff_ffff
    );
    run.write_file("notes/relay-word.txt", &format!("{secret}\n"))?;

    let session = run.session().await?;
    let _host = run.watch(&session, true).await?;
    let events = run
        .turn(
            &session,
            "Use the TeamCreate tool (load it with ToolSearch if needed) to run a team named \
             relay, with permission_mode default and two stages:\n\
             - stage 1, one agent labelled scout: \"Read notes/relay-word.txt and save its \
             content to team memory with the TeamMemorySave tool, name secret-word, type \
             project, description 'the relay word'. Do not write any file.\"\n\
             - stage 2, one agent labelled scribe: \"Read the team memory named secret-word \
             with the TeamMemoryRead tool and write the word it holds to relay.txt. Do not \
             read anything under notes/.\"\n\
             Do not read notes/ or write relay.txt yourself. When the team returns, reply \
             DONE.",
        )
        .await?;

    assert!(
        events.tool_uses.iter().any(|(n, _)| n == "TeamCreate"),
        "{:?}",
        events.tool_uses
    );
    assert_eq!(
        run.read_file("relay.txt").map(|s| s.trim().to_string()),
        Some(secret.clone()),
        "the scribe must write the word it read from team memory"
    );
    let used = |tool: &str| {
        run.provider.calls().iter().any(|c| {
            c.body["messages"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|m| {
                    m["content"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .any(|b| b["type"] == "tool_use" && b["name"] == tool)
                })
        })
    };
    assert!(
        used("TeamMemorySave"),
        "the scout never saved to team memory"
    );
    assert!(used("TeamMemoryRead"), "the scribe never read team memory");
    run.finish().await;
    Ok(())
}

fn tool_used(run: &LiveRun, tool: &str) -> bool {
    run.provider.calls().iter().any(|c| {
        c.body["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| m["content"].as_array())
            .flatten()
            .any(|b| b["type"] == "tool_use" && b["name"] == tool)
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn members_of_a_stage_pass_a_message_and_the_team_is_listed_then_deleted(
) -> anyhow::Result<()> {
    let mut run = LiveRun::start(
        "mail",
        json!({ "execution": { "team_enabled": true } }),
        Mode::InProcess,
    )
    .await?;
    let token = format!(
        "PARCEL-{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
            % 0xffff_ffff
    );
    run.write_file("notes/parcel.txt", &format!("{token}\n"))?;
    let session = run.session().await?;
    let watcher = run.watch(&session, true).await?;
    let events = run
        .turn(
            &session,
            "Load TeamCreate, TeamList and TeamDelete with ToolSearch if needed. Use TeamCreate \
             to run a team named mail, with permission_mode default and one stage \
             named swap with two agents:\n\
             - label sender: \"Read notes/parcel.txt. Call ListPeers, then call SendMessage \
             with peer receiver and the file's content as the message. Then answer SENT.\"\n\
             - label receiver: \"Call ReadMail. While it has no message, run `sleep 1` with \
             Bash and call ReadMail again, up to 20 times. When a message arrives, write the \
             word it carries to received.txt with the Write tool and answer GOT. Do not read \
             anything under notes/.\"\n\
             When the team returns: call TeamList, then TeamDelete with name mail, then \
             TeamList again. Do not read notes/ or write received.txt yourself. Reply DONE.",
        )
        .await?;
    assert_eq!(
        run.read_file("received.txt").map(|s| s.trim().to_string()),
        Some(token),
        "the word reached the receiver only through its mailbox"
    );
    for tool in ["ListPeers", "SendMessage", "ReadMail"] {
        assert!(tool_used(&run, tool), "no member called {tool}");
    }

    let lists: Vec<&(String, String, bool)> = events
        .tool_results
        .iter()
        .filter(|(n, _, _)| n == "TeamList")
        .collect();
    eprintln!("[live mail] {lists:?}");
    assert!(lists.len() >= 2, "{:?}", events.tool_uses);
    assert!(lists[0].1.contains("\"mail\""), "{lists:?}");
    assert!(
        !lists.last().is_some_and(|(_, t, _)| t.contains("\"mail\"")),
        "TeamDelete left the team listed: {lists:?}"
    );
    assert!(
        events
            .tool_results
            .iter()
            .any(|(n, _, failed)| n == "TeamDelete" && !failed),
        "{:?}",
        events.tool_results
    );

    let stages: Vec<(String, String)> = watcher
        .events("team_progress")
        .iter()
        .map(|f| &f["event"])
        .filter(|e| e["team"] == "mail")
        .map(|e| {
            (
                e["stage"].as_str().unwrap_or_default().to_string(),
                e["status"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    eprintln!("[live mail] team_progress {stages:?}");
    assert_eq!(
        stages,
        [
            ("swap".to_string(), "started".to_string()),
            ("swap".to_string(), "completed".to_string())
        ]
    );
    drop(watcher);
    run.finish().await;
    Ok(())
}
