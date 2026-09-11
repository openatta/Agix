//! The model, as a server.
//!
//! A daemon in another process cannot be handed an `Arc<dyn AnthropicClient>`,
//! so the only seam that works for both modes is the one the deployment
//! already has: `ANTHROPIC_BASE_URL`. This is a server to point it at — it
//! speaks the Anthropic Messages SSE wire, answers from a script written in
//! the test, and keeps every request it was sent.
//!
//! That last part is why it is worth the wire-level work. What reaches the
//! model is the only place a script binding, a prompt block or a withdrawn
//! tool becomes visible from outside the engine, and going through HTTP means
//! the request under observation is the one that really left — headers,
//! serialization, SSE decoding and the retry loop included, none of which an
//! injected client exercises.
//!
//! # This is not a cassette
//!
//! The script is Rust written in the test, not traffic recorded from a
//! provider, so it is indifferent to prompt wording. Keep it that way:
//! assert on the few fields a case is about and never diff a whole request.
//! A stub that compares everything goes stale on every prompt edit, which is
//! exactly the failure `test_runner::scripted_model` exists to avoid.
//!
//! # Ordering
//!
//! One queue, first come first served. Two sessions running turns at the same
//! time consume from it in whatever order their requests arrive, so a
//! scenario that needs a particular reply to reach a particular session must
//! keep its turns sequential.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// What every scripted answer reports having spent. Fixed and distinct, so a
/// case can tell an input count from an output one at a glance.
pub const INPUT_TOKENS: u64 = 11;
pub const OUTPUT_TOKENS: u64 = 7;
pub const CACHE_CREATION_TOKENS: u64 = 5;
pub const CACHE_READ_TOKENS: u64 = 23;

/// One content block of a scripted answer.
pub enum Block {
    Text(String),
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
}

impl Block {
    pub fn text(s: impl Into<String>) -> Self {
        Self::Text(s.into())
    }

    pub fn tool(id: impl Into<String>, name: impl Into<String>, input: serde_json::Value) -> Self {
        Self::ToolUse {
            id: id.into(),
            name: name.into(),
            input,
        }
    }
}

/// What the stub does with the next request it receives.
pub enum Reply {
    /// A successful stream of these blocks. `stop_reason` follows from the
    /// blocks: a tool call ends the message with `tool_use`, anything else
    /// with `end_turn`.
    Blocks(Vec<Block>),
    /// An HTTP failure — the shape the engine's retry and fallback paths key
    /// on, and one no real provider can be asked for on demand.
    Status {
        code: u16,
        body: String,
        headers: Vec<(String, String)>,
    },
}

impl Reply {
    pub fn text(s: impl Into<String>) -> Self {
        Self::Blocks(vec![Block::text(s)])
    }

    pub fn calls(id: &str, name: &str, input: serde_json::Value) -> Self {
        Self::Blocks(vec![Block::tool(id, name, input)])
    }

    pub fn status(code: u16, body: impl Into<String>) -> Self {
        Self::Status {
            code,
            body: body.into(),
            headers: Vec::new(),
        }
    }

    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        if let Self::Status { headers, .. } = &mut self {
            headers.push((name.to_string(), value.to_string()));
        }
        self
    }
}

/// A reply queue and the text a request must carry to draw on it.
struct RoutedQueue {
    needle: String,
    replies: VecDeque<Reply>,
}

/// One request the stub was sent.
#[derive(Clone, Debug)]
pub struct SeenRequest {
    pub path: String,
    pub body: serde_json::Value,
}

