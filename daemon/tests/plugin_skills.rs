//! Skills a plugin package ships, seen from a host: installed over RPC they
//! reach the session's `Skill` tool and `commands.list`, go away with
//! `plugin.disable`, give way to a skill of the same name on disk, and a
//! package with a broken one does not install at all.
#![cfg(all(feature = "plugin-packages", feature = "skills"))]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use daemon::config::{load_daemon_config, StaticDaemonPaths};
use daemon::DaemonServer;
use model::mock::MockAnthropicClient;
use model::stream::{BlockDelta, ContentBlockStart, MessageDeltaPayload, StreamEvent, Usage};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio_util::sync::CancellationToken;

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
        StreamEvent::MessageDelta {
            delta: MessageDeltaPayload {
                stop_reason: Some(base::message::StopReason::EndTurn),
                stop_sequence: None,
            },
            usage: Some(Usage::default()),
        },
    ]
}

fn skill_round(id: &str, skill: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlockStart::ToolUse {
                id: id.into(),
                name: "Skill".into(),
                input: Value::Null,
            },
        },
        StreamEvent::ContentBlockDelta {
            index: 0,
            delta: BlockDelta::InputJsonDelta {
                partial_json: json!({ "skill": skill }).to_string(),
            },
        },
        StreamEvent::ContentBlockStop { index: 0 },
        StreamEvent::MessageDelta {
            delta: MessageDeltaPayload {
                stop_reason: Some(base::message::StopReason::ToolUse),
                stop_sequence: None,
            },
            usage: Some(Usage::default()),
        },
    ]
}

fn package(dir: &Path, name: &str, files: &[(&str, &str)]) -> (PathBuf, String) {
    let mut buf = Vec::new();
    {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
        for (path, body) in files {
            writer.start_file(*path, opts).unwrap();
            writer.write_all(body.as_bytes()).unwrap();
        }
        writer.finish().unwrap();
    }
    let archive = dir.join(format!("{name}.zip"));
    std::fs::write(&archive, &buf).unwrap();
    (archive, hex::encode(Sha256::digest(&buf)))
}

fn manifest(name: &str) -> String {
    format!(
        "[plugin]\nname = \"{name}\"\nversion = \"1.0.0\"\napi_version = \"0.1\"\n\n\
         [skills]\ndir = \"skills\"\n"
    )
}

struct Daemon {
    sock: PathBuf,
    global: PathBuf,
    model: Arc<MockAnthropicClient>,
    invocations: std::sync::atomic::AtomicUsize,
    dir: tempfile::TempDir,
}

impl Daemon {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("agix");
        let scene_root = global.join("code");
        let work = dir.path().join("work");
        for d in [&scene_root, &work] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(
            global.join("settings.json"),
            json!({ "sandbox": {"dangerously_disable_sandbox": true} }).to_string(),
        )
        .unwrap();

        let sock = dir.path().join("agix.sock");
        let paths =
            StaticDaemonPaths::with_project(scene_root, work.clone()).with_global(global.clone());
        let mut config = load_daemon_config("claude-sonnet-4-6", 2000, Some(&sock), "code", &paths);
        config.settings.values.memory_enabled = false;

        let model = Arc::new(MockAnthropicClient::new());
        let pool = daemon::assemble::pool(
            &config,
            Arc::new(scene::code::spec(scene::code::Interaction::Interactive)),
            daemon::Assembly {
                cwd: Some(work),
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
            global,
            model,
            invocations: std::sync::atomic::AtomicUsize::new(0),
            dir,
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
            let n = tokio::time::timeout(Duration::from_secs(120), reader.read_line(&mut line))
                .await
                .expect("the call answered")
                .unwrap();
            assert!(n > 0, "connection closed before `{method}` answered");
            let frame: Value = serde_json::from_str(&line).unwrap();
            if frame.get("id").is_some() {
                return frame;
            }
        }
    }

    async fn install(&self, name: &str, files: &[(&str, &str)]) -> Value {
        let (archive, checksum) = package(self.dir.path(), name, files);
        self.call(
            "plugin.install",
            json!({"name": name, "version": "1.0.0",
                   "download_url": format!("file://{}", archive.display()),
                   "checksum": checksum}),
        )
        .await
    }

    async fn command(&self, name: &str) -> Option<Value> {
        let listed = self.call("commands.list", json!({})).await;
        listed["result"]["commands"]
            .as_array()
            .unwrap_or_else(|| panic!("commands.list: {listed}"))
            .iter()
            .find(|c| c["name"] == name)
            .cloned()
    }

    async fn new_session(&self) -> String {
        let created = self.call("session.create", json!({})).await;
        created["result"]["session_id"]
            .as_str()
            .unwrap_or_else(|| panic!("session.create: {created}"))
            .to_string()
    }

    /// Runs one turn of `session` in which the model calls `Skill` for
    /// `skill`; returns what the model was shown from that call on.
    async fn invoke_skill(&self, session: &str, skill: &str) -> String {
        let n = self
            .invocations
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let id = format!("toolu_skill_{n}");
        self.model.push_turn(skill_round(&id, skill));
        self.model.push_turn(text_round("done"));
        let turn = self
            .call(
                "session.run_turn",
                json!({"session_id": session, "message": "use the skill"}),
            )
            .await;
        assert!(turn.get("result").is_some(), "turn failed: {turn}");
        let marker = format!("\"id\":\"{id}\"");
        (0..self.model.calls())
            .filter_map(|i| self.model.nth_request(i))
            .map(|r| serde_json::to_string(&r).unwrap())
            .find_map(|shown| shown.find(&marker).map(|at| shown[at..].to_string()))
            .unwrap_or_else(|| panic!("no request carried the call `{id}`"))
    }
}

