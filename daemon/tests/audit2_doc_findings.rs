//! 发布前对抗式审查（第二轮）：文档承诺与实际行为不一致的地方，各从宿主那一侧复现。
//! 修好之前保持 ignored，修好之后作为回归用例。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base::settings::PermissionMode;
use daemon::config::{load_daemon_config, StaticDaemonPaths};
use daemon::DaemonServer;
use model::mock::MockAnthropicClient;
use model::stream::{BlockDelta, ContentBlockStart, MessageDeltaPayload, StreamEvent, Usage};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio_util::sync::CancellationToken;

fn stop(reason: base::message::StopReason) -> StreamEvent {
    StreamEvent::MessageDelta {
        delta: MessageDeltaPayload {
            stop_reason: Some(reason),
            stop_sequence: None,
        },
        usage: Some(Usage::default()),
    }
}

fn text_round(text: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlockStart::Text {
                text: String::new(),
            },
        },
        StreamEvent::ContentBlockDelta {
            index: 0,
            delta: BlockDelta::TextDelta { text: text.into() },
        },
        StreamEvent::ContentBlockStop { index: 0 },
        stop(base::message::StopReason::EndTurn),
    ]
}

fn tool_round(id: &str, tool: &str, input: Value) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlockStart::ToolUse {
                id: id.into(),
                name: tool.into(),
                input: Value::Null,
            },
        },
        StreamEvent::ContentBlockDelta {
            index: 0,
            delta: BlockDelta::InputJsonDelta {
                partial_json: input.to_string(),
            },
        },
        StreamEvent::ContentBlockStop { index: 0 },
        stop(base::message::StopReason::ToolUse),
    ]
}

struct Setup {
    mode: PermissionMode,
    global: Value,
    managed: Option<&'static str>,
}

impl Default for Setup {
    fn default() -> Self {
        Self {
            mode: PermissionMode::Default,
            global: json!({}),
            managed: None,
        }
    }
}

struct Daemon {
    sock: PathBuf,
    model: Arc<MockAnthropicClient>,
    _dir: tempfile::TempDir,
}

impl Daemon {
    /// `prepare` runs on `(home, work)` before the daemon reads its settings.
    async fn start(setup: Setup, prepare: impl FnOnce(&Path, &Path)) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let (home, work) = (dir.path().join("home"), dir.path().join("work"));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        let mut global = setup.global;
        global["sandbox"]["dangerously_disable_sandbox"] = json!(true);
        std::fs::write(home.join("settings.json"), global.to_string()).unwrap();
        prepare(&home, &work);