impl SeenRequest {
    /// The system prompt as one string. Blocks are joined so a case can ask
    /// whether a mark is in the prompt without caring which block carries it.
    pub fn system_text(&self) -> String {
        self.body
            .get("system")
            .and_then(|v| v.as_array())
            .map(|blocks| {
                blocks
                    .iter()
                    .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default()
    }

    pub fn tool_names(&self) -> Vec<String> {
        self.body
            .get("tools")
            .and_then(|v| v.as_array())
            .map(|tools| {
                tools
                    .iter()
                    .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn message_count(&self) -> usize {
        self.body
            .get("messages")
            .and_then(|v| v.as_array())
            .map(Vec::len)
            .unwrap_or(0)
    }

    /// Every string anywhere in `messages`, joined — what the conversation
    /// says, without a case having to walk the block union to find it.
    pub fn messages_text(&self) -> String {
        let mut out = String::new();
        collect_strings(
            self.body
                .get("messages")
                .unwrap_or(&serde_json::Value::Null),
            &mut out,
        );
        out
    }
}

fn collect_strings(v: &serde_json::Value, out: &mut String) {
    match v {
        serde_json::Value::String(s) => {
            out.push_str(s);
            out.push('\n');
        }
        serde_json::Value::Array(a) => a.iter().for_each(|x| collect_strings(x, out)),
        serde_json::Value::Object(o) => o.values().for_each(|x| collect_strings(x, out)),
        _ => {}
    }
}

#[derive(Clone)]
pub struct ProviderStub {
    base_url: url::Url,
    script: Arc<Mutex<VecDeque<Reply>>>,
    /// Queues that only answer requests they recognize — see
    /// [`ProviderStub::script_when`]. Ordered, and searched before the
    /// unconditional queue.
    routed: Arc<Mutex<Vec<RoutedQueue>>>,
    seen: Arc<Mutex<Vec<SeenRequest>>>,
}

impl ProviderStub {
    pub async fn start() -> anyhow::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let stub = Self {
            base_url: url::Url::parse(&format!("http://127.0.0.1:{port}/"))?,
            script: Arc::new(Mutex::new(VecDeque::new())),
            routed: Arc::new(Mutex::new(Vec::new())),
            seen: Arc::new(Mutex::new(Vec::new())),
        };

        let serving = stub.clone();
        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                let stub = serving.clone();
                tokio::spawn(async move {
                    let _ = stub.serve_one(sock).await;
                });
            }
        });
        Ok(stub)
    }

    /// What to put in `ANTHROPIC_BASE_URL`, or hand to
    /// `HttpAnthropicClient::with_base`.
    pub fn base_url(&self) -> url::Url {
        self.base_url.clone()
    }

    /// Queue answers. Appends, so a scenario can add the next turn's replies
    /// after asserting on the last one's.
    pub fn script(&self, replies: impl IntoIterator<Item = Reply>) {
        self.script.lock().unwrap().extend(replies);
    }

    /// Queue answers for requests that carry `needle` anywhere in them.
    ///
    /// One queue per session is the only way to script two turns that run at
    /// the same time: [`script`](Self::script) is a single FIFO, so with two
    /// turns in flight the order replies come out in is the order the
    /// requests happened to arrive, and a turn that takes two calls can be
    /// handed the other turn's second reply. A `needle` that identifies the
    /// session — a mark in its project's prompt, say — makes each turn's
    /// script its own.
    ///
    /// Searched in the order queues were added, and only queues that still
    /// have a reply left are considered; a request matching none falls
    /// through to the unconditional queue.
    pub fn script_when(&self, needle: &str, replies: impl IntoIterator<Item = Reply>) {
        let mut routed = self.routed.lock().unwrap();
        match routed.iter_mut().find(|q| q.needle == needle) {
            Some(queue) => queue.replies.extend(replies),
            None => routed.push(RoutedQueue {
                needle: needle.to_string(),
                replies: replies.into_iter().collect(),
            }),
        }
    }

    pub fn seen(&self) -> Vec<SeenRequest> {
        self.seen.lock().unwrap().clone()
    }

    /// The requests that carried a conversation — `/v1/messages` and nothing
    /// else, so a stray probe cannot shift the indices a case counts on.
    pub fn calls(&self) -> Vec<SeenRequest> {
        self.seen()
            .into_iter()
            .filter(|r| r.path.ends_with("/v1/messages"))
            .collect()
    }

    pub fn call_count(&self) -> usize {
        self.calls().len()
    }

    /// The reply this request gets: from the first routed queue that
    /// recognizes it and still has one, else from the unconditional queue.
    fn next_reply_for(&self, body: &[u8]) -> Option<Reply> {
        let text = String::from_utf8_lossy(body);
        let mut routed = self.routed.lock().unwrap();
        for queue in routed.iter_mut() {
            if !queue.replies.is_empty() && text.contains(queue.needle.as_str()) {
                return queue.replies.pop_front();
            }
        }
        drop(routed);
        self.script.lock().unwrap().pop_front()
    }

    async fn serve_one(&self, mut sock: tokio::net::TcpStream) -> anyhow::Result<()> {
        let Some((path, body)) = read_request(&mut sock).await? else {
            return Ok(());
        };
        self.seen.lock().unwrap().push(SeenRequest {
            path,
            body: serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null),
        });

        let reply = self.next_reply_for(&body);
        let response = match reply {
            Some(Reply::Status {
                code,
                body,
                headers,
            }) => http_response(code, &headers, &body),
            Some(Reply::Blocks(blocks)) => sse_response(&blocks),
            // A turn that outran its script is a real failure, and the mark
            // says so in whatever the test was going to assert on anyway.
            None => sse_response(&[Block::text("PROVIDER-STUB-SCRIPT-EXHAUSTED")]),
        };
        sock.write_all(response.as_bytes()).await?;
        sock.flush().await?;
        let _ = sock.shutdown().await;
        Ok(())
    }
}

