//! MCP OAuth with a real model, against `live::OAuthWorld`: a server that
//! needs a sign-in offers the model nothing until the host signs the user in
//! (loopback, or an external redirect handed back with `mcp.auth.complete`),
//! a token that expires mid-session is refreshed under the model's tool call,
//! a refused refresh and a logout take the tools away again, a tool needing
//! more scope works after a step-up, `client_credentials` and the enterprise
//! grant connect with nobody at a browser, and the refusals — a forged
//! callback, a sign-in left too long, an authorization server outside the
//! policy — leave the model without the server. No token reaches the model,
//! the transcript, an export, telemetry or the daemon's stderr.

use std::time::Duration;

use live::oauth::{click, consent, follow};
use live::{DaemonEvents, LiveRun, Mode, OAuthWorld, Purpose, SeenRequest};
use rpc_client::{Target, TurnEvents};
use serde_json::{json, Value};

const SERVER: &str = "docs";
const WAIT: Duration = Duration::from_secs(30);
const NOTICE: &str = "requires the user to sign in";

const USE_ECHO: &str = "Call the MCP tool mcp__docs__echo directly, once, with no arguments — it \
     is in your tool list now even if it was not before; do not go through ToolSearch or RunCode. \
     Reply with exactly the text it returned.";
const TRY_ECHO: &str = "Call the MCP tool mcp__docs__echo once, with no arguments. If it is not \
     in your tool list, run one ToolSearch for it; if that finds nothing, reply UNAVAILABLE. If \
     the call fails, reply FAILED. Do not try any other way and do not call it twice.";

async fn session(run: &mut LiveRun) -> anyhow::Result<String> {
    let telemetry = run.world.telemetry_file("session.jsonl");
    run.session_with(json!({
        "options": { "telemetry": { "output": telemetry.to_string_lossy() } }
    }))
    .await
}

async fn status(run: &mut LiveRun) -> anyhow::Result<Value> {
    let resp = run.client.mcp_status().await?;
    Ok(resp.result.unwrap_or_default()["servers"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|s| s["name"] == SERVER)
        .cloned()
        .unwrap_or(Value::Null))
}

