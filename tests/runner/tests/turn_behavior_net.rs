//! Behavior net for the turn loop.
//!
//! Each case drives a real `Agent` with a scripted model
//! (`test_runner::scripted_model`) and snapshots two things: the sequence of
//! `AgentEvent`s the host observes, and the session log the run leaves behind.
//! Both are normalized — ids, timestamps and durations are replaced with
//! placeholders — so the only thing a diff can be about is what the engine
//! *decided*.
//!
//! # Reading a failure
//!
//! A failing case prints the expected and actual traces side by side. If the
//! change was intended, re-run with `AGIX_UPDATE_GOLDEN=1` to rewrite the
//! golden file, **and read the diff before committing it** — a regression net
//! whose goldens get regenerated reflexively is a slower way of having none.
//!
//! # Why scripted rather than replayed from a cassette
//!
//! See `test_runner::scripted_model`'s module doc: cassettes are gitignored by
//! design (they bake in the exact prompt, so they go stale on every prompt
//! change), and they contain no error outcomes at all, so the recovery paths
//! this net most needs to cover are not reachable that way.

use base::event::AgentEvent;
use base::id::Id;
use base::interface::model::Model;
use base::interface::scene::AgentScene;
use base::interface::settings::{PathSettings, PermissionMode, Settings, ThinkingMode};
use base::interface::tool::{InMemoryToolRegistry, Tool};
use history::store::HistoryStore as _;
use runtime::agent::{Builder, InputMessage};
use std::sync::Arc;
use test_runner::scripted_model::{Reply, ScriptedModel};
use tokio_util::sync::CancellationToken;

mod support;
use support::{
    compactable_tool, fake_tools, normalize_events, normalize_log, BudgetScene, GoldenFile,
};

/// How a case is configured. Defaults are the boring ones; a case overrides
/// only what it is about.
struct Case {
    name: &'static str,
    turns: Vec<&'static str>,
    replies: Vec<Reply>,
    /// Compaction threshold in tokens. `0` disables it, which is the default
    /// everywhere else in this file.
    compact_threshold: usize,
    permission_mode: PermissionMode,
    fallback_model: Option<String>,
    /// How the harness answers a `PermissionPrompt`. `None` means the case
    /// does not expect one, and getting one is a failure rather than a hang.
    approve_prompts: Option<bool>,
    /// Raw `settings.hooks_config`, for cases about hooks.
    hooks_config: Option<serde_json::Value>,
    /// Ceiling on model calls in one turn. `None` leaves the scene's own.
    max_api_calls: Option<u32>,
    /// Feature flags this case needs on. Reactive compaction is off by
    /// default, so a case about it has to say so.
    reactive_compact: bool,
    /// Tools registered only for this case. The default set is shared, so a
    /// tool added there changes the schemas every other case sends.
    extra_tools: Vec<Arc<dyn Tool>>,
}

impl Case {
    fn new(name: &'static str, turns: Vec<&'static str>, replies: Vec<Reply>) -> Self {
        Self {
            name,
            turns,
            replies,
            compact_threshold: 0,
            permission_mode: PermissionMode::BypassPermissions,
            fallback_model: None,
            approve_prompts: None,
            hooks_config: None,
            max_api_calls: None,
            reactive_compact: false,
            extra_tools: Vec::new(),
        }
    }
}