/// Read one HTTP request: the head, then exactly `content-length` bytes.
async fn read_request(
    sock: &mut tokio::net::TcpStream,
) -> anyhow::Result<Option<(String, Vec<u8>)>> {
    let mut buf = Vec::new();
    let head_end = loop {
        if let Some(at) = find(&buf, b"\r\n\r\n") {
            break at + 4;
        }
        let mut chunk = [0u8; 4096];
        let n = sock.read(&mut chunk).await?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&chunk[..n]);
    };

    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let path = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("/")
        .to_string();
    let len: usize = head
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
        .and_then(|l| l.split(':').nth(1))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);

    let mut body = buf[head_end..].to_vec();
    while body.len() < len {
        let mut chunk = [0u8; 4096];
        let n = sock.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(len);
    Ok(Some((path, body)))
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn http_response(code: u16, headers: &[(String, String)], body: &str) -> String {
    let mut out = format!("HTTP/1.1 {code} X\r\n");
    for (name, value) in headers {
        out.push_str(&format!("{name}: {value}\r\n"));
    }
    out.push_str("content-type: text/plain\r\n");
    out.push_str(&format!("content-length: {}\r\n", body.len()));
    out.push_str("connection: close\r\n\r\n");
    out.push_str(body);
    out
}

/// No content-length: the body ends when the connection does, which is how a
/// streaming response is framed and what lets the deltas below be a stream
/// rather than one blob the client sees all at once.
fn sse_response(blocks: &[Block]) -> String {
    let mut out = String::from(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\nconnection: close\r\n\r\n",
    );

    // Usage is split the way the real wire splits it: the input count
    // arrives in `message_start` and only the output count is repeated in
    // `message_delta`. A stub that put both in one event would let a reader
    // that ignores half of them look correct.
    out.push_str(&event(&serde_json::json!({
        "type": "message_start",
        "message": {
            "id": "msg_stub",
            "model": "claude-sonnet-4-6",
            "role": "assistant",
            "usage": {
                "input_tokens": INPUT_TOKENS,
                "output_tokens": 0,
                "cache_creation_input_tokens": CACHE_CREATION_TOKENS,
                "cache_read_input_tokens": CACHE_READ_TOKENS
            }
        }
    })));

    let mut calls_a_tool = false;
    for (index, block) in blocks.iter().enumerate() {
        match block {
            Block::Text(text) => {
                out.push_str(&event(&serde_json::json!({
                    "type": "content_block_start",
                    "index": index,
                    "content_block": { "type": "text", "text": "" }
                })));
                for piece in halves(text) {
                    out.push_str(&event(&serde_json::json!({
                        "type": "content_block_delta",
                        "index": index,
                        "delta": { "type": "text_delta", "text": piece }
                    })));
                }
            }
            Block::ToolUse { id, name, input } => {
                calls_a_tool = true;
                out.push_str(&event(&serde_json::json!({
                    "type": "content_block_start",
                    "index": index,
                    "content_block": { "type": "tool_use", "id": id, "name": name, "input": {} }
                })));
                let json = input.to_string();
                for piece in halves(&json) {
                    out.push_str(&event(&serde_json::json!({
                        "type": "content_block_delta",
                        "index": index,
                        "delta": { "type": "input_json_delta", "partial_json": piece }
                    })));
                }
            }
        }
        out.push_str(&event(&serde_json::json!({
            "type": "content_block_stop",
            "index": index
        })));
    }

    out.push_str(&event(&serde_json::json!({
        "type": "message_delta",
        "delta": { "stop_reason": if calls_a_tool { "tool_use" } else { "end_turn" } },
        "usage": { "output_tokens": OUTPUT_TOKENS }
    })));
    out.push_str(&event(&serde_json::json!({ "type": "message_stop" })));
    out
}

/// Split into two deltas, so a client that only works when a block arrives
/// whole fails here rather than against a real provider.
fn halves(s: &str) -> Vec<String> {
    if s.len() < 2 {
        return vec![s.to_string()];
    }
    let mut at = s.len() / 2;
    while at > 0 && !s.is_char_boundary(at) {
        at -= 1;
    }
    if at == 0 {
        return vec![s.to_string()];
    }
    vec![s[..at].to_string(), s[at..].to_string()]
}

fn event(payload: &serde_json::Value) -> String {
    let kind = payload
        .get("type")
        .and_then(|t| t.as_str())
        .unwrap_or("message");
    format!("event: {kind}\ndata: {payload}\n\n")
}
