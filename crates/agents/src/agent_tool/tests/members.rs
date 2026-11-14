//! Tests: team.

use super::*;

pub(super) fn team_key(team: &str, member: &str) -> (String, String) {
    (team.to_string(), member.to_string())
}

pub(super) fn team_enabled_agent_tool() -> AgentTool {
    test_parent().with_team().agent_tool()
}

/// Called directly (not through `spawn_or_message_team_member`, which only
/// returns a task id) so the built `Agent`'s own scene is inspectable.
#[tokio::test]
async fn team_member_inherits_the_parents_scene_instead_of_always_coding() {
    let agent_tool = TestParent {
        scene: Arc::new(scene::harnix::spec()),
        ..test_parent().with_team()
    }
    .agent_tool();

    let (agent, _event_rx, _input_tx, _) = agent_tool
        .build_team_member_agent(
            std::path::PathBuf::from("/tmp"),
            None,
            base::settings::PermissionMode::Plan,
            None,
        )
        .await
        .expect("build should succeed");
    assert_eq!(agent.scene().id.as_str(), "harnix");
}

/// A delegate runs its hooks on its parent's runner rather than one it
/// builds from the same settings: `once` holds across the tree, and the
/// parent's file watcher is the only one.
#[tokio::test]
async fn a_delegate_runs_its_hooks_on_the_parents_runner() {
    let runner = Arc::new(hooks::HookRunner::empty());
    let agent_tool = TestParent {
        hooks: Arc::downgrade(&runner),
        ..test_parent().with_team()
    }
    .agent_tool();

    let (agent, _event_rx, _input_tx, _) = agent_tool
        .build_team_member_agent(
            std::path::PathBuf::from("/tmp"),
            None,
            base::settings::PermissionMode::Plan,
            None,
        )
        .await
        .expect("build should succeed");
    assert!(std::ptr::eq(agent.hooks(), Arc::as_ptr(&runner)));
}

pub(super) fn agent_tool_with_config(config: EngineConfig) -> AgentTool {
    TestParent {
        config: Arc::new(config),
        ..TestParent::new(Arc::new(DummyModel))
    }
    .with_team()
    .agent_tool()
}

