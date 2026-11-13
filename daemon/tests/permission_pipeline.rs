//! 权限管线里已知还没修的缺陷，各自从宿主这一侧的 socket 或装配入口复现。
//! 还没修的带 `#[ignore]`，对应的修复落地时去掉；它们同时是那些修复的回归用例。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base::interface::elicitation::{ElicitOutcome, ElicitRequest, Elicitation};
use base::interface::permission::{
    PermissionOutcome, PermissionPolicy, PermissionRequest, PermissionSet,
};
use base::settings::PermissionMode;
use daemon::config::{load_daemon_config, StaticDaemonPaths};
use daemon::DaemonServer;
use model::mock::MockAnthropicClient;
use model::stream::{BlockDelta, ContentBlockStart, MessageDeltaPayload, StreamEvent, Usage};
use runtime::agent::Builder;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio_util::sync::CancellationToken;

const SECRET: &str = "PIPELINE-SECRET-91c4";

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

type Customize = Arc<dyn Fn(Builder) -> Builder + Send + Sync>;

fn customizer(f: Customize) -> daemon::SessionCustomizer {
    Arc::new(move |builder, _| f(builder))
}

struct Setup {
    mode: PermissionMode,
    global: Value,
    project: Value,
    host_policy: Option<Arc<dyn PermissionPolicy>>,
    customize: Option<Customize>,
}

impl Default for Setup {
    fn default() -> Self {
        Self {
            mode: PermissionMode::Default,
            global: json!({}),
            project: json!({}),
            host_policy: None,
            customize: None,
        }
    }
}

/// `home` is the state root (global tier), `work` the project.
struct Daemon {
    sock: PathBuf,
    home: PathBuf,
    work: PathBuf,
    model: Arc<MockAnthropicClient>,
    _dir: tempfile::TempDir,
}

impl Daemon {
    /// `prepare` runs on `(home, work)` before the daemon reads its settings.
    async fn start(setup: Setup, prepare: impl FnOnce(&Path, &Path)) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let (home, work) = (dir.path().join("home"), dir.path().join("work"));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(work.join(".agix")).unwrap();
        let mut global = setup.global;
        global["sandbox"]["dangerously_disable_sandbox"] = json!(true);
        std::fs::write(home.join("settings.json"), global.to_string()).unwrap();
        std::fs::write(work.join(".agix/settings.json"), setup.project.to_string()).unwrap();
        prepare(&home, &work);

        let sock = dir.path().join("pipeline.sock");
        let paths =
            StaticDaemonPaths::with_project(home.clone(), work.clone()).with_global(home.clone());
        let mut config = load_daemon_config("claude-sonnet-4-6", 2000, Some(&sock), "code", &paths);
        config.settings.values.memory_enabled = false;
        config.settings.values.permission_mode = setup.mode;

        let model = Arc::new(MockAnthropicClient::new());
        let customize = setup.customize;
        let pool = daemon::assemble::pool(
            &config,
            Arc::new(scene::code::spec(scene::code::Interaction::Interactive)),
            daemon::Assembly {
                cwd: Some(work.clone()),
                model_client: Some(model.clone()),
                transcripts: daemon::Transcripts::Nowhere,
                host_policy: setup.host_policy,
                customize_session: customize.map(customizer),
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
            home,
            work,
            model,
            _dir: dir,
        }
    }

    async fn call(&self, method: &str, params: Value) -> Value {
        let request = json!({"jsonrpc":"2.0","method":method,"params":params,"id":1});
        let mut conn = UnixStream::connect(&self.sock).await.unwrap();
        conn.write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        let (r, _w) = conn.split();
        let mut reader = BufReader::new(r);
        loop {
            let mut line = String::new();
            let n = tokio::time::timeout(Duration::from_secs(60), reader.read_line(&mut line))
                .await
                .unwrap_or_else(|_| panic!("{method}: no response"))
                .unwrap();
            assert!(n > 0, "{method}: connection closed before a response");
            let frame: Value = serde_json::from_str(&line).unwrap();
            if frame.get("id").is_some() {
                assert!(frame.get("result").is_some(), "{method} failed: {frame}");
                return frame["result"].clone();
            }
        }
    }

