//! A session's limits, against a scripted model: the per-connection cap on
//! requests in flight, the pool's capacity and idle evictions, a subscriber
//! too slow to keep up (its session subscription dropped, its daemon events
//! lagged), retries on an overloaded endpoint, the endings the engine
//! recovers from (`max_tokens`, `pause_turn`), thinking that streams apart
//! from the answer and goes back with its signature, a stream cut short, and
//! an interrupt while the model has not answered at all or a compaction
//! summary is still being written, and a closed session whose engine has not
//! stopped yet being resumed.
//!
//! The live tier cannot ask for any of these on demand; here the model's side
//! is held fixed so the product's side is all that varies.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use live::{Block, LiveConfig, LiveRun, LiveRunBuilder, Mode, ProviderStub, Purpose, Reply};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

fn stubbed(name: &str, stub: &ProviderStub) -> LiveRunBuilder {
    LiveRun::builder(name).config(LiveConfig::for_endpoint(
        &stub.base_url(),
        live::ModelEndpoint::model(stub),
    ))
}

const OVERLOADED: &str =
    "{\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"busy\"}}";

/// Every `daemon.event` a second connection is pushed, in order.
async fn daemon_events(
    run: &LiveRun,
) -> anyhow::Result<(Arc<Mutex<Vec<Value>>>, tokio::task::JoinHandle<()>)> {
    let mut subscriber = run.daemon.connect().await?;
    let subscribed = subscriber
        .call("daemon.subscribeEvents", Value::Null)
        .await?;
    anyhow::ensure!(subscribed.error.is_none(), "{subscribed:?}");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let into = seen.clone();
    let reader = tokio::spawn(async move {
        while let Ok(Some(frame)) = subscriber.next_frame().await {
            if frame["method"] == "daemon.event" {
                into.lock().unwrap().push(frame["params"].clone());
            }
        }
    });
    Ok((seen, reader))
}

async fn wait_for_event(
    seen: &Mutex<Vec<Value>>,
    timeout: Duration,
    wanted: impl Fn(&Value) -> bool,
) -> Option<Value> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(e) = seen.lock().unwrap().iter().find(|e| wanted(e)) {
            return Some(e.clone());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    None
}

/// A connection spoken to line by line, so a case can have many requests on
/// it at once, or stop reading it altogether.
struct RawConnection {
    lines: tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    write: tokio::net::unix::OwnedWriteHalf,
}

impl RawConnection {
    async fn open(run: &LiveRun) -> anyhow::Result<Self> {
        let (read, write) = tokio::net::UnixStream::connect(run.daemon.socket())
            .await?
            .into_split();
        Ok(Self {
            lines: BufReader::new(read).lines(),
            write,
        })
    }

    async fn send(&mut self, requests: &[Value]) -> anyhow::Result<()> {
        let mut batch = String::new();
        for request in requests {
            batch.push_str(&request.to_string());
            batch.push('\n');
        }
        self.write.write_all(batch.as_bytes()).await?;
        Ok(())
    }

    /// Frames until `done` says stop, or nothing arrives for `quiet`.
    async fn read_until(
        &mut self,
        quiet: Duration,
        mut done: impl FnMut(&[Value]) -> bool,
    ) -> anyhow::Result<Vec<Value>> {
        let mut frames = Vec::new();
        while !done(&frames) {
            match tokio::time::timeout(quiet, self.lines.next_line()).await {
                Ok(Ok(Some(line))) => frames.push(serde_json::from_str(&line)?),
                Ok(Ok(None)) | Err(_) => break,
                Ok(Err(e)) => return Err(e.into()),
            }
        }
        Ok(frames)
    }
}

