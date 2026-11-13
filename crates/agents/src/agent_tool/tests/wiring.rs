//! Tests: wiring.

use super::*;

/// Parent `Settings` for a sub-agent spawn, pointed at a throwaway temp
/// directory so `Builder::build()`'s skill/agent/plugin scans have
/// nothing real to find.
pub(super) fn parent_settings_for_test(scope: &str) -> (LoadedSettings, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "agix-subagent-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let mut s = base::settings::LoadedSettings::new(
        Settings::defaults_for("test-model"),
        base::settings::PathSettings::default(),
    );
    s.paths = PathSettings {
        user_data_dir: dir.clone(),
        global_data_dir: dir.clone(),
        workspace: base::settings::Workspace::project_of(&dir),
        local_data_dir: dir.clone(),
        scope: scope.to_string(),
    };
    // Keep the turn from making an extra post-turn extraction call.
    s.values.memory_enabled = false;
    (s, dir)
}

pub(super) struct CapturesHost(pub(super) std::sync::Mutex<Option<runtime::agent::HostContracts>>);

impl runtime::module::Module for CapturesHost {
    fn id(&self) -> &'static str {
        "captures-host"
    }
    fn on_session(
        &self,
        cx: &mut runtime::module::SessionContext<'_>,
    ) -> Result<(), runtime::module::ModuleError> {
        *self.0.lock().unwrap() = Some(cx.host.clone());
        Ok(())
    }
}

/// The host contracts a session built with nothing set resolves, without
/// its modules: what a wired `AgentTool` is handed, ready to adjust.
pub(super) fn host_contracts() -> runtime::agent::HostContracts {
    let (settings, dir) = parent_settings_for_test("code");
    let probe = Arc::new(CapturesHost(Default::default()));
    let built = Builder::new()
        .scene(Arc::new(scene::code::CodeScene::default()))
        .model(Arc::new(DummyModel))
        .settings(Arc::new(settings))
        .skip_warmup(true)
        .module(probe.clone())
        .build();
    let _ = std::fs::remove_dir_all(&dir);
    drop(built.expect("the session builds"));
    let mut host = probe
        .0
        .lock()
        .unwrap()
        .take()
        .expect("the module saw the host");
    host.modules.clear();
    host
}

/// One tool-use round then a text answer — the minimum needed to observe
/// a sub-agent's tool call and its final text.
pub(super) struct ProbeThenTextModel {
    pub(super) calls: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait]
impl Model for ProbeThenTextModel {
    fn api_type(&self) -> base::provider::ApiType {
        base::provider::ApiType::Anthropic
    }
    async fn stream(
        &self,
        _prompt_blocks: Vec<base::prompt::PromptBlock>,
        _tools: Vec<base::interface::model::ToolDef>,
        _messages: Vec<ModelMessage>,
        _params: base::interface::model::StreamParams,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError> {
        use base::interface::model::ModelEvent;
        let call_no = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let events: Vec<Result<ModelEvent, base::interface::model::ModelError>> = if call_no == 0 {
            vec![
                Ok(ModelEvent::ToolUse {
                    id: "toolu_probe".into(),
                    name: "Probe".into(),
                    input: serde_json::json!({}),
                }),
                Ok(ModelEvent::EndTurn {
                    stop_reason: "tool_use".into(),
                    usage: Default::default(),
                }),
            ]
        } else {
            vec![
                Ok(ModelEvent::TextDelta {
                    text: "sub-agent answer".into(),
                }),
                Ok(ModelEvent::EndTurn {
                    stop_reason: "end_turn".into(),
                    usage: Default::default(),
                }),
            ]
        };
        Ok(Box::new(futures::stream::iter(events)))
    }
}

/// Records every execution so a test can prove a denial actually stopped
/// the tool from running, not merely logged something.
pub(super) struct CountingProbeTool {
    pub(super) runs: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait]
impl base::tool::Tool for CountingProbeTool {
    fn name(&self) -> &str {
        "Probe"
    }
    fn spec(&self) -> base::tool::ToolSpec {
        base::tool::ToolSpec::new()
    }

    fn description(&self) -> &str {
        "counting probe tool (test tool)"
    }