    /// Runs one turn, answering each permission prompt with what `answer`
    /// returns for it (`None` leaves it unanswered); returns the prompts raised.
    async fn turn(&self, mut answer: impl FnMut(&Value) -> Option<Value>) -> Vec<Value> {
        let request = json!({"jsonrpc":"2.0","method":"session.run_turn",
                             "params": {"message": "go"},"id":1});
        let mut conn = UnixStream::connect(&self.sock).await.unwrap();
        conn.write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        let (r, _w) = conn.split();
        let mut reader = BufReader::new(r);
        let mut prompts = Vec::new();
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
                return prompts;
            }
            let event = &frame["params"]["event"];
            if event["kind"] == "prompt" {
                assert_eq!(event["prompt_type"], "permission", "{event}");
                prompts.push(event.clone());
                if let Some(decision) = answer(event) {
                    self.respond(
                        frame["params"]["session_id"].as_str().unwrap(),
                        event["prompt_id"].as_str().unwrap(),
                        decision,
                    )
                    .await;
                }
            }
        }
    }

    async fn respond(&self, session_id: &str, prompt_id: &str, decision: Value) {
        let reply = self
            .call(
                "session.respondToPrompt",
                json!({"session_id": session_id, "prompt_id": prompt_id, "decision": decision}),
            )
            .await;
        assert_eq!(reply["prompt_id"], prompt_id, "{reply}");
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

    fn local_settings(&self) -> String {
        std::fs::read_to_string(self.work.join(".agix/settings.local.json")).unwrap_or_default()
    }
}

fn permit_always(scope: &str) -> Option<Value> {
    Some(json!({"type": "permit_always", "scope": scope}))
}

fn deny() -> Option<Value> {
    Some(json!({"type": "deny", "reason": "pipeline"}))
}

/// "Always allow, this project" answered to a sub-agent's prompt is kept as
/// the person said — written to the project's `settings.local.json` — not
/// quietly downgraded to an allow for the rest of the session.
#[tokio::test(flavor = "multi_thread")]
async fn a_sub_agents_local_permit_always_is_written_to_the_project() {
    let daemon = Daemon::start(Setup::default(), |_, _| {}).await;
    daemon.model.push_turn(tool_round(
        "toolu_agent",
        "Agent",
        json!({"description": "touch a file", "prompt": "touch it",
               "subagent_type": "general-purpose"}),
    ));
    daemon.model.push_turn(tool_round(
        "toolu_touch",
        "Bash",
        json!({"command": "touch sub-agent-grant.txt"}),
    ));
    daemon.model.push_turn(text_round("touched"));
    daemon.model.push_turn(text_round("done"));

    let prompts = daemon.turn(|_| permit_always("local")).await;
    assert_eq!(
        prompts.len(),
        1,
        "precondition: the sub-agent asks: {prompts:?}"
    );
    let written = daemon.local_settings();
    assert!(
        written.contains("sub-agent-grant.txt"),
        "the answer was not kept as a project allow: {written:?}"
    );
}

#[derive(Default)]
struct RecordsOptions(Mutex<Vec<Vec<String>>>);

struct Recorder(Arc<RecordsOptions>);

#[async_trait::async_trait]
impl Elicitation for Recorder {
    async fn ask(&self, request: ElicitRequest) -> ElicitOutcome {
        self.0
             .0
            .lock()
            .unwrap()
            .push(request.options.iter().map(|o| o.key.clone()).collect());
        ElicitOutcome::declined("the recording host answers nothing")
    }
}

struct AsksAboutEverything;

#[async_trait::async_trait]
impl PermissionPolicy for AsksAboutEverything {
    async fn decide(&self, _: &PermissionRequest<'_>) -> PermissionOutcome {
        PermissionOutcome::Prompt {
            prompt_id: uuid::Uuid::new_v4().to_string(),
            message: "the host asks".into(),
            paths: Vec::new(),
            reason: base::interface::permission::Reason::new(
                base::interface::permission::Stage::ModeDefault,
                "asked",
            ),
            answerable: base::interface::permission::Answerable::HookOrPerson,
        }
    }
}

/// A session whose permission keeps no grants cannot honour "allow every
/// time", so it does not offer it; one that keeps them does.
#[tokio::test(flavor = "multi_thread")]
async fn a_session_that_keeps_no_grants_does_not_offer_permit_always() {
    let offered = offered_answers(true).await;
    assert!(
        offered.iter().any(|key| key == "permit_always"),
        "precondition: a session that keeps grants offers it: {offered:?}"
    );
    let offered = offered_answers(false).await;
    assert!(
        !offered.iter().any(|key| key == "permit_always"),
        "offered an answer that keeps nothing: {offered:?}"
    );
}

