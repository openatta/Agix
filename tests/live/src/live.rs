//! A real model, behind a recording proxy.
//!
//! [`LiveProvider`] is the real-model counterpart of
//! [`ProviderStub`](crate::ProviderStub): a local HTTP server the daemon is
//! pointed at exactly as it is pointed at the stub, which forwards every
//! request to a real Anthropic-compatible endpoint and streams the answer
//! back. Forwarding rather than handing the daemon the endpoint keeps two
//! things the stub tier relies on: every request is recorded, so a case can
//! still ask what reached the model (which model a sub-agent asked for, say),
//! and the credential never leaves this process — the daemon is given a
//! dummy key, and the proxy swaps the real one in.
//!
//! The credential comes from [`LiveConfig::load`]: the workspace's `.env`
//! (git-ignored), then the process environment. A run is capped at
//! `AGIX_LIVE_MAX_CALLS` model calls; past it the proxy answers with an error
//! instead of spending, so a case that loops costs a bounded amount.
//!
//! [`LiveRun`] is the scaffolding every live case repeats: a world, one
//! project with permissive settings, the proxy, a daemon, a client, and turns
//! bounded by a timeout. [`Watcher`] is the host's second pair of eyes — a
//! connection that only watches a session, and answers its questions when
//! told to. Assertions belong to the cases, and they should be about
//! outcomes — files, task states, frames, values only a real run can
//! produce — never about the model's wording.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rpc_client::{DaemonRpcClient, TurnEvents};
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

use crate::provider::{http_response, read_request, RawRequest, SeenRequest};
use crate::{Daemon, DaemonOptions, Mode, ModelEndpoint, World};

type HostScenes = Vec<Arc<dyn base::interface::scene::Scene>>;

const DEFAULT_MAX_CALLS: usize = 400;
const DEFAULT_TURN_TIMEOUT_SECS: u64 = 900;
const RESPONSE_KEEP_BYTES: usize = 1 << 20;

/// Where the real model is and how to reach it.
#[derive(Clone)]
pub struct LiveConfig {
    pub base_url: String,
    token: String,
    /// The model sessions use unless told otherwise.
    pub model: String,
    /// Every model a case may hand out — `AGIX_LIVE_MODELS`, comma-separated,
    /// else just [`model`](Self::model).
    pub models: Vec<String>,
    pub max_calls: usize,
    pub turn_timeout: Duration,
}

impl std::fmt::Debug for LiveConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveConfig")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("models", &self.models)
            .field("max_calls", &self.max_calls)
            .finish_non_exhaustive()
    }
}

impl LiveConfig {
    /// `AGIX_LIVE_ENV_FILE` (default: the workspace's `.env`) first, then the
    /// process environment. The file wins because a shell this runs from may
    /// well carry `ANTHROPIC_*` variables meant for something else.
    pub fn load() -> anyhow::Result<Self> {
        let file = std::env::var("AGIX_LIVE_ENV_FILE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| workspace_root().join(".env"));
        let from_file = std::fs::read_to_string(&file)
            .map(|text| parse_env_file(&text))
            .unwrap_or_default();
        let get = |key: &str| {
            from_file
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
                .or_else(|| std::env::var(key).ok())
                .filter(|v| !v.is_empty())
        };

        let missing = |what: &str| {
            anyhow::anyhow!(
                "no {what} for the live tier: put it in {} or the environment",
                file.display()
            )
        };
        let base_url = get("ANTHROPIC_BASE_URL").ok_or_else(|| missing("ANTHROPIC_BASE_URL"))?;
        let token = get("ANTHROPIC_AUTH_TOKEN")
            .or_else(|| get("ANTHROPIC_API_KEY"))
            .ok_or_else(|| missing("ANTHROPIC_AUTH_TOKEN"))?;
        let model = get("AGIX_LIVE_MODEL")
            .or_else(|| get("ANTHROPIC_MODEL"))
            .ok_or_else(|| missing("ANTHROPIC_MODEL"))?;
        let mut models: Vec<String> = get("AGIX_LIVE_MODELS")
            .map(|list| {
                list.split(',')
                    .map(|m| m.trim().to_string())
                    .filter(|m| !m.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        if !models.contains(&model) {
            models.insert(0, model.clone());
        }
        if let Some(pro) = models.iter().find(|m| is_pro_model(m)) {
            anyhow::bail!("live tier runs on Flash only, but `{pro}` is configured");
        }
        let max_calls = get("AGIX_LIVE_MAX_CALLS")
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_MAX_CALLS);
        let turn_timeout = Duration::from_secs(
            get("AGIX_LIVE_TURN_TIMEOUT_SECS")
                .and_then(|v| v.parse().ok())
                .unwrap_or(DEFAULT_TURN_TIMEOUT_SECS),
        );
        Ok(Self {
            base_url,
            token,
            model,
            models,
            max_calls,
            turn_timeout,
        })
    }

    /// The `i`-th model, wrapping — a case that wants "a different model per
    /// sub-agent" gets as many distinct ones as are configured.
    pub fn model_at(&self, i: usize) -> &str {
        &self.models[i % self.models.len()]
    }
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from("."))
}

fn parse_env_file(text: &str) -> Vec<(String, String)> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| l.strip_prefix("export ").unwrap_or(l).split_once('='))
        .map(|(k, v)| {
            (
                k.trim().to_string(),
                v.trim().trim_matches('"').trim_matches('\'').to_string(),
            )
        })
        .collect()
}