/// Run one case and return its normalized trace.
async fn trace(case: Case) -> String {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    std::fs::create_dir_all(root.join("workdir")).expect("workdir");

    let model_arc = ScriptedModel::new(case.replies);
    let model: Arc<dyn Model> = model_arc.clone();

    let mut settings = Settings::defaults_for("golden-model");
    settings.model.model_name = "golden-model".into();
    settings.model.max_tokens = 1024;
    settings.model.thinking_mode = ThinkingMode::Off;
    settings.model.fallback_model = case.fallback_model.clone();
    settings.paths = PathSettings {
        user_data_dir: root.join("user"),
        global_data_dir: root.join("global"),
        local_data_dir: root.join("local"),
        scope: "code".into(),
    };
    settings.memory_enabled = false;
    settings.permission_mode = case.permission_mode;
    settings.hooks_config = case.hooks_config.clone();
    settings.feature_flags.reactive_compact = case.reactive_compact;
    let settings = Arc::new(settings);

    let registry = Arc::new(InMemoryToolRegistry::new());
    for t in fake_tools() {
        registry.register(t as Arc<dyn Tool>);
    }
    for t in &case.extra_tools {
        registry.register(t.clone());
    }

    // A history store is what makes the session log observable at all — the
    // default `Builder` leaves it `None` and writes nothing. It also forces a
    // parseable session id, hence the fixed `Id` rather than a readable label.
    let store = Arc::new(
        history::store::JsonlHistoryStore::with_roots(
            &root.join("workdir"),
            history::path::HistoryRoots::under(&root.join("global")),
        )
        .await
        .expect("history store"),
    );
    let session_id = Id::new();

    // Without this the Builder falls back to its `AllowAll` default, which
    // permits everything regardless of the tool's own `check_permissions` and
    // regardless of `settings.permission_mode` — so a case about approval
    // would pass while proving nothing. The first draft of this file did
    // exactly that: the approve and deny goldens came out byte-identical.
    let permission: Arc<dyn base::interface::permission::Permission> = Arc::new(
        permissions::rule_set_permission::RuleSetPermission::from_settings(
            &settings,
            case.permission_mode.into(),
            registry.clone(),
            [],
        ),
    );

    let scene: Arc<dyn AgentScene> = {
        let scene = BudgetScene::new(case.compact_threshold);
        Arc::new(match case.max_api_calls {
            Some(max) => scene.with_max_api_calls(max),
            None => scene,
        })
    };
    let (mut agent, mut event_rx, input_tx) = Builder::new()
        .scene(scene)
        .model(model)
        .tools(registry)
        .permission(permission)
        .settings(settings)
        .session_id(session_id.to_string())
        .history_store(store.clone())
        .skip_warmup(true)
        .build()
        .expect("agent builds");

    let cancel = CancellationToken::new();
    let run_cancel = cancel.clone();
    let join = tokio::spawn(async move { agent.run(run_cancel).await });

    let mut events: Vec<AgentEvent> = Vec::new();
    for (i, input) in case.turns.iter().enumerate() {
        input_tx
            .send(InputMessage::User {
                content: (*input).to_string(),
                attachments: vec![],
                turn_id: format!("t{i}"),
            })
            .expect("send turn");

        // Bounded so a loop that never terminates fails as a timeout with the
        // trace so far, instead of hanging the suite.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let ev = tokio::time::timeout_at(deadline, event_rx.recv())
                .await
                .unwrap_or_else(|_| {
                    panic!(
                        "case `{}` turn {i}: no TurnComplete within 30s. Events so far:\n{}",
                        case.name,
                        normalize_events(&events)
                    )
                });
            let Some(ev) = ev else {
                panic!("case `{}` turn {i}: event channel closed early", case.name)
            };
            if let AgentEvent::PermissionPrompt { prompt_id, .. } = &ev {
                let Some(approve) = case.approve_prompts else {
                    panic!(
                        "case `{}` got an unexpected PermissionPrompt. Set \
                         `approve_prompts` if that is the point of the case.",
                        case.name
                    )
                };
                let decision = if approve {
                    runtime::agent::PermissionDecision::Permit
                } else {
                    runtime::agent::PermissionDecision::Deny {
                        reason: "denied by the behavior net".into(),
                    }
                };
                input_tx
                    .send(InputMessage::PermissionResponse {
                        prompt_id: prompt_id.clone(),
                        decision,
                    })
                    .expect("send permission response");
            }
            // `Error` ends a turn as surely as `TurnComplete` does — the
            // engine emits it and goes back to waiting for input. Treating
            // only `TurnComplete` as terminal makes any case that ends in an
            // error hang for the full timeout instead of recording what
            // happened.
            let done = matches!(
                ev,
                AgentEvent::TurnComplete { .. } | AgentEvent::Error { .. }
            );
            events.push(ev);
            if done {
                break;
            }
        }
    }

    cancel.cancel();
    drop(input_tx);
    let _ = join.await;

    let leftover = model_arc.unconsumed();
    let entries = store
        .load(base::session::SessionId(session_id))
        .await
        .unwrap_or_default();

    let mut out = String::new();
    out.push_str("== events ==\n");
    out.push_str(&normalize_events(&events));
    out.push_str("\n== session log ==\n");
    out.push_str(&normalize_log(&entries));
    out.push_str("\n== model calls ==\n");
    for c in model_arc.calls() {
        out.push_str(&format!(
            "{} max_tokens={} messages={} content_bytes={} truncated_results={}\n",
            c.model, c.max_tokens, c.messages, c.content_bytes, c.truncated_results
        ));
    }
    out.push_str(&format!("unconsumed_replies={leftover}\n"));
    out
}