        let sock = dir.path().join("audit.sock");
        let paths =
            StaticDaemonPaths::with_project(home.clone(), work.clone()).with_global(home.clone());
        let mut config = load_daemon_config("claude-sonnet-4-6", 2000, Some(&sock), "code", &paths);
        config.settings.values.memory_enabled = false;
        config.settings.values.permission_mode = setup.mode;
        config.managed_settings_path = setup.managed.map(|content| {
            let path = dir.path().join("it").join("managed-settings.json");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, content).unwrap();
            path
        });

        let model = Arc::new(MockAnthropicClient::new());
        let pool = daemon::assemble::pool(
            &config,
            Arc::new(scene::code::spec(scene::code::Interaction::Interactive)),
            daemon::Assembly {
                cwd: Some(work.clone()),
                model_client: Some(model.clone()),
                transcripts: daemon::Transcripts::Nowhere,
                ..Default::default()
            },
        )
        .await
        .expect("the daemon assembles");

        let server = Arc::new(DaemonServer::new(pool, CancellationToken::new()));
        let listener = DaemonServer::bind_unix(&sock).unwrap();
        tokio::spawn(async move {
            let _ = server.serve_unix_listener(listener).await;
        });
        Self {
            sock,
            model,
            _dir: dir,
        }
    }

    /// Runs one turn, denying every permission prompt it raises; returns the
    /// tool calls the prompts were about.
    async fn turn_denying_prompts(&self) -> Vec<String> {
        self.turn_with_frames().await.0
    }

    /// Like [`Self::turn_denying_prompts`], also returning every event frame
    /// as the raw line it arrived on.
    async fn turn_with_frames(&self) -> (Vec<String>, Vec<String>) {
        let request = json!({"jsonrpc":"2.0","method":"session.run_turn",
                             "params":{"message":"go"},"id":1});
        let mut conn = UnixStream::connect(&self.sock).await.unwrap();
        conn.write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        let (r, _w) = conn.split();
        let mut reader = BufReader::new(r);
        let mut asked = Vec::new();
        let mut frames = Vec::new();
        loop {
            let mut line = String::new();
            let n = tokio::time::timeout(Duration::from_secs(120), reader.read_line(&mut line))
                .await
                .expect("the turn ended")
                .unwrap();
            assert!(n > 0, "connection closed before the turn ended");
            let frame: Value = serde_json::from_str(&line).unwrap();
            if frame.get("id").is_some() {
                assert!(frame.get("result").is_some(), "turn failed: {frame}");
                return (asked, frames);
            }
            frames.push(line.trim_end().to_string());
            let event = &frame["params"]["event"];
            if event["kind"] == "prompt" {
                assert_eq!(event["prompt_type"], "permission", "{event}");
                asked.push(event["tool_use_id"].as_str().unwrap_or("").to_string());
                deny(
                    &self.sock,
                    frame["params"]["session_id"].as_str().unwrap(),
                    event["prompt_id"].as_str().unwrap(),
                )
                .await;
            }
        }
    }

    fn tool_result(&self, id: &str) -> Value {
        (0..self.model.calls())
            .filter_map(|i| self.model.nth_request(i))
            .map(|r| serde_json::to_value(r).unwrap())
            .find_map(|request| {
                request["messages"]
                    .as_array()?
                    .iter()
                    .filter_map(|m| m["content"].as_array())
                    .flatten()
                    .find(|block| block["type"] == "tool_result" && block["tool_use_id"] == id)
                    .cloned()
            })
            .unwrap_or_else(|| panic!("no request carries a tool_result for {id}"))
    }
}

async fn deny(sock: &Path, session_id: &str, prompt_id: &str) {
    let request = json!({"jsonrpc":"2.0","method":"session.respondToPrompt","params":{
        "session_id": session_id, "prompt_id": prompt_id,
        "decision": {"type": "deny", "reason": "audit"}},"id":2});
    let mut conn = UnixStream::connect(sock).await.unwrap();
    conn.write_all(format!("{request}\n").as_bytes())
        .await
        .unwrap();
    let mut line = String::new();
    BufReader::new(conn).read_line(&mut line).await.unwrap();
    let reply: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(reply["result"]["prompt_id"], prompt_id, "{reply}");
}

/// 迁移指南说 URL 规则改成按部分匹配后"deny / ask 照旧按整串 glob 拦"，`RuleSet` 的注释也说
/// 以前写下的拒绝不能失效。但只有写不成 `scheme://host[/path]` 的才照旧：形状对的 deny
/// 换成了路径 `*` 不跨 `/`、不写路径只认 `/` 的新语义。0.3.10 里
/// `WebFetch(https://evil.example/*)`、`WebFetch(https://evil.example*)` 都拒绝
/// `https://evil.example/a/b`，现在这次调用落到询问上。
#[tokio::test(flavor = "multi_thread")]
async fn audit2_a_url_shaped_deny_written_for_0_3_10_still_refuses_deeper_paths() {
    let mut asked_instead = Vec::new();
    for rule in [
        "WebFetch(https://evil.example/**)",
        "WebFetch(https://evil.example/*)",
        "WebFetch(https://evil.example*)",
    ] {
        let daemon = Daemon::start(
            Setup {
                global: json!({"permission_rules": [{"tool": rule, "action": "deny"}]}),
                ..Default::default()
            },
            |_, _| {},
        )
        .await;
        daemon.model.push_turn(tool_round(
            "toolu_fetch",
            "WebFetch",
            json!({"url": "https://evil.example/a/b"}),
        ));
        daemon.model.push_turn(text_round("done"));
        if !daemon.turn_denying_prompts().await.is_empty() {
            asked_instead.push(rule);
        }
        assert_eq!(
            daemon.tool_result("toolu_fetch")["is_error"],
            true,
            "{rule}"
        );
    }
    assert!(
        asked_instead.is_empty(),
        "these denies no longer cover https://evil.example/a/b, the call was asked about: \
         {asked_instead:?}"
    );
}

