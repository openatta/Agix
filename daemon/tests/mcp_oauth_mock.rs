//! A mock OAuth authorization server and a mock streamable-HTTP MCP server
//! that only answers with a token it issued — enough of RFC 9728, RFC 8414,
//! RFC 7591, PKCE, refresh rotation, RFC 7009 revocation and the MCP
//! lifecycle to drive `mcp::oauth` end to end. Plus a stand-in for the user's
//! browser.
//!
//! Each server is a bare HTTP/1.1 loop on 127.0.0.1 answering one request per
//! connection; everything they saw and issued is in shared state the tests
//! read and steer.

#![cfg(feature = "mcp")]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use base64::Engine;
use sha2::Digest;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

pub struct Request {
    pub method: String,
    pub path: String,
    pub query: HashMap<String, String>,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn form(&self) -> HashMap<String, String> {
        url::form_urlencoded::parse(&self.body)
            .into_owned()
            .collect()
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or(serde_json::Value::Null)
    }

    pub fn bearer(&self) -> Option<String> {
        self.headers
            .get("authorization")?
            .strip_prefix("Bearer ")
            .map(str::to_string)
    }

    fn basic(&self) -> Option<(String, String)> {
        let encoded = self.headers.get("authorization")?.strip_prefix("Basic ")?;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .ok()?;
        let text = String::from_utf8(decoded).ok()?;
        let (id, secret) = text.split_once(':')?;
        let unescape = |s: &str| {
            url::form_urlencoded::parse(format!("x={s}").as_bytes())
                .next()
                .map(|(_, v)| v.into_owned())
                .unwrap_or_default()
        };
        Some((unescape(id), unescape(secret)))
    }
}

pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn json(status: u16, body: serde_json::Value) -> Self {
        Self {
            status,
            headers: vec![("content-type".into(), "application/json".into())],
            body: body.to_string().into_bytes(),
        }
    }

    pub fn empty(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
}

type Handler = Arc<dyn Fn(Request) -> Response + Send + Sync>;