async fn check(case: Case) {
    let name = case.name;
    let actual = trace(case).await;
    GoldenFile::new(name).assert_eq(&actual);
}

// ── the cases ───────────────────────────────────────────────────────────

/// Plain conversation: one call, text out, done. The floor of the net — if
/// this moves, everything moved.
#[tokio::test]
async fn plain_conversation() {
    check(Case::new(
        "plain_conversation",
        vec!["hello"],
        vec![Reply::TextChunks(&["Hel", "lo", " there"])],
    ))
    .await;
}

/// One tool call, its result, then a closing message.
#[tokio::test]
async fn single_tool_call() {
    check(Case::new(
        "single_tool_call",
        vec!["read the note"],
        vec![
            Reply::Tool {
                id: "c1",
                name: "GoldenEcho",
                input: serde_json::json!({"say": "note contents"}),
            },
            Reply::Text("The note says: note contents"),
        ],
    ))
    .await;
}

/// Two concurrency-safe tools in one assistant message. What the trace pins is
/// that both run and both results come back before the next model call — the
/// dispatch batching decision.
#[tokio::test]
async fn parallel_tool_calls() {
    check(Case::new(
        "parallel_tool_calls",
        vec!["do both"],
        vec![
            Reply::Tools(vec![
                ("c1", "GoldenEcho", serde_json::json!({"say": "first"})),
                ("c2", "GoldenEcho", serde_json::json!({"say": "second"})),
            ]),
            Reply::Text("both done"),
        ],
    ))
    .await;
}

/// A tool that fails. The loop must carry the error to the model as a result
/// rather than aborting the turn.
#[tokio::test]
async fn tool_error_is_reported_to_the_model() {
    check(Case::new(
        "tool_error",
        vec!["break it"],
        vec![
            Reply::Tool {
                id: "c1",
                name: "GoldenBoom",
                input: serde_json::json!({}),
            },
            Reply::Text("that failed"),
        ],
    ))
    .await;
}

/// A tool that runs and reports its own failure.
///
/// Distinct from `tool_error` above, and the distinction is the whole point:
/// there the engine could not run the tool, here it ran and says the work
/// failed. The model has to be able to tell that from an answer, so the
/// result carries `is_error=Some(true)` — which for a long time it did not,
/// because the flag had nowhere to sit between the tool and the wire.
#[tokio::test]
async fn a_tool_that_reports_its_own_failure_says_so_to_the_model() {
    check(Case::new(
        "tool_reports_failure",
        vec!["run it"],
        vec![
            Reply::Tool {
                id: "c1",
                name: "GoldenReports",
                input: serde_json::json!({}),
            },
            Reply::Text("it says that failed"),
        ],
    ))
    .await;
}