const TOUR: &str = "---\nname: tour\ndescription: Walk through the demo package\n---\n\
                    PLUGIN-TOUR-BODY: show the package's tools.\n";

#[tokio::test(flavor = "multi_thread")]
async fn a_packaged_skill_follows_its_package_and_gives_way_to_the_disk() {
    let daemon = Daemon::start().await;

    let installed = daemon
        .install(
            "touring",
            &[
                ("plugin.toml", &manifest("touring")),
                ("skills/tour/SKILL.md", TOUR),
            ],
        )
        .await;
    let disclosure = &installed["result"]["disclosure"];
    assert_eq!(disclosure["inert"], false, "{installed}");
    assert_eq!(disclosure["skills"], json!(["tour"]), "{installed}");
    let visible = disclosure["model_visible"].as_array().unwrap();
    for (origin, text) in [
        ("skill `tour` description", "Walk through the demo package"),
        (
            "skill `tour` body",
            "PLUGIN-TOUR-BODY: show the package's tools.",
        ),
    ] {
        assert!(
            visible
                .iter()
                .any(|v| v["origin"] == origin && v["text"].as_str().unwrap().trim() == text),
            "`{origin}` is disclosed: {disclosure}"
        );
    }

    let listed = daemon
        .command("tour")
        .await
        .expect("the packaged skill is a command");
    assert_eq!(listed["source"], "plugin", "{listed}");
    let running = daemon.new_session().await;
    let shown = daemon.invoke_skill(&running, "tour").await;
    assert!(shown.contains("PLUGIN-TOUR-BODY"), "{shown}");

    let disabled = daemon
        .call("plugin.disable", json!({"name": "touring"}))
        .await;
    assert_eq!(disabled["result"]["enabled"], false, "{disabled}");
    assert!(daemon.command("tour").await.is_none());
    let shown = daemon.invoke_skill(&running, "tour").await;
    assert!(
        !shown.contains("PLUGIN-TOUR-BODY"),
        "the running session lost the skill with its package: {shown}"
    );
    let told = serde_json::to_string(&daemon.model.nth_request(daemon.model.calls() - 1)).unwrap();
    assert!(
        told.contains(r#"\"tour\" was just removed"#),
        "the running session was told: {told}"
    );

    daemon
        .call("plugin.enable", json!({"name": "touring"}))
        .await;
    assert!(daemon.command("tour").await.is_some());
    let shown = daemon.invoke_skill(&running, "tour").await;
    assert!(
        shown.contains("Skill 'tour' is already loaded"),
        "back with its package, and the session already holds its text: {shown}"
    );
    let told = serde_json::to_string(&daemon.model.nth_request(daemon.model.calls() - 1)).unwrap();
    assert!(told.contains(r#"\"tour\" was just added"#), "{told}");

    let disk = daemon.global.join("skills").join("tour");
    std::fs::create_dir_all(&disk).unwrap();
    std::fs::write(
        disk.join("SKILL.md"),
        "---\nname: tour\ndescription: The user's own tour\n---\nDISK-TOUR-BODY\n",
    )
    .unwrap();
    daemon.call("config.reload", json!({})).await;

    let plugins = daemon.call("plugin.list", json!({})).await;
    let touring = plugins["result"]["plugins"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "touring")
        .cloned()
        .unwrap_or_else(|| panic!("plugin.list: {plugins}"));
    assert_eq!(touring["skills"], json!(["tour"]), "{touring}");
    let conflicts = touring["skill_conflicts"].as_array().unwrap();
    assert_eq!(conflicts.len(), 1, "{touring}");
    assert_eq!(conflicts[0]["skill"], "tour");
    assert_eq!(conflicts[0]["shadowed_by"], "user");
    assert!(
        conflicts[0]["path"]
            .as_str()
            .unwrap()
            .ends_with("skills/tour/SKILL.md"),
        "{touring}"
    );

    assert_eq!(daemon.command("tour").await.unwrap()["source"], "user");
    let shown = daemon
        .invoke_skill(&daemon.new_session().await, "tour")
        .await;
    assert!(shown.contains("DISK-TOUR-BODY"), "{shown}");
    assert!(!shown.contains("PLUGIN-TOUR-BODY"), "{shown}");

    std::fs::remove_dir_all(&disk).unwrap();
    daemon.call("config.reload", json!({})).await;
    assert_eq!(daemon.command("tour").await.unwrap()["source"], "plugin");
    let removed = daemon
        .call("plugin.uninstall", json!({"name": "touring"}))
        .await;
    assert_eq!(removed["result"]["success"], true, "{removed}");
    assert!(daemon.command("tour").await.is_none());
    assert!(!daemon.global.join("plugins/cache/touring").exists());
    assert_eq!(
        std::fs::read_dir(daemon.global.join("skills"))
            .unwrap()
            .count(),
        0,
        "the package's skills never went into a skill directory"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_package_with_a_broken_skill_does_not_install() {
    let daemon = Daemon::start().await;
    let refused = daemon
        .install(
            "broken",
            &[
                ("plugin.toml", &manifest("broken")),
                ("skills/tour/SKILL.md", TOUR),
                (
                    "skills/lost/SKILL.md",
                    "---\nname: lost\ncontext: elsewhere\ndescription: d\n---\nbody\n",
                ),
            ],
        )
        .await;
    let message = refused["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("skill `lost`") && message.contains("context"),
        "the refusal names the skill and the field: {refused}"
    );
    assert!(!daemon.global.join("plugins/cache/broken/1.0.0").exists());
    assert!(daemon.command("tour").await.is_none());
}