async fn wait_state(run: &mut LiveRun, state: &str) -> anyhow::Result<Value> {
    let deadline = std::time::Instant::now() + WAIT;
    loop {
        let now = status(run).await?;
        if now["state"] == state {
            return Ok(now);
        }
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "`{SERVER}` never became {state}: {now}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn result_of(events: &TurnEvents, tool: &str) -> Vec<(String, bool)> {
    events
        .tool_results
        .iter()
        .filter(|(name, _, _)| name == tool)
        .map(|(_, text, failed)| (text.clone(), *failed))
        .collect()
}

/// The turn's model calls: what the proxy recorded from `from` on.
fn turn_requests(run: &LiveRun, from: usize) -> Vec<SeenRequest> {
    run.provider
        .calls()
        .into_iter()
        .skip(from)
        .filter(|r| r.purpose() == Purpose::Turn)
        .collect()
}

/// A turn that asks for the server's tool while it is not usable: the model
/// is told why, offered none of the server's tools, and gets no result only
/// the server could give.
async fn assert_server_out_of_reach(
    run: &mut LiveRun,
    world: &OAuthWorld,
    session: &str,
) -> anyhow::Result<()> {
    let from = run.provider.calls().len();
    let events = run.turn(session, TRY_ECHO).await?;
    let requests = turn_requests(run, from);
    anyhow::ensure!(!requests.is_empty(), "the turn reached no model");
    for request in &requests {
        let offered = request.tool_names();
        assert!(
            offered.iter().all(|n| !n.starts_with("mcp__docs__")),
            "a tool of a server that needs a sign-in was offered: {offered:?}"
        );
        assert!(
            request.system_contains(NOTICE) && request.system_contains(SERVER),
            "the model is not told the server needs a sign-in: {}",
            request.system_text()
        );
    }
    assert!(
        events
            .tool_results
            .iter()
            .all(|(_, text, _)| !text.contains(&world.marker())),
        "the server answered without a sign-in: {:?}",
        events.tool_results
    );
    Ok(())
}

/// A turn in which the model calls `tool` and gets the answer only the
/// signed-in server gives.
async fn assert_tool_works(
    run: &mut LiveRun,
    world: &OAuthWorld,
    session: &str,
    tool: &str,
) -> anyhow::Result<()> {
    let prompt = if tool == "mcp__docs__echo" {
        USE_ECHO.to_string()
    } else {
        USE_ECHO.replace("mcp__docs__echo", tool)
    };
    let from = run.provider.calls().len();
    let events = run.turn(session, &prompt).await?;
    let offered = turn_requests(run, from)
        .first()
        .map(SeenRequest::tool_names)
        .unwrap_or_default();
    assert!(
        offered.iter().any(|n| n == tool),
        "{tool} was not offered: {offered:?}"
    );
    let short = tool.trim_start_matches("mcp__docs__");
    assert!(
        result_of(&events, tool)
            .iter()
            .any(|(text, failed)| !failed
                && text.contains(&format!("{short}: ok {}", world.marker()))),
        "{tool} did not answer: {:?}",
        events.tool_results
    );
    Ok(())
}

/// Sign the user in through the daemon's loopback listener, as a host does:
/// `mcp.auth.start`, the browser to `authorize_url` and back.
async fn sign_in(
    run: &mut LiveRun,
    events: &DaemonEvents,
    scopes: Option<&[&str]>,
) -> anyhow::Result<Value> {
    events.clear();
    let resp = run
        .client
        .mcp_auth_start(&Target::own(), SERVER, None, scopes)
        .await?;
    let start = resp
        .result
        .ok_or_else(|| anyhow::anyhow!("mcp.auth.start: {:?}", resp.error))?;
    let (code, page) = click(start["authorize_url"].as_str().unwrap_or_default()).await;
    anyhow::ensure!(
        code == 200 && page.contains("Signed in to docs"),
        "{code} {page}"
    );
    events.next("mcp_auth_completed", SERVER, WAIT).await?;
    events.next("mcp_connected", SERVER, WAIT).await?;
    Ok(start)
}

/// No token the authorization server (or IdP) issued is in what the model
/// was sent, in the session's export, or in any file under the world — the
/// transcript, the telemetry file, a spawned daemon's stderr — other than the
/// credential store.
async fn assert_no_token_leaked(
    run: &mut LiveRun,
    world: &OAuthWorld,
    sessions: &[&str],
    also: &[&str],
) -> anyhow::Result<()> {
    let mut secrets = world.tokens();
    anyhow::ensure!(!secrets.is_empty(), "nothing was issued");
    secrets.extend(also.iter().map(|s| s.to_string()));
    for (i, request) in run.provider.calls().iter().enumerate() {
        let body = request.body.to_string();
        assert!(
            secrets.iter().all(|t| !body.contains(t.as_str())),
            "a token reached model request {i}"
        );
    }
    let exports = run.world.root().join("exports");
    std::fs::create_dir_all(&exports)?;
    for session in sessions {
        let resp = run
            .client
            .session_export(session, &exports.join(session), Some("dir"), None)
            .await?;
        anyhow::ensure!(resp.error.is_none(), "session.export: {:?}", resp.error);
    }
    let telemetry = run.world.telemetry_file("session.jsonl");
    live::telemetry_until(&telemetry, |events| !events.is_empty()).await;
    anyhow::ensure!(telemetry.exists(), "no telemetry was written");
    let mut leaked = world.files_with_a_token(run.world.root());
    for secret in also {
        leaked.extend(files_containing(run.world.root(), secret));
    }
    assert!(leaked.is_empty(), "a token reached {leaked:?}");
    Ok(())
}

fn files_containing(dir: &std::path::Path, needle: &str) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if String::from_utf8_lossy(&std::fs::read(&path).unwrap_or_default())
                .contains(needle)
            {
                found.push(path);
            }
        }
    }
    found
}