/// One request the proxy forwarded, and what came back.
#[derive(Clone, Debug)]
pub struct Exchange {
    pub request: SeenRequest,
    pub status: u16,
    /// The response body as received, truncated past a megabyte.
    pub response: String,
    /// When the request arrived, relative to the proxy's start.
    pub at: Duration,
    pub elapsed: Duration,
}

/// What one response said it cost. `input` excludes the two cache counts, as
/// the Messages API reports it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tokens {
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output: u64,
}

impl std::ops::AddAssign for Tokens {
    fn add_assign(&mut self, o: Self) {
        self.input += o.input;
        self.cache_read += o.cache_read;
        self.cache_write += o.cache_write;
        self.output += o.output;
    }
}

impl Tokens {
    /// Everything the model read on the call, cached or not.
    pub fn prompt(&self) -> u64 {
        self.input + self.cache_read + self.cache_write
    }
}

impl Exchange {
    /// Input and output tokens the response reported.
    pub fn usage(&self) -> (u64, u64) {
        let t = self.tokens();
        (t.input, t.output)
    }

    /// How many tool calls the response asked for, streamed or not.
    pub fn tool_uses(&self) -> usize {
        if let Ok(whole) = serde_json::from_str::<Value>(&self.response) {
            return whole["content"]
                .as_array()
                .map_or(0, |c| c.iter().filter(|b| b["type"] == "tool_use").count());
        }
        self.response
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .filter_map(|data| serde_json::from_str::<Value>(data.trim()).ok())
            .filter(|event| {
                event["type"] == "content_block_start"
                    && event["content_block"]["type"] == "tool_use"
            })
            .count()
    }

    pub fn tokens(&self) -> Tokens {
        let mut t = Tokens::default();
        let mut take = |usage: &Value| {
            let n = |k: &str| usage[k].as_u64().unwrap_or(0);
            t.input = t.input.max(n("input_tokens"));
            t.cache_read = t.cache_read.max(n("cache_read_input_tokens"));
            t.cache_write = t.cache_write.max(n("cache_creation_input_tokens"));
            t.output = t.output.max(n("output_tokens"));
        };
        if let Ok(whole) = serde_json::from_str::<Value>(&self.response) {
            if let Some(usage) = whole.get("usage") {
                take(usage);
            }
        }
        for line in self.response.lines() {
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let Ok(event) = serde_json::from_str::<Value>(data.trim()) else {
                continue;
            };
            if let Some(usage) = event
                .pointer("/message/usage")
                .or_else(|| event.get("usage"))
            {
                take(usage);
            }
        }
        t
    }
}

/// DeepSeek's Anthropic endpoint serves `claude-opus*` names on its Pro model,
/// so a name does not have to say "pro" to be billed as one.
pub fn is_pro_model(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    m.contains("pro") || m.starts_with("claude-opus")
}