/// The answers a session — given a permission that keeps grants or not —
/// offers about a call its permission asks about.
async fn offered_answers(keeps_grants: bool) -> Vec<String> {
    let recorded = Arc::new(RecordsOptions::default());
    let sink = recorded.clone();
    let daemon = Daemon::start(
        Setup {
            customize: Some(Arc::new(move |builder| {
                let permission = if keeps_grants {
                    PermissionSet::of(Arc::new(permissions::policy::Policy::empty()))
                } else {
                    PermissionSet::without_grants(Arc::new(AsksAboutEverything))
                };
                builder
                    .permission(permission)
                    .elicitation(Arc::new(Recorder(sink.clone())))
            })),
            ..Default::default()
        },
        |_, _| {},
    )
    .await;
    daemon.model.push_turn(tool_round(
        "toolu_touch",
        "Bash",
        json!({"command": "touch no-grants.txt"}),
    ));
    daemon.model.push_turn(text_round("done"));

    daemon.turn(|_| None).await;
    let mut recorded = recorded.0.lock().unwrap().clone();
    assert_eq!(
        recorded.len(),
        1,
        "precondition: the call is asked about: {recorded:?}"
    );
    recorded.remove(0)
}

/// What a person allowed for good is the command they were shown, not every
/// command the text happens to match as a pattern.
#[tokio::test(flavor = "multi_thread")]
async fn a_permanent_allow_of_a_command_with_a_star_allows_only_that_command() {
    let daemon = Daemon::start(Setup::default(), |_, _| {}).await;
    daemon.model.push_turn(tool_round(
        "toolu_star",
        "Bash",
        json!({"command": "touch *.rs"}),
    ));
    daemon.model.push_turn(tool_round(
        "toolu_other",
        "Bash",
        json!({"command": "touch other.rs"}),
    ));
    daemon.model.push_turn(text_round("done"));

    let mut asked = 0;
    let prompts = daemon
        .turn(|_| {
            asked += 1;
            if asked == 1 {
                permit_always("session")
            } else {
                deny()
            }
        })
        .await;
    assert_eq!(
        prompts.len(),
        2,
        "the second, different command was allowed by the first answer: {prompts:?}"
    );
}

/// "Allow every time, this project" is written as the command itself,
/// `"match": "exact"`, and a later session reads it back that way.
#[tokio::test(flavor = "multi_thread")]
async fn a_project_permanent_allow_is_written_and_read_back_exact() {
    let daemon = Daemon::start(Setup::default(), |_, _| {}).await;
    daemon.model.push_turn(tool_round(
        "toolu_star",
        "Bash",
        json!({"command": "touch *.rs"}),
    ));
    daemon.model.push_turn(text_round("done"));
    daemon.turn(|_| permit_always("local")).await;
    let written: Value = serde_json::from_str(&daemon.local_settings()).unwrap();
    let rule = json!({"tool": "Bash(touch *.rs)", "action": "allow", "match": "exact"});
    assert_eq!(written["permission_rules"], json!([rule]), "{written}");

    let later = Daemon::start(Setup::default(), |_, work| {
        std::fs::write(
            work.join(".agix/settings.local.json"),
            json!({"permission_rules": [rule]}).to_string(),
        )
        .unwrap();
    })
    .await;
    later.model.push_turn(tool_round(
        "toolu_same",
        "Bash",
        json!({"command": "touch *.rs"}),
    ));
    later.model.push_turn(tool_round(
        "toolu_other",
        "Bash",
        json!({"command": "touch other.rs"}),
    ));
    later.model.push_turn(text_round("done"));
    let prompts = later.turn(|_| deny()).await;
    assert_eq!(prompts.len(), 1, "{prompts:?}");
    assert_eq!(
        prompts[0]["tool_use_id"], "toolu_other",
        "the command allowed for good was asked about again: {prompts:?}"
    );
}

/// A scene directory linked in from elsewhere is the same directory under its
/// real name, so its settings are credentials under that name too.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_linked_scenes_credentials_are_refused_under_their_real_path() {
    let daemon = Daemon::start(Setup::default(), |home, _| {
        let elsewhere = home.parent().unwrap().join("dotfiles/scene");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("settings.json"), SECRET).unwrap();
        std::fs::create_dir_all(home.join("scenes")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, home.join("scenes/linked")).unwrap();
    })
    .await;
    let real = daemon
        .home
        .parent()
        .unwrap()
        .join("dotfiles/scene/settings.json");
    daemon.model.push_turn(tool_round(
        "toolu_real",
        "Read",
        json!({"file_path": real.display().to_string()}),
    ));
    daemon.model.push_turn(text_round("done"));

    let prompts = daemon.turn(|_| deny()).await;
    assert!(
        prompts.is_empty(),
        "a credential read is refused, not asked about: {prompts:?}"
    );
    assert_eq!(daemon.tool_result("toolu_real")["is_error"], true);
}

/// A host policy that refuses every `touch` and lets everything else through.
struct HostRefusesTouch;

