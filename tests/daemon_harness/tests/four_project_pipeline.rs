//! One daemon, four projects, one pipeline.
//!
//! The shape is a real orchestration: **A** produces data, a person reviews
//! it and commits, **B** and **C** pick the new data up and work in parallel,
//! **D** checks whether they agree, and a disagreement goes back to whichever
//! of them can fix it. Four independent repositories, four sessions, one
//! daemon — and A is a dependency of B and C, the way a submodule is, so the
//! same file exists at two paths and the two must not be confused.
//!
//! What this asks that the per-tier cases in `daemon_surface.rs` do not: the
//! pipeline runs the tiers *against each other*. Every project here has its
//! own settings, its own skill and its own sub-agent type, and the assertion
//! is never "B can see B's" alone — it is "B can see B's and cannot see
//! A's, C's or D's", while all four sessions are alive in one process and
//! two of them are running at the same time.
//!
//! The orchestrator is the test. That is the point of the shape: the daemon
//! offers sessions, not a workflow, so the ordering, the review gate and the
//! send-back are the host's to write, and the handoff between projects is
//! git — here, a file copy. Nothing in the daemon knows these four sessions
//! have anything to do with each other.

use daemon_harness::{Block, Daemon, DaemonOptions, Mode, ProviderStub, Reply, World};
use rpc_client::Target;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The four projects, and the marks that tell their prompts apart.
///
/// One struct rather than four sets of literals: every stage asserts "this
/// mark and none of the other three", and a list that can be iterated is what
/// makes the negative half of that assertion possible to write once.
struct Pipeline {
    world: World,
    provider: ProviderStub,
    daemon: Daemon,
    roots: Vec<(&'static str, PathBuf)>,
}

/// Scene per project: A/B/C write code, D only reads and reports, so it runs
/// under `chat` — which is not this daemon's own scene, and has to be
/// activated before a session can ask for it.
const D_SCENE: &str = "chat";

impl Pipeline {
    async fn start(mode: Mode) -> anyhow::Result<Self> {
        let world = World::new()?;

        // Memory off, daemon-wide. It is exercised in its own case; here it
        // would fire a model call after every turn, out of band, and this
        // file scripts the provider call by call.
        world.write_global_settings(&json!({ "memory_enabled": false }))?;

        let mut roots = Vec::new();
        for name in ["a-data", "b-left", "c-right", "d-verify"] {
            let root = world.project(name)?;
            world.write_project_settings(
                name,
                &json!({ "prompt_append": format!("MARK-{}", name.to_uppercase()) }),
            )?;
            world.write_project_skill(name, &format!("skill-{name}"), "a skill of this project")?;
            world.write_project_agent_type(
                name,
                &format!("agent-{name}"),
                "an agent of this project",
            )?;
            roots.push((name, root));
        }

        // A is the daemon's own project — the one a session gets when it
        // names none. B, C and D are reachable only because `session.create`
        // takes a `project_root`.
        let provider = ProviderStub::start().await?;
        let daemon = Daemon::start(
            &world,
            &provider,
            DaemonOptions::new(world.project("a-data")?).mode(mode),
        )
        .await?;

        Ok(Self {
            world,
            provider,
            daemon,
            roots,
        })
    }