/// `mcp_auth.md` §1:"密钥只交给它所属的授权服务器"。`client_secret_env` 指向的变量不进
/// 任何子进程的环境,模型一条 `printenv` 读不到。
#[tokio::test(flavor = "multi_thread")]
async fn audit2_an_mcp_oauth_client_secret_is_not_handed_to_commands() {
    const VAR: &str = "AUDIT2_DOCS_MCP_SECRET";
    const CANARY: &str = "AUDIT2-CANARY-5d1e";
    // SAFETY: the name is unique to this test; no other test reads it.
    unsafe { std::env::set_var(VAR, CANARY) };
    let daemon = Daemon::start(
        Setup {
            mode: PermissionMode::BypassPermissions,
            global: json!({"mcp_servers": {"docs": {
                "type": "streamable_http",
                "url": "http://127.0.0.1:9/mcp",
                "oauth": {"client_id": "agix", "client_secret_env": VAR,
                          "issuer": "https://login.example.com"}
            }}}),
            ..Default::default()
        },
        |_, _| {},
    )
    .await;
    daemon.model.push_turn(tool_round(
        "toolu_env",
        "Bash",
        json!({"command": format!("printenv {VAR}; echo end")}),
    ));
    daemon.model.push_turn(text_round("done"));
    daemon.turn_denying_prompts().await;
    let result = daemon.tool_result("toolu_env").to_string();
    assert!(
        result.contains("end"),
        "precondition: the command ran: {result}"
    );
    assert!(
        !result.contains(CANARY),
        "the MCP OAuth client secret reached the model through printenv: {result}"
    );
}

/// ARCHITECTURE §5 第 5 条:任何一层设置里 provider 的 `api_key_env` 指向的变量同样不交给
/// 命令。provider 写在另一个项目的设置里(会话在用的模型留在这个项目),daemon 读过那份设置之后,
/// 这个项目里的 `printenv` 也读不到它。
#[tokio::test(flavor = "multi_thread")]
async fn audit2_a_provider_key_env_is_not_handed_to_commands() {
    const VAR: &str = "AUDIT2_OTHER_PROJECT_PROVIDER_KEY";
    const CANARY: &str = "AUDIT2-CANARY-8a3f";
    // SAFETY: the name is unique to this test; no other test reads it.
    unsafe { std::env::set_var(VAR, CANARY) };
    let mut other = PathBuf::new();
    let daemon = Daemon::start(
        Setup {
            mode: PermissionMode::BypassPermissions,
            ..Default::default()
        },
        |home, _| {
            other = home.parent().unwrap().join("other");
            std::fs::create_dir_all(other.join(".agix")).unwrap();
            std::fs::write(
                other.join(".agix").join("settings.json"),
                json!({"providers": {"spare": {
                    "api_type": "openai_compatible",
                    "base_url": "http://127.0.0.1:9/v1",
                    "api_key_env": VAR,
                    "default_model": "m",
                }}, "default_provider": "spare"})
                .to_string(),
            )
            .unwrap();
        },
    )
    .await;
    let request = json!({"jsonrpc":"2.0","method":"project.status",
                         "params":{"project_root": other},"id":3});
    let mut conn = UnixStream::connect(&daemon.sock).await.unwrap();
    conn.write_all(format!("{request}\n").as_bytes())
        .await
        .unwrap();
    let mut line = String::new();
    BufReader::new(conn).read_line(&mut line).await.unwrap();
    let reply: Value = serde_json::from_str(&line).unwrap();
    assert!(reply.get("result").is_some(), "precondition: {reply}");

    daemon.model.push_turn(tool_round(
        "toolu_env",
        "Bash",
        json!({"command": format!("printenv {VAR}; echo end")}),
    ));
    daemon.model.push_turn(text_round("done"));
    daemon.turn_denying_prompts().await;
    let result = daemon.tool_result("toolu_env").to_string();
    assert!(
        result.contains("end"),
        "precondition: the command ran: {result}"
    );
    assert!(
        !result.contains(CANARY),
        "the provider key reached the model through printenv: {result}"
    );
}