#[derive(Clone)]
pub struct LiveProvider {
    local: url::Url,
    config: LiveConfig,
    started: Instant,
    exchanges: Arc<Mutex<Vec<Exchange>>>,
    calls: Arc<AtomicUsize>,
}

impl ModelEndpoint for LiveProvider {
    fn base_url(&self) -> url::Url {
        self.local.clone()
    }

    fn model(&self) -> &str {
        &self.config.model
    }
}

impl LiveProvider {
    pub async fn start(config: LiveConfig) -> anyhow::Result<Self> {
        base::interface::exec::local::install_tls_provider();
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(600))
            .build()?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let provider = Self {
            local: url::Url::parse(&format!("http://127.0.0.1:{port}/"))?,
            config,
            started: Instant::now(),
            exchanges: Arc::new(Mutex::new(Vec::new())),
            calls: Arc::new(AtomicUsize::new(0)),
        };

        let serving = provider.clone();
        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                let provider = serving.clone();
                let http = http.clone();
                tokio::spawn(async move {
                    let _ = provider.forward_one(sock, &http).await;
                });
            }
        });
        Ok(provider)
    }

    pub fn config(&self) -> &LiveConfig {
        &self.config
    }

    pub fn exchanges(&self) -> Vec<Exchange> {
        self.exchanges.lock().unwrap().clone()
    }

    /// The requests that carried a conversation, as with the stub.
    pub fn calls(&self) -> Vec<SeenRequest> {
        self.exchanges()
            .into_iter()
            .map(|e| e.request)
            .filter(|r| r.path.ends_with("/v1/messages"))
            .collect()
    }

    /// Calls, input tokens and output tokens so far.
    pub fn spend(&self) -> (usize, u64, u64) {
        let exchanges = self.exchanges();
        let (input, output) = exchanges
            .iter()
            .map(Exchange::usage)
            .fold((0, 0), |(a, b), (i, o)| (a + i, b + o));
        (exchanges.len(), input, output)
    }

    async fn forward_one(
        &self,
        mut sock: tokio::net::TcpStream,
        http: &reqwest::Client,
    ) -> anyhow::Result<()> {
        let Some(RawRequest {
            path,
            headers,
            body,
        }) = read_request(&mut sock).await?
        else {
            return Ok(());
        };
        let at = self.started.elapsed();
        let seen = SeenRequest {
            path: path.clone(),
            body: serde_json::from_slice(&body).unwrap_or(Value::Null),
        };

        if let Some(model) = seen.body["model"].as_str().filter(|m| is_pro_model(m)) {
            let body = json!({
                "type": "error",
                "error": {
                    "type": "invalid_request_error",
                    "message": format!("live tier runs on Flash only; refused model `{model}`")
                }
            })
            .to_string();
            self.record(seen, 400, body.clone(), at);
            sock.write_all(http_response(400, &[], &body).as_bytes())
                .await?;
            return Ok(());
        }

        if self.calls.fetch_add(1, Ordering::SeqCst) >= self.config.max_calls {
            let message = format!(
                "live call budget of {} exhausted (AGIX_LIVE_MAX_CALLS)",
                self.config.max_calls
            );
            let body = json!({
                "type": "error",
                "error": { "type": "invalid_request_error", "message": message }
            })
            .to_string();
            self.record(seen, 400, body.clone(), at);
            sock.write_all(http_response(400, &[], &body).as_bytes())
                .await?;
            return Ok(());
        }

        let url = format!("{}{}", self.config.base_url.trim_end_matches('/'), path);
        let mut request = http.post(url).body(body);
        for (name, value) in &headers {
            if !matches!(
                name.as_str(),
                "host"
                    | "content-length"
                    | "connection"
                    | "accept-encoding"
                    | "x-api-key"
                    | "authorization"
            ) {
                request = request.header(name.as_str(), value.as_str());
            }
        }
        request = request.header("x-api-key", &self.config.token);

        let mut response = match request.send().await {
            Ok(response) => response,
            Err(e) => {
                let body = format!("live proxy could not reach the endpoint: {e}");
                self.record(seen, 502, body.clone(), at);
                sock.write_all(http_response(502, &[], &body).as_bytes())
                    .await?;
                return Ok(());
            }
        };

        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/json")
            .to_string();
        let mut head = format!("HTTP/1.1 {status} X\r\ncontent-type: {content_type}\r\n");
        if let Some(retry) = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
        {
            head.push_str(&format!("retry-after: {retry}\r\n"));
        }
        head.push_str("connection: close\r\n\r\n");
        sock.write_all(head.as_bytes()).await?;

        let mut kept = Vec::new();
        while let Ok(Some(chunk)) = response.chunk().await {
            if kept.len() < RESPONSE_KEEP_BYTES {
                kept.extend_from_slice(&chunk);
            }
            if sock.write_all(&chunk).await.is_err() {
                break;
            }
        }
        let _ = sock.flush().await;
        let _ = sock.shutdown().await;
        self.record(
            seen,
            status,
            String::from_utf8_lossy(&kept).into_owned(),
            at,
        );
        Ok(())
    }

    fn record(&self, request: SeenRequest, status: u16, response: String, at: Duration) {
        self.exchanges.lock().unwrap().push(Exchange {
            request,
            status,
            response,
            at,
            elapsed: self.started.elapsed().saturating_sub(at),
        });
    }
}