    fn root(&self, name: &str) -> PathBuf {
        self.roots
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, p)| p.clone())
            .expect("a project of this pipeline")
    }

    /// The last request a *turn* made.
    ///
    /// A turn is not the only thing that calls the model: naming a session
    /// and extracting memories both fire their own request, with their own
    /// (or no) system prompt and never a tool table. Reading `last()` gets
    /// whichever of those landed most recently.
    fn last_turn_call(&self) -> daemon_harness::SeenRequest {
        self.provider
            .calls()
            .into_iter()
            .rev()
            .find(|c| !c.system_text().is_empty() && !c.tool_names().is_empty())
            .expect("some request carried a turn's prompt and tools")
    }

    fn last_prompt(&self) -> String {
        self.last_turn_call().system_text()
    }

    /// The assertion this whole file is built around: what the session was
    /// just told belongs to `project`, and to no other project in the
    /// pipeline.
    ///
    /// Three tiers on one request, so there is no chance of reading one
    /// session's prompt against another session's tool table. The sub-agent
    /// catalog rides on the `Agent` tool's description and so is only there
    /// for a scene that offers that tool — `chat` does not, which is checked
    /// where D runs rather than smuggled in here.
    fn only_saw(&self, project: &str) {
        let call = self.last_turn_call();
        let prompt = call.system_text();
        let catalog = call.body["tools"]
            .as_array()
            .and_then(|tools| {
                tools
                    .iter()
                    .find(|t| t["name"] == "Agent")
                    .and_then(|t| t["description"].as_str().map(str::to_string))
            })
            .unwrap_or_default();
        for (name, _) in &self.roots {
            let mine = *name == project;
            let mark = format!("MARK-{}", name.to_uppercase());
            assert_eq!(
                prompt.contains(&mark),
                mine,
                "settings of `{name}` in a `{project}` session's prompt: {mine} expected"
            );
            assert_eq!(
                prompt.contains(&format!("skill-{name}")),
                mine,
                "skills of `{name}` in a `{project}` session's prompt: {mine} expected"
            );
            if !catalog.is_empty() {
                assert_eq!(
                    catalog.contains(&format!("agent-{name}")),
                    mine,
                    "agent types of `{name}` in a `{project}` session's catalog: {mine} expected"
                );
            }
        }
    }

    fn transcript_of(&self, project: &str, session: &str) -> PathBuf {
        self.world
            .transcript_dir(&self.root(project))
            .join(format!("{session}.jsonl"))
    }
}

