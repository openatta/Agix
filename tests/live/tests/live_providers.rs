//! `task_models` routing with a real model: in the `harnix` scene, which
//! names its sessions, the naming call goes to the model `task_models.title`
//! names and the summary of a compaction to the one `task_models.compact`
//! names, while every turn stays on the default provider's model.

use std::time::{Duration, Instant};

use live::{LiveRun, Purpose};
use serde_json::{json, Value};

/// A `providers` entry for the proxy that serves `model`.
fn provider(run: &LiveRun, model: &str) -> Value {
    json!({
        "api_type": "anthropic",
        "base_url": live::ModelEndpoint::base_url(&run.provider).as_str(),
        "api_key": "harness",
        "default_model": model,
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn title_and_compact_calls_go_to_the_models_task_models_names() -> anyhow::Result<()> {
    let name = "providers-routed";
    let mut run = LiveRun::builder(name)
        .options(|o| o.scene("harnix"))
        .start()
        .await?;
    let (main, compactor, titler) = (
        run.config().model.clone(),
        run.config().model_at(1).to_string(),
        run.config().model_at(2).to_string(),
    );
    assert!(
        main != compactor && main != titler && compactor != titler,
        "AGIX_LIVE_MODELS needs three Flash names"
    );
    let mut settings = LiveRun::base_settings();
    settings["providers"] = json!({
        "main": provider(&run, &main),
        "compact": provider(&run, &compactor),
        "title": provider(&run, &titler),
    });
    settings["default_provider"] = json!("main");
    settings["task_models"] = json!({ "compact": "compact", "title": "title" });
    run.world.write_project_settings(name, &settings)?;
    let reloaded = run.client.config_reload(&rpc_client::Target::own()).await?;
    let report = reloaded.result.unwrap_or_default();
    assert_eq!(report["routing"]["router_rebuilt"], true, "{report}");

    let session = run.session().await?;
    run.turn(
        &session,
        "We are planning a database migration for the billing service. Reply with the word \
         NOTED and nothing else.",
    )
    .await?;
    let deadline = Instant::now() + Duration::from_secs(60);
    let named = loop {
        let got = run
            .client
            .session_get(&session)
            .await?
            .result
            .unwrap_or_default();
        if let Some(name) = got["name"].as_str() {
            break name.to_string();
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "the session was never named: {got}"
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
    };
    assert!(!named.is_empty());
    run.turn(
        &session,
        "List three risks of such a migration, one line each.",
    )
    .await?;
    let compacted = run
        .client
        .call("session.compact", json!({ "session_id": session }))
        .await?;
    assert!(compacted.error.is_none(), "{compacted:?}");

    let calls = run.provider.calls();
    let of = |purpose: Purpose| -> Vec<String> {
        calls
            .iter()
            .filter(|c| c.purpose() == purpose)
            .map(|c| c.model().to_string())
            .collect()
    };
    let (titles, compactions, turns) =
        (of(Purpose::Title), of(Purpose::Compact), of(Purpose::Turn));
    assert_eq!(titles, std::slice::from_ref(&titler), "naming calls");
    assert_eq!(
        compactions,
        std::slice::from_ref(&compactor),
        "compaction calls"
    );
    assert!(
        !turns.is_empty() && turns.iter().all(|m| *m == main),
        "turns: {turns:?}"
    );
    run.finish().await;
    Ok(())
}