/// One live case's world, project, model, daemon and connection.
pub struct LiveRun {
    pub world: World,
    pub project: PathBuf,
    pub provider: LiveProvider,
    pub daemon: Daemon,
    pub client: DaemonRpcClient,
    name: String,
    turns: usize,
    mode: Mode,
    scenes: HostScenes,
}

impl LiveRun {
    /// Settings every live project starts from: no memory (it would make one
    /// run's answers depend on the last), no permission prompts (nobody is
    /// there to answer them).
    pub fn base_settings() -> Value {
        json!({
            "memory_enabled": false,
            "permission_mode": "bypassPermissions"
        })
    }

    /// `settings` is merged over [`base_settings`](Self::base_settings), one
    /// level deep.
    pub async fn start(name: &str, settings: Value, mode: Mode) -> anyhow::Result<Self> {
        Self::start_with(name, settings, mode, Vec::new()).await
    }

    /// [`start`](Self::start) on a daemon in this process that also serves
    /// `scenes`, as an embedder's does.
    pub async fn start_with_scenes(
        name: &str,
        settings: Value,
        scenes: HostScenes,
    ) -> anyhow::Result<Self> {
        Self::start_with(name, settings, Mode::InProcess, scenes).await
    }

    async fn start_with(
        name: &str,
        settings: Value,
        mode: Mode,
        scenes: HostScenes,
    ) -> anyhow::Result<Self> {
        let config = LiveConfig::load()?;
        let world = World::new()?;
        let project = world.project(name)?;
        let mut merged = Self::base_settings();
        if let (Some(base), Some(extra)) = (merged.as_object_mut(), settings.as_object()) {
            for (k, v) in extra {
                match (base.get_mut(k), v) {
                    (Some(Value::Object(b)), Value::Object(e)) => {
                        b.extend(e.iter().map(|(k, v)| (k.clone(), v.clone())));
                    }
                    _ => {
                        base.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        world.write_project_settings(name, &merged)?;
        let provider = LiveProvider::start(config).await?;
        let daemon = Daemon::start(
            &world,
            &provider,
            DaemonOptions::new(project.clone())
                .mode(mode)
                .scenes(scenes.clone()),
        )
        .await?;
        let client = daemon.connect().await?;
        eprintln!(
            "[live {name}] {mode:?} daemon, model {}, models {:?}",
            provider.config().model,
            provider.config().models
        );
        Ok(Self {
            world,
            project,
            provider,
            daemon,
            client,
            name: name.to_string(),
            turns: 0,
            mode,
            scenes,
        })
    }

    /// Stop the daemon and start another on the same world and model, as a
    /// host restarting its engine does: what comes back is what is on disk.
    pub async fn restart(&mut self) -> anyhow::Result<()> {
        self.daemon.halt().await;
        self.daemon = Daemon::start(
            &self.world,
            &self.provider,
            DaemonOptions::new(self.project.clone())
                .mode(self.mode)
                .scenes(self.scenes.clone()),
        )
        .await?;
        self.client = self.daemon.connect().await?;
        Ok(())
    }

    /// A second connection watching `session`; with `approve`, it answers
    /// every question with a permit.
    pub async fn watch(&self, session: &str, approve: bool) -> anyhow::Result<Watcher> {
        Watcher::start(&self.daemon, session, approve).await
    }

    pub fn config(&self) -> &LiveConfig {
        self.provider.config()
    }

    pub fn write_file(&self, relative: &str, contents: &str) -> anyhow::Result<PathBuf> {
        self.world
            .write_project_file(&self.name, relative, contents)
    }

    pub fn read_file(&self, relative: &str) -> Option<String> {
        std::fs::read_to_string(self.project.join(relative)).ok()
    }

    pub async fn session(&mut self) -> anyhow::Result<String> {
        self.session_with(json!({})).await
    }

    /// A session on this run's project; `extra` is merged into the
    /// `session.create` params.
    pub async fn session_with(&mut self, extra: Value) -> anyhow::Result<String> {
        let mut params = json!({ "project_root": self.project.to_string_lossy() });
        if let (Some(p), Some(e)) = (params.as_object_mut(), extra.as_object()) {
            p.extend(e.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
        self.client.session_create(params).await
    }

    /// One turn, bounded by `AGIX_LIVE_TURN_TIMEOUT_SECS`. An RPC error is
    /// returned as an error, so a case cannot mistake a failed turn for an
    /// answer.
    pub async fn turn(&mut self, session: &str, message: &str) -> anyhow::Result<TurnEvents> {
        self.turns += 1;
        let turn_id = format!("{}-t{}", self.name, self.turns);
        let timeout = self.config().turn_timeout;
        let started = Instant::now();
        let events = tokio::time::timeout(
            timeout,
            self.client
                .session_run_turn(session, message, &turn_id, None),
        )
        .await
        .map_err(|_| anyhow::anyhow!("turn {turn_id} still running after {timeout:?}"))??;
        if let Some(error) = events.response.as_ref().and_then(|r| r.error.as_ref()) {
            anyhow::bail!("turn {turn_id} failed: {} {}", error.code, error.message);
        }
        let tools: Vec<&str> = events.tool_uses.iter().map(|(n, _)| n.as_str()).collect();
        eprintln!(
            "[live {}] {turn_id} {:.1}s tools={tools:?}\n  > {}",
            self.name,
            started.elapsed().as_secs_f64(),
            events.text.trim().replace('\n', "\n    ")
        );
        Ok(events)
    }

    /// `session.tasks`, as the list.
    pub async fn tasks(&mut self, session: &str) -> anyhow::Result<Vec<Value>> {
        let resp = self
            .client
            .call("session.tasks", json!({ "session_id": session }))
            .await?;
        Ok(resp.result.unwrap_or_default()["tasks"]
            .as_array()
            .cloned()
            .unwrap_or_default())
    }

    /// Poll until no task of the session is still running or waiting.
    pub async fn wait_tasks_settled(
        &mut self,
        session: &str,
        timeout: Duration,
    ) -> anyhow::Result<Vec<Value>> {
        let deadline = Instant::now() + timeout;
        loop {
            let tasks = self.tasks(session).await?;
            if tasks.iter().all(|t| {
                t["status"] != "running" && (t["status"] != "waiting" || t["kind"] == "cron")
            }) {
                return Ok(tasks);
            }
            anyhow::ensure!(
                Instant::now() < deadline,
                "tasks still running after {timeout:?}: {tasks:?}"
            );
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    pub async fn finish(self) {
        let (calls, input, output) = self.provider.spend();
        eprintln!(
            "[live {}] {calls} model calls, {input} input tokens, {output} output tokens",
            self.name
        );
        self.daemon.stop().await;
    }
}

/// A question a session put to its host, and the session's task list as the
/// host saw it on reading the question.
#[derive(Clone, Debug)]
pub struct Asked {
    /// The `session.event` params of the `prompt` frame.
    pub prompt: Value,
    pub tasks: Vec<Value>,
}

/// A connection that only watches a session — what a host's monitoring view
/// is. It keeps every `session.event` it is sent, in order; one that approves
/// answers each `prompt` with a permit, after reading `session.tasks` over a
/// connection of its own.
pub struct Watcher {
    seen: Arc<Mutex<Vec<Value>>>,
    asked: Arc<Mutex<Vec<Asked>>>,
    reader: tokio::task::JoinHandle<()>,
}

impl Watcher {
    pub async fn start(daemon: &Daemon, session: &str, approve: bool) -> anyhow::Result<Self> {
        let mut watcher = daemon.connect().await?;
        let subscribed = watcher.session_subscribe(session).await?;
        anyhow::ensure!(subscribed.error.is_none(), "{subscribed:?}");
        let mut answerer = daemon.connect().await?;
        let (seen, asked) = (
            Arc::<Mutex<Vec<Value>>>::default(),
            Arc::<Mutex<Vec<Asked>>>::default(),
        );
        let (into_seen, into_asked, session) = (seen.clone(), asked.clone(), session.to_string());
        let reader = tokio::spawn(async move {
            while let Ok(Some(frame)) = watcher.next_frame().await {
                if frame["method"] != "session.event" {
                    continue;
                }
                let params = frame["params"].clone();
                lock(&into_seen).push(params.clone());
                if !approve || params["event"]["kind"] != "prompt" {
                    continue;
                }
                let tasks = answerer
                    .call("session.tasks", json!({ "session_id": session }))
                    .await
                    .ok()
                    .and_then(|r| r.result)
                    .and_then(|r| r["tasks"].as_array().cloned())
                    .unwrap_or_default();
                lock(&into_asked).push(Asked {
                    prompt: params.clone(),
                    tasks,
                });
                let _ = answerer
                    .call(
                        "session.respondToPrompt",
                        json!({
                            "session_id": session,
                            "prompt_id": params["event"]["prompt_id"],
                            "decision": { "type": "permit" }
                        }),
                    )
                    .await;
            }
        });
        Ok(Self {
            seen,
            asked,
            reader,
        })
    }

    /// Every frame so far.
    pub fn frames(&self) -> Vec<Value> {
        lock(&self.seen).clone()
    }

    /// The frames so far whose event is a `kind`.
    pub fn events(&self, kind: &str) -> Vec<Value> {
        lock(&self.seen)
            .iter()
            .filter(|f| f["event"]["kind"] == kind)
            .cloned()
            .collect()
    }

    pub fn asked(&self) -> Vec<Asked> {
        lock(&self.asked).clone()
    }

    /// The first frame whose event satisfies `wanted`, waiting up to
    /// `timeout` for it.
    pub async fn wait_for(
        &self,
        what: &str,
        timeout: Duration,
        wanted: impl Fn(&Value) -> bool,
    ) -> anyhow::Result<Value> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(frame) = lock(&self.seen).iter().find(|f| wanted(&f["event"])) {
                return Ok(frame.clone());
            }
            anyhow::ensure!(Instant::now() < deadline, "no {what} within {timeout:?}");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exchange(response: &str) -> Exchange {
        Exchange {
            request: SeenRequest {
                path: "/v1/messages".into(),
                body: Value::Null,
            },
            status: 200,
            response: response.into(),
            at: Duration::ZERO,
            elapsed: Duration::ZERO,
        }
    }

    #[test]
    fn tool_uses_are_counted_in_streamed_and_whole_responses() {
        let streamed = "\
event: content_block_start
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\"}}

event: content_block_start
data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"a\",\"name\":\"Read\"}}

event: content_block_start
data: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"tool_use\",\"id\":\"b\",\"name\":\"Grep\"}}
";
        assert_eq!(exchange(streamed).tool_uses(), 2);

        let whole = r#"{"content":[{"type":"text","text":"x"},{"type":"tool_use","id":"a","name":"Bash","input":{}}]}"#;
        assert_eq!(exchange(whole).tool_uses(), 1);
        assert_eq!(exchange("").tool_uses(), 0);
    }
}