/// Overload on the first call, success on the retry. With a fallback model
/// configured, the retry must go to it — visible only in the recorded call
/// list, since the response is the script's either way.
#[tokio::test]
async fn overload_falls_back_to_the_second_model() {
    let mut case = Case::new(
        "overload_fallback",
        vec!["hi"],
        vec![
            Reply::Fail(base::interface::model::ModelError::Overloaded),
            Reply::Text("recovered"),
        ],
    );
    case.fallback_model = Some("golden-fallback".into());
    check(case).await;
}

/// `max_tokens` is a stop reason the loop treats as "not finished" — it
/// continues rather than ending the turn.
#[tokio::test]
async fn max_tokens_stop_reason() {
    check(Case::new(
        "max_tokens",
        vec!["write a lot"],
        vec![
            Reply::TextWithStop {
                text: "part one",
                stop_reason: "max_tokens",
            },
            Reply::Text(" and part two"),
        ],
    ))
    .await;
}

/// Two turns in one session. Pins that the second turn sees the first in its
/// history and that the log accumulates rather than restarting.
#[tokio::test]
async fn multi_turn_session() {
    check(Case::new(
        "multi_turn",
        vec!["first question", "second question"],
        vec![Reply::Text("first answer"), Reply::Text("second answer")],
    ))
    .await;
}

/// Compaction. The threshold is set absurdly low so the second turn crosses
/// it; what the trace pins is *when* the loop decides to compact and what it
/// leaves in the log.
#[tokio::test]
async fn compaction_triggers_and_is_logged() {
    let mut case = Case::new(
        "compaction",
        vec!["first", "second"],
        vec![
            Reply::Text("a fairly long first answer that fills the budget"),
            Reply::Text("second answer"),
        ],
    );
    case.compact_threshold = 10;
    check(case).await;
}

/// A tool whose `check_permissions` returns `Ask`, in a session that is not
/// bypassing permissions. The prompt must reach the host and the approval must
/// let the call proceed.
#[tokio::test]
async fn permission_prompt_approved() {
    let mut case = Case::new(
        "permission_approved",
        vec!["do the guarded thing"],
        vec![
            Reply::Tool {
                id: "c1",
                name: "GoldenAsk",
                input: serde_json::json!({}),
            },
            Reply::Text("done"),
        ],
    );
    case.permission_mode = PermissionMode::Default;
    case.approve_prompts = Some(true);
    check(case).await;
}

/// The same prompt, denied. The tool must not run, and the model must be told
/// — a denial that silently looks like success is the failure worth pinning.
#[tokio::test]
async fn permission_prompt_denied() {
    let mut case = Case::new(
        "permission_denied",
        vec!["do the guarded thing"],
        vec![
            Reply::Tool {
                id: "c1",
                name: "GoldenAsk",
                input: serde_json::json!({}),
            },
            Reply::Text("understood, skipping"),
        ],
    );
    case.permission_mode = PermissionMode::Default;
    case.approve_prompts = Some(false);
    check(case).await;
}

/// A `PreToolUse` hook that blocks the call. The tool must not run, and the
/// model must see a denial rather than a result.
///
/// This one shells out, because the only hook backend that can answer a
/// decision without a subprocess or a model call does not exist yet — that
/// missing in-process backend is the gap `P2-7` is about. It is a `printf` of
/// a fixed string, so the content is deterministic even though the mechanism
/// is heavier than the rest of this file.
#[tokio::test]
async fn pre_tool_use_hook_blocks_the_call() {
    let mut case = Case::new(
        "hook_blocks_tool",
        vec!["try the blocked thing"],
        vec![
            Reply::Tool {
                id: "c1",
                name: "GoldenEcho",
                input: serde_json::json!({"say": "should not run"}),
            },
            Reply::Text("the hook stopped me"),
        ],
    );
    case.hooks_config = Some(serde_json::json!({
        "PreToolUse": [{
            "type": "command",
            "command": "printf '{\"decision\":\"block\",\"message\":\"blocked by the behavior net\"}'"
        }]
    }));
    check(case).await;
}

