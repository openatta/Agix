//! Tests: wiring.

use super::*;

/// Parent `Settings` for a sub-agent spawn, pointed at a throwaway temp
/// directory so `Builder::build()`'s skill/agent/plugin scans have
/// nothing real to find.
pub(super) fn parent_settings_for_test(scope: &str) -> (Settings, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "agix-subagent-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let mut s = Settings::defaults_for("test-model");
    s.paths = PathSettings {
        user_data_dir: dir.clone(),
        global_data_dir: dir.clone(),
        local_data_dir: dir.clone(),
        scope: scope.to_string(),
    };
    // Keep the turn from making an extra post-turn extraction call.
    s.memory_enabled = false;
    (s, dir)
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
        _prompt_blocks: Vec<base::interface::prompt::PromptBlock>,
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
    fn input_schema(&self) -> Value {
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

/// A parent `Permission` impl that records what it was asked about and
/// answers with a fixed outcome.
pub(super) struct RecordingPermission {
    pub(super) seen: Arc<std::sync::Mutex<Vec<String>>>,
    pub(super) deny: bool,
}

#[async_trait]
impl Permission for RecordingPermission {
    async fn check(
        &self,
        tool_name: &str,
        _input: &Value,
        _cwd: &Path,
        _session_id: &str,
    ) -> base::interface::permission::PermissionOutcome {
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

/// Build an `AgentTool` wired the way `Builder::build()` wires it, with a
/// scripted model and a `Probe` tool the sub-agent can call.
pub(super) fn wired_agent_tool(
    scope: &str,
) -> (
    AgentTool,
    Arc<InMemoryToolRegistry>,
    Arc<std::sync::atomic::AtomicUsize>,
    std::path::PathBuf,
) {
    let runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let tools = InMemoryToolRegistry::new();
    tools.register(Arc::new(CountingProbeTool { runs: runs.clone() }));
    let tools = Arc::new(tools);
    let model: Arc<dyn Model> = Arc::new(ProbeThenTextModel {
        calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    });
    let config = Arc::new(EngineConfig::defaults_for("test-model"));
    let (settings, dir) = parent_settings_for_test(scope);
    let agent_tool =
        AgentTool::with_parent_tools(model, config, tools.clone(), tools.clone(), &[], &[])
            .with_settings(Arc::new(settings));
    (agent_tool, tools, runs, dir)
}

/// A sub-agent spawned with a history store and a parent session id
/// must persist its own transcript as a sidechain linked back to the
/// parent — this is what makes `HistoryStore::child_sessions` (and thus
/// `session.list {parent_session_id}`) find it.
#[tokio::test]
async fn subagent_transcript_is_written_as_a_linked_sidechain() {
    let (agent_tool, tools, _runs, dir) = wired_agent_tool("code");
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
    agent_tool.set_history_store(store.clone());
    agent_tool.set_parent_session_id("parent-session-1".to_string());

    let _text = agent_tool
        .run_sub_tagged(
            "go".into(),
            tools,
            dir.clone(),
            tokio_util::sync::CancellationToken::new(),
            Arc::new(AlwaysPermit),
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
    let (agent_tool, tools, _runs, dir) = wired_agent_tool("code");
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
    agent_tool.set_history_store(store.clone());
    agent_tool.set_parent_session_id("parent-session-1".to_string());

    let cancel = tokio_util::sync::CancellationToken::new();
    cancel.cancel();
    let _text = agent_tool
        .run_sub_tagged(
            "go".into(),
            tools,
            dir.clone(),
            cancel,
            Arc::new(AlwaysPermit),
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
    let (agent_tool, tools, _runs, dir) = wired_agent_tool("code");
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
    agent_tool.set_history_store(store.clone());
    agent_tool.set_parent_session_id("parent-session-1".to_string());

    let _text = agent_tool
        .run_sub_tagged(
            "go".into(),
            tools,
            dir.clone(),
            tokio_util::sync::CancellationToken::new(),
            Arc::new(AlwaysPermit),
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
    let (agent_tool, tools, _runs, dir) = wired_agent_tool("code");
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    agent_tool.set_telemetry_handle(telemetry::TelemetryHandle::new(tx));

    let _text = agent_tool
        .run_sub_tagged(
            "go".into(),
            tools,
            dir.clone(),
            tokio_util::sync::CancellationToken::new(),
            Arc::new(AlwaysPermit),
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
    let (agent_tool, tools, _runs, dir) = wired_agent_tool("code");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    agent_tool.set_event_sender(runtime::event_bus::EventBus::new(tx));

    let text = agent_tool
        .run_sub_tagged(
            "go".into(),
            tools,
            dir.clone(),
            tokio_util::sync::CancellationToken::new(),
            Arc::new(AlwaysPermit),
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
            Arc::new(AlwaysPermit),
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

/// A sub-agent's permission checks must reach the parent's
/// `Permission` impl, and a `Deny` there must actually stop the tool.
#[tokio::test]
async fn subagent_permission_check_routes_to_parent_and_deny_blocks_the_tool() {
    let (agent_tool, tools, runs, dir) = wired_agent_tool("code");
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    agent_tool.set_parent_permission(Arc::new(RecordingPermission {
        seen: seen.clone(),
        deny: true,
    }));

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
        "the sub-agent's tool call must be checked against the parent's Permission impl, saw {seen:?}"
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
    impl Permission for AlwaysPrompts {
        async fn check(
            &self,
            _t: &str,
            _i: &Value,
            _c: &Path,
            _s: &str,
        ) -> base::interface::permission::PermissionOutcome {
            base::interface::permission::PermissionOutcome::Prompt {
                prompt_id: "p1".into(),
                message: "run rm -rf?".into(),
                paths: vec![],
                managed: false,
            }
        }
    }

    let agent_tool = test_agent_tool();
    agent_tool.set_parent_permission(Arc::new(AlwaysPrompts));
    let outcome = agent_tool
        .sub_permission()
        .check("Bash", &serde_json::json!({}), Path::new("/tmp"), "s")
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
    let agent_tool = test_agent_tool();
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    agent_tool.set_parent_permission(Arc::new(RecordingPermission {
        seen: seen.clone(),
        deny: false,
    }));
    let outcome = agent_tool
        .sub_permission()
        .check("Bash", &serde_json::json!({}), Path::new("/tmp"), "s")
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
    let parent = Arc::new(permissions::rule_set_permission::RuleSetPermission::new(
        Arc::new(permissions::gate::PermissionGate::empty()),
        registry.clone(),
        PermissionMode::AcceptEdits,
    ));
    let parent_session = Arc::new(
        base::context::SessionState::new(project.path().to_path_buf())
            .with_permission_mode(PermissionMode::AcceptEdits),
    );
    parent.bind_session_state(parent_session.clone());
    let agent_tool = test_agent_tool();
    agent_tool.set_parent_permission(parent);
    let sub = agent_tool.sub_permission();
    sub.bind_tool_registry(registry);
    let sub_session = Arc::new(base::context::SessionState::new(
        project.path().to_path_buf(),
    ));
    sub.bind_session_state(sub_session.clone());
    let check = |tool: &'static str, input: Value| {
        let sub = sub.clone();
        let cwd = project.path().to_path_buf();
        async move { sub.check(tool, &input, &cwd, "sub").await }
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
    let parent = Arc::new(permissions::rule_set_permission::RuleSetPermission::new(
        Arc::new(permissions::gate::PermissionGate::new(
            permissions::ruleset::RuleSet::new(vec![PermissionRule {
                source: RuleSource::ProjectSettings,
                behavior: RuleBehavior::Deny,
                tool_name: "Write".into(),
                rule_content: Some("**/locked/**".into()),
            }]),
        )),
        registry.clone(),
        PermissionMode::AcceptEdits,
    ));
    let agent_tool = test_agent_tool();
    agent_tool.set_parent_permission(parent);
    let member = agent_tool.inner.team_member_permission(
        &registry,
        base::interface::settings::PermissionMode::BypassPermissions,
    );
    member.bind_tool_registry(registry);
    member.bind_session_state(Arc::new(
        base::context::SessionState::new(project.path().to_path_buf())
            .with_permission_mode(PermissionMode::AcceptEdits),
    ));
    let outcome = member
        .check(
            "Write",
            &serde_json::json!({"file_path": project.path().join("locked/a.txt"), "content": "x"}),
            project.path(),
            "member",
        )
        .await;
    assert!(
        matches!(outcome, PermissionOutcome::Deny { .. }),
        "{outcome:?}"
    );
}

pub(super) struct PromptsForEverything;
#[async_trait]
impl Permission for PromptsForEverything {
    async fn check(
        &self,
        _t: &str,
        _i: &Value,
        _c: &Path,
        _s: &str,
    ) -> base::interface::permission::PermissionOutcome {
        base::interface::permission::PermissionOutcome::Prompt {
            prompt_id: base::id::Id::new().to_string(),
            message: "run the probe?".into(),
            paths: vec![],
            managed: false,
        }
    }
}