fn request(id: i64, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

fn response(frames: &[Value], id: i64) -> Option<&Value> {
    frames
        .iter()
        .find(|f| f["id"] == id && (f.get("result").is_some() || f.get("error").is_some()))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_connection_past_sixty_four_requests_in_flight_is_refused_not_queued(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("in-flight", &stub).start().await?;
    let session = run.session().await?;
    stub.script([Reply::delayed(Duration::from_secs(4), Reply::text("first"))]);
    let first = run.spawn_turn(&session, "hold", json!({})).await?;
    run.wait_turn_state(&session, "running", Duration::from_secs(10))
        .await?;

    let mut raw = RawConnection::open(&run).await?;
    let mut requests: Vec<Value> = (1..=64)
        .map(|id| {
            request(
                id,
                "session.run_turn",
                json!({
                    "session_id": session,
                    "message": format!("queued {id}"),
                    "on_busy": "queue",
                }),
            )
        })
        .collect();
    requests.push(request(65, "daemon.ping", Value::Null));
    raw.send(&requests).await?;
    let frames = raw
        .read_until(Duration::from_secs(3), |f| response(f, 65).is_some())
        .await?;
    let refused = response(&frames, 65).expect("the 65th request was answered");
    assert_eq!(refused["error"]["code"], -32017, "{refused}");
    assert!(
        frames
            .iter()
            .filter(|f| f["id"].as_i64().is_some_and(|id| id <= 64))
            .all(|f| f.get("result").is_none() && f.get("error").is_none()),
        "a queued turn finished while the first was still running"
    );

    first.await??;
    let frames = raw
        .read_until(Duration::from_secs(30), |f| {
            (1..=64).all(|id| response(f, id).is_some())
        })
        .await?;
    for id in 1..=64 {
        let answer = response(&frames, id).unwrap_or(&Value::Null);
        assert!(answer.get("result").is_some(), "queued turn {id}: {answer}");
    }
    raw.send(&[request(66, "daemon.ping", Value::Null)]).await?;
    let frames = raw
        .read_until(Duration::from_secs(5), |f| response(f, 66).is_some())
        .await?;
    let pong = response(&frames, 66).expect("a ping once the queue drained");
    assert!(pong.get("result").is_some(), "{pong}");
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_full_pool_evicts_its_least_recent_idle_session_which_comes_back_on_use(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("capacity", &stub)
        .options(|o| o.session_cap(2))
        .start()
        .await?;
    let (seen, _reader) = daemon_events(&run).await?;
    let oldest = run.session().await?;
    stub.script([Reply::text("OLDEST-ANSWER")]);
    run.turn(&oldest, "OLDEST-QUESTION").await?;
    let middle = run.session().await?;
    stub.script([Reply::text("middle")]);
    run.turn(&middle, "middle").await?;
    let newest = run.session().await?;

    let evicted = wait_for_event(&seen, Duration::from_secs(10), |e| {
        e["kind"] == "session_evicted"
    })
    .await
    .expect("a session_evicted event");
    assert_eq!(evicted["session_id"], oldest.as_str(), "{evicted}");
    assert_eq!(evicted["reason"], "capacity");
    let got = run
        .client
        .session_get(&oldest)
        .await?
        .result
        .unwrap_or_default();
    assert_eq!(got["status"], "inactive", "{got}");

    stub.script([Reply::text("back")]);
    run.turn(&oldest, "again").await?;
    let last = stub.calls().last().cloned().expect("a call");
    assert!(
        last.messages_contain("OLDEST-QUESTION") && last.messages_contain("OLDEST-ANSWER"),
        "the evicted session came back without its history"
    );
    assert!(
        wait_for_event(&seen, Duration::from_secs(5), |e| {
            e["kind"] == "session_resumed" && e["session_id"] == oldest.as_str()
        })
        .await
        .is_some(),
        "no session_resumed for the evicted session"
    );
    assert!(
        seen.lock()
            .unwrap()
            .iter()
            .all(|e| e["kind"] != "session_evicted" || e["session_id"] != newest.as_str()),
        "the newest session was evicted"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "spawns a real daemon process"]
async fn an_idle_session_is_evicted_and_resumes_with_its_history() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("idle", &stub)
        .mode(Mode::Spawned)
        .options(|o| o.arg("--session-idle-timeout").arg("1"))
        .start()
        .await?;
    let (seen, _reader) = daemon_events(&run).await?;
    let session = run.session().await?;
    stub.script([Reply::text("IDLE-ANSWER")]);
    run.turn(&session, "IDLE-QUESTION").await?;
    let evicted = wait_for_event(&seen, Duration::from_secs(70), |e| {
        e["kind"] == "session_evicted" && e["session_id"] == session.as_str()
    })
    .await
    .expect("the idle session was never evicted");
    assert_eq!(evicted["reason"], "idle");
    stub.script([Reply::text("back")]);
    run.turn(&session, "again").await?;
    let last = stub.calls().last().cloned().expect("a call");
    assert!(last.messages_contain("IDLE-QUESTION") && last.messages_contain("IDLE-ANSWER"));
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reader_that_stops_reading_loses_its_subscription_and_is_told_events_lagged(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("slow-reader", &stub).start().await?;
    let session = run.session().await?;

    let mut slow = RawConnection::open(&run).await?;
    slow.send(&[
        request(1, "daemon.subscribeEvents", Value::Null),
        request(2, "session.subscribe", json!({ "session_id": session })),
    ])
    .await?;
    let acks = slow
        .read_until(Duration::from_secs(5), |f| {
            response(f, 1).is_some() && response(f, 2).is_some()
        })
        .await?;
    assert!(response(&acks, 2).is_some_and(|r| r.get("result").is_some()));

    let blocks: Vec<Block> = (0..3000)
        .map(|i| Block::text(format!("chunk {i:04} ")))
        .collect();
    stub.script([Reply::Blocks(blocks)]);
    run.turn(&session, "flood").await?;
    for _ in 0..1500 {
        run.turn(&session, "/help").await?;
    }

    let frames = slow.read_until(Duration::from_secs(2), |_| false).await?;
    let session_frame = |f: &Value| f["method"] == "session.event";
    let dropped_at = frames
        .iter()
        .position(|f| session_frame(f) && f["params"]["event"]["kind"] == "subscription_dropped")
        .expect("a subscription_dropped frame");
    assert_eq!(
        frames[dropped_at]["params"]["event"]["reason"], "overflow",
        "{}",
        frames[dropped_at]
    );
    assert!(
        frames[dropped_at + 1..].iter().all(|f| !session_frame(f)),
        "session frames kept coming after the drop"
    );
    let lagged = frames
        .iter()
        .find(|f| f["method"] == "daemon.event" && f["params"]["kind"] == "events_lagged")
        .expect("an events_lagged daemon event");
    assert!(
        lagged["params"]["missed"].as_u64().unwrap_or(0) > 0,
        "{lagged}"
    );

    slow.send(&[request(
        3,
        "session.subscribe",
        json!({ "session_id": session }),
    )])
    .await?;
    let again = slow
        .read_until(Duration::from_secs(5), |f| response(f, 3).is_some())
        .await?;
    assert!(response(&again, 3).is_some_and(|r| r.get("result").is_some()));
    stub.script([Reply::text("AFTER-RESUBSCRIBE")]);
    run.turn(&session, "after").await?;
    let after = slow.read_until(Duration::from_secs(2), |_| false).await?;
    assert!(
        after.iter().any(|f| session_frame(f)
            && f["params"]["event"]["kind"] == "text_delta"
            && f["params"]["event"]["text"]
                .as_str()
                .is_some_and(|t| !t.is_empty())),
        "the resubscribed connection saw nothing of the next turn"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn overloaded_answers_are_retried_with_backoff_and_a_bad_request_is_not() -> anyhow::Result<()>
{
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("retry", &stub).start().await?;
    let session = run.session().await?;

    stub.inject_failures(2, Reply::status(529, OVERLOADED));
    stub.script([Reply::text("THIRD-TIME-LUCKY")]);
    let started = Instant::now();
    let events = run.turn(&session, "one").await?;
    let waited = started.elapsed();
    assert!(events.text.contains("THIRD-TIME-LUCKY"));
    assert_eq!(stub.call_count(), 3, "two failures, one success");
    assert!(
        waited >= Duration::from_millis(2_200),
        "retried without backing off: {waited:?}"
    );

    stub.inject_failures(
        1,
        Reply::status(
            400,
            "{\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\"message\":\"BAD-REQUEST-MARK\"}}",
        ),
    );
    stub.script([Reply::text("never asked")]);
    let failed = run
        .client
        .run_turn_with(json!({ "session_id": session, "message": "two" }))
        .await?;
    let error = failed.response.and_then(|r| r.error).expect("an error");
    assert_eq!(error.code, -32002, "{error:?}");
    assert_eq!(stub.call_count(), 4, "a 400 was retried");

    let events = run.turn(&session, "three").await?;
    assert!(events.text.contains("never asked"), "the session is usable");
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_max_tokens_stop_is_retried_with_a_higher_limit_then_nudged() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("max-tokens", &stub).start().await?;
    let session = run.session().await?;
    stub.script([
        Reply::ended(vec![Block::text("cut one")], "max_tokens"),
        Reply::ended(vec![Block::text("cut two")], "max_tokens"),
        Reply::text("WHOLE-ANSWER"),
    ]);
    let events = run.turn(&session, "write a lot").await?;
    let calls = stub.calls();
    assert_eq!(calls.len(), 3, "{:?}", events.events);
    let limit = |i: usize| calls[i].body["max_tokens"].as_u64().unwrap_or(0);
    assert!(limit(0) < 64_000, "first call at {}", limit(0));
    assert_eq!(limit(1), 64_000, "first escalation");
    assert!(
        calls[2].messages_contain("Output token limit hit"),
        "the third call carries the nudge"
    );
    assert!(!calls[1].messages_contain("Output token limit hit"));
    assert!(events.text.contains("WHOLE-ANSWER"));
    let complete = events.turn_complete_event.clone().unwrap_or_default();
    assert_eq!(complete["stop_reason"], "end_turn", "{complete}");
    assert_eq!(complete["api_calls"], 3);
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pause_turn_stop_is_continued_in_the_same_turn() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("pause-turn", &stub).start().await?;
    let session = run.session().await?;
    stub.script([
        Reply::ended(vec![Block::text("PAUSED-PART ")], "pause_turn"),
        Reply::text("RESUMED-PART"),
    ]);
    let events = run.turn(&session, "search for a while").await?;
    let calls = stub.calls();
    assert_eq!(calls.len(), 2, "the paused turn was not continued");
    assert!(
        calls[1].messages_contain("PAUSED-PART"),
        "the paused response went back"
    );
    assert!(events.text.contains("RESUMED-PART"));
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn thinking_streams_apart_from_the_answer_and_goes_back_with_its_signature(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("thinking", &stub).start().await?;
    let session = run.session().await?;
    stub.script([
        Reply::Blocks(vec![
            Block::thinking("PONDERING-THE-FILE"),
            Block::tool("b1", "Bash", json!({ "command": "echo hi" })),
        ]),
        Reply::Blocks(vec![
            Block::thinking("SECOND-THOUGHT"),
            Block::text("ANSWER-TEXT"),
        ]),
    ]);
    let events = run.turn(&session, "think, then run it").await?;
    let thought: String = events
        .events
        .iter()
        .filter(|e| e["kind"] == "thinking_delta")
        .filter_map(|e| e["text"].as_str())
        .collect();
    assert!(
        thought.contains("PONDERING-THE-FILE") && thought.contains("SECOND-THOUGHT"),
        "{thought}"
    );
    assert!(!events.text.contains("PONDERING") && !events.text.contains("SECOND-THOUGHT"));
    assert!(events.text.contains("ANSWER-TEXT"));

    let second = &stub.calls()[1];
    let thinking_sent: Vec<&Value> = second.body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["role"] == "assistant")
        .flat_map(|m| m["content"].as_array().into_iter().flatten())
        .filter(|b| b["type"] == "thinking")
        .collect();
    assert_eq!(thinking_sent.len(), 1, "{}", second.body["messages"]);
    assert_eq!(thinking_sent[0]["thinking"], "PONDERING-THE-FILE");
    assert_eq!(thinking_sent[0]["signature"], "stub-signature");
    run.finish().await;
    Ok(())
}

/// A stream that breaks off after `message_start` and before any block.
fn cut_stream() -> Reply {
    Reply::cut(420, Reply::text(format!("NEVER-WHOLE {}", "x".repeat(400))))
}

async fn cut_turn(
    run: &mut LiveRun,
    stub: &ProviderStub,
    session: &str,
) -> anyhow::Result<rpc_client::TurnEvents> {
    stub.inject_failures(8, cut_stream());
    let events = tokio::time::timeout(
        Duration::from_secs(90),
        run.client
            .run_turn_with(json!({ "session_id": session, "message": "during" })),
    )
    .await??;
    stub.inject_failures(0, Reply::text("unused"));
    run.wait_turn_state(session, "idle", Duration::from_secs(10))
        .await?;
    Ok(events)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stream_cut_short_fails_the_turn() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("cut-fails", &stub).start().await?;
    let session = run.session().await?;
    let failed = cut_turn(&mut run, &stub, &session).await?;
    let error = failed.response.and_then(|r| r.error);
    assert!(
        error.as_ref().is_some_and(|e| e.code == -32002),
        "after {} cut streams: {error:?} {:?}",
        stub.call_count(),
        failed.events
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_session_carries_on_after_a_stream_cut_short() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("cut", &stub).start().await?;
    let session = run.session().await?;
    stub.script([Reply::text("WHOLE-BEFORE")]);
    run.turn(&session, "before").await?;
    let cut = cut_turn(&mut run, &stub, &session).await?;
    assert!(!cut.text.contains("NEVER-WHOLE"), "{}", cut.text);

    stub.script([Reply::text("AFTER-THE-CUT")]);
    let events = run.turn(&session, "after").await?;
    assert!(events.text.contains("AFTER-THE-CUT"), "{}", events.text);
    let last = stub.calls().last().cloned().expect("a call");
    assert!(
        last.messages_contain("WHOLE-BEFORE"),
        "history survived the cut"
    );
    let messages = last.body["messages"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    for m in &messages {
        let empty = match &m["content"] {
            Value::String(s) => s.is_empty(),
            Value::Array(blocks) => blocks.is_empty(),
            _ => true,
        };
        assert!(!empty, "an empty message went back to the model: {m}");
    }
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn an_interrupt_ends_a_turn_whose_model_has_not_answered_yet() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("slow-model", &stub).start().await?;
    let session = run.session().await?;
    stub.script([Reply::delayed(
        Duration::from_secs(30),
        Reply::text("TOO-LATE"),
    )]);
    let handle = run
        .spawn_turn(&session, "take your time", json!({}))
        .await?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while stub.call_count() == 0 {
        anyhow::ensure!(Instant::now() < deadline, "the model was never called");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let interrupted_at = Instant::now();
    let resp = run
        .client
        .call("session.interrupt", json!({ "session_id": session }))
        .await?;
    assert_eq!(resp.result.unwrap_or_default()["interrupted"], json!(true));
    let events = tokio::time::timeout(Duration::from_secs(40), handle).await???;
    assert!(
        interrupted_at.elapsed() < Duration::from_secs(5),
        "the turn waited out the model: {:?}",
        interrupted_at.elapsed()
    );
    assert!(!events.text.contains("TOO-LATE"));
    run.finish().await;
    Ok(())
}

/// The turn outgrows the threshold and compacts with a model-written summary
/// that is slow to come; an interrupt then ends the turn without waiting for
/// it, and the conversation is left as it was.
#[tokio::test(flavor = "multi_thread")]
async fn an_interrupt_ends_a_turn_whose_compaction_summary_has_not_come_back() -> anyhow::Result<()>
{
    const SUMMARY: &str = "You are compacting an AI coding agent's conversation";
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("slow-summary", &stub)
        .settings(json!({ "compaction": { "threshold_tokens": 22000, "keep_recent": 2 } }))
        .start()
        .await?;
    let mut script = Vec::new();
    for i in 1..=4 {
        let body: String = (0..600)
            .map(|l| format!("part {i} line {l}: routine reading, all values nominal\n"))
            .collect();
        let path = run.write_file(&format!("part{i}.log"), &body)?;
        script.push(Reply::calls(
            &format!("p{i}"),
            "Read",
            json!({ "file_path": path.to_string_lossy() }),
        ));
    }
    script.push(Reply::text("DONE"));
    stub.script(script);
    stub.script_when(
        SUMMARY,
        [Reply::delayed(
            Duration::from_secs(60),
            Reply::text("SUMMARY-TOO-LATE"),
        )],
    );
    let session = run.session().await?;
    let watcher = run.watch(&session, false).await?;
    let handle = run
        .spawn_turn(&session, "read the four parts", json!({}))
        .await?;
    let deadline = Instant::now() + Duration::from_secs(30);
    while !stub.calls().iter().any(|c| c.purpose() == Purpose::Compact) {
        anyhow::ensure!(
            Instant::now() < deadline,
            "the turn never asked for a summary"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let interrupted_at = Instant::now();
    let resp = run
        .client
        .call("session.interrupt", json!({ "session_id": session }))
        .await?;
    assert_eq!(resp.result.unwrap_or_default()["interrupted"], json!(true));
    tokio::time::timeout(Duration::from_secs(70), handle).await???;
    assert!(
        interrupted_at.elapsed() < Duration::from_secs(5),
        "the turn waited out the summary: {:?}",
        interrupted_at.elapsed()
    );
    assert_eq!(watcher.events("compact"), Vec::<Value>::new());
    let history = run
        .client
        .call(
            "session.history",
            json!({ "session_id": session, "limit": 100 }),
        )
        .await?
        .result
        .unwrap_or_default()
        .to_string();
    assert!(
        history.contains("part 1 line 0:") && !history.contains("SUMMARY-TOO-LATE"),
        "the conversation was changed: {}",
        &history[..history.len().min(2000)]
    );
    drop(watcher);
    run.finish().await;
    Ok(())
}

/// A `SessionStart` hook that has not returned does not hold the engine once
/// the session is closed: the hook runs on in the background, the engine
/// stops, and the session resumes on a new one straight away.
#[tokio::test(flavor = "multi_thread")]
async fn a_closed_session_resumes_while_its_session_start_hook_still_runs() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let scratch = tempfile::tempdir()?;
    let (started, release) = (
        scratch.path().join("started"),
        scratch.path().join("release"),
    );
    let hook = scratch.path().join("hold.sh");
    std::fs::write(
        &hook,
        format!(
            "#!/bin/sh\ncat >/dev/null\nif [ ! -e '{started}' ]; then\n  touch '{started}'\n  \
             while [ ! -e '{release}' ]; do sleep 0.05; done\nfi\necho '{{}}'\n",
            started = started.display(),
            release = release.display(),
        ),
    )?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755))?;
    }
    let mut run = stubbed("stuck-engine", &stub)
        .settings(json!({
            "hooks_config": {
                "SessionStart": [{ "type": "command", "command": hook, "timeout": 120_000 }]
            }
        }))
        .start()
        .await?;
    let session = run.session().await?;
    let deadline = Instant::now() + Duration::from_secs(20);
    while !started.exists() {
        anyhow::ensure!(Instant::now() < deadline, "the SessionStart hook never ran");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let closed = run.client.session_close(&session).await?;
    anyhow::ensure!(closed.error.is_none(), "{closed:?}");
    let resumed = run
        .client
        .call("session.resume", json!({ "session_id": session }))
        .await?;
    assert_eq!(
        resumed.result.as_ref().map(|r| r["status"].clone()),
        Some(json!("resumed")),
        "{resumed:?}"
    );
    std::fs::write(&release, "")?;
    run.finish().await;
    Ok(())
}