/// Prompt-too-long recovery.
///
/// **This golden currently records the recovery failing.** The path compacts
/// with a threshold of `max(scene_threshold, 50_000)` and only retries if the
/// message count actually dropped; a two-message conversation cannot shrink,
/// so the turn ends in `Error` and the scripted retry is never consumed
/// (`unconsumed_replies=1` says so). That is the behavior today, recorded as
/// found rather than as wished for — if someone makes this path work, this
/// case fails and the diff is the proof.
#[tokio::test]
async fn prompt_too_long_compacts_and_retries() {
    check(Case::new(
        "prompt_too_long",
        vec!["build up some history", "now overflow"],
        vec![
            Reply::Text("a first answer long enough to be worth dropping later"),
            Reply::Fail(base::interface::model::ModelError::Internal(
                "prompt too long".into(),
            )),
            Reply::Text("recovered after compaction"),
        ],
    ))
    .await;
}

/// Sub-agent spawn. `Builder::build()` registers the `Agent` tool itself, so
/// the model can call it; the child runs on the same scripted model, which is
/// why the script carries the child's answer too.
#[tokio::test]
async fn subagent_spawn() {
    check(Case::new(
        "subagent_spawn",
        vec!["delegate this"],
        vec![
            Reply::Tool {
                id: "c1",
                name: "Agent",
                input: serde_json::json!({
                    "description": "do a thing",
                    "prompt": "do a thing",
                    "subagent_type": "general-purpose"
                }),
            },
            Reply::Text("child answer"),
            Reply::Text("parent wrap-up"),
        ],
    ))
    .await;
}

// ── Phase 3 decisions ───────────────────────────────────────────────────
//
// The four decisions Phase 3's first six work orders move, each of which had
// no case before. Written *before* the code moves, so the trace they pin is
// the behavior as it is today rather than as it came out.

/// **D2 — stop on the per-turn call ceiling.**
///
/// The model asks for a tool forever; the loop must stop itself. What the
/// trace pins is both halves: which reason it stops with (`max_turns`, not
/// `end_turn`), and how many model calls it made before deciding — a
/// ceiling that is off by one is a real change and an invisible one.
///
/// The ceiling comes from `min(settings, scene)`. This case sets the scene's,
/// which is the half that used to be ignored entirely.
#[tokio::test]
async fn stops_at_the_per_turn_call_ceiling() {
    let mut case = Case::new(
        "max_turns_ceiling",
        vec!["keep going"],
        vec![
            Reply::Tool {
                id: "c1",
                name: "GoldenEcho",
                input: serde_json::json!({"say": "one"}),
            },
            Reply::Tool {
                id: "c2",
                name: "GoldenEcho",
                input: serde_json::json!({"say": "two"}),
            },
            Reply::Tool {
                id: "c3",
                name: "GoldenEcho",
                input: serde_json::json!({"say": "three"}),
            },
            Reply::Text("never reached"),
        ],
    );
    case.max_api_calls = Some(2);
    check(case).await;
}

/// **D13 — the tool-result budget truncates an oversized result.**
///
/// `GoldenFlood` returns 60_000 bytes, past the 50_000-byte per-result cap.
/// The decision is invisible in every other case because every other tool
/// returns a few bytes, so this is the only thing standing between that cap
/// and a silent change to it.
#[tokio::test]
async fn an_oversized_tool_result_is_truncated_by_the_budget() {
    check(Case::new(
        "tool_result_budget",
        vec!["flood me"],
        vec![
            Reply::Tool {
                id: "c1",
                name: "GoldenFlood",
                input: serde_json::json!({}),
            },
            Reply::Text("that was a lot"),
        ],
    ))
    .await;
}