#[async_trait::async_trait]
impl PermissionPolicy for HostRefusesTouch {
    async fn decide(&self, req: &PermissionRequest<'_>) -> PermissionOutcome {
        let command = req.input["command"].as_str().unwrap_or_default();
        if command.starts_with("touch") {
            return PermissionOutcome::deny(
                base::interface::permission::Stage::Rule,
                "this host keeps its files",
            );
        }
        PermissionOutcome::Permit {
            basis: base::interface::permission::Basis::Bypass,
            profile: Default::default(),
            obligations: Default::default(),
        }
    }
}

/// The host's own policy is one tier of a session's rules: its refusal
/// stands even under `bypassPermissions`.
#[tokio::test(flavor = "multi_thread")]
async fn a_hosts_refusal_stands_under_bypass() {
    let daemon = Daemon::start(
        Setup {
            mode: PermissionMode::BypassPermissions,
            host_policy: Some(Arc::new(HostRefusesTouch)),
            ..Default::default()
        },
        |_, _| {},
    )
    .await;
    daemon.model.push_turn(tool_round(
        "toolu_touch",
        "Bash",
        json!({"command": "touch host-refused.txt"}),
    ));
    daemon.model.push_turn(text_round("done"));

    let prompts = daemon.turn(|_| deny()).await;
    assert!(prompts.is_empty(), "{prompts:?}");
    let result = daemon.tool_result("toolu_touch");
    assert_eq!(result["is_error"], true, "{result}");
    assert!(
        result.to_string().contains("this host keeps its files"),
        "{result}"
    );
    assert!(!daemon.work.join("host-refused.txt").exists());
}

/// The host's permit is one allow rule: what it lets through runs without a
/// question, and a deny rule of the project's still refuses what it names.
#[tokio::test(flavor = "multi_thread")]
async fn a_hosts_permit_is_an_allow_rule_a_deny_rule_still_overrides() {
    let daemon = Daemon::start(
        Setup {
            project: json!({"permission_rules": [{"tool": "Bash(rm:*)", "action": "deny"}]}),
            host_policy: Some(Arc::new(HostRefusesTouch)),
            ..Default::default()
        },
        |_, work| std::fs::write(work.join("kept.txt"), "kept").unwrap(),
    )
    .await;
    daemon.model.push_turn(tool_round(
        "toolu_mkdir",
        "Bash",
        json!({"command": "mkdir host-allowed"}),
    ));
    daemon.model.push_turn(tool_round(
        "toolu_rm",
        "Bash",
        json!({"command": "rm kept.txt"}),
    ));
    daemon.model.push_turn(text_round("done"));

    let prompts = daemon.turn(|_| deny()).await;
    assert!(prompts.is_empty(), "{prompts:?}");
    assert!(
        daemon.work.join("host-allowed").is_dir(),
        "{}",
        daemon.tool_result("toolu_mkdir")
    );
    assert_eq!(daemon.tool_result("toolu_rm")["is_error"], true);
    assert!(daemon.work.join("kept.txt").exists());
}

/// An untrusted project's own settings take no key that loosens the
/// instance, not only the ones a deny-list happens to name.
#[tokio::test(flavor = "multi_thread")]
async fn an_untrusted_project_cannot_turn_off_reaping_or_telemetry() {
    let daemon = Daemon::start(
        Setup {
            global: json!({
                "project_trust": {"default": "untrusted"},
                "sandbox": {"reap_process_groups": true},
            }),
            project: json!({
                "sandbox": {"reap_process_groups": false},
                "telemetry_enabled": false,
            }),
            ..Default::default()
        },
        |_, _| {},
    )
    .await;

    let settings = daemon
        .call(
            "config.get",
            json!({"project_root": daemon.work, "scene": "code"}),
        )
        .await["settings"]
        .clone();
    assert_eq!(
        settings["sandbox"]["reap_process_groups"], true,
        "background processes are no longer reaped: {settings}"
    );
    assert_eq!(
        settings["telemetry_enabled"], true,
        "the audit record was switched off: {settings}"
    );
}

/// A scheduled prompt that outlives the session is something the person
/// decides on, so `durable` asks where a session-only one does not.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "D1: fix pending"]
async fn a_durable_cron_job_is_asked_about() {
    let daemon = Daemon::start(Setup::default(), |_, _| {}).await;
    daemon.model.push_turn(tool_round(
        "toolu_cron",
        "CronCreate",
        json!({"cron": "0 9 * * *", "prompt": "report", "durable": true}),
    ));
    daemon.model.push_turn(text_round("done"));

    let prompts = daemon.turn(|_| deny()).await;
    let result = daemon.tool_result("toolu_cron").to_string();
    assert!(
        !result.contains("not found"),
        "precondition: the session has CronCreate: {result}"
    );
    assert_eq!(
        prompts.len(),
        1,
        "a durable job was created silently: {result}"
    );
}