/// Poll until the given key's member has gone idle (finished its most
/// recent turn) — bounded so a regression here fails fast instead of
/// hanging the test binary.
pub(super) async fn wait_until_idle(agent_tool: &AgentTool, key: &(String, String)) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let idle = {
            let members = agent_tool
                .delegator
                .members
                .live
                .read()
                .unwrap_or_else(|e| e.into_inner());
            members
                .get(key)
                .map(|m| m.idle_since.load(std::sync::atomic::Ordering::SeqCst))
        };
        match idle {
            Some(v) if v != i64::MAX => return,
            None => panic!("member {key:?} disappeared from the pool while waiting"),
            _ => {}
        }
        assert!(
            std::time::Instant::now() < deadline,
            "member {key:?} never went idle within 10s"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn call_rejects_team_name_and_name_when_team_coordination_is_not_enabled() {
    let agent_tool = test_agent_tool(); // no set_team_registry — team coordination "off"
    let ctx = platform::tool_context_for_test(std::path::PathBuf::from("/tmp"));
    let input = serde_json::json!({ "prompt": "hi", "team_name": "t", "name": "worker" });
    let result = base::tool::Tool::call(&agent_tool, input, ctx, ProgressSender::noop("t")).await;
    assert!(
        result.is_err(),
        "team_name/name must be rejected when team coordination isn't enabled \
         for this session, not silently spawn a persistent member anyway"
    );
}

#[tokio::test]
async fn call_rejects_team_name_given_without_name() {
    let agent_tool = team_enabled_agent_tool();
    let ctx = platform::tool_context_for_test(std::path::PathBuf::from("/tmp"));
    let input = serde_json::json!({ "prompt": "hi", "team_name": "t" });
    let result = base::tool::Tool::call(&agent_tool, input, ctx, ProgressSender::noop("t")).await;
    assert!(
        result.is_err(),
        "team_name without name must be rejected, not silently treated as a normal call"
    );
}

#[tokio::test]
async fn spawn_or_message_team_member_reuses_the_same_member_across_calls() {
    let agent_tool = team_enabled_agent_tool();
    let session = agent_tool.parent_state();
    let key = team_key("t1", "worker");

    let tid1 = agent_tool
        .spawn_or_message_team_member(
            "t1".into(),
            "worker".into(),
            "first message".into(),
            None,
            std::path::PathBuf::from("/tmp"),
            session.clone(),
            None,
        )
        .await
        .expect("first spawn should succeed");
    wait_until_idle(&agent_tool, &key).await;
    let id1 = agent_tool
        .delegator
        .members
        .live
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
        .unwrap()
        .id
        .clone();
    assert_eq!(
        agent_tool
            .delegator
            .members
            .live
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .len(),
        1
    );

    let tid2 = agent_tool
        .spawn_or_message_team_member(
            "t1".into(),
            "worker".into(),
            "second message".into(),
            None,
            std::path::PathBuf::from("/tmp"),
            session.clone(),
            None,
        )
        .await
        .expect("second call to the same key should succeed");
    assert_ne!(tid1, tid2, "each message gets its own task id");
    wait_until_idle(&agent_tool, &key).await;

    let members = agent_tool
        .delegator
        .members
        .live
        .read()
        .unwrap_or_else(|e| e.into_inner());
    assert_eq!(members.len(), 1, "still exactly one member under this key");
    assert_eq!(
        members.get(&key).unwrap().id,
        id1,
        "second call must reuse the same member instance, not spawn a new one"
    );
}

#[tokio::test]
async fn stop_team_member_makes_the_next_call_spawn_a_fresh_one() {
    let agent_tool = team_enabled_agent_tool();
    let session = agent_tool.parent_state();
    let key = team_key("t2", "worker");

    agent_tool
        .spawn_or_message_team_member(
            "t2".into(),
            "worker".into(),
            "msg1".into(),
            None,
            std::path::PathBuf::from("/tmp"),
            session.clone(),
            None,
        )
        .await
        .unwrap();
    wait_until_idle(&agent_tool, &key).await;
    let id1 = agent_tool
        .delegator
        .members
        .live
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
        .unwrap()
        .id
        .clone();

    agent_tool.stop_team_member("t2", "worker").await;
    assert!(agent_tool
        .delegator
        .members
        .live
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
        .is_none());

    agent_tool
        .spawn_or_message_team_member(
            "t2".into(),
            "worker".into(),
            "msg2".into(),
            None,
            std::path::PathBuf::from("/tmp"),
            session.clone(),
            None,
        )
        .await
        .unwrap();
    wait_until_idle(&agent_tool, &key).await;
    let id2 = agent_tool
        .delegator
        .members
        .live
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
        .unwrap()
        .id
        .clone();
    assert_ne!(
        id1, id2,
        "after stop_team_member, the same key must spawn a genuinely new member"
    );
}

/// A pool at the cap makes room for a newcomer by
/// evicting the oldest-idle member — just one, not everything.
#[tokio::test]
async fn reclaim_evicts_oldest_idle_members_once_total_exceeds_the_cap() {
    let mut config = EngineConfig::defaults_for("test");
    config.team_max_persistent_members = 2;
    config.team_member_idle_timeout_secs = 1_000_000; // effectively disabled here
    let agent_tool = agent_tool_with_config(config);
    let session = agent_tool.parent_state();

    for name in ["a", "b", "c", "d"] {
        agent_tool
            .spawn_or_message_team_member(
                "team".into(),
                name.into(),
                "hi".into(),
                None,
                std::path::PathBuf::from("/tmp"),
                session.clone(),
                None,
            )
            .await
            .unwrap();
        wait_until_idle(&agent_tool, &team_key("team", name)).await;
    }

    let members = agent_tool
        .delegator
        .members
        .live
        .read()
        .unwrap_or_else(|e| e.into_inner());
    assert_eq!(
        members.len(),
        2,
        "the pool stays at its cap: {:?}",
        members.keys().collect::<Vec<_>>()
    );
    for name in ["a", "b"] {
        assert!(
            !members.contains_key(&team_key("team", name)),
            "oldest-idle member '{name}' should have been reclaimed"
        );
    }
    for name in ["c", "d"] {
        assert!(
            members.contains_key(&team_key("team", name)),
            "{name} should remain"
        );
    }
}

/// A member idle past `team_member_idle_timeout_secs` leaves the pool by
/// itself, with no later spawn to sweep it.
#[tokio::test]
async fn a_member_idle_past_the_timeout_leaves_the_pool_on_its_own() {
    let mut config = EngineConfig::defaults_for("test");
    config.team_member_idle_timeout_secs = 0;
    let agent_tool = agent_tool_with_config(config);
    let session = agent_tool.parent_state();
    let key = team_key("team", "a");

    agent_tool
        .spawn_or_message_team_member(
            "team".into(),
            "a".into(),
            "hi".into(),
            None,
            std::path::PathBuf::from("/tmp"),
            session.clone(),
            None,
        )
        .await
        .unwrap();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while agent_tool
        .delegator
        .members
        .live
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(&key)
    {
        assert!(
            std::time::Instant::now() < deadline,
            "an idle member past its timeout stayed in the pool"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// Two concurrent calls for the same never-before-seen
/// `(team_name, member_name)` key must not both observe "doesn't exist
/// yet" and both spawn a member, orphaning the first.
#[tokio::test]
async fn concurrent_first_calls_for_a_new_key_spawn_exactly_one_member() {
    let agent_tool = Arc::new(team_enabled_agent_tool());
    let session = agent_tool.parent_state();
    let key = team_key("t4", "worker");

    let (r1, r2) = tokio::join!(
        agent_tool.spawn_or_message_team_member(
            "t4".into(),
            "worker".into(),
            "msg-a".into(),
            None,
            std::path::PathBuf::from("/tmp"),
            session.clone(),
            None,
        ),
        agent_tool.spawn_or_message_team_member(
            "t4".into(),
            "worker".into(),
            "msg-b".into(),
            None,
            std::path::PathBuf::from("/tmp"),
            session.clone(),
            None,
        ),
    );
    let tid1 = r1.expect("first concurrent call should succeed");
    let tid2 = r2.expect("second concurrent call should succeed");
    assert_ne!(
        tid1, tid2,
        "each call gets its own task id even when they race"
    );

    wait_until_idle(&agent_tool, &key).await;
    let members = agent_tool
        .delegator
        .members
        .live
        .read()
        .unwrap_or_else(|e| e.into_inner());
    assert_eq!(
        members.len(),
        1,
        "two concurrent first-calls for the same key must not double-spawn: {:?}",
        members.keys().collect::<Vec<_>>()
    );
}

/// End-to-end across the `runtime`/`team` boundary: `TeamDelete`'s
/// `cleanup_team` (via `Delegator::stop_member`) must actually
/// stop a still-alive persistent member, not just remove its registry
/// bookkeeping — what `TeamDeleteTool` holds a delegator for.
#[tokio::test]
async fn team_delete_cleanup_stops_a_persistent_member() {
    let agent_tool = Arc::new(team_enabled_agent_tool());
    let session = agent_tool.parent_state();
    let key = team_key("t3", "worker");

    agent_tool
        .spawn_or_message_team_member(
            "t3".into(),
            "worker".into(),
            "hi".into(),
            None,
            std::path::PathBuf::from("/tmp"),
            session.clone(),
            None,
        )
        .await
        .unwrap();
    wait_until_idle(&agent_tool, &key).await;
    assert!(agent_tool
        .delegator
        .members
        .live
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(&key));

    let registry = agent_tool
        .delegator
        .members
        .registry
        .clone()
        .expect("team_enabled_agent_tool sets a registry");
    let coordinator =
        team::coordinator::DefaultCoordinator::with_delegator(agent_tool.delegator.clone())
            .with_registry(registry);

    use team::coordinator::Coordinator as _;
    coordinator.cleanup_team("t3").await.expect(
        "cleanup_team should find 't3' — the loop registered it via update_member_lifecycle",
    );

    assert!(
        agent_tool
            .delegator
            .members
            .live
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .is_none(),
        "TeamDelete's cleanup must actually stop the persistent member, \
         not just clean up the registry entry"
    );
}

#[tokio::test]
async fn auto_for_a_persistent_member_is_bypass() {
    let agent_tool = team_enabled_agent_tool();
    let ctx = platform::tool_context_for_test(std::path::PathBuf::from("/tmp"));
    let input = serde_json::json!({
        "prompt": "hi", "team_name": "t-auto", "name": "worker", "permission_mode": "auto",
    });
    base::tool::Tool::call(&agent_tool, input, ctx, ProgressSender::noop("t"))
        .await
        .unwrap();
    wait_until_idle(&agent_tool, &team_key("t-auto", "worker")).await;

    let registry = agent_tool.delegator.members.registry.clone().unwrap();
    assert_eq!(
        registry.team_permission_mode("t-auto"),
        Some(base::settings::PermissionMode::BypassPermissions)
    );
}

#[tokio::test]
async fn call_rejects_bubble_permission_mode_for_persistent_members() {
    let agent_tool = team_enabled_agent_tool();
    let ctx = platform::tool_context_for_test(std::path::PathBuf::from("/tmp"));
    let input = serde_json::json!({
        "prompt": "hi", "team_name": "t", "name": "worker", "permission_mode": "bubble",
    });
    let result = base::tool::Tool::call(&agent_tool, input, ctx, ProgressSender::noop("t")).await;
    assert!(
        result.is_err(),
        "\"bubble\" needs an available lead, which a persistent member's lead isn't"
    );
}

/// "One team, one authorization": the *first* call that spawns a member
/// under a team name establishes its permission grant; a later call
/// under the same name that doesn't repeat `permission_mode` must
/// inherit it, not silently fall back to the zero-config default.
#[tokio::test]
async fn spawn_or_message_team_member_establishes_team_permission_mode_for_later_spawns() {
    let agent_tool = team_enabled_agent_tool();
    let session = agent_tool.parent_state();

    agent_tool
        .spawn_or_message_team_member(
            "t4".into(),
            "a".into(),
            "hi".into(),
            None,
            std::path::PathBuf::from("/tmp"),
            session.clone(),
            Some(base::settings::PermissionMode::BypassPermissions),
        )
        .await
        .unwrap();
    wait_until_idle(&agent_tool, &team_key("t4", "a")).await;

    // Second member, same team, no explicit override this time.
    agent_tool
        .spawn_or_message_team_member(
            "t4".into(),
            "b".into(),
            "hi".into(),
            None,
            std::path::PathBuf::from("/tmp"),
            session.clone(),
            None,
        )
        .await
        .unwrap();
    wait_until_idle(&agent_tool, &team_key("t4", "b")).await;

    let registry = agent_tool.delegator.members.registry.clone().unwrap();
    assert_eq!(
        registry.team_permission_mode("t4"),
        Some(base::settings::PermissionMode::BypassPermissions),
        "the team's grant, established by the first member, must not have been \
         overwritten by the second spawn's implicit default"
    );
}

/// Zero-config spawn (no team-wide grant established yet, no explicit
/// override) must resolve to the safe default (`Plan`), not something
/// unrestricted — this is what a caller gets if they don't think about
/// permissions at all.
#[tokio::test]
async fn spawn_or_message_team_member_defaults_to_plan_when_nothing_specified() {
    let agent_tool = team_enabled_agent_tool();
    let session = agent_tool.parent_state();

    agent_tool
        .spawn_or_message_team_member(
            "t5".into(),
            "a".into(),
            "hi".into(),
            None,
            std::path::PathBuf::from("/tmp"),
            session.clone(),
            None,
        )
        .await
        .unwrap();
    wait_until_idle(&agent_tool, &team_key("t5", "a")).await;

    let registry = agent_tool.delegator.members.registry.clone().unwrap();
    assert_eq!(
        registry.team_permission_mode("t5"),
        Some(base::settings::PermissionMode::Plan),
        "an unconfigured spawn must resolve to (and record) the safe Plan default"
    );
}

/// A non-read-only tool that claims nothing about its call, as most real
/// tools do for their *general/uncertain* case (`Bash` is contained only for
/// known-safe read-only commands).
pub(super) struct AskingWriteTool;
#[async_trait]
impl base::tool::Tool for AskingWriteTool {
    fn name(&self) -> &str {
        "Write"
    }
    fn spec(&self) -> base::tool::ToolSpec {
        base::tool::ToolSpec::new()
            .read_only(false)
            .concurrency_safe(false)
    }

    fn description(&self) -> &str {
        "asking write tool (test tool)"
    }

    fn input_schema(&self, _: base::tool::PathStyle) -> Value {
        serde_json::json!({})
    }
    async fn permission_facts(
        &self,
        _: &serde_json::Value,
        _: &base::tool::ToolContext,
    ) -> base::interface::permission::PermissionFacts {
        use base::interface::permission::{PermissionFacts, Reach, Subject};
        PermissionFacts::new(Subject::None, Reach::Unknown).with_question("write?")
    }
    async fn call(
        &self,
        _input: Value,
        _ctx: ToolContext,
        _progress: ProgressSender,
    ) -> Result<base::tool::ToolResult, base::error::ToolError> {
        Ok(base::tool::ToolResult::text("ok"))
    }
}

/// Empirical check (not just reading the gate's source): does the
/// `Policy` built for team members under the zero-config
/// `Plan` default actually deny `SendMessage`, the way `Policy`'s
/// bare mode-dispatch (`Plan if !read_only => Deny`) would suggest in
/// isolation? `SendMessage` is a session-only call with the coordination
/// stance, which spares it plan mode. `SendMessage`/`ReadMail`/`ListPeers`
/// are exempt from mode-based restriction by design.
#[tokio::test]
async fn plan_mode_permits_send_message_despite_not_being_read_only() {
    let mailbox = std::sync::Arc::new(team::mailbox::MailboxStore::new(vec![
        "sender".into(),
        "peer".into(),
    ]));
    let tools = Arc::new(InMemoryToolRegistry::new());
    tools.register(Arc::new(team::mailbox::SendMessageTool::new(
        mailbox.clone(),
        "sender",
    )));
    tools.register(Arc::new(AskingWriteTool));

    let perm = PermissionSet::of(Arc::new(permissions::policy::Policy::new(
        permissions::ruleset::RuleSet::new(vec![]),
    )));
    let session = Arc::new(
        base::context::SessionState::for_tests("/tmp".into())
            .with_permission_mode(base::permission::PermissionMode::Plan),
    );
    let decide = |name: &'static str, input: Value| {
        let (perm, session, tool) = (perm.clone(), session.clone(), tools.get(name));
        async move {
            sub_decides(
                &perm,
                tool.as_ref(),
                name,
                &input,
                std::path::Path::new("/tmp"),
                &session,
            )
            .await
        }
    };

    let send_outcome = decide(
        "SendMessage",
        serde_json::json!({"peer": "peer", "message": "y"}),
    )
    .await;
    assert!(
        matches!(
            send_outcome,
            base::interface::permission::PermissionOutcome::Permit { .. }
        ),
        "SendMessage must be permitted under Plan mode (via its coordination \
         stance, not the read-only mode dispatch) — got {send_outcome:?}"
    );

    // Sanity check the other half of the claim: Plan mode still denies
    // an ordinary non-read-only tool that doesn't self-authorize the
    // way the mailbox tools do — proving Plan's restriction is real for
    // everything except the deliberately-exempted mailbox tools, not
    // that this test's setup accidentally permits everything.
    let write_outcome = decide("Write", serde_json::json!({})).await;
    assert!(
        matches!(write_outcome, base::interface::permission::PermissionOutcome::Deny { .. }),
        "an ordinary non-read-only tool must still be denied under Plan mode — got {write_outcome:?}"
    );
}

/// Holds a turn call whose conversation carries `mark` until the call is
/// cancelled; answers every other one at once.
struct HoldsUntilCancelled {
    mark: &'static str,
    held: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl Model for HoldsUntilCancelled {
    fn api_type(&self) -> base::provider::ApiType {
        base::provider::ApiType::Anthropic
    }
    async fn stream(
        &self,
        _prompt_blocks: Vec<base::prompt::PromptBlock>,
        _tools: Vec<base::interface::model::ToolDef>,
        messages: Vec<ModelMessage>,
        _params: base::interface::model::StreamParams,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError> {
        use base::interface::model::ModelEvent;
        if serde_json::to_string(&messages)
            .unwrap_or_default()
            .contains(self.mark)
        {
            self.held.notify_one();
            cancel.cancelled().await;
        }
        Ok(Box::new(futures::stream::iter(vec![
            Ok(ModelEvent::TextDelta {
                text: "done".into(),
            }),
            Ok(ModelEvent::EndTurn {
                stop_reason: "end_turn".into(),
                usage: Default::default(),
            }),
        ])))
    }
}

/// A member stopped while one message runs and more wait behind it ends the
/// waiting messages' tasks as stopped: none is left running for `TaskWait`
/// to hang on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stopping_a_member_ends_the_tasks_of_its_queued_messages() {
    let held = Arc::new(tokio::sync::Notify::new());
    let registry = Arc::new(team::registry::TeamRegistry::new());
    let agent_tool = TestParent {
        team_registry: Some(registry.clone()),
        ..TestParent::new(Arc::new(HoldsUntilCancelled {
            mark: "FIRST-MESSAGE-MARK",
            held: held.clone(),
        }))
    }
    .agent_tool();
    let session = agent_tool.parent_state();
    let send = |prompt: &str| {
        agent_tool.spawn_or_message_team_member(
            "team".into(),
            "worker".into(),
            prompt.into(),
            None,
            std::env::temp_dir(),
            session.clone(),
            None,
        )
    };
    send("FIRST-MESSAGE-MARK work")
        .await
        .expect("the member is spawned");
    tokio::time::timeout(std::time::Duration::from_secs(10), held.notified())
        .await
        .expect("the first message reaches the model");
    let mut queued = Vec::new();
    for i in 0..4 {
        queued.push(
            send(&format!("queued message {i}"))
                .await
                .expect("the message is queued"),
        );
    }
    agent_tool.stop_team_member("team", "worker").await;

    let shut_down = || {
        registry.list().iter().any(|t| {
            t.members.iter().any(|m| {
                m.label == "worker" && m.lifecycle == team::coordinator::TeammateLifecycle::Shutdown
            })
        })
    };
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !shut_down() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the member's loop ends");
    let still_running: Vec<_> = session
        .task_infos()
        .into_iter()
        .filter(|t| {
            queued.contains(&t.task_id) && t.status == base::context::task::TaskStatus::Running
        })
        .collect();
    assert!(
        still_running.is_empty(),
        "queued messages' tasks left running: {still_running:?}"
    );
}

/// Waits, within a bound, for the parent's task `task_id` to end, and says
/// how it ended.
async fn ended_status(agent_tool: &AgentTool, task_id: &str) -> base::context::task::TaskStatus {
    use base::context::task::TaskStatus;
    let state = agent_tool.parent_state();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let status = state
            .task_infos()
            .into_iter()
            .find(|t| t.task_id == task_id)
            .map(|t| t.status);
        match status {
            Some(TaskStatus::Running | TaskStatus::Waiting) => {}
            Some(ended) => return ended,
            None => panic!("task {task_id} is not the parent's"),
        }
        assert!(
            std::time::Instant::now() < deadline,
            "task {task_id} never ended"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// A root session holds no slot, so its member waits for one; the member
/// runs its message's turn on it and gives it back when the turn ends.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_root_sessions_member_holds_a_slot_only_for_its_turn() {
    let started = Arc::new(tokio::sync::Notify::new());
    let slots = SubagentSlots::new(1);
    let agent_tool = TestParent {
        slots: slots.clone(),
        ..TestParent::new(Arc::new(AnswersOnlyCancellation {
            started: started.clone(),
            dropped: Default::default(),
        }))
    }
    .with_team()
    .agent_tool();
    let held = slots.slots.clone().try_acquire_owned().unwrap();
    let task_id = agent_tool
        .spawn_or_message_team_member(
            "crew".into(),
            "a".into(),
            "work".into(),
            None,
            std::env::temp_dir(),
            agent_tool.parent_state(),
            None,
        )
        .await
        .expect("the message is queued");

    drop(held);
    tokio::time::timeout(std::time::Duration::from_secs(10), started.notified())
        .await
        .expect("the member's turn starts once the slot is free");
    assert_eq!(slots.running(), 1, "the member's turn runs on a slot");

    agent_tool.stop_team_member("crew", "a").await;
    assert_eq!(
        ended_status(&agent_tool, &task_id).await,
        base::context::task::TaskStatus::Stopped
    );
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while slots.running() != 0 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("a member that is not running holds no slot");
}

/// A member of a session that runs on a slot itself is refused at the cap,
/// rather than waiting on a slot only that session can give back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_delegates_member_is_refused_at_the_cap() {
    let slots = SubagentSlots::new(1);
    let agent_tool = TestParent {
        slots: slots.clone(),
        depth: 1,
        ..TestParent::new(Arc::new(DummyModel))
    }
    .with_team()
    .agent_tool();
    let _held = slots.slots.clone().try_acquire_owned().unwrap();
    let task_id = agent_tool
        .spawn_or_message_team_member(
            "crew".into(),
            "a".into(),
            "work".into(),
            None,
            std::env::temp_dir(),
            agent_tool.parent_state(),
            None,
        )
        .await
        .expect("the message is queued");
    assert_eq!(
        ended_status(&agent_tool, &task_id).await,
        base::context::task::TaskStatus::Failed
    );
}

/// Holds a turn call whose conversation carries `mark` until `release`,
/// whether or not the call is cancelled; answers every other one at once.
struct HoldsUntilReleased {
    mark: &'static str,
    held: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl Model for HoldsUntilReleased {
    fn api_type(&self) -> base::provider::ApiType {
        base::provider::ApiType::Anthropic
    }
    async fn stream(
        &self,
        _prompt_blocks: Vec<base::prompt::PromptBlock>,
        _tools: Vec<base::interface::model::ToolDef>,
        messages: Vec<ModelMessage>,
        _params: base::interface::model::StreamParams,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError> {
        use base::interface::model::ModelEvent;
        if serde_json::to_string(&messages)
            .unwrap_or_default()
            .contains(self.mark)
        {
            self.held.notify_one();
            self.release.notified().await;
        }
        Ok(Box::new(futures::stream::iter(vec![
            Ok(ModelEvent::TextDelta {
                text: "done".into(),
            }),
            Ok(ModelEvent::EndTurn {
                stop_reason: "end_turn".into(),
                usage: Default::default(),
            }),
        ])))
    }
}

/// A member stopped mid-turn and replaced under the same name: when the old
/// one's turn finally ends, it does not write over what `TeamList` shows for
/// the new one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stopped_members_late_turn_leaves_its_replacement_alone() {
    use team::coordinator::TeammateLifecycle;
    let held = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let registry = Arc::new(team::registry::TeamRegistry::new());
    let agent_tool = TestParent {
        team_registry: Some(registry.clone()),
        ..TestParent::new(Arc::new(HoldsUntilReleased {
            mark: "OLD-MEMBER-MARK",
            held: held.clone(),
            release: release.clone(),
        }))
    }
    .agent_tool();
    let session = agent_tool.parent_state();
    let send = |prompt: &str| {
        agent_tool.spawn_or_message_team_member(
            "team".into(),
            "worker".into(),
            prompt.into(),
            None,
            std::env::temp_dir(),
            session.clone(),
            None,
        )
    };
    let wait_ended = |task_id: String| {
        let session = session.clone();
        async move {
            if let Some(task) = session.find_running_task(&task_id) {
                tokio::time::timeout(std::time::Duration::from_secs(10), task.wait_ended())
                    .await
                    .expect("the message's task ends");
            }
        }
    };
    let lifecycle = || {
        registry
            .list()
            .iter()
            .flat_map(|t| t.members.clone())
            .find(|m| m.label == "worker")
            .map(|m| m.lifecycle)
    };

    let old = send("OLD-MEMBER-MARK work").await.expect("spawned");
    tokio::time::timeout(std::time::Duration::from_secs(10), held.notified())
        .await
        .expect("the old member's turn reaches the model");
    agent_tool.stop_team_member("team", "worker").await;
    let new = send("new work").await.expect("a new member is spawned");
    wait_ended(new).await;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while lifecycle() != Some(TeammateLifecycle::Idle) {
        assert!(std::time::Instant::now() < deadline, "{:?}", lifecycle());
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    release.notify_one();
    wait_ended(old).await;
    for _ in 0..50 {
        assert_eq!(lifecycle(), Some(TeammateLifecycle::Idle));
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// A member stopped mid-turn ends its message's task as stopped at once,
/// though the turn ignores the cancel and runs on: the session is not left
/// busy with it, and the late turn's end does not change how it ended.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stopping_a_member_mid_turn_ends_its_messages_task_at_once() {
    use base::context::task::TaskStatus;
    let held = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let registry = Arc::new(team::registry::TeamRegistry::new());
    let agent_tool = TestParent {
        team_registry: Some(registry.clone()),
        ..TestParent::new(Arc::new(HoldsUntilReleased {
            mark: "STUCK-MARK",
            held: held.clone(),
            release: release.clone(),
        }))
    }
    .agent_tool();
    let session = agent_tool.parent_state();
    let task_id = agent_tool
        .spawn_or_message_team_member(
            "team".into(),
            "worker".into(),
            "STUCK-MARK work".into(),
            None,
            std::env::temp_dir(),
            session.clone(),
            None,
        )
        .await
        .expect("spawned");
    let task = session.find_running_task(&task_id).expect("its task");
    tokio::time::timeout(std::time::Duration::from_secs(10), held.notified())
        .await
        .expect("the member's turn reaches the model");

    agent_tool.stop_team_member("team", "worker").await;
    let status = || {
        session
            .task_infos()
            .into_iter()
            .find(|t| t.task_id == task_id)
            .map(|t| t.status)
    };
    assert_eq!(status(), Some(TaskStatus::Stopped));
    assert!(
        !session.running_background_task_ids().contains(&task_id),
        "{:?}",
        session.running_background_task_ids()
    );

    release.notify_one();
    let shut_down = || {
        registry.list().iter().any(|t| {
            t.members.iter().any(|m| {
                m.label == "worker" && m.lifecycle == team::coordinator::TeammateLifecycle::Shutdown
            })
        })
    };
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !shut_down() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the member's loop ends once its turn does");
    assert!(task.has_ended());
    assert_eq!(status(), Some(TaskStatus::Stopped));
}

/// A member stopped with a message running and one queued tells the parent
/// each message's task was stopped, once each, though the running turn
/// ignores the cancel and ends later.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stopping_a_member_tells_the_parent_each_message_was_stopped() {
    let held = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let registry = Arc::new(team::registry::TeamRegistry::new());
    let agent_tool = TestParent {
        team_registry: Some(registry.clone()),
        ..TestParent::new(Arc::new(HoldsUntilReleased {
            mark: "STUCK-MARK",
            held: held.clone(),
            release: release.clone(),
        }))
    }
    .agent_tool();
    let session = agent_tool.parent_state();
    let message = |prompt: &'static str| {
        agent_tool.spawn_or_message_team_member(
            "team".into(),
            "worker".into(),
            prompt.into(),
            None,
            std::env::temp_dir(),
            session.clone(),
            None,
        )
    };
    let running = message("STUCK-MARK work").await.expect("spawned");
    tokio::time::timeout(std::time::Duration::from_secs(10), held.notified())
        .await
        .expect("the member's turn reaches the model");
    let queued = message("more work").await.expect("queued");

    agent_tool.stop_team_member("team", "worker").await;
    release.notify_one();
    let shut_down = || {
        registry.list().iter().any(|t| {
            t.members.iter().any(|m| {
                m.label == "worker" && m.lifecycle == team::coordinator::TeammateLifecycle::Shutdown
            })
        })
    };
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !shut_down() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the member's loop ends once its turn does");

    let notices = session.take_notices();
    for task_id in [&running, &queued] {
        let told: Vec<_> = notices
            .iter()
            .filter(|n| n.contains(&format!("<task-id>{task_id}</task-id>")))
            .collect();
        assert_eq!(told.len(), 1, "{task_id}: {notices:?}");
        assert!(told[0].contains("<status>stopped</status>"), "{told:?}");
    }
}

/// A team deleted while one of its members is mid-turn stays deleted when
/// that turn ends.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_deleted_teams_late_turn_does_not_bring_the_team_back() {
    use team::coordinator::Coordinator as _;
    let held = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let registry = Arc::new(team::registry::TeamRegistry::new());
    let agent_tool = TestParent {
        team_registry: Some(registry.clone()),
        ..TestParent::new(Arc::new(HoldsUntilReleased {
            mark: "MEMBER-MARK",
            held: held.clone(),
            release: release.clone(),
        }))
    }
    .agent_tool();
    let session = agent_tool.parent_state();
    let task_id = agent_tool
        .spawn_or_message_team_member(
            "team".into(),
            "worker".into(),
            "MEMBER-MARK work".into(),
            None,
            std::env::temp_dir(),
            session.clone(),
            None,
        )
        .await
        .expect("spawned");
    let task = session.find_running_task(&task_id).expect("its task");
    tokio::time::timeout(std::time::Duration::from_secs(10), held.notified())
        .await
        .expect("the member's turn reaches the model");
    team::coordinator::DefaultCoordinator::with_delegator(agent_tool.delegator.clone())
        .with_registry(registry.clone())
        .cleanup_team("team")
        .await
        .expect("the team is deleted");

    release.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(10), task.wait_ended())
        .await
        .expect("the member's turn ends");
    for _ in 0..50 {
        assert!(registry.list().is_empty(), "{:?}", registry.list());
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}