/// The pipeline, end to end.
async fn four_projects_run_one_pipeline(mode: Mode) -> anyhow::Result<()> {
    let p = Pipeline::start(mode).await?;
    let mut client = p.daemon.connect().await?;

    // ── Stage A: produce ─────────────────────────────────────────────────
    //
    // The path is relative, which is the whole point: it resolves against
    // the session's own project, and the same reply will be used again for
    // B and C without naming a directory.
    p.provider.script([
        Reply::Blocks(vec![Block::tool(
            "a-1",
            "Write",
            json!({ "file_path": "report.json", "content": "{\"rows\":2}\n" }),
        )]),
        Reply::text("A-PRODUCED-THE-DATA"),
    ]);
    let a = client.session_create(json!({})).await?;
    let turn = client
        .session_run_turn(&a, "generate the report", "a-1", None)
        .await?;
    assert!(turn.text.contains("A-PRODUCED-THE-DATA"), "{turn:?}");
    p.only_saw("a-data");
    assert_eq!(
        std::fs::read_to_string(p.root("a-data").join("report.json"))?,
        "{\"rows\":2}\n",
        "A's tool call must resolve against A's own directory"
    );

    // ── The review gate, and the commit ──────────────────────────────────
    //
    // No daemon involved, deliberately. A person reads what A produced and
    // decides; the handoff to B and C is git, which is a file appearing at a
    // path they already look at. The daemon has no shared state for this and
    // does not need any.
    let produced = std::fs::read_to_string(p.root("a-data").join("report.json"))?;
    assert!(produced.contains("rows"), "nothing to review");
    for consumer in ["b-left", "c-right"] {
        let vendored = p.root(consumer).join("vendor").join("a-data");
        std::fs::create_dir_all(&vendored)?;
        std::fs::write(vendored.join("report.json"), &produced)?;
    }

    // ── Stage B and C: in parallel ───────────────────────────────────────
    //
    // Two sessions, two connections, one daemon, at the same time. Both get
    // the same two scripted replies — the FIFO makes no promise about which
    // turn gets which — so the assertion cannot be about the answers. It is
    // about where the relative write landed: same reply, two projects, two
    // files.
    for consumer in ["b-left", "c-right"] {
        p.provider.script_when(
            &format!("MARK-{}", consumer.to_uppercase()),
            [
                Reply::Blocks(vec![Block::tool(
                    "x-1",
                    "Write",
                    json!({ "file_path": "out.txt", "content": format!("consumed by {consumer}\n") }),
                )]),
                Reply::text(format!("{}-DONE", consumer.to_uppercase())),
            ],
        );
    }
    let b = client
        .session_create(json!({ "project_root": p.root("b-left").to_string_lossy() }))
        .await?;
    let c = client
        .session_create(json!({ "project_root": p.root("c-right").to_string_lossy() }))
        .await?;

    let mut client_c = p.daemon.connect().await?;
    let (b_turn, c_turn) = tokio::join!(
        client.session_run_turn(&b, "consume the new data", "b-1", None),
        client_c.session_run_turn(&c, "consume the new data", "c-1", None),
    );
    let (b_turn, c_turn) = (b_turn?, c_turn?);
    assert!(
        b_turn.text.contains("B-LEFT-DONE") && c_turn.text.contains("C-RIGHT-DONE"),
        "two turns in flight got each other's answers: {b_turn:?} {c_turn:?}"
    );
    for consumer in ["b-left", "c-right"] {
        assert_eq!(
            std::fs::read_to_string(p.root(consumer).join("out.txt"))?,
            format!("consumed by {consumer}\n"),
            "`{consumer}`'s turn did not run in `{consumer}`"
        );
        assert!(
            p.root(consumer)
                .join("vendor")
                .join("a-data")
                .join("report.json")
                .exists(),
            "`{consumer}` should still be holding the data it was given"
        );
    }
    // A's own directory is not where either of them wrote, even though A is
    // vendored inside both and is this daemon's own project besides.
    assert!(
        !p.root("a-data").join("out.txt").exists(),
        "a consumer's relative write reached the daemon's project instead of its own"
    );

    // ── Stage D: verify, under another scene ─────────────────────────────
    // Two replies: the verdict, then the title `chat` asks for after a first
    // turn. This daemon's own scene is `coding`, which does not name — so a
    // name coming back at all is D's scene being asked, not the pool's.
    p.provider.script([
        Reply::text("D-SAYS-THEY-DISAGREE"),
        Reply::text("一致性复核"),
    ]);
    client
        .call("scene.activate", json!({ "scene": D_SCENE }))
        .await?;
    let d = client
        .session_create(json!({
            "scene": D_SCENE,
            "project_root": p.root("d-verify").to_string_lossy(),
        }))
        .await?;
    let verdict = client
        .session_run_turn(&d, "do B and C agree?", "d-1", None)
        .await?;
    assert!(verdict.text.contains("D-SAYS-THEY-DISAGREE"), "{verdict:?}");
    assert_eq!(
        verdict
            .response
            .as_ref()
            .and_then(|r| r.result.as_ref())
            .map(|r| r["name"].clone())
            .unwrap_or_default(),
        "一致性复核",
        "D's own scene names its sessions; the daemon's does not: {verdict:?}"
    );
    p.only_saw("d-verify");
    // `chat` has no Bash in its whitelist and `coding` does — the scene a
    // session asked for reached it, on a daemon whose own scene is the other
    // one, while three `coding` sessions were live beside it.
    let d_tools = p.provider.calls().last().expect("D's call").tool_names();
    assert!(
        !d_tools.contains(&"Bash".to_string()),
        "D ran under `coding`'s tool surface, not the scene it asked for: {d_tools:?}"
    );

    // ── The send-back: D rejected, so B runs again ───────────────────────
    //
    // Through a close first, which frees the running engine and leaves only
    // the transcript — the state a daemon restart or an idle eviction would
    // also leave. What comes back has to be B again: B's project, and the
    // scene B was created under.
    client.session_close(&b).await?;
    p.provider.script([Reply::text("B-FIXED-IT")]);
    let redo = client
        .session_run_turn(&b, "D rejected it, try again", "b-2", None)
        .await?;
    assert!(redo.text.contains("B-FIXED-IT"), "{redo:?}");
    p.only_saw("b-left");

    // ── Four sessions, four projects, one list ───────────────────────────
    let listed = client.session_list().await?;
    let sessions = listed.result.as_ref().expect("session.list result")["sessions"]
        .as_array()
        .expect("an array")
        .clone();
    for (id, project, scene) in [
        (&a, "a-data", "coding"),
        (&b, "b-left", "coding"),
        (&c, "c-right", "coding"),
        (&d, "d-verify", D_SCENE),
    ] {
        let entry = sessions
            .iter()
            .find(|s| s["session_id"] == id.as_str())
            .unwrap_or_else(|| panic!("session for `{project}` missing from the list: {listed:?}"));
        assert_eq!(
            entry["project_root"].as_str().map(canonical),
            Some(canonical(&p.root(project).to_string_lossy())),
            "the list disagrees about which project this session belongs to: {entry}"
        );
        assert_eq!(entry["scene"], scene, "{entry}");
        assert!(
            p.transcript_of(project, id).exists(),
            "`{project}`'s transcript is not in `{project}`'s directory"
        );
    }

    // ── An operator console, belonging to no project at all ──────────────
    //
    // The fifth session in the same daemon, and the one that must not be
    // filed under any of the four: `project_root: null` is not "the
    // daemon's project", it is "no project", and its transcript has its own
    // place to go.
    p.provider
        .script([Reply::text("CONSOLE-ANSWERED"), Reply::text("控制台")]);
    let console = client
        .session_create(json!({ "scene": D_SCENE, "project_root": null }))
        .await?;
    client
        .session_run_turn(&console, "status?", "k-1", None)
        .await?;
    for (name, _) in &p.roots {
        assert!(
            !p.last_prompt()
                .contains(&format!("MARK-{}", name.to_uppercase())),
            "a session bound to no project read `{name}`'s settings"
        );
    }
    assert!(
        p.world
            .no_project_transcript_dir()
            .join(format!("{console}.jsonl"))
            .exists(),
        "a no-project session's transcript was filed under a project"
    );

    // ── The verifier's scene is switched off ─────────────────────────────
    //
    // An operator deactivates `chat` while D's transcript is still on disk.
    // Reaching for D now must fail loudly: D is a chat session, and serving
    // it under `coding` — this daemon's own scene — would quietly give it a
    // different system prompt and a different tool surface than the one it
    // has been running with all along.
    client.session_close(&d).await?;
    client.session_close(&console).await?;
    client
        .call("scene.deactivate", json!({ "scene": D_SCENE }))
        .await?;
    let refused = client
        .call(
            "session.run_turn",
            json!({ "session_id": d, "message": "one more look" }),
        )
        .await?;
    let error = refused.error.as_ref().expect("an error, not a fresh turn");
    assert_eq!(error.code, -32006, "expected SCENE_MISMATCH: {refused:?}");
    assert_eq!(
        error.data.as_ref().map(|d| d["recorded_scene"].clone()),
        Some(json!(D_SCENE)),
        "the refusal must name the scene the session actually belongs to: {refused:?}"
    );
    // B is untouched by any of it — the refusal is about D's scene, not
    // about this daemon having stopped serving sessions.
    p.provider.script([Reply::text("B-STILL-WORKS")]);
    let still = client
        .session_run_turn(&b, "still there?", "b-3", None)
        .await?;
    assert!(still.text.contains("B-STILL-WORKS"), "{still:?}");

    p.daemon.stop().await;
    Ok(())
}