async fn serve(handler: Handler) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let handler = handler.clone();
            tokio::spawn(async move {
                let Some(request) = read_request(&mut stream).await else {
                    return;
                };
                let response = handler(request);
                let reason = match response.status {
                    200 => "OK",
                    201 => "Created",
                    202 => "Accepted",
                    302 => "Found",
                    401 => "Unauthorized",
                    403 => "Forbidden",
                    _ => "Status",
                };
                let mut head = format!("HTTP/1.1 {} {reason}\r\n", response.status);
                for (k, v) in &response.headers {
                    head.push_str(&format!("{k}: {v}\r\n"));
                }
                head.push_str(&format!(
                    "content-length: {}\r\nconnection: close\r\n\r\n",
                    response.body.len()
                ));
                let _ = stream.write_all(head.as_bytes()).await;
                let _ = stream.write_all(&response.body).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    addr
}

async fn read_request(stream: &mut tokio::net::TcpStream) -> Option<Request> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.lines();
    let mut first = lines.next()?.split(' ');
    let method = first.next()?.to_string();
    let target = first.next()?.to_string();
    let headers: HashMap<String, String> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let length: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < length {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
    Some(Request {
        method,
        path: path.to_string(),
        query: url::form_urlencoded::parse(query.as_bytes())
            .into_owned()
            .collect(),
        headers,
        body,
    })
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[derive(Clone)]
pub struct Client {
    pub secret: Option<String>,
    pub redirect_uris: Vec<String>,
}

struct Code {
    client_id: String,
    redirect_uri: String,
    challenge: String,
    scopes: Vec<String>,
    resource: String,
}

#[derive(Clone)]
pub struct Token {
    pub scopes: Vec<String>,
    pub expires_at: u64,
    pub resource: String,
}

/// What the authorization server is told to do, and what it saw.
pub struct AsState {
    pub issuer: String,
    pub resource: String,
    pub advertise_s256: bool,
    pub dynamic_registration: bool,
    pub revocation: bool,
    pub token_ttl: u64,
    pub reject_refresh: bool,
    pub clients: HashMap<String, Client>,
    codes: HashMap<String, Code>,
    pub tokens: HashMap<String, Token>,
    refresh: HashMap<String, (String, Vec<String>, String)>,
    pub registrations: usize,
    pub refreshes: usize,
    pub issued: Vec<String>,
    pub revoked: Vec<String>,
    pub requests: Vec<String>,
    counter: usize,
}

impl AsState {
    fn next(&mut self, prefix: &str) -> String {
        self.counter += 1;
        format!(
            "{prefix}-{}-{}",
            self.counter,
            uuid::Uuid::new_v4().simple()
        )
    }

    fn issue(
        &mut self,
        client_id: &str,
        scopes: Vec<String>,
        resource: String,
    ) -> serde_json::Value {
        let access = self.next("at");
        let refresh = self.next("rt");
        self.tokens.insert(
            access.clone(),
            Token {
                scopes: scopes.clone(),
                expires_at: now() + self.token_ttl,
                resource: resource.clone(),
            },
        );
        self.refresh.insert(
            refresh.clone(),
            (client_id.to_string(), scopes.clone(), resource),
        );
        self.issued.push(access.clone());
        self.issued.push(refresh.clone());
        serde_json::json!({
            "access_token": access,
            "token_type": "Bearer",
            "expires_in": self.token_ttl,
            "refresh_token": refresh,
            "scope": scopes.join(" "),
        })
    }

    fn client_auth(&self, req: &Request, form: &HashMap<String, String>) -> Option<String> {
        if let Some((id, secret)) = req.basic() {
            let client = self.clients.get(&id)?;
            return (client.secret.as_deref() == Some(secret.as_str())).then_some(id);
        }
        let id = form.get("client_id")?;
        let client = self.clients.get(id)?;
        match &client.secret {
            None => Some(id.clone()),
            Some(secret) => (form.get("client_secret") == Some(secret)).then(|| id.clone()),
        }
    }
}

pub struct AuthServer {
    pub url: String,
    pub state: Arc<Mutex<AsState>>,
}

fn error(status: u16, code: &str) -> Response {
    Response::json(status, serde_json::json!({"error": code}))
}

impl AuthServer {
    pub async fn start(resource: &str) -> Self {
        let state = Arc::new(Mutex::new(AsState {
            issuer: String::new(),
            resource: resource.to_string(),
            advertise_s256: true,
            dynamic_registration: true,
            revocation: true,
            token_ttl: 3600,
            reject_refresh: false,
            clients: HashMap::new(),
            codes: HashMap::new(),
            tokens: HashMap::new(),
            refresh: HashMap::new(),
            registrations: 0,
            refreshes: 0,
            issued: Vec::new(),
            revoked: Vec::new(),
            requests: Vec::new(),
            counter: 0,
        }));
        let handler_state = state.clone();
        let addr = serve(Arc::new(move |req| {
            Self::handle(&mut handler_state.lock().unwrap(), req)
        }))
        .await;
        let url = format!("http://{addr}");
        state.lock().unwrap().issuer = url.clone();
        Self { url, state }
    }

    pub fn preregister(&self, id: &str, secret: Option<&str>) {
        self.state.lock().unwrap().clients.insert(
            id.into(),
            Client {
                secret: secret.map(str::to_string),
                redirect_uris: Vec::new(),
            },
        );
    }

    fn handle(s: &mut AsState, req: Request) -> Response {
        s.requests.push(format!("{} {}", req.method, req.path));
        match (req.method.as_str(), req.path.as_str()) {
            ("GET", "/.well-known/oauth-authorization-server") => {
                let mut metadata = serde_json::json!({
                    "issuer": s.issuer,
                    "authorization_endpoint": format!("{}/authorize", s.issuer),
                    "token_endpoint": format!("{}/token", s.issuer),
                    "response_types_supported": ["code"],
                    "grant_types_supported": ["authorization_code", "refresh_token"],
                    "token_endpoint_auth_methods_supported": ["none", "client_secret_basic", "client_secret_post"],
                    "scopes_supported": ["mcp:read", "mcp:write"],
                    "code_challenge_methods_supported":
                        if s.advertise_s256 { serde_json::json!(["S256"]) } else { serde_json::json!(["plain"]) },
                    "authorization_response_iss_parameter_supported": true,
                });
                if s.dynamic_registration {
                    metadata["registration_endpoint"] = format!("{}/register", s.issuer).into();
                }
                if s.revocation {
                    metadata["revocation_endpoint"] = format!("{}/revoke", s.issuer).into();
                }
                Response::json(200, metadata)
            }
            ("POST", "/register") if s.dynamic_registration => {
                let body = req.json();
                let redirect_uris: Vec<String> =
                    serde_json::from_value(body["redirect_uris"].clone()).unwrap_or_default();
                let id = s.next("dyn");
                s.registrations += 1;
                s.clients.insert(
                    id.clone(),
                    Client {
                        secret: None,
                        redirect_uris: redirect_uris.clone(),
                    },
                );
                Response::json(
                    201,
                    serde_json::json!({
                        "client_id": id, "redirect_uris": redirect_uris,
                        "client_name": body["client_name"], "token_endpoint_auth_method": "none",
                    }),
                )
            }
            ("GET", "/authorize") => {
                let q = &req.query;
                let Some(client) = q.get("client_id").and_then(|id| s.clients.get(id)) else {
                    return Response::json(400, serde_json::json!({"error": "unknown client"}));
                };
                let redirect = q.get("redirect_uri").cloned().unwrap_or_default();
                let registered = client.redirect_uris.is_empty()
                    || client.redirect_uris.iter().any(|r| {
                        r == &redirect
                            || (r.starts_with("http://127.0.0.1:")
                                && redirect.starts_with("http://127.0.0.1:"))
                    });
                if !registered
                    || q.get("response_type").map(String::as_str) != Some("code")
                    || q.get("code_challenge_method").map(String::as_str) != Some("S256")
                    || q.get("resource") != Some(&s.resource)
                {
                    return Response::json(
                        400,
                        serde_json::json!({"error": "invalid_request", "query": q}),
                    );
                }
                let code = s.next("code");
                s.codes.insert(
                    code.clone(),
                    Code {
                        client_id: q["client_id"].clone(),
                        redirect_uri: redirect.clone(),
                        challenge: q.get("code_challenge").cloned().unwrap_or_default(),
                        scopes: q
                            .get("scope")
                            .map(|s| s.split_whitespace().map(str::to_string).collect())
                            .unwrap_or_default(),
                        resource: q["resource"].clone(),
                    },
                );
                let mut location = url::Url::parse(&redirect).unwrap();
                location
                    .query_pairs_mut()
                    .append_pair("code", &code)
                    .append_pair("state", q.get("state").map(String::as_str).unwrap_or(""))
                    .append_pair("iss", &s.issuer);
                Response::empty(302).with_header("location", location.as_str())
            }
            ("POST", "/token") => {
                let form = req.form();
                let Some(client_id) = s.client_auth(&req, &form) else {
                    return error(401, "invalid_client");
                };
                if form.get("resource") != Some(&s.resource) {
                    return error(400, "invalid_target");
                }
                match form.get("grant_type").map(String::as_str) {
                    Some("authorization_code") => {
                        let Some(code) = form.get("code").and_then(|c| s.codes.remove(c)) else {
                            return error(400, "invalid_grant");
                        };
                        let verifier = form.get("code_verifier").cloned().unwrap_or_default();
                        let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
                            .encode(sha2::Sha256::digest(verifier.as_bytes()));
                        if code.client_id != client_id
                            || Some(&code.redirect_uri) != form.get("redirect_uri")
                            || challenge != code.challenge
                            || code.resource != s.resource
                        {
                            return error(400, "invalid_grant");
                        }
                        let resource = s.resource.clone();
                        Response::json(200, s.issue(&client_id, code.scopes, resource))
                    }
                    Some("refresh_token") => {
                        s.refreshes += 1;
                        let presented = form.get("refresh_token").cloned().unwrap_or_default();
                        if s.reject_refresh {
                            return error(400, "invalid_grant");
                        }
                        let Some((owner, scopes, resource)) = s.refresh.remove(&presented) else {
                            return error(400, "invalid_grant");
                        };
                        if owner != client_id {
                            return error(400, "invalid_grant");
                        }
                        Response::json(200, s.issue(&client_id, scopes, resource))
                    }
                    _ => error(400, "unsupported_grant_type"),
                }
            }
            ("POST", "/revoke") => {
                let token = req.form().get("token").cloned().unwrap_or_default();
                s.tokens.remove(&token);
                s.refresh.remove(&token);
                s.revoked.push(token);
                Response::empty(200)
            }
            _ => Response::empty(404),
        }
    }
}

/// Steering for the MCP server.
pub struct McpState {
    pub base: String,
    pub tool_scopes: HashMap<String, String>,
    /// How many of the next `initialize` requests answer 503.
    pub unavailable: usize,
    pub calls: Vec<String>,
    pub tokens_seen: Vec<String>,
    pub resource_metadata: bool,
    /// The `scope` a 401 challenge names.
    pub challenge_scope: Option<String>,
}

pub struct McpServer {
    pub url: String,
    pub state: Arc<Mutex<McpState>>,
}

impl McpServer {
    /// An MCP server at `http://127.0.0.1:<port>/mcp` trusting tokens `auth`
    /// issues, its listener bound before `auth` so `auth` can know the
    /// resource it serves.
    pub async fn start() -> (Self, AuthServer) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}");
        let url = format!("{base}/mcp");
        let auth = AuthServer::start(&url).await;
        let state = Arc::new(Mutex::new(McpState {
            base: base.clone(),
            tool_scopes: HashMap::from([("write".to_string(), "mcp:write".to_string())]),
            unavailable: 0,
            calls: Vec::new(),
            tokens_seen: Vec::new(),
            resource_metadata: true,
            challenge_scope: Some("mcp:read".into()),
        }));
        let (mcp, tokens, issuer) = (state.clone(), auth.state.clone(), auth.url.clone());
        let handler: Handler = Arc::new(move |req| {
            Self::handle(
                &mut mcp.lock().unwrap(),
                &tokens.lock().unwrap(),
                &issuer,
                req,
            )
        });
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let handler = handler.clone();
                tokio::spawn(async move {
                    let Some(request) = read_request(&mut stream).await else {
                        return;
                    };
                    let response = handler(request);
                    let mut head = format!("HTTP/1.1 {} Status\r\n", response.status);
                    for (k, v) in &response.headers {
                        head.push_str(&format!("{k}: {v}\r\n"));
                    }
                    head.push_str(&format!(
                        "content-length: {}\r\nconnection: close\r\n\r\n",
                        response.body.len()
                    ));
                    let _ = stream.write_all(head.as_bytes()).await;
                    let _ = stream.write_all(&response.body).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        (Self { url, state }, auth)
    }

    fn handle(s: &mut McpState, issued: &AsState, issuer: &str, req: Request) -> Response {
        let prm = format!("{}/.well-known/oauth-protected-resource/mcp", s.base);
        if req.method == "GET"
            && req
                .path
                .starts_with("/.well-known/oauth-protected-resource")
        {
            if !s.resource_metadata {
                return Response::empty(404);
            }
            return Response::json(
                200,
                serde_json::json!({
                    "resource": format!("{}/mcp", s.base),
                    "authorization_servers": [issuer],
                    "scopes_supported": ["mcp:read", "mcp:write"],
                    "bearer_methods_supported": ["header"],
                }),
            );
        }
        if req.path != "/mcp" {
            return Response::empty(404);
        }
        let token = req.bearer().and_then(|t| {
            let issued = issued.tokens.get(&t)?;
            (issued.expires_at > now() && issued.resource == format!("{}/mcp", s.base))
                .then(|| (t, issued.clone()))
        });
        let Some((token, granted)) = token else {
            let scope = s
                .challenge_scope
                .as_ref()
                .map(|scope| format!(r#", scope="{scope}""#))
                .unwrap_or_default();
            return Response::empty(401).with_header(
                "www-authenticate",
                &format!(r#"Bearer resource_metadata="{prm}"{scope}"#),
            );
        };
        s.tokens_seen.push(token);
        match req.method.as_str() {
            "GET" => return Response::empty(405),
            "DELETE" => return Response::empty(200),
            _ => {}
        }
        let msg = req.json();
        let method = msg["method"].as_str().unwrap_or_default().to_string();
        if method == "initialize" && s.unavailable > 0 {
            s.unavailable -= 1;
            return Response::empty(503);
        }
        let Some(id) = msg.get("id").cloned() else {
            return Response::empty(202);
        };
        s.calls.push(method.clone());
        let result = match method.as_str() {
            "initialize" => serde_json::json!({
                "protocolVersion": msg["params"]["protocolVersion"],
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "mock-docs", "version": "1.0.0"},
                "instructions": "Docs server.",
            }),
            "tools/list" => serde_json::json!({"tools": [
                {"name": "echo", "description": "Echo", "inputSchema": {"type": "object"}},
                {"name": "write", "description": "Write", "inputSchema": {"type": "object"}},
            ]}),
            "tools/call" => {
                let tool = msg["params"]["name"].as_str().unwrap_or_default();
                if let Some(scope) = s.tool_scopes.get(tool) {
                    if !granted.scopes.contains(scope) {
                        return Response::empty(403).with_header(
                            "www-authenticate",
                            &format!(
                                r#"Bearer error="insufficient_scope", scope="{scope}", resource_metadata="{prm}""#
                            ),
                        );
                    }
                }
                serde_json::json!({"content": [{"type": "text", "text": format!("{tool}: ok")}]})
            }
            _ => {
                return Response::json(
                    200,
                    serde_json::json!({"jsonrpc": "2.0", "id": id,
                        "error": {"code": -32601, "message": "method not found"}}),
                )
            }
        };
        Response::json(
            200,
            serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result}),
        )
        .with_header("mcp-session-id", "session-1")
    }
}

/// The user's browser: open `authorize_url`, accept, and return where the
/// authorization server sends it back to.
pub async fn consent(authorize_url: &str) -> String {
    let browser = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let response = browser.get(authorize_url).send().await.unwrap();
    assert_eq!(
        response.status(),
        302,
        "the authorization server refused: {}",
        response.text().await.unwrap_or_default()
    );
    response.headers()["location"].to_str().unwrap().to_string()
}

/// The browser following the redirect to the daemon's loopback listener;
/// returns the page it shows.
pub async fn follow(location: &str) -> (u16, String) {
    let browser = reqwest::Client::builder().no_proxy().build().unwrap();
    let response = browser.get(location).send().await.unwrap();
    (response.status().as_u16(), response.text().await.unwrap())
}