/// 协议 §7.3 的例子是 `"progress":42`、`"total":120`,`Bash` 报的 `progress` 是"行数"。
/// 线上发的却是浮点数 `1.0`:按整数解析这个字段的宿主(Rust `u64`、Go `int`)解析失败。
#[tokio::test(flavor = "multi_thread")]
async fn audit2_a_bash_line_count_goes_out_as_an_integer() {
    let daemon = Daemon::start(
        Setup {
            mode: PermissionMode::BypassPermissions,
            ..Default::default()
        },
        |_, _| {},
    )
    .await;
    daemon.model.push_turn(tool_round(
        "toolu_lines",
        "Bash",
        json!({"command": "printf 'one\\ntwo\\n'"}),
    ));
    daemon.model.push_turn(text_round("done"));
    let (_, frames) = daemon.turn_with_frames().await;
    let progress: Vec<&String> = frames
        .iter()
        .filter(|f| f.contains("\"kind\":\"tool_progress\""))
        .collect();
    assert!(
        !progress.is_empty(),
        "precondition: Bash reported progress: {frames:?}"
    );
    for frame in progress {
        let event = &serde_json::from_str::<Value>(frame).unwrap()["params"]["event"];
        assert!(
            event["progress"].is_u64(),
            "a line count went out as {}: {frame}",
            event["progress"]
        );
    }
}

/// 协议 §1:只有"合法 JSON 但缺 `method`"的帧被静默丢弃。字段类型不对的请求(这里
/// `"jsonrpc"` 写成数字)同样什么也不回,客户端等这个 `id` 的响应会一直等下去。
#[tokio::test]
async fn audit2_a_request_with_a_mistyped_field_is_answered() {
    let daemon = Daemon::start(Setup::default(), |_, _| {}).await;
    let mut conn = UnixStream::connect(&daemon.sock).await.unwrap();
    conn.write_all(b"{\"jsonrpc\":2.0,\"method\":\"daemon.ping\",\"id\":7}\n")
        .await
        .unwrap();
    let mut reader = BufReader::new(conn);
    let mut line = String::new();
    let answered = tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line)).await;
    assert!(
        matches!(answered, Ok(Ok(n)) if n > 0),
        "a request whose `jsonrpc` is a number got no response at all"
    );
}

/// 协议 §4.6:"与调用的 URL 原样相等的也命中('永久允许'记下的就是原样的 URL)"。记下的
/// 原样 URL 带着 query 时,它又被当成 URL 模式:`?` 成了路径里匹配任意一个字符的通配,
/// 永久允许 `https://h.example/a?b` 连 `https://h.example/axb` 一起放行。
#[test]
fn audit2_permit_always_on_a_url_with_a_query_allows_only_that_url() {
    use base::permission::{RuleBehavior, RuleSource};
    use permissions::ruleset::{RuleHit, RuleSet};
    let rule = permissions::rule::parse_rule_string(
        "WebFetch(https://h.example/a?b)",
        RuleSource::Session,
        RuleBehavior::Allow,
    )
    .unwrap();
    let rules = RuleSet::new(vec![rule]);
    assert!(
        matches!(
            rules.evaluate("WebFetch", Some("https://h.example/a?b")),
            RuleHit::Allow(_)
        ),
        "precondition: the URL that was allowed"
    );
    assert!(
        matches!(
            rules.evaluate("WebFetch", Some("https://h.example/axb")),
            RuleHit::None
        ),
        "permit_always on https://h.example/a?b also allows https://h.example/axb"
    );
}