/// `/tmp` is a symlink on macOS and the daemon records the resolved path, so
/// the two sides of a path comparison have to be resolved the same way.
fn canonical(path: &str) -> String {
    std::fs::canonicalize(Path::new(path))
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string())
}

/// One project's configuration changes while the daemon is serving three
/// others. Nothing is restarted and nothing calls `config.reload`.
async fn a_consumers_settings_change_reaches_it_mid_pipeline(mode: Mode) -> anyhow::Result<()> {
    let p = Pipeline::start(mode).await?;
    let mut client = p.daemon.connect().await?;
    let b = p.root("b-left");
    let target = Target::project(b.clone()).scene("coding");

    let before = client.config_get(&target, "effective").await?;
    assert_eq!(
        before.result.as_ref().expect("config.get result")["settings"]["prompt_append"],
        "MARK-B-LEFT",
        "{before:?}"
    );

    p.world
        .write_project_settings("b-left", &json!({ "prompt_append": "MARK-B-LEFT-V2" }))?;

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut seen = false;
    while std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let now = client.config_get(&target, "effective").await?;
        if now.result.as_ref().expect("config.get result")["settings"]["prompt_append"]
            == "MARK-B-LEFT-V2"
        {
            seen = true;
            break;
        }
    }
    assert!(
        seen,
        "a project this daemon serves had to wait for a restart to pick up its own settings"
    );

    // And it is the *prompt* that changed, not only what `config.get` reports.
    p.provider.script([Reply::text("after")]);
    let session = client
        .session_create(json!({ "project_root": b.to_string_lossy() }))
        .await?;
    client
        .session_run_turn(&session, "hello", "t1", None)
        .await?;
    assert!(
        p.last_prompt().contains("MARK-B-LEFT-V2"),
        "the reloaded settings never reached a session's prompt"
    );

    // The other three projects are untouched by any of it.
    for other in ["a-data", "c-right", "d-verify"] {
        let theirs = client
            .config_get(&Target::project(p.root(other)).scene("coding"), "effective")
            .await?;
        assert_eq!(
            theirs.result.as_ref().expect("config.get result")["settings"]["prompt_append"],
            format!("MARK-{}", other.to_uppercase()),
            "reloading one project moved another's settings"
        );
    }

    p.daemon.stop().await;
    Ok(())
}

