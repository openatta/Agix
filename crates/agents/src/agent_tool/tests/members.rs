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

/// Reclaim rule (a): a pool at the cap makes room for a newcomer by
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

/// Reclaim rule (b): idle-timeout is independent of the total-count
/// rule — a single member past the timeout gets reclaimed even with a
/// pool far under the cap.
#[tokio::test]
async fn reclaim_evicts_a_member_idle_past_the_timeout_regardless_of_total_count() {
    let mut config = EngineConfig::defaults_for("test");
    config.team_max_persistent_members = 100; // effectively disabled here
    config.team_member_idle_timeout_secs = 0; // any idle time at all reclaims
    let agent_tool = agent_tool_with_config(config);
    let session = agent_tool.parent_state();

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
    wait_until_idle(&agent_tool, &team_key("team", "a")).await;

    // Spawning a second member triggers a reclaim sweep at its start.
    agent_tool
        .spawn_or_message_team_member(
            "team".into(),
            "b".into(),
            "hi".into(),
            None,
            std::path::PathBuf::from("/tmp"),
            session.clone(),
            None,
        )
        .await
        .unwrap();

    let members = agent_tool
        .delegator
        .members
        .live
        .read()
        .unwrap_or_else(|e| e.into_inner());
    assert!(
        !members.contains_key(&team_key("team", "a")),
        "member idle past the timeout must be reclaimed regardless of total count"
    );
    assert!(members.contains_key(&team_key("team", "b")));
}

/// The reclaim sweep run just before looking up an existing member must
/// not evict that exact member — here one idle past the timeout, which
/// any other member would be reclaimed for — and spawn a fresh
/// replacement instead of reaching the one the caller meant to message.
#[tokio::test]
async fn spawn_or_message_team_member_never_reclaims_the_member_it_is_about_to_message() {
    let mut config = EngineConfig::defaults_for("test");
    config.team_member_idle_timeout_secs = 0;
    let agent_tool = agent_tool_with_config(config);
    let session = agent_tool.parent_state();
    let key_a = team_key("team", "a");

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
    wait_until_idle(&agent_tool, &key_a).await;
    let id_a_before = agent_tool
        .delegator
        .members
        .live
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key_a)
        .expect("a is in the pool")
        .id
        .clone();

    agent_tool
        .spawn_or_message_team_member(
            "team".into(),
            "a".into(),
            "second message".into(),
            None,
            std::path::PathBuf::from("/tmp"),
            session.clone(),
            None,
        )
        .await
        .expect("messaging the existing, about-to-be-protected member should succeed");
    wait_until_idle(&agent_tool, &key_a).await;

    let members = agent_tool
        .delegator
        .members
        .live
        .read()
        .unwrap_or_else(|e| e.into_inner());
    assert_eq!(
        members.get(&key_a).map(|m| m.id.clone()),
        Some(id_a_before),
        "the call's own target member must survive its own reclaim sweep"
    );
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

/// A non-read-only tool that behaves like most real production tools do
/// for their *general/uncertain* case (`Bash`'s own `check_permissions`
/// returns `Allow` only for known-safe read-only commands, `Ask`
/// otherwise) — unlike `NamedTool` above, which unconditionally reports
/// a read-only `call_traits()` and never overrides `check_permissions()`
/// (so it always short-circuits to `Allow`, regardless of mode — not
/// representative of a real mutating tool for this test's purpose).
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
    async fn check_permissions(
        &self,
        _: &Value,
        _: &ToolContext,
    ) -> base::tool::PermissionDecision {
        base::tool::PermissionDecision::ask("write?")
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
/// isolation? `SendMessageTool::check_permissions()` returns `Allow`
/// unconditionally, and `Policy::decide`'s step 2 consults the
/// tool's own `check_permissions()` *before* falling through to mode
/// dispatch — so the real answer depends on call-path order, not just
/// the mode-dispatch snippet alone. `SendMessage`/`ReadMail`/`ListPeers`
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
        base::context::SessionState::new("/tmp".into())
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
        "SendMessage must be permitted under Plan mode (via its own \
         check_permissions(), not the read-only mode dispatch) — got {send_outcome:?}"
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
