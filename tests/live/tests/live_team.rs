//! A `TeamCreate` team with a real model: a later stage works from what an
//! earlier stage saved to the team's shared memory.

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
    let events = run
        .turn(
            &session,
            "Use the TeamCreate tool (load it with ToolSearch if needed) to run a team named \
             relay, with permission_mode bypassPermissions and two stages:\n\
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