/// Provider configuration is per project too, and writing one project's is
/// not writing the daemon's.
async fn a_provider_can_be_configured_for_one_project_only(mode: Mode) -> anyhow::Result<()> {
    let p = Pipeline::start(mode).await?;
    let mut client = p.daemon.connect().await?;
    let c = p.root("c-right");

    let written = client
        .config_set_provider(
            &Target::project(c.clone()),
            json!({
                "provider_id": "cheap",
                "config": {
                    "api_type": "openai_compatible",
                    "base_url": "https://example.invalid/v1",
                    "api_key": "k",
                    "default_model": "cheap-1"
                },
                "default_provider": "cheap"
            }),
        )
        .await?;
    let written = written.result.as_ref().expect("setProvider result");
    assert_eq!(
        canonical(written["written_to"].as_str().expect("written_to")),
        canonical(&c.join(".atta").join("settings.json").to_string_lossy()),
        "setProvider wrote somewhere other than the project it was given: {written}"
    );

    let on_disk: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(
        c.join(".atta").join("settings.json"),
    )?)?;
    assert_eq!(on_disk["providers"]["cheap"]["default_model"], "cheap-1");
    assert_eq!(
        on_disk["prompt_append"], "MARK-C-RIGHT",
        "the rest of the project's settings were clobbered by the provider write"
    );

    let theirs = client
        .config_get_provider(&Target::project(p.root("a-data")), false)
        .await?;
    let theirs = theirs.result.as_ref().expect("getProvider result");
    assert!(
        theirs["providers"]["cheap"].is_null(),
        "one project's provider配置 reached another's: {theirs}"
    );

    p.daemon.stop().await;
    Ok(())
}

// ── In this process ──────────────────────────────────────────────────────

#[tokio::test]
async fn four_projects_run_one_pipeline_here() -> anyhow::Result<()> {
    four_projects_run_one_pipeline(Mode::InProcess).await
}

#[tokio::test]
async fn a_consumers_settings_change_reaches_it_mid_pipeline_here() -> anyhow::Result<()> {
    a_consumers_settings_change_reaches_it_mid_pipeline(Mode::InProcess).await
}

#[tokio::test]
async fn a_provider_can_be_configured_for_one_project_only_here() -> anyhow::Result<()> {
    a_provider_can_be_configured_for_one_project_only(Mode::InProcess).await
}

// ── Against a spawned daemon ─────────────────────────────────────────────

/// The same three, against `attacored` as a process. A spawned daemon
/// resolves its own paths, settings and model endpoint from a command line
/// and an environment; every project-scoped thing this file asserts is
/// resolved again there, and a difference between the modes is a real one.
#[tokio::test]
#[ignore = "spawns a daemon process"]
async fn the_pipeline_holds_against_a_spawned_daemon() {
    four_projects_run_one_pipeline(Mode::Spawned)
        .await
        .expect("the pipeline");
    a_consumers_settings_change_reaches_it_mid_pipeline(Mode::Spawned)
        .await
        .expect("the reload");
    a_provider_can_be_configured_for_one_project_only(Mode::Spawned)
        .await
        .expect("the provider");
}
