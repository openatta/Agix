//! One session over all three transports of a spawned daemon, with a real
//! model: created over the Unix socket, a turn over TCP, the next over
//! WebSocket reading what the first wrote, the history the same from all
//! three; a wrong token is refused on both network listeners, and so is a
//! browser page from another origin.

use std::net::SocketAddr;
use std::time::Duration;

use live::{LiveRun, Mode};
use rpc_client::DaemonRpcClient;
use serde_json::{json, Value};

const TOKEN: &str = "live-transport-token";

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

fn free_port() -> anyhow::Result<SocketAddr> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?)
}

async fn turn_over(
    client: &mut DaemonRpcClient,
    session: &str,
    turn_id: &str,
    message: &str,
) -> anyhow::Result<rpc_client::TurnEvents> {
    let events = tokio::time::timeout(
        Duration::from_secs(300),
        client.run_turn_with(json!({
            "session_id": session,
            "turn_id": turn_id,
            "message": message,
        })),
    )
    .await??;
    if let Some(error) = events.response.as_ref().and_then(|r| r.error.as_ref()) {
        anyhow::bail!("{turn_id} over {}: {error:?}", client.transport_name());
    }
    Ok(events)
}

async fn history_total(client: &mut DaemonRpcClient, session: &str) -> anyhow::Result<Value> {
    let resp = client
        .call(
            "session.history",
            json!({ "session_id": session, "offset": 0, "limit": 1 }),
        )
        .await?;
    Ok(resp.result.unwrap_or_default()["total"].clone())
}

#[tokio::test(flavor = "multi_thread")]
async fn one_session_over_unix_tcp_and_websocket() -> anyhow::Result<()> {
    let (tcp, ws) = (free_port()?, free_port()?);
    let mut run = LiveRun::builder("transport")
        .mode(Mode::Spawned)
        .options(move |o| {
            o.arg("--listen")
                .arg(tcp.to_string())
                .arg("--listen-ws")
                .arg(ws.to_string())
                .arg("--token")
                .arg(TOKEN)
        })
        .start()
        .await?;
    let session = run.session().await?;

    let mut over_tcp = DaemonRpcClient::connect_tcp(tcp, TOKEN).await?;
    let auth = over_tcp.daemon_status().await?;
    assert!(auth.error.is_none(), "{auth:?}");
    let word = format!("TCP-{}", nonce());
    turn_over(
        &mut over_tcp,
        &session,
        "transport-tcp",
        &format!("Write the word {word} to tcp.txt with the Write tool, then reply DONE."),
    )
    .await?;
    assert_eq!(
        run.read_file("tcp.txt").map(|s| s.trim().to_string()),
        Some(word.clone())
    );

    let mut over_ws = DaemonRpcClient::connect_ws(ws, TOKEN, None).await?;
    let events = turn_over(
        &mut over_ws,
        &session,
        "transport-ws",
        "Read tcp.txt and write exactly its content followed by -WS to ws.txt with the Write \
         tool, then reply DONE.",
    )
    .await?;
    assert!(
        events.events.iter().any(|e| e["kind"] == "text_delta"),
        "the turn streamed nothing over WebSocket"
    );
    assert_eq!(
        run.read_file("ws.txt")
            .map(|s| s.split_whitespace().collect::<String>()),
        Some(format!("{word}-WS"))
    );

    let totals = [
        history_total(&mut run.client, &session).await?,
        history_total(&mut over_tcp, &session).await?,
        history_total(&mut over_ws, &session).await?,
    ];
    assert!(totals[0].as_u64().unwrap_or(0) >= 4, "{totals:?}");
    assert!(totals.iter().all(|t| *t == totals[0]), "{totals:?}");

    for refused in [
        DaemonRpcClient::connect_tcp(tcp, "not-the-token").await,
        DaemonRpcClient::connect_ws(ws, "not-the-token", None).await,
        DaemonRpcClient::connect_ws(ws, TOKEN, Some("https://evil.example")).await,
    ] {
        assert!(refused.is_err(), "a bad handshake was let in");
    }
    let mut raw = rpc_client::DaemonRpcClient::with_transport(Box::new(
        rpc_client::transport::tcp(tcp).await?,
    ));
    if let Ok(resp) = raw.call("session.list", Value::Null).await {
        assert_eq!(
            resp.error.map(|e| e.code),
            Some(-32003),
            "no handshake, yet answered"
        );
    }
    run.finish().await;
    Ok(())
}