    fn input_schema(&self, _: base::tool::PathStyle) -> Value {
        serde_json::json!({})
    }
    async fn call(
        &self,
        _input: Value,
        _ctx: ToolContext,
        _progress: ProgressSender,
    ) -> Result<base::tool::ToolResult, base::error::ToolError> {
        self.runs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(base::tool::ToolResult {
            content: ToolResultContent::Text("probed".into()),
            is_error: false,
            structured_content: None,
            mcp_meta: None,
            new_messages: None,
        })
    }
}

/// A parent policy that records what it was asked about and
/// answers with a fixed outcome.
pub(super) struct RecordingPermission {
    pub(super) seen: Arc<std::sync::Mutex<Vec<String>>>,
    pub(super) deny: bool,
}

#[async_trait]
impl PermissionPolicy for RecordingPermission {
    async fn decide(
        &self,
        req: &base::interface::permission::PermissionRequest<'_>,
    ) -> base::interface::permission::PermissionOutcome {
        let tool_name = req.tool_name;
        self.seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(tool_name.to_string());
        if self.deny {
            base::interface::permission::PermissionOutcome::Deny {
                reason: "parent said no".into(),
            }
        } else {
            base::interface::permission::PermissionOutcome::Permit
        }
    }
}

/// A parent wired the way `Builder::build()` wires it, with a scripted model
/// and a `Probe` tool the sub-agent can call.
pub(super) fn wired_parent(
    scope: &str,
) -> (
    TestParent,
    Arc<InMemoryToolRegistry>,
    Arc<std::sync::atomic::AtomicUsize>,
    std::path::PathBuf,
) {
    let runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let tools = InMemoryToolRegistry::new();
    tools.register(Arc::new(CountingProbeTool { runs: runs.clone() }));
    let tools = Arc::new(tools);
    let (settings, dir) = parent_settings_for_test(scope);
    let parent = TestParent {
        settings: Some(settings),
        ..TestParent::on(
            Arc::new(ProbeThenTextModel {
                calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }),
            tools.clone(),
        )
    };
    (parent, tools, runs, dir)
}

pub(super) fn wired_agent_tool(
    scope: &str,
) -> (
    AgentTool,
    Arc<InMemoryToolRegistry>,
    Arc<std::sync::atomic::AtomicUsize>,
    std::path::PathBuf,
) {
    let (parent, tools, runs, dir) = wired_parent(scope);
    (parent.agent_tool(), tools, runs, dir)
}

/// A sub-agent spawned with a history store and a parent session id
/// must persist its own transcript as a sidechain linked back to the
/// parent — this is what makes `HistoryStore::child_sessions` (and thus
/// `session.list {parent_session_id}`) find it.
#[tokio::test]
async fn subagent_transcript_is_written_as_a_linked_sidechain() {
    let (parent, tools, _runs, dir) = wired_parent("code");
    let history_dir = std::env::temp_dir().join(format!(
        "agix-subagent-history-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let store: Arc<dyn HistoryStore> = Arc::new(
        history::store::JsonlHistoryStore::with_roots(
            &dir,
            history::path::HistoryRoots::under(&history_dir),
        )
        .await
        .unwrap(),
    );
    let agent_tool = TestParent {
        host: Some(runtime::agent::HostContracts {
            history_store: Some(store.clone()),
            ..host_contracts()
        }),
        id: "parent-session-1".to_string(),
        ..parent
    }
    .agent_tool();

    let _text = agent_tool
        .run_sub_tagged(
            "go".into(),
            tools,
            dir.clone(),
            tokio_util::sync::CancellationToken::new(),
            PermissionSet::without_grants(Arc::new(AlwaysPermit)),
            Some("explore"),
            Some(("parent-session-1", 1, "")),
            SubRun::default(),
        )
        .await
        .expect("sub-agent turn should complete");

    let children = store.child_sessions("parent-session-1").await.unwrap();
    assert_eq!(
        children.len(),
        1,
        "exactly one sidechain must be linked to the parent"
    );
    let entries = store.load(children[0]).await.unwrap();
    match &entries[0].entry {
        history::entry::LogEntry::Meta {
            parent_session_id,
            session_kind,
            scene,
            ..
        } => {
            assert_eq!(parent_session_id.as_deref(), Some("parent-session-1"));
            assert_eq!(*session_kind, history::entry::SessionKind::Sidechain);
            assert_eq!(scene.as_deref(), Some("code"));
        }
        other => panic!("expected LogEntry::Meta as the first entry, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&history_dir);
}

/// A cancelled sub-agent turn (`cancel` already fired) must *not* get a
/// terminal marker — cancellation means the work was interrupted, not
/// that the conversation reached a real endpoint, so a stale
/// `session.resume` against it must still be allowed to continue rather
/// than being rejected as `SIDECHAIN_TERMINAL`.
#[tokio::test]
async fn cancelled_subagent_run_writes_no_terminal_marker() {
    let (parent, tools, _runs, dir) = wired_parent("code");
    let history_dir = std::env::temp_dir().join(format!(
        "agix-subagent-cancelled-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let store: Arc<dyn HistoryStore> = Arc::new(
        history::store::JsonlHistoryStore::with_roots(
            &dir,
            history::path::HistoryRoots::under(&history_dir),
        )
        .await
        .unwrap(),
    );
    let agent_tool = TestParent {
        host: Some(runtime::agent::HostContracts {
            history_store: Some(store.clone()),
            ..host_contracts()
        }),
        id: "parent-session-1".to_string(),
        ..parent
    }
    .agent_tool();

    let cancel = tokio_util::sync::CancellationToken::new();
    cancel.cancel();
    let _text = agent_tool
        .run_sub_tagged(
            "go".into(),
            tools,
            dir.clone(),
            cancel,
            PermissionSet::without_grants(Arc::new(AlwaysPermit)),
            Some("explore"),
            Some(("parent-session-1", 1, "")),
            SubRun::default(),
        )
        .await
        .expect("a cancelled turn still resolves Ok — cancellation isn't an error");

    let children = store.child_sessions("parent-session-1").await.unwrap();
    let entries = store.load(children[0]).await.unwrap();
    assert!(
        !entries
            .iter()
            .any(|env| matches!(env.entry, history::entry::LogEntry::SessionEnd { .. })),
        "a cancelled sub-agent must not be marked terminal, entries: {entries:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&history_dir);
}

/// A one-shot sub-agent's transcript must end with a
/// `SessionEnd` marker once its single turn concludes — that marker is
/// what lets `session.resume` reject a stale resume against a
/// "finished" sidechain with `SIDECHAIN_TERMINAL` instead of silently
/// reattaching to it.
#[tokio::test]
async fn subagent_run_writes_a_terminal_marker_when_its_task_concludes() {
    let (parent, tools, _runs, dir) = wired_parent("code");
    let history_dir = std::env::temp_dir().join(format!(
        "agix-subagent-terminal-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let store: Arc<dyn HistoryStore> = Arc::new(
        history::store::JsonlHistoryStore::with_roots(
            &dir,
            history::path::HistoryRoots::under(&history_dir),
        )
        .await
        .unwrap(),
    );
    let agent_tool = TestParent {
        host: Some(runtime::agent::HostContracts {
            history_store: Some(store.clone()),
            ..host_contracts()
        }),
        id: "parent-session-1".to_string(),
        ..parent
    }
    .agent_tool();

    let _text = agent_tool
        .run_sub_tagged(
            "go".into(),
            tools,
            dir.clone(),
            tokio_util::sync::CancellationToken::new(),
            PermissionSet::without_grants(Arc::new(AlwaysPermit)),
            Some("explore"),
            Some(("parent-session-1", 1, "")),
            SubRun::default(),
        )
        .await
        .expect("sub-agent turn should complete");

    let children = store.child_sessions("parent-session-1").await.unwrap();
    let entries = store.load(children[0]).await.unwrap();
    match &entries
        .last()
        .expect("transcript should not be empty")
        .entry
    {
        history::entry::LogEntry::SessionEnd { state } => {
            assert_eq!(*state, history::entry::SessionEndState::Completed);
        }
        other => panic!("expected LogEntry::SessionEnd as the last entry, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&history_dir);
}

/// A sub-agent must inherit the parent's telemetry handle
/// instead of `Builder::build()`'s noop default (a channel whose
/// receiver is dropped immediately) — otherwise every event the
/// sub-agent produces (tool timings, permission decisions, cost) is
/// silently discarded, which is indistinguishable from "nothing was
/// ever recorded" from the outside.
#[tokio::test]
async fn subagent_telemetry_events_reach_the_parents_sink() {
    let (parent, tools, _runs, dir) = wired_parent("code");
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let agent_tool = TestParent {
        host: Some(runtime::agent::HostContracts {
            telemetry: telemetry::TelemetryHandle::new(tx),
            ..host_contracts()
        }),
        ..parent
    }
    .agent_tool();

    let _text = agent_tool
        .run_sub_tagged(
            "go".into(),
            tools,
            dir.clone(),
            tokio_util::sync::CancellationToken::new(),
            PermissionSet::without_grants(Arc::new(AlwaysPermit)),
            Some("explore"),
            Some(("parent-session-1", 1, "")),
            SubRun::default(),
        )
        .await
        .expect("sub-agent turn should complete");
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        rx.try_recv().is_ok(),
        "the sub-agent's turn must have emitted at least one telemetry event onto the \
         parent's sink instead of the noop default"
    );
}

/// A sub-agent's events must reach the *parent's* channel, tagged
/// with a stable label, and the text return value the model sees must be
/// unchanged by that forwarding.
#[tokio::test]
async fn subagent_events_reach_the_parent_channel_tagged_without_changing_the_text_result() {
    let (parent, tools, _runs, dir) = wired_parent("code");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let agent_tool = TestParent {
        events: Some(runtime::event_bus::EventBus::new(tx)),
        ..parent
    }
    .agent_tool();

    let text = agent_tool
        .run_sub_tagged(
            "go".into(),
            tools,
            dir.clone(),
            tokio_util::sync::CancellationToken::new(),
            PermissionSet::without_grants(Arc::new(AlwaysPermit)),
            Some("explore"),
            Some(("parent-session", 7, "toolu_parent")),
            SubRun::default(),
        )
        .await
        .expect("sub-agent turn should complete");
    let _ = std::fs::remove_dir_all(&dir);

    // Forwarding is additive: the value handed back to the model must be
    // byte-identical to what the un-forwarded path produces.
    let (plain_tool, plain_tools, _r, plain_dir) = wired_agent_tool("code");
    let baseline = plain_tool
        .run_sub_tagged(
            "go".into(),
            plain_tools,
            plain_dir.clone(),
            tokio_util::sync::CancellationToken::new(),
            PermissionSet::without_grants(Arc::new(AlwaysPermit)),
            Some("explore"),
            None,
            SubRun::default(),
        )
        .await
        .expect("baseline sub-agent turn should complete");
    let _ = std::fs::remove_dir_all(&plain_dir);
    assert_eq!(text, baseline);
    assert!(text.ends_with("sub-agent answer"), "got {text:?}");

    let mut labels = std::collections::HashSet::new();
    let mut kinds = Vec::new();
    let mut saw_probe_tool_use = false;
    let mut lifecycle = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        // The run is bracketed by `AgentSpawned`/`AgentCompleted` on the
        // parent channel; those describe the parent's timeline, so they
        // arrive unwrapped rather than inside `SubagentProgress`.
        // Asserted below, after the progress events.
        let AgentEvent::SubagentProgress {
            agent_label,
            agent_type,
            parent_session_id,
            parent_turn,
            parent_tool_use_id,
            event,
            ..
        } = ev
        else {
            match ev {
                AgentEvent::AgentSpawned { agent_id, .. } => lifecycle.push(("spawned", agent_id)),
                AgentEvent::AgentCompleted {
                    agent_id, outcome, ..
                } => {
                    assert_eq!(outcome, "completed");
                    lifecycle.push(("completed", agent_id));
                }
                other => panic!("unexpected event on the parent channel: {other:?}"),
            }
            continue;
        };
        assert_eq!(agent_type.as_deref(), Some("explore"));
        assert_eq!(parent_session_id, "parent-session");
        assert_eq!(parent_turn, 7);
        assert_eq!(parent_tool_use_id.as_deref(), Some("toolu_parent"));
        labels.insert(agent_label);
        if let AgentEvent::ToolUse { ref name, .. } = *event {
            saw_probe_tool_use = name == "Probe";
        }
        kinds.push(match *event {
            AgentEvent::TextDelta { .. } => "text",
            AgentEvent::ToolUse { .. } => "tool_use",
            AgentEvent::TurnComplete { .. } => "turn_complete",
            _ => "other",
        });
    }

    assert!(
        !kinds.is_empty(),
        "the host must see sub-agent activity, not silence"
    );
    assert_eq!(
        labels.len(),
        1,
        "every event of one sub-agent run must carry the same label, got {labels:?}"
    );
    assert!(
        labels.iter().next().unwrap().starts_with("explore#"),
        "label should name the agent type, got {labels:?}"
    );
    assert!(
        saw_probe_tool_use,
        "the sub-agent's tool call must be visible to the host, saw {kinds:?}"
    );
    assert!(
        kinds.contains(&"turn_complete"),
        "the host needs a terminal marker for the sub-agent node, saw {kinds:?}"
    );

    // `AgentEvent::AgentSpawned`/`AgentCompleted` were declared but never
    // emitted anywhere, so an embedder wiring itself to them got silence
    // and had to reconstruct the boundaries from `agent_label` instead.
    let label = labels.iter().next().unwrap().clone();
    assert_eq!(
        lifecycle,
        vec![("spawned", label.clone()), ("completed", label),],
        "the delegation must be bracketed by AgentSpawned/AgentCompleted \
         carrying the same label its progress events use"
    );
}

/// `parent` with `policy` as its permission, keeping no grants, for a
/// parent session in `/tmp`.
pub(super) fn wire_parent(parent: TestParent, policy: Arc<dyn PermissionPolicy>) -> TestParent {
    TestParent {
        permission: Some(PermissionSet::without_grants(policy)),
        state: Arc::new(base::context::SessionState::new("/tmp".into())),
        ..parent
    }
}

/// `name` with `input`, as the sub-agent's engine puts it to `permission`:
/// the tool its registry has under that name, its own session.
pub(super) async fn sub_decides(
    permission: &PermissionSet,
    tool: Option<&Arc<dyn base::tool::Tool>>,
    name: &str,
    input: &Value,
    cwd: &Path,
    session: &Arc<base::context::SessionState>,
) -> base::interface::permission::PermissionOutcome {
    permission
        .policy
        .decide(&base::interface::permission::PermissionRequest::new(
            name, tool, input, cwd, "sub", session,
        ))
        .await
}

/// A sub-agent's permission checks must reach the parent's
/// policy, and a `Deny` there must actually stop the tool.
#[tokio::test]
async fn subagent_permission_check_routes_to_parent_and_deny_blocks_the_tool() {
    let (parent, tools, runs, dir) = wired_parent("code");
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let agent_tool = TestParent {
        permission: Some(PermissionSet::without_grants(Arc::new(
            RecordingPermission {
                seen: seen.clone(),
                deny: true,
            },
        ))),
        state: Arc::new(base::context::SessionState::new("/tmp".into())),
        ..parent
    }
    .agent_tool();

    // This is the handler the `Agent` tool itself uses for a spawn.
    let perm = agent_tool.sub_permission();
    let _ = agent_tool
        .run_sub_tagged(
            "go".into(),
            tools,
            dir.clone(),
            tokio_util::sync::CancellationToken::new(),
            perm,
            None,
            None,
            SubRun::default(),
        )
        .await;
    let _ = std::fs::remove_dir_all(&dir);

    let seen = seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
    assert!(
        seen.iter().any(|t| t == "Probe"),
        "the sub-agent's tool call must be checked against the parent's policy, saw {seen:?}"
    );
    assert_eq!(
        runs.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "a Deny from the parent must actually prevent the sub-agent's tool from running"
    );
}

/// An `Ask` from the parent must not stall the sub-agent forever — a
/// sub-agent has no channel a human could answer on.
#[tokio::test]
async fn parent_prompt_outcome_becomes_a_legible_denial_inside_a_subagent() {
    struct AlwaysPrompts;
    #[async_trait]
    impl PermissionPolicy for AlwaysPrompts {
        async fn decide(
            &self,
            _req: &base::interface::permission::PermissionRequest<'_>,
        ) -> base::interface::permission::PermissionOutcome {
            base::interface::permission::PermissionOutcome::Prompt {
                prompt_id: "p1".into(),
                message: "run rm -rf?".into(),
                paths: vec![],
                managed: false,
            }
        }
    }

    let parent = test_parent();
    let agent_tool = TestParent {
        permission: Some(PermissionSet::without_grants(Arc::new(AlwaysPrompts))),
        state: Arc::new(base::context::SessionState::new("/tmp".into())),
        ..parent
    }
    .agent_tool();
    let outcome = sub_decides(
        &agent_tool.sub_permission(),
        None,
        "Bash",
        &serde_json::json!({}),
        Path::new("/tmp"),
        &Arc::new(base::context::SessionState::new("/tmp".into())),
    )
    .await;
    match outcome {
        base::interface::permission::PermissionOutcome::Deny { reason } => {
            assert!(reason.contains("parent session"), "got {reason}");
            assert!(reason.contains("run rm -rf?"), "got {reason}");
        }
        other => panic!("expected a denial, got {other:?}"),
    }
}

/// A `Permit` from the parent still permits — the wrapper only adapts
/// the `Prompt` case.
#[tokio::test]
async fn parent_permit_passes_through_unchanged() {
    let parent = test_parent();
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let agent_tool = TestParent {
        permission: Some(PermissionSet::without_grants(Arc::new(
            RecordingPermission {
                seen: seen.clone(),
                deny: false,
            },
        ))),
        state: Arc::new(base::context::SessionState::new("/tmp".into())),
        ..parent
    }
    .agent_tool();
    let outcome = sub_decides(
        &agent_tool.sub_permission(),
        None,
        "Bash",
        &serde_json::json!({}),
        Path::new("/tmp"),
        &Arc::new(base::context::SessionState::new("/tmp".into())),
    )
    .await;
    assert!(matches!(
        outcome,
        base::interface::permission::PermissionOutcome::Permit
    ));
    assert_eq!(seen.lock().unwrap().as_slice(), ["Bash"]);
}

/// The parent's rules decide a sub-agent's call in the sub-agent's own
/// mode when that is stricter, and in the parent's when it is not.
#[tokio::test]
async fn a_sub_agents_call_is_judged_in_its_own_mode_unless_the_parent_plans() {
    use base::interface::permission::PermissionOutcome;
    use base::permission::PermissionMode;
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("notes.txt"), "x").unwrap();
    let registry = Arc::new(InMemoryToolRegistry::new());
    tools::register_builtin_tools(&registry);
    let parent = PermissionSet::of(Arc::new(
        permissions::rule_set_permission::RuleSetPermission::new(Arc::new(
            permissions::gate::PermissionGate::empty(),
        )),
    ));
    let parent_session = Arc::new(
        base::context::SessionState::new(project.path().to_path_buf())
            .with_permission_mode(PermissionMode::AcceptEdits),
    );
    let agent_tool = TestParent {
        permission: Some(parent),
        state: parent_session.clone(),
        ..test_parent()
    }
    .agent_tool();
    let sub = agent_tool.sub_permission();
    let sub_session = Arc::new(base::context::SessionState::new(
        project.path().to_path_buf(),
    ));
    let check = |tool: &'static str, input: Value| {
        let (sub, sub_session, registry) = (sub.clone(), sub_session.clone(), registry.clone());
        let cwd = project.path().to_path_buf();
        async move {
            let found = registry.get(tool);
            sub_decides(&sub, found.as_ref(), tool, &input, &cwd, &sub_session).await
        }
    };
    let write = serde_json::json!({"file_path": project.path().join("a.txt"), "content": "x"});
    // A read-only `Monitor` command: `acceptEdits` lets it through, `default`
    // asks.
    let watch = serde_json::json!({"command": "tail -f notes.txt", "description": "watch"});

    sub_session.set_permission_mode(PermissionMode::AcceptEdits);
    assert!(matches!(
        check("Write", write.clone()).await,
        PermissionOutcome::Permit
    ));
    assert!(matches!(
        check("Monitor", watch.clone()).await,
        PermissionOutcome::Permit
    ));

    sub_session.set_permission_mode(PermissionMode::Plan);
    match check("Write", write.clone()).await {
        PermissionOutcome::Deny { reason } => assert!(reason.contains("plan mode"), "{reason}"),
        other => panic!("a plan-mode sub-agent wrote: {other:?}"),
    }

    sub_session.set_permission_mode(PermissionMode::Default);
    assert!(
        matches!(
            check("Monitor", watch.clone()).await,
            PermissionOutcome::Deny { .. }
        ),
        "a default-mode sub-agent is asked about a Monitor command; with no channel, refused"
    );

    sub_session.set_permission_mode(PermissionMode::BypassPermissions);
    assert!(
        matches!(
            check("Bash", serde_json::json!({"command": "touch made"})).await,
            PermissionOutcome::Permit
        ),
        "a bypassing sub-agent is not asked what its acceptEdits parent would be"
    );

    parent_session.set_permission_mode(PermissionMode::Plan);
    sub_session.set_permission_mode(PermissionMode::AcceptEdits);
    match check("Write", write.clone()).await {
        PermissionOutcome::Deny { reason } => assert!(reason.contains("plan mode"), "{reason}"),
        other => panic!("a sub-agent wrote while its parent planned: {other:?}"),
    }
}

/// A team member is held to the parent's rules, not given a rule set of
/// its own.
#[tokio::test]
async fn a_team_member_is_judged_by_the_parents_rules() {
    use base::interface::permission::PermissionOutcome;
    use base::permission::{PermissionMode, PermissionRule, RuleBehavior, RuleSource};
    let project = tempfile::tempdir().unwrap();
    let registry = Arc::new(InMemoryToolRegistry::new());
    tools::register_builtin_tools(&registry);
    let parent = PermissionSet::of(Arc::new(
        permissions::rule_set_permission::RuleSetPermission::new(Arc::new(
            permissions::gate::PermissionGate::new(permissions::ruleset::RuleSet::new(vec![
                PermissionRule {
                    source: RuleSource::ProjectSettings,
                    behavior: RuleBehavior::Deny,
                    tool_name: "Write".into(),
                    rule_content: Some("**/locked/**".into()),
                },
            ])),
        )),
    ));
    let agent_tool = TestParent {
        permission: Some(parent),
        state: Arc::new(
            base::context::SessionState::new(project.path().to_path_buf())
                .with_permission_mode(PermissionMode::AcceptEdits),
        ),
        ..test_parent()
    }
    .agent_tool();
    let member = agent_tool.inner.permission_handler();
    let write = registry.get("Write");
    let outcome = sub_decides(
        &member,
        write.as_ref(),
        "Write",
        &serde_json::json!({"file_path": project.path().join("locked/a.txt"), "content": "x"}),
        project.path(),
        &Arc::new(
            base::context::SessionState::new(project.path().to_path_buf())
                .with_permission_mode(PermissionMode::AcceptEdits),
        ),
    )
    .await;
    assert!(
        matches!(outcome, PermissionOutcome::Deny { .. }),
        "{outcome:?}"
    );
}

/// A tool whose own check always asks, matched by the content it carries.
struct AsksAboutDeploys;
#[async_trait]
impl base::tool::Tool for AsksAboutDeploys {
    fn name(&self) -> &str {
        "Deploy"
    }
    fn spec(&self) -> ToolSpec {
        ToolSpec::new()
    }
    fn description(&self) -> &str {
        "deploy (test tool)"
    }
    fn input_schema(&self, _: PathStyle) -> Value {
        serde_json::json!({"type": "object"})
    }
    async fn check_permissions(
        &self,
        _: &Value,
        _: &ToolContext,
    ) -> base::tool::PermissionDecision {
        base::tool::PermissionDecision::ask("deploy?")
    }
    fn permission_match_content(&self, input: &Value) -> Option<String> {
        input["target"].as_str().map(str::to_string)
    }
    async fn call(
        &self,
        _: Value,
        _: ToolContext,
        _: ProgressSender,
    ) -> Result<base::tool::ToolResult, base::error::ToolError> {
        Ok(base::tool::ToolResult::text("deployed"))
    }
}

/// "Always allow" answered to a sub-agent's question is kept in the parent
/// session's rule set: the parent's next call of the same kind runs without
/// asking, a different one is still asked about.
#[tokio::test]
async fn a_sub_agents_permit_always_lands_in_the_parents_rules() {
    use base::interface::permission::{PermissionOutcome, PermissionRequest};
    let rules = Arc::new(permissions::rule_set_permission::RuleSetPermission::new(
        Arc::new(permissions::gate::PermissionGate::empty()),
    ));
    let parent_session = Arc::new(base::context::SessionState::new("/tmp".into()));
    let parent = test_parent();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let pending: runtime::agent::PendingPermissions = Arc::default();
    let agent_tool = TestParent {
        permission: Some(PermissionSet::of(rules.clone())),
        state: parent_session.clone(),
        events: Some(runtime::event_bus::EventBus::new(tx)),
        pending: pending.clone(),
        ..parent
    }
    .agent_tool();

    let deploy: Arc<dyn base::tool::Tool> = Arc::new(AsksAboutDeploys);
    let staging = serde_json::json!({"target": "staging"});
    let sub = agent_tool.sub_permission();
    let sub_session = Arc::new(base::context::SessionState::new("/tmp".into()));
    let asked = tokio::spawn({
        let (sub, sub_session, deploy, staging) = (
            sub.clone(),
            sub_session.clone(),
            deploy.clone(),
            staging.clone(),
        );
        async move {
            sub_decides(
                &sub,
                Some(&deploy),
                "Deploy",
                &staging,
                Path::new("/tmp"),
                &sub_session,
            )
            .await
        }
    });
    let prompt_id = loop {
        match rx
            .recv()
            .await
            .expect("the question reaches the parent's channel")
        {
            AgentEvent::PermissionPrompt { prompt_id, .. } => break prompt_id,
            _ => continue,
        }
    };
    pending
        .lock()
        .unwrap()
        .remove(&prompt_id)
        .expect("registered with the parent")
        .send(runtime::agent::PermissionDecision::PermitAlways {
            scope: runtime::agent::PersistScope::Session,
        })
        .unwrap();
    assert!(matches!(asked.await.unwrap(), PermissionOutcome::Permit));

    let parent_decides = |input: Value| {
        let (rules, parent_session, deploy) =
            (rules.clone(), parent_session.clone(), deploy.clone());
        async move {
            rules
                .decide(&PermissionRequest::new(
                    "Deploy",
                    Some(&deploy),
                    &input,
                    Path::new("/tmp"),
                    "parent",
                    &parent_session,
                ))
                .await
        }
    };
    assert!(matches!(
        parent_decides(staging.clone()).await,
        PermissionOutcome::Permit
    ));
    assert!(matches!(
        parent_decides(serde_json::json!({"target": "production"})).await,
        PermissionOutcome::Prompt { .. }
    ));
}

/// A question a sub-agent running as a task asks is put to the parent's host
/// naming that task, and the host's answer decides the call.
#[tokio::test]
async fn a_task_sub_agents_question_reaches_the_parents_host_naming_its_task() {
    use base::interface::permission::PermissionOutcome;
    let parent = test_parent();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let pending: runtime::agent::PendingPermissions = Arc::default();
    let agent_tool = TestParent {
        permission: Some(PermissionSet::without_grants(Arc::new(
            PromptsForEverything,
        ))),
        state: Arc::new(base::context::SessionState::new("/tmp".into())),
        events: Some(runtime::event_bus::EventBus::new(tx)),
        pending: pending.clone(),
        ..parent
    }
    .agent_tool();
    let parent_session = Arc::new(base::context::SessionState::new("/tmp".into()));
    let task = parent_session.register_task(
        "task-1",
        base::context::TaskType::Agent,
        tokio_util::sync::CancellationToken::new(),
        serde_json::json!({}),
    );
    let sub = agent_tool.inner.permission_handler_for(Some(task));
    let asked = tokio::spawn(async move {
        sub_decides(
            &sub,
            None,
            "Bash",
            &serde_json::json!({}),
            Path::new("/tmp"),
            &Arc::new(base::context::SessionState::new("/tmp".into())),
        )
        .await
    });
    let (prompt_id, task_id) = loop {
        match rx.recv().await.expect("the question reaches the parent") {
            AgentEvent::PermissionPrompt {
                prompt_id, task_id, ..
            } => break (prompt_id, task_id),
            _ => continue,
        }
    };
    assert_eq!(task_id.as_deref(), Some("task-1"));
    pending
        .lock()
        .unwrap()
        .remove(&prompt_id)
        .expect("registered with the parent")
        .send(runtime::agent::PermissionDecision::Deny {
            reason: "HOST-SAYS-NO".into(),
        })
        .unwrap();
    match asked.await.unwrap() {
        PermissionOutcome::Deny { reason } => assert_eq!(reason, "HOST-SAYS-NO"),
        other => panic!("the host's answer did not decide the call: {other:?}"),
    }
}

/// An unattended sub-agent is handed the parent's question as it is, for its
/// own session to refuse; nothing is put to the parent's host.
#[tokio::test]
async fn an_unattended_sub_agent_is_handed_the_parents_question_unasked() {
    let parent = test_parent();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let agent_tool = TestParent {
        permission: Some(PermissionSet::without_grants(Arc::new(
            PromptsForEverything,
        ))),
        state: Arc::new(base::context::SessionState::new("/tmp".into())),
        events: Some(runtime::event_bus::EventBus::new(tx)),
        pending: Arc::default(),
        ..parent
    }
    .agent_tool();
    let mut sub_session = base::context::SessionState::new("/tmp".into());
    sub_session.unattended = true;
    let outcome = sub_decides(
        &agent_tool.sub_permission(),
        None,
        "Bash",
        &serde_json::json!({}),
        Path::new("/tmp"),
        &Arc::new(sub_session),
    )
    .await;
    assert!(
        matches!(
            outcome,
            base::interface::permission::PermissionOutcome::Prompt { .. }
        ),
        "{outcome:?}"
    );
    while let Ok(event) = rx.try_recv() {
        assert!(
            !matches!(event, AgentEvent::PermissionPrompt { .. }),
            "an unattended sub-agent's question was put to the host: {event:?}"
        );
    }
}

pub(super) struct PromptsForEverything;
#[async_trait]
impl PermissionPolicy for PromptsForEverything {
    async fn decide(
        &self,
        _req: &base::interface::permission::PermissionRequest<'_>,
    ) -> base::interface::permission::PermissionOutcome {
        base::interface::permission::PermissionOutcome::Prompt {
            prompt_id: base::id::Id::new().to_string(),
            message: "run the probe?".into(),
            paths: vec![],
            managed: false,
        }
    }
}
