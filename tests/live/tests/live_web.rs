//! `WebFetch` and `WebSearch` with a real model, against pages this test
//! serves: a plain page, one past the 50 000-character cap, error statuses,
//! a redirect the tool hands back instead of following, JSON, a slow page the
//! host interrupts, extraction on the model `task_models.web_fetch` names
//! (and the page text when that model fails), and a search routed by
//! `task_models.web_search`.

use std::time::{Duration, Instant};

use live::{LiveRun, Page, PageServer, Purpose, Reply};
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

fn results_of<'a>(
    events: &'a rpc_client::TurnEvents,
    tool: &str,
) -> Vec<&'a (String, String, bool)> {
    events
        .tool_results
        .iter()
        .filter(|(name, _, _)| name == tool)
        .collect()
}

fn trimmed(run: &LiveRun, file: &str) -> Option<String> {
    run.read_file(file).map(|s| s.trim().to_string())
}

/// Rewrite the project's settings with `extra` over the base ones and have
/// the daemon reload them — for settings that name the proxy, whose address
/// is only known once the run has started.
async fn reload_with(run: &mut LiveRun, name: &str, extra: Value) -> anyhow::Result<()> {
    let mut settings = LiveRun::base_settings();
    if let (Some(base), Some(extra)) = (settings.as_object_mut(), extra.as_object()) {
        base.extend(extra.iter().map(|(k, v)| (k.clone(), v.clone())));
    }
    run.world.write_project_settings(name, &settings)?;
    let reloaded = run.client.config_reload(&rpc_client::Target::own()).await?;
    anyhow::ensure!(reloaded.error.is_none(), "{:?}", reloaded.error);
    let report = reloaded.result.unwrap_or_default();
    anyhow::ensure!(
        report["routing"]["router_rebuilt"] == true,
        "the providers did not take: {report}"
    );
    Ok(())
}

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
async fn webfetch_brings_a_page_back_as_text_without_an_extraction_call() -> anyhow::Result<()> {
    let pages = PageServer::start().await?;
    let token = format!("T{}", nonce());
    pages.route(
        "/doc",
        Page::html(format!(
            "<html><head><title>Notes</title></head><body><h1>Release notes</h1>\
             <p>Build token: {token} &amp; 中文说明</p></body></html>"
        )),
    );
    let mut run = LiveRun::builder("web-plain")
        .settings(json!({ "sandbox": PageServer::reachable() }))
        .start()
        .await?;
    let session = run.session().await?;
    let _host = run.watch(&session, true).await?;
    let events = run
        .turn(
            &session,
            &format!(
                "Fetch {} with the WebFetch tool, passing the prompt \"What is the build \
                 token?\". Write the build token (the word after `Build token: `) to t.txt \
                 with the Write tool, then reply DONE.",
                pages.url("/doc")
            ),
        )
        .await?;
    assert_eq!(pages.hits("/doc"), 1, "{:?}", events.tool_results);
    assert_eq!(trimmed(&run, "t.txt"), Some(token));
    let fetched = results_of(&events, "WebFetch");
    assert!(
        fetched.iter().any(|(_, t, failed)| !failed
            && t.contains("& 中文说明")
            && t.contains("mode=page text")),
        "{fetched:?}"
    );
    assert!(
        run.provider
            .calls()
            .iter()
            .all(|c| c.purpose() != Purpose::WebFetch),
        "an extraction call with no task_models.web_fetch"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn webfetch_cuts_a_huge_page_at_fifty_thousand_characters() -> anyhow::Result<()> {
    let pages = PageServer::start().await?;
    let id = nonce();
    let (early, edge, past) = (
        format!("EARLY-{id}"),
        format!("EDGE-{id}"),
        format!("PAST-{id}"),
    );
    pages.route(
        "/big",
        Page::huge(70_000, &[(200, &early), (49_000, &edge), (51_000, &past)]),
    );
    let mut run = LiveRun::builder("web-huge")
        .settings(json!({ "sandbox": PageServer::reachable() }))
        .start()
        .await?;
    let session = run.session().await?;
    let _host = run.watch(&session, true).await?;
    let events = run
        .turn(
            &session,
            &format!(
                "Fetch {} once with the WebFetch tool, then reply with the first word on the \
                 page that starts with EARLY-.",
                pages.url("/big")
            ),
        )
        .await?;
    assert_eq!(pages.hits("/big"), 1);
    let fetched = results_of(&events, "WebFetch");
    let (_, text, failed) = fetched.first().copied().expect("a WebFetch result");
    assert!(!failed, "{text}");
    assert!(
        text.contains(&early) && text.contains(&edge),
        "the kept part"
    );
    assert!(!text.contains(&past), "past the cut");
    assert!(
        text.contains("[truncated: the page is 70000 characters]"),
        "the cut is marked"
    );
    assert!(
        run.provider
            .calls()
            .iter()
            .any(|c| c.messages_contain(&early)),
        "the head of the page reached the model"
    );
    assert!(
        run.provider
            .calls()
            .iter()
            .all(|c| !c.messages_contain(&past)),
        "text past the cut reached the model"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn webfetch_reports_error_statuses_and_hands_redirects_back() -> anyhow::Result<()> {
    let pages = PageServer::start().await?;
    let (moved_token, build) = (format!("R{}", nonce()), format!("J{}", nonce()));
    pages.route("/broken", Page::status(500, "upstream exploded"));
    pages.route("/moved", Page::redirect("/target"));
    pages.route(
        "/target",
        Page::html(format!("<p>Redirect token: {moved_token}</p>")),
    );
    pages.route(
        "/data.json",
        Page::json(&json!({ "service": "pages", "build": build })),
    );
    let mut run = LiveRun::builder("web-statuses")
        .settings(json!({ "sandbox": PageServer::reachable() }))
        .start()
        .await?;
    let session = run.session().await?;
    let _host = run.watch(&session, true).await?;
    let events = run
        .turn(
            &session,
            &format!(
                "Use the WebFetch tool for each step; write files with the Write tool.\n\
                 1. Fetch {missing} and {broken}. Write the two HTTP status codes you got, \
                 separated by a space, to codes.txt.\n\
                 2. Fetch {moved}. If it tells you it was redirected, fetch the URL it names. \
                 Write the word after `Redirect token: ` to r.txt.\n\
                 3. Fetch {data}. Write the value of its `build` field to j.txt.\n\
                 Then reply DONE.",
                missing = pages.url("/missing"),
                broken = pages.url("/broken"),
                moved = pages.url("/moved"),
                data = pages.url("/data.json"),
            ),
        )
        .await?;

    let fetched = results_of(&events, "WebFetch");
    assert!(
        fetched
            .iter()
            .any(|(_, t, failed)| *failed && live::inside_fence(t).starts_with("HTTP 404")),
        "{fetched:?}"
    );
    assert!(
        fetched.iter().any(|(_, t, failed)| *failed
            && live::inside_fence(t).starts_with("HTTP 500")
            && t.contains("upstream exploded")),
        "{fetched:?}"
    );
    assert!(
        fetched.iter().any(|(_, t, failed)| !failed
            && t.contains(&format!("Redirected to: {}", pages.url("/target")))),
        "{fetched:?}"
    );
    let asked_for_target = events
        .tool_uses
        .iter()
        .filter(|(n, input)| n == "WebFetch" && input["url"] == pages.url("/target"))
        .count();
    assert_eq!(
        pages.hits("/target"),
        asked_for_target,
        "the target was fetched only when the model asked for it"
    );
    assert!(asked_for_target >= 1);
    let codes = trimmed(&run, "codes.txt").unwrap_or_default();
    assert!(codes.contains("404") && codes.contains("500"), "{codes}");
    assert_eq!(trimmed(&run, "r.txt"), Some(moved_token));
    assert_eq!(trimmed(&run, "j.txt"), Some(build));
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn an_interrupt_ends_a_turn_that_waits_on_a_slow_page() -> anyhow::Result<()> {
    let pages = PageServer::start().await?;
    pages.route(
        "/slow",
        Page::slow(Duration::from_secs(12), Page::text("SLOW-PAGE-BODY")),
    );
    let mut run = LiveRun::builder("web-slow")
        .settings(json!({ "sandbox": PageServer::reachable() }))
        .start()
        .await?;
    let session = run.session().await?;
    let _host = run.watch(&session, true).await?;
    let handle = run
        .spawn_turn(
            &session,
            &format!(
                "Fetch {} once with the WebFetch tool and reply with what it says.",
                pages.url("/slow")
            ),
            json!({}),
        )
        .await?;
    let deadline = Instant::now() + Duration::from_secs(180);
    while pages.hits("/slow") == 0 {
        anyhow::ensure!(
            Instant::now() < deadline,
            "the model never fetched the page"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let interrupted_at = Instant::now();
    let resp = run
        .client
        .call("session.interrupt", json!({ "session_id": session }))
        .await?;
    assert_eq!(resp.result.unwrap_or_default()["interrupted"], json!(true));
    let events = tokio::time::timeout(Duration::from_secs(20), handle).await???;
    assert!(
        interrupted_at.elapsed() < Duration::from_secs(8),
        "the turn sat out the page: {:?}",
        interrupted_at.elapsed()
    );
    assert!(
        results_of(&events, "WebFetch")
            .iter()
            .all(|(_, t, _)| !t.contains("SLOW-PAGE-BODY")),
        "{:?}",
        events.tool_results
    );
    run.wait_turn_state(&session, "idle", Duration::from_secs(10))
        .await?;
    run.finish().await;
    Ok(())
}

/// A long page whose answer is near the top and whose noise is near the
/// bottom: only a call that read the whole page sees the noise.
fn long_page(build: &str, noise: &str) -> Page {
    Page::huge(
        45_000,
        &[
            (3_000, &format!(" The build id is {build}. ")),
            (40_000, &format!(" {noise} ")),
        ],
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn task_models_web_fetch_extracts_on_its_own_model_and_falls_back_to_the_page(
) -> anyhow::Result<()> {
    let pages = PageServer::start().await?;
    let (build, noise) = (
        format!("BID{}", nonce()),
        format!("FOOTER-NOISE-{}", nonce()),
    );
    pages.route("/doc", long_page(&build, &noise));
    let mut run = LiveRun::builder("web-extract")
        .settings(json!({ "sandbox": PageServer::reachable() }))
        .start()
        .await?;
    let (main, extractor) = (
        run.config().model.clone(),
        run.config().model_at(1).to_string(),
    );
    assert_ne!(
        main, extractor,
        "AGIX_LIVE_MODELS needs a second Flash name"
    );
    let settings = json!({
        "providers": {
            "main": provider(&run, &main),
            "extract": provider(&run, &extractor),
        },
        "default_provider": "main",
        "task_models": { "web_fetch": "extract" },
        "sandbox": PageServer::reachable(),
    });
    reload_with(&mut run, "web-extract", settings).await?;
    let session = run.session().await?;
    let _host = run.watch(&session, true).await?;
    let ask = |url: &str, file: &str| {
        format!(
            "Call WebFetch on {url} with the prompt \"What is the build id? Answer with the id \
             only.\" Write the build id to {file} with the Write tool, then reply DONE."
        )
    };
    let events = run
        .turn(&session, &ask(&pages.url("/doc"), "id.txt"))
        .await?;
    assert_eq!(trimmed(&run, "id.txt"), Some(build.clone()));
    let fetched = results_of(&events, "WebFetch");
    assert!(
        fetched
            .iter()
            .any(|(_, t, failed)| !failed && t.contains(&format!("mode=extracted by {extractor}"))),
        "{fetched:?}"
    );
    let calls = run.provider.calls();
    let extractions: Vec<_> = calls
        .iter()
        .filter(|c| c.purpose() == Purpose::WebFetch)
        .collect();
    assert_eq!(extractions.len(), 1, "one extraction call");
    assert_eq!(extractions[0].model(), extractor);
    assert!(
        extractions[0].messages_contain(&noise),
        "the extraction model read the whole page"
    );
    let turns: Vec<_> = calls
        .iter()
        .filter(|c| c.purpose() == Purpose::Turn)
        .collect();
    assert!(
        turns.iter().all(|c| c.model() == main),
        "the turn stayed on {main}"
    );
    assert!(
        turns.iter().all(|c| !c.messages_contain(&noise)),
        "the session's model was handed the whole page"
    );

    let (build2, noise2) = (
        format!("BID{}", nonce()),
        format!("FOOTER-NOISE-{}", nonce()),
    );
    pages.route("/doc2", long_page(&build2, &noise2));
    run.provider.inject_failures_when(
        "You are an information-extraction assistant",
        5,
        Reply::status(
            400,
            "{\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\"message\":\"extraction refused\"}}",
        ),
    );
    let before = run.provider.exchanges().len();
    let events = run
        .turn(&session, &ask(&pages.url("/doc2"), "id2.txt"))
        .await?;
    assert_eq!(trimmed(&run, "id2.txt"), Some(build2));
    let fetched = results_of(&events, "WebFetch");
    assert!(
        fetched.iter().any(|(_, t, failed)| !failed
            && t.contains("mode=page text; extraction failed")
            && t.contains(&noise2)),
        "{fetched:?}"
    );
    let later = &run.provider.exchanges()[before..];
    assert!(
        later
            .iter()
            .filter(|e| e.request.purpose() == Purpose::WebFetch)
            .all(|e| e.status == 400),
        "an extraction call got through"
    );
    assert!(
        later
            .iter()
            .any(|e| e.request.purpose() == Purpose::Turn && e.request.messages_contain(&noise2)),
        "the page text went to the session's model instead"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn task_models_web_search_routes_the_search_to_its_own_model() -> anyhow::Result<()> {
    let mut run = LiveRun::builder("web-search-routed").start().await?;
    let (main, searcher) = (
        run.config().model.clone(),
        run.config().model_at(2).to_string(),
    );
    assert_ne!(main, searcher, "AGIX_LIVE_MODELS needs a third Flash name");
    let settings = json!({
        "providers": {
            "main": provider(&run, &main),
            "search": provider(&run, &searcher),
        },
        "default_provider": "main",
        "task_models": { "web_search": "search" },
    });
    reload_with(&mut run, "web-search-routed", settings).await?;
    let session = run.session().await?;
    let events = run
        .turn(
            &session,
            "Use the WebSearch tool to find the Rust 1.80.0 release announcement, then reply \
             with its release date in the form YYYY-MM-DD and nothing else.",
        )
        .await?;
    assert!(
        events.tool_uses.iter().any(|(n, _)| n == "WebSearch"),
        "{:?}",
        events.tool_uses
    );
    let is_provider_search = |tool: &Value| {
        tool["type"]
            .as_str()
            .is_some_and(|t| t.starts_with("web_search_"))
    };
    let calls = run.provider.calls();
    let (searches, turns): (Vec<_>, Vec<_>) = calls.iter().partition(|c| {
        c.body["tools"]
            .as_array()
            .is_some_and(|tools| tools.iter().any(is_provider_search))
    });
    assert!(!searches.is_empty(), "the search went to the provider");
    for search in &searches {
        assert_eq!(search.model(), searcher, "routed by task_models.web_search");
    }
    for turn in turns.iter().filter(|c| c.purpose() == Purpose::Turn) {
        assert_eq!(turn.model(), main);
    }
    run.finish().await;
    Ok(())
}