/// **D11 — an output-token target keeps the turn going.**
///
/// `+50k` in the user message sets a target; the loop then injects a nudge
/// and continues rather than ending on the model's `end_turn`. Three things
/// are pinned: that the directive is stripped from what the model is shown,
/// that the continuation happens at all, and *when it gives up* — the
/// diminishing-returns rule (three continuations with two deltas under 500)
/// is three magic numbers with no configuration entry, so the trace is the
/// only thing holding them.
///
/// One reply is deliberately left over. Consuming exactly as many as were
/// scripted would prove nothing: a loop that stopped because it ran out of
/// replies looks identical to one that decided to stop. `unconsumed_replies=1`
/// is the difference.
#[tokio::test]
async fn an_output_token_target_continues_then_gives_up_on_diminishing_returns() {
    check(Case::new(
        "output_token_target",
        vec!["+50k write me a long thing"],
        vec![
            Reply::Text("first pass"),
            Reply::Text("second pass"),
            Reply::Text("third pass"),
            Reply::Text("fourth pass"),
            Reply::Text("never reached"),
        ],
    ))
    .await;
}

/// **D14 — reactive compaction that fires and clears nothing.**
///
/// The flag is on and the trigger fires: with a 20k context limit the
/// reactive check's `trigger = 50_000` remaining-token threshold is satisfied
/// on every call, so the pass runs every round. It still changes nothing,
/// because `micro_compact` clears results only for a fixed whitelist of tool
/// names and `GoldenFlood` is not on it.
///
/// That is the case's subject: firing is not the same as helping, and the
/// circuit breaker is fed by the difference. `reactive_compaction_clears_old_tool_results`
/// is the other half — the same trigger with a whitelisted tool, where the
/// request does shrink.
///
/// (The threshold path, `compaction_triggers_and_is_logged`, is unaffected
/// and stays out of this case: 20k is far above what these messages reach.)
#[tokio::test]
async fn reactive_compaction_fires_but_clears_nothing() {
    let mut case = Case::new(
        "reactive_compaction_no_effect",
        vec!["one", "two", "three"],
        vec![
            Reply::Tool {
                id: "c1",
                name: "GoldenFlood",
                input: serde_json::json!({}),
            },
            Reply::Text("first"),
            Reply::Tool {
                id: "c2",
                name: "GoldenFlood",
                input: serde_json::json!({}),
            },
            Reply::Text("second"),
            Reply::Text("third"),
        ],
    );
    case.reactive_compact = true;
    case.compact_threshold = 20_000;
    check(case).await;
}

/// **D14 — reactive compaction with a consequence.**
///
/// Seven `Read` rounds in one turn, well under the 20k threshold, so the
/// budget never forces anything: everything this case shows is the predictive
/// pass. `Read` is on the micro-compact whitelist and `compact_keep_recent`
/// is 2 (floored to 5 rounds by the pass), so from the sixth round on the
/// oldest results are blanked and `content_bytes` in the model-call column
/// stops climbing.
///
/// The pass used to be unobservable — it adopted its result only when the
/// message *count* dropped, and blanking a body in place never drops the
/// count, so the work was done and thrown away on every round. This case
/// exists because that is now fixed, and it is what would notice if it broke
/// again. Reverse-verified two ways: turning the feature flag off for this
/// case, and putting the message-count criterion back, each leave
/// `content_bytes` climbing and fail here.
#[tokio::test]
async fn reactive_compaction_clears_old_tool_results() {
    fn read(id: &'static str) -> Reply {
        Reply::Tool {
            id,
            name: "Read",
            input: serde_json::json!({}),
        }
    }
    let mut case = Case::new(
        "reactive_compaction_clears",
        vec!["walk the tree"],
        vec![
            read("r1"),
            read("r2"),
            read("r3"),
            read("r4"),
            read("r5"),
            read("r6"),
            read("r7"),
            Reply::Text("done"),
        ],
    );
    case.reactive_compact = true;
    case.compact_threshold = 20_000;
    case.extra_tools = vec![compactable_tool()];
    check(case).await;
}