/// `ARCHITECTURE.md` §6.1 第 5 条与迁移指南:项目或插件 skill 的 `allowed-tools` 里"主机钉不住
/// 域名的 URL 模式"按整类授权处理。`*.co.uk` 两段都是字面量,`host_is_open` 当它钉住了,可
/// 任何人都能注册 `x.co.uk`。
#[test]
fn audit2_a_skill_url_grant_on_a_public_suffix_is_unscoped() {
    assert!(
        !permissions::dangerous::is_unscoped_grant("WebFetch", Some("https://*.example.com/**")),
        "precondition: a registrable domain is pinned"
    );
    for open in [
        "https://*.co.uk/**",
        "https://*.com.cn/**",
        "https://*.github.io/**",
    ] {
        assert!(
            permissions::dangerous::is_unscoped_grant("WebFetch", Some(open)),
            "{open} pins no one's domain but a project skill may grant it"
        );
    }
}

/// `settings_json_schema` 的注释说这份 schema "can never drift from what `LoadedSettings::load()`
/// actually accepts",README 让用户拿它做编辑器校验。可它要求顶层的 `model`、`paths`,
/// `sandbox` 要求 `deny_read`、`allowed_domains`:空文件、README 的路由示例、迁移指南让写的
/// `{"sandbox": {"unattended_requires_network_policy": true}}` 都过不了校验。`bubble` 则反过来:
/// schema 收,协议说写进 settings.json 会被退回 `default`。
#[test]
fn audit2_the_settings_schema_accepts_what_the_loader_accepts() {
    let schema: Value = serde_json::from_str(
        &std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../docs/schemas/settings.schema.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let examples = [
        json!({}),
        json!({"sandbox": {"unattended_requires_network_policy": true}}),
        json!({"execution": {"max_turns": 10}}),
        json!({
            "providers": {
                "anthropic": { "api_type": "anthropic", "default_model": "claude-sonnet-4-6" }
            },
            "default_provider": "anthropic"
        }),
    ];
    let mut refused = Vec::new();
    for example in &examples {
        let mut missing = Vec::new();
        missing_required(&schema, &schema, example, "", &mut missing);
        if !missing.is_empty() {
            refused.push(format!("{example} lacks {missing:?}"));
        }
    }
    let modes = schema["$defs"]["PermissionMode"].to_string();
    if modes.contains("\"bubble\"") {
        refused.push("PermissionMode admits \"bubble\"".into());
    }
    assert!(refused.is_empty(), "{refused:#?}");
}

fn resolve<'a>(root: &'a Value, node: &'a Value) -> &'a Value {
    match node["$ref"].as_str() {
        Some(r) => resolve(root, &root["$defs"][r.trim_start_matches("#/$defs/")]),
        None => node,
    }
}

fn missing_required(root: &Value, node: &Value, value: &Value, at: &str, out: &mut Vec<String>) {
    let node = resolve(root, node);
    if let Some(options) = node["anyOf"].as_array().or(node["oneOf"].as_array()) {
        let mut best: Option<Vec<String>> = None;
        for option in options {
            let mut found = Vec::new();
            missing_required(root, option, value, at, &mut found);
            if best.as_ref().is_none_or(|b| found.len() < b.len()) {
                best = Some(found);
            }
        }
        out.extend(best.unwrap_or_default());
        return;
    }
    let Some(object) = value.as_object() else {
        return;
    };
    for key in node["required"].as_array().into_iter().flatten() {
        let key = key.as_str().unwrap();
        if !object.contains_key(key) {
            out.push(format!("{at}/{key}"));
        }
    }
    for (key, child) in object {
        let schema = match node["properties"].get(key) {
            Some(s) => s,
            None => match node.get("additionalProperties") {
                Some(s) if s.is_object() => s,
                _ => continue,
            },
        };
        missing_required(root, schema, child, &format!("{at}/{key}"), out);
    }
}