async fn start(name: &str, world: &OAuthWorld) -> anyhow::Result<LiveRun> {
    LiveRun::builder(name)
        .settings(json!({ "mcp_servers": { SERVER: world.config() } }))
        .start()
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_that_needs_a_sign_in_is_used_once_the_host_signs_the_user_in(
) -> anyhow::Result<()> {
    let world = OAuthWorld::start().await;
    let mut run = start("oauth-loopback", &world).await?;
    let events = run.daemon_events().await?;
    let waiting = wait_state(&mut run, "auth_required").await?;
    assert_eq!(waiting["tool_count"], 0, "{waiting}");
    assert_eq!(waiting["auth"]["state"], "required", "{waiting}");

    let session = session(&mut run).await?;
    assert_server_out_of_reach(&mut run, &world, &session).await?;

    let start = run
        .client
        .mcp_auth_start(&Target::own(), SERVER, None, None)
        .await?
        .result
        .unwrap_or_default();
    let redirect = start["redirect_uri"].as_str().unwrap_or_default();
    assert!(redirect.starts_with("http://127.0.0.1:"), "{start}");
    assert_eq!(status(&mut run).await?["auth"]["state"], "authorizing");
    let (code, page) = click(start["authorize_url"].as_str().unwrap_or_default()).await;
    assert_eq!(code, 200, "{page}");
    events.next("mcp_auth_completed", SERVER, WAIT).await?;
    let connected = events.next("mcp_connected", SERVER, WAIT).await?;
    assert_eq!(connected["tool_count"], 2, "{connected}");
    let now = wait_state(&mut run, "connected").await?;
    assert_eq!(now["auth"]["state"], "authorized", "{now}");
    assert_eq!(now["auth"]["issuer"], world.auth.url.as_str(), "{now}");

    assert_tool_works(&mut run, &world, &session, "mcp__docs__echo").await?;
    assert_eq!(world.auth.state.lock().unwrap().registrations, 1);
    let store = run
        .daemon
        .pool()
        .and_then(|p| p.mcp_auth().credential_dir().map(|d| d.to_path_buf()))
        .expect("the default store is files");
    assert!(
        store.starts_with(run.world.root()) && store.ends_with("mcp/oauth"),
        "{}",
        store.display()
    );
    assert!(
        files_containing(&store, &world.tokens()[0]).len() == 1,
        "the sign-in is in the credential store"
    );
    assert_no_token_leaked(&mut run, &world, &[&session], &[]).await?;
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn an_external_redirect_is_handed_back_by_the_host_and_signs_the_user_in(
) -> anyhow::Result<()> {
    let world = OAuthWorld::start().await;
    world.auth.preregister("host-app", None);
    let mut run = LiveRun::builder("oauth-external")
        .settings(json!({ "mcp_servers": { SERVER: world.config_with(json!({
            "client_id": "host-app", "scopes": ["mcp:read"]
        })) } }))
        .start()
        .await?;
    let events = run.daemon_events().await?;
    wait_state(&mut run, "auth_required").await?;
    let session = session(&mut run).await?;

    let back_to = "https://app.example/oauth/done";
    let start = run
        .client
        .mcp_auth_start(&Target::own(), SERVER, Some(back_to), None)
        .await?
        .result
        .unwrap_or_default();
    assert_eq!(start["redirect_uri"], back_to, "{start}");
    let callback = consent(start["authorize_url"].as_str().unwrap_or_default()).await;
    assert!(callback.starts_with(back_to), "{callback}");
    let done = run
        .client
        .mcp_auth_complete(start["flow_id"].as_str().unwrap_or_default(), &callback)
        .await?;
    assert_eq!(
        done.result.unwrap_or_default()["server"],
        SERVER,
        "{:?}",
        done.error
    );
    events.next("mcp_connected", SERVER, WAIT).await?;

    assert_tool_works(&mut run, &world, &session, "mcp__docs__echo").await?;
    assert_eq!(
        world.auth.state.lock().unwrap().registrations,
        0,
        "the configured client was used"
    );
    assert_no_token_leaked(&mut run, &world, &[&session], &[]).await?;
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_token_that_expires_mid_session_is_refreshed_under_the_models_call() -> anyhow::Result<()>
{
    let world = OAuthWorld::start().await;
    let mut run = start("oauth-refresh", &world).await?;
    let events = run.daemon_events().await?;
    wait_state(&mut run, "auth_required").await?;
    sign_in(&mut run, &events, None).await?;
    let session = session(&mut run).await?;
    assert_tool_works(&mut run, &world, &session, "mcp__docs__echo").await?;

    let refreshed = world.auth.state.lock().unwrap().refreshes;
    world.auth.expire_access_tokens();
    assert_tool_works(&mut run, &world, &session, "mcp__docs__echo").await?;
    assert!(
        world.auth.state.lock().unwrap().refreshes > refreshed,
        "the expired token was not refreshed"
    );
    let now = status(&mut run).await?;
    assert_eq!(now["auth"]["state"], "authorized", "{now}");
    assert!(
        events.pending("mcp_auth_required", SERVER).is_empty(),
        "a refresh that worked asked for a sign-in: {:?}",
        events.all()
    );
    assert_no_token_leaked(&mut run, &world, &[&session], &[]).await?;
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_refresh_takes_the_server_away_until_the_user_signs_in_again(
) -> anyhow::Result<()> {
    let world = OAuthWorld::start().await;
    let mut run = start("oauth-refused", &world).await?;
    let events = run.daemon_events().await?;
    wait_state(&mut run, "auth_required").await?;
    sign_in(&mut run, &events, None).await?;
    let session = session(&mut run).await?;
    assert_tool_works(&mut run, &world, &session, "mcp__docs__echo").await?;

    world.auth.state.lock().unwrap().reject_refresh = true;
    world.auth.expire_access_tokens();
    let failing = run.turn(&session, TRY_ECHO).await?;
    assert!(
        failing
            .tool_results
            .iter()
            .all(|(_, text, _)| !text.contains(&world.marker())),
        "a refused token was answered: {:?}",
        failing.tool_results
    );
    events.next("mcp_auth_required", SERVER, WAIT).await?;
    let now = wait_state(&mut run, "auth_required").await?;
    assert_eq!(now["auth"]["state"], "required", "{now}");
    assert_server_out_of_reach(&mut run, &world, &session).await?;

    world.auth.state.lock().unwrap().reject_refresh = false;
    sign_in(&mut run, &events, None).await?;
    assert_tool_works(&mut run, &world, &session, "mcp__docs__echo").await?;
    assert_no_token_leaked(&mut run, &world, &[&session], &[]).await?;
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tool_that_needs_more_scope_works_after_a_step_up() -> anyhow::Result<()> {
    let world = OAuthWorld::start().await;
    let mut run = start("oauth-step-up", &world).await?;
    let events = run.daemon_events().await?;
    wait_state(&mut run, "auth_required").await?;
    sign_in(&mut run, &events, Some(&["mcp:read"])).await?;
    let session = session(&mut run).await?;

    let refused = run
        .turn(
            &session,
            &TRY_ECHO.replace("mcp__docs__echo", "mcp__docs__write"),
        )
        .await?;
    assert!(
        result_of(&refused, "mcp__docs__write")
            .iter()
            .all(|(text, _)| !text.contains(&world.marker())),
        "write answered without its scope: {:?}",
        refused.tool_results
    );
    let required = events.next("mcp_auth_required", SERVER, WAIT).await?;
    let scopes = required["scopes"].to_string();
    assert!(
        scopes.contains("mcp:read") && scopes.contains("mcp:write"),
        "{required}"
    );
    let now = status(&mut run).await?;
    assert_eq!(now["state"], "connected", "the server stays: {now}");
    assert_eq!(now["auth"]["state"], "required", "{now}");

    let start = sign_in(&mut run, &events, None).await?;
    let asked = start["authorize_url"].as_str().unwrap_or_default();
    assert!(
        asked.contains("mcp%3Awrite") || asked.contains("mcp:write"),
        "{asked}"
    );
    assert_tool_works(&mut run, &world, &session, "mcp__docs__write").await?;
    assert_no_token_leaked(&mut run, &world, &[&session], &[]).await?;
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn after_a_logout_the_servers_tools_are_gone() -> anyhow::Result<()> {
    let world = OAuthWorld::start().await;
    let mut run = start("oauth-logout", &world).await?;
    let events = run.daemon_events().await?;
    wait_state(&mut run, "auth_required").await?;
    sign_in(&mut run, &events, None).await?;
    let session = session(&mut run).await?;
    assert_tool_works(&mut run, &world, &session, "mcp__docs__echo").await?;

    let out = run.client.mcp_auth_logout(&Target::own(), SERVER).await?;
    assert!(out.error.is_none(), "{:?}", out.error);
    assert!(
        !world.auth.state.lock().unwrap().revoked.is_empty(),
        "nothing was revoked"
    );
    let now = wait_state(&mut run, "auth_required").await?;
    assert_eq!(now["tool_count"], 0, "{now}");
    assert_server_out_of_reach(&mut run, &world, &session).await?;
    assert_no_token_leaked(&mut run, &world, &[&session], &[]).await?;
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_credentials_server_is_used_with_nobody_signing_in() -> anyhow::Result<()> {
    let world = OAuthWorld::start().await;
    world.auth.preregister("batch", Some("batch-secret"));
    let mut run = LiveRun::builder("oauth-cc")
        .settings(json!({ "mcp_servers": { SERVER: world.config_with(json!({
            "grant": "client_credentials", "client_id": "batch",
            "client_secret_env": "AGIX_LIVE_MCP_SECRET", "scopes": ["mcp:read"]
        })) } }))
        .mode(Mode::Spawned)
        .options(|o| o.env("AGIX_LIVE_MCP_SECRET", "batch-secret"))
        .start()
        .await?;
    let now = wait_state(&mut run, "connected").await?;
    assert_eq!(now["auth"]["state"], "authorized", "{now}");
    let refused = run
        .client
        .mcp_auth_start(&Target::own(), SERVER, None, None)
        .await?;
    assert_eq!(
        refused.error.map(|e| e.code),
        Some(-32602),
        "there is nothing to sign in to"
    );

    let session = session(&mut run).await?;
    assert_tool_works(&mut run, &world, &session, "mcp__docs__echo").await?;
    let requests = world.auth_requests();
    assert!(
        requests.iter().all(|r| !r.contains("/authorize")),
        "{requests:?}"
    );
    assert!(world.auth.state.lock().unwrap().count("client_credentials") >= 1);
    assert_no_token_leaked(&mut run, &world, &[&session], &["batch-secret"]).await?;
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn an_enterprise_identity_from_the_host_connects_the_server() -> anyhow::Result<()> {
    let identity = format!("id-token-{}", std::process::id());
    let world = OAuthWorld::start()
        .await
        .with_enterprise_idp("agix-at-idp", &identity, "ema-client")
        .await;
    let scratch = tempfile::tempdir()?;
    let policy = scratch.path().join("mcp-policy.json");
    std::fs::write(
        &policy,
        world.enterprise_idp_policy("agix-at-idp").to_string(),
    )?;
    let mut run = LiveRun::builder("oauth-enterprise")
        .settings(json!({ "mcp_servers": { SERVER: world.config_with(json!({
            "grant": "enterprise", "client_id": "ema-client", "scopes": ["mcp:read"]
        })) } }))
        .options(move |o| o.mcp_policy(policy.clone()))
        .start()
        .await?;
    let events = run.daemon_events().await?;
    let waiting = wait_state(&mut run, "auth_required").await?;
    assert!(
        waiting["auth"]["error"]
            .as_str()
            .unwrap_or_default()
            .contains("mcp.auth.setIdentity"),
        "{waiting}"
    );
    let session = session(&mut run).await?;
    assert_server_out_of_reach(&mut run, &world, &session).await?;

    let set = run.client.mcp_auth_set_identity(&identity).await?;
    assert_eq!(
        set.result.unwrap_or_default()["reconnected"],
        json!([SERVER]),
        "{:?}",
        set.error
    );
    events.next("mcp_connected", SERVER, WAIT).await?;
    assert_tool_works(&mut run, &world, &session, "mcp__docs__echo").await?;
    let auth = world
        .auth
        .state
        .lock()
        .unwrap()
        .count(live::oauth::JWT_BEARER);
    assert_eq!(auth, 1, "one ID-JAG was exchanged for the server token");
    assert_no_token_leaked(&mut run, &world, &[&session], &[&identity]).await?;
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_forged_callback_is_turned_away_and_the_real_one_still_signs_in() -> anyhow::Result<()> {
    let world = OAuthWorld::start().await;
    let mut run = start("oauth-forged", &world).await?;
    let events = run.daemon_events().await?;
    wait_state(&mut run, "auth_required").await?;
    let session = session(&mut run).await?;

    let start = run
        .client
        .mcp_auth_start(&Target::own(), SERVER, None, None)
        .await?
        .result
        .unwrap_or_default();
    let callback = consent(start["authorize_url"].as_str().unwrap_or_default()).await;
    let mut forged = url::Url::parse(&callback)?;
    let pairs: Vec<(String, String)> = forged
        .query_pairs()
        .map(|(k, v)| {
            let v = if k == "state" {
                "forged".to_string()
            } else {
                v.into_owned()
            };
            (k.into_owned(), v)
        })
        .collect();
    forged.query_pairs_mut().clear().extend_pairs(pairs);
    let (code, page) = follow(forged.as_str()).await;
    assert_eq!(code, 400, "{page}");
    assert_eq!(status(&mut run).await?["auth"]["state"], "authorizing");
    assert_server_out_of_reach(&mut run, &world, &session).await?;

    let (code, page) = follow(&callback).await;
    assert_eq!(code, 200, "{page}");
    events.next("mcp_connected", SERVER, WAIT).await?;
    assert_tool_works(&mut run, &world, &session, "mcp__docs__echo").await?;
    assert_no_token_leaked(&mut run, &world, &[&session], &[]).await?;
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sign_in_left_too_long_expires_and_cannot_be_completed() -> anyhow::Result<()> {
    let world = OAuthWorld::start().await;
    let mut run = start("oauth-expired", &world).await?;
    let events = run.daemon_events().await?;
    wait_state(&mut run, "auth_required").await?;
    let auth = run
        .daemon
        .pool()
        .expect("a daemon in this process")
        .mcp_auth();
    auth.set_flow_ttl(Duration::from_millis(300));
    let back_to = "https://app.example/oauth/done";
    let start = run
        .client
        .mcp_auth_start(&Target::own(), SERVER, Some(back_to), None)
        .await?
        .result
        .unwrap_or_default();
    let failed = events.next("mcp_auth_failed", SERVER, WAIT).await?;
    assert!(
        failed["error"]
            .as_str()
            .unwrap_or_default()
            .contains("not completed in time"),
        "{failed}"
    );
    assert_eq!(status(&mut run).await?["auth"]["state"], "failed");
    let callback = consent(start["authorize_url"].as_str().unwrap_or_default()).await;
    let late = run
        .client
        .mcp_auth_complete(start["flow_id"].as_str().unwrap_or_default(), &callback)
        .await?;
    assert_eq!(late.error.map(|e| e.code), Some(-32602));
    let session = session(&mut run).await?;
    assert_server_out_of_reach(&mut run, &world, &session).await?;

    auth.set_flow_ttl(Duration::from_secs(600));
    sign_in(&mut run, &events, None).await?;
    assert_tool_works(&mut run, &world, &session, "mcp__docs__echo").await?;
    assert_no_token_leaked(&mut run, &world, &[&session], &[]).await?;
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn an_authorization_server_outside_the_policy_is_never_sent_the_user() -> anyhow::Result<()> {
    let world = OAuthWorld::start().await;
    let scratch = tempfile::tempdir()?;
    let policy = scratch.path().join("mcp-policy.json");
    std::fs::write(
        &policy,
        json!({ "allowed_authorization_servers": ["https://login.corp.example"] }).to_string(),
    )?;
    let mut run = LiveRun::builder("oauth-policy")
        .settings(json!({ "mcp_servers": { SERVER: world.config() } }))
        .options(move |o| o.mcp_policy(policy.clone()))
        .start()
        .await?;
    wait_state(&mut run, "auth_required").await?;
    let refused = run
        .client
        .mcp_auth_start(&Target::own(), SERVER, None, None)
        .await?;
    let error = refused.error.expect("the sign-in is refused");
    assert_eq!(error.code, -32020, "{}", error.message);
    assert!(
        error.message.contains("allowed_authorization_servers"),
        "{}",
        error.message
    );
    let requests = world.auth_requests();
    assert!(
        requests.iter().all(|r| r.contains("/.well-known/")),
        "the authorization server was sent more than a metadata read: {requests:?}"
    );
    let session = session(&mut run).await?;
    assert_server_out_of_reach(&mut run, &world, &session).await?;
    assert!(world.tokens().is_empty(), "a token was issued");
    run.finish().await;
    Ok(())
}
