//! Does a `scripts` section actually change what the model is sent?
//!
//! Every other test of the script carrier stops one step short: the unit
//! tests call an adapter directly, and the binding tests in `script-host`
//! check that a file on disk produces an adapter. Neither runs a session, so
//! neither would notice a point that binds correctly and is then never
//! consulted, or an adapter installed on a builder nobody reads.
//!
//! So this drives a real `Agent`, from real `Settings`, with the scripts read
//! off `tests/fixtures/scripts/`, and asserts on what reached
//! `ScriptedModel`. Each fixture leaves a `SCRIPT-TRACE-` mark that nothing
//! else in the engine produces — a script that logged, or that made a change
//! the engine would have made anyway, would let this pass without running.
//!
//! # Three runs per point
//!
//! **Bound** is the one that looks like the feature. **Unbound** is the one
//! that makes it mean something: without it, "the mark is in the request"
//! would also be satisfied by a mark that was in the request all along.
//! **Broken** — the same point, bound to a script that throws — is the one
//! that would catch the worst failure, an adapter that half-applies what a
//! dead script asked for. It reads the ledger rather than the request,
//! because the whole claim is that nothing observable happened; a run where
//! the script was never bound at all would look identical.
//!
//! # Adding a point
//!
//! Add a fixture beside the others, add a row to [`cases`], and give it a
//! mark of its own. A point that has no adapter yet has no row: a case
//! asserting that an unbound point changes nothing passes for the wrong
//! reason.

use base::interface::script::ScriptOutcome;
use base::interface::tool::{
    PermissionDecision, ProgressSender, PromptContext, Tool, ToolContext, ToolResult,
};
use base::memory::{DurableMemory, MemoryStore, MemoryType};
use std::sync::Arc;
use test_runner::script_session::{drive, Ran, Session};
use test_runner::scripted_model::Reply;

/// What the session needs beyond a model and a tool for the point to fire.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Needs {
    Nothing,
    /// A memory store with something in it, and recall switched on. Applied
    /// whether or not the script is bound, so the unbound run differs in the
    /// script and nothing else.
    Recall,
}

/// What the bound run must show, and therefore what the unbound run must not.
///
/// Two shapes rather than one, because not every point leaves a mark. A point
/// whose only effect is to take something away has nothing to add to the
/// prompt, and a table that could only say "this string appears" left the one
/// point shaped that way — `model.request` — with no row at all.
enum Expect {
    /// This mark reaches the model.
    MarkAppears(&'static str),
    /// This tool is no longer among the ones the request offers.
    ToolWithdrawn(&'static str),
}

/// One bindable point, its fixture, and what binding it does.
struct ScriptCase {
    /// Catalog id of the point. Also what a failure names.
    point: &'static str,
    /// File under `tests/fixtures/scripts/`.
    script: &'static str,
    entry: &'static str,
    expect: Expect,
    /// Must reach the model in neither run. For a script whose job is partly
    /// to take something away: the mark above proves the script ran, this
    /// proves the removing half of it did.
    absent: Option<&'static str>,
    /// Bindings beyond the one under test, bound in both runs.
    ///
    /// A variable is only visible once some block references it, so proving
    /// one expanded takes a second script to put `{{name}}` somewhere. Bound
    /// in the unbound run too, so the two runs still differ in exactly the
    /// script being tested.
    also: &'static [(&'static str, &'static str, &'static str)],
    /// One entry per user message, sent in order. More than one when the
    /// mark only reaches the model as history — a script that rewrites an
    /// assistant message changes the *next* request, not the one in flight.
    turns: &'static [&'static str],
    replies: fn() -> Vec<Reply>,
    needs: Needs,
}

fn cases() -> Vec<ScriptCase> {
    vec![
        ScriptCase {
            point: "prompt.assemble",
            script: "prompt_assemble.js",
            entry: "onAssemble",
            expect: Expect::MarkAppears("SCRIPT-TRACE-ASSEMBLE"),
            absent: None,
            also: &[],
            turns: &["hello"],
            replies: || vec![Reply::Text("hi")],
            needs: Needs::Nothing,
        },
        ScriptCase {
            point: "tool.result",
            script: "tool_result.js",
            entry: "onResult",
            // The tool's name is in the mark, so this also pins that the
            // script was told which call it was looking at.
            expect: Expect::MarkAppears("SCRIPT-TRACE-RESULT(ScriptEcho)"),
            absent: None,
            also: &[],
            turns: &["use the tool"],
            replies: || {
                vec![
                    Reply::Tool {
                        id: "call-1",
                        name: "ScriptEcho",
                        input: serde_json::json!({"say": "anything"}),
                    },
                    Reply::Text("done"),
                ]
            },
            needs: Needs::Nothing,
        },
        ScriptCase {
            point: "memory.retrieval_hook",
            script: "memory_retrieval.js",
            entry: "onRetrieval",
            // The rewritten query is the only thing that matches either
            // memory, so a recall reaching the model at all is proof the
            // `before` half ran.
            expect: Expect::MarkAppears("SCRIPT-TRACE-RECALL"),
            // Which the `after` half then dropped, though the rewritten
            // query found it too.
            absent: Some("secret-key-rotation"),
            also: &[],
            turns: &["when can I ship this?"],
            replies: || vec![Reply::Text("on tuesday")],
            needs: Needs::Recall,
        },
        ScriptCase {
            point: "prompt.block",
            script: "prompt_block.js",
            entry: "onBlock",
            expect: Expect::MarkAppears("SCRIPT-TRACE-BLOCK"),
            absent: None,
            also: &[],
            turns: &["hello"],
            replies: || vec![Reply::Text("hi")],
            needs: Needs::Nothing,
        },
        ScriptCase {
            point: "prompt.context",
            script: "prompt_context.js",
            entry: "onContext",
            // The mark carries `cwd`, which the identity call cannot see —
            // so a block that was registered with static text at bind time
            // would arrive without it and this would fail.
            expect: Expect::MarkAppears("SCRIPT-TRACE-CONTEXT: working in"),
            absent: None,
            also: &[],
            turns: &["hello"],
            replies: || vec![Reply::Text("hi")],
            needs: Needs::Nothing,
        },
        ScriptCase {
            point: "prompt.variable",
            script: "prompt_variable.js",
            entry: "onVariable",
            expect: Expect::MarkAppears("SCRIPT-TRACE-VARIABLE("),
            absent: None,
            // A variable nothing mentions expands nowhere, so a block that
            // mentions it comes along.
            also: &[("prompt_block_var.js", "prompt.block", "onBlock")],
            turns: &["hello"],
            replies: || vec![Reply::Text("hi")],
            needs: Needs::Nothing,
        },
        ScriptCase {
            point: "tool.around",
            script: "tool_around.js",
            entry: "onAround",
            // The tool's own answer never appears, because the ring answered
            // instead of dispatching — but `absent` means "in neither run",
            // and the unbound run is exactly where the tool does answer. So
            // the mark carries the proof on its own: nothing but this script
            // produces it, and the model still got a result.
            expect: Expect::MarkAppears("SCRIPT-TRACE-AROUND"),
            absent: None,
            also: &[],
            turns: &["use the tool"],
            replies: || {
                vec![
                    Reply::Tool {
                        id: "call-1",
                        name: "ScriptEcho",
                        input: serde_json::json!({"say": "anything"}),
                    },
                    Reply::Text("done"),
                ]
            },
            needs: Needs::Nothing,
        },
        ScriptCase {
            point: "model.message",
            script: "model_message.js",
            entry: "onMessage",
            expect: Expect::MarkAppears("SCRIPT-TRACE-MESSAGE"),
            absent: None,
            also: &[],
            // Two turns: the mark is put on the *first* turn's reply, and
            // only reaches the model as history on the second.
            turns: &["hello", "and again"],
            replies: || vec![Reply::Text("first answer"), Reply::Text("second answer")],
            needs: Needs::Nothing,
        },
        ScriptCase {
            point: "model.request",
            script: "model_request.js",
            entry: "onRequest",
            // Nothing this point can do is visible in the prompt: it is handed
            // the knobs and the tool *names*, and the only one of those the
            // model's own copy of the request shows is which tools it was
            // offered. So the absence is the assertion.
            expect: Expect::ToolWithdrawn("WebSearch"),
            absent: None,
            also: &[],
            turns: &["hello"],
            replies: || vec![Reply::Text("hi")],
            needs: Needs::Nothing,
        },
    ]
}

/// A store holding the two memories, under the run's own root so the run
/// leaves nothing behind it.
fn seeded_memories(root: &std::path::Path) -> Arc<MemoryStore> {
    let store = Arc::new(MemoryStore::new(
        root.join("mem-user"),
        root.join("mem-local"),
    ));
    seed_memories(&store);
    store
}

/// Two memories the fixture's rewritten query finds and the user's own words
/// do not. One of them is the one the fixture then filters out.
fn seed_memories(store: &MemoryStore) {
    let memory = |name: &str, content: &str| DurableMemory {
        name: name.to_string(),
        description: "seeded by the script fixture test".into(),
        memory_type: MemoryType::Project,
        content: content.to_string(),
        source_session_id: String::new(),
        confidence: 0.9,
        last_seen: "2026-08-01T00:00:00Z".into(),
        recall_count: 0,
    };
    store
        .persist_batch(vec![
            memory(
                "deploy-window",
                "SCRIPT-TRACE-RECALL: deploys land on Tuesdays.",
            ),
            memory(
                "secret-key-rotation",
                "SCRIPT-TRACE-RECALL: rotate the signing key quarterly.",
            ),
        ])
        .expect("seed memories");
}

fn fixtures() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("tests/")
        .join("fixtures")
        .join("scripts")
}

/// Which of the three runs this is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Bind {
    /// The point's own fixture.
    Fixture,
    /// Nothing at this point. The `also` bindings stay, so the two runs still
    /// differ in exactly the script under test.
    Nothing,
    /// A script that throws, at the same point, with the same everything else.
    Throwing,
}

fn session(turns: &[&str], replies: Vec<Reply>) -> Session {
    Session::new(fixtures(), turns, replies)
        .scene(Arc::new(scene::scene::coding::CodingScene))
        .tool(Arc::new(ScriptEcho) as Arc<dyn Tool>)
}

fn from_case(root: &std::path::Path, case: &ScriptCase, bind: Bind) -> Session {
    let mut s = session(case.turns, (case.replies)());
    for (path, point, entry) in case.also {
        s = s.bind(path, point, entry);
    }
    if case.needs == Needs::Recall {
        s = s.memory(seeded_memories(root));
    }
    match bind {
        Bind::Fixture => s.bind(case.script, case.point, case.entry),
        Bind::Throwing => s.bind("broken/throws.js", case.point, "boom"),
        Bind::Nothing => s,
    }
}

async fn run(case: &ScriptCase, bind: Bind) -> Ran {
    let tmp = tempfile::tempdir().expect("tempdir");
    drive(tmp.path(), from_case(tmp.path(), case, bind)).await
}

/// The session is built from the run's own root, so anything a case needs on
/// disk lands inside the directory that goes away with it.
async fn run_session(build: impl FnOnce(&std::path::Path) -> Session) -> Ran {
    let tmp = tempfile::tempdir().expect("tempdir");
    let session = build(tmp.path());
    drive(tmp.path(), session).await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bound_script_leaves_its_mark_on_what_the_model_receives() {
    for case in cases() {
        let ran = run(&case, Bind::Fixture).await;
        assert!(
            !ran.requests.is_empty(),
            "case `{}`: the model was never called",
            case.point
        );
        match case.expect {
            Expect::MarkAppears(mark) => assert!(
                ran.holds(mark),
                "case `{}`: `{mark}` never reached the model. Requests:\n{}",
                case.point,
                ran.requests.join("\n---\n")
            ),
            Expect::ToolWithdrawn(tool) => assert!(
                !ran.offers(tool),
                "case `{}`: `{tool}` was still offered, so the script did not \
                 narrow the request. Tools: {:?}",
                case.point,
                ran.tools
            ),
        }
        if let Some(absent) = case.absent {
            assert!(
                !ran.holds(absent),
                "case `{}`: `{absent}` reached the model, so the script's \
                 removing half did not run. Requests:\n{}",
                case.point,
                ran.requests.join("\n---\n")
            );
        }
    }
}

/// The same sessions with the `scripts` section taken out. Everything else —
/// the tools, the memories, the model's answers — is identical, so a mark
/// that survives came from somewhere other than the script.
#[tokio::test(flavor = "multi_thread")]
async fn with_no_scripts_section_no_mark_reaches_the_model() {
    for case in cases() {
        let ran = run(&case, Bind::Nothing).await;
        assert!(
            !ran.requests.is_empty(),
            "case `{}`: the model was never called",
            case.point
        );
        match case.expect {
            Expect::MarkAppears(mark) => assert!(
                !ran.holds(mark),
                "case `{}`: `{mark}` reached the model with no script bound, so \
                 the case proves nothing. Requests:\n{}",
                case.point,
                ran.requests.join("\n---\n")
            ),
            Expect::ToolWithdrawn(tool) => assert!(
                ran.offers(tool),
                "case `{}`: `{tool}` was missing with no script bound, so the \
                 case proves nothing. Tools: {:?}",
                case.point,
                ran.tools
            ),
        }
        if let Some(absent) = case.absent {
            assert!(!ran.holds(absent), "case `{}`: `{absent}`", case.point);
        }
    }
}

/// The same point, bound to a script that throws.
///
/// Every point promises the same thing about failure: the point is left as the
/// adapter found it. What makes that hard to check is that a point left alone
/// looks exactly like a point nothing was bound to — so the observable half of
/// this (no mark, tool still offered) is only half the case. The ledger is the
/// other half: it says the script was called and did not come back, which is
/// the difference between an adapter that survived a failure and an adapter
/// that was never reached.
#[tokio::test(flavor = "multi_thread")]
async fn a_script_that_throws_leaves_its_point_alone_and_the_turn_finishes() {
    for case in cases() {
        let ran = run(&case, Bind::Throwing).await;
        assert!(
            !ran.requests.is_empty(),
            "case `{}`: the turn did not reach the model with a throwing script \
             bound — a failing script must cost its own contribution and nothing \
             else",
            case.point
        );
        match case.expect {
            Expect::MarkAppears(mark) => assert!(
                !ran.holds(mark),
                "case `{}`: `{mark}` reached the model although the script threw",
                case.point
            ),
            Expect::ToolWithdrawn(tool) => assert!(
                ran.offers(tool),
                "case `{}`: `{tool}` was withdrawn although the script threw, so \
                 a dead script still changed the request",
                case.point
            ),
        }

        let outcomes = ran.outcomes_at(case.point);
        assert!(
            outcomes
                .iter()
                .any(|o| matches!(o, ScriptOutcome::Failed { .. })),
            "case `{}`: nothing in the ledger says the script failed, so this \
             run cannot be told apart from one where it was never called. \
             Ledger at this point: {outcomes:?}",
            case.point
        );
    }
}

// ── crossings ───────────────────────────────────────────────────────────
//
// The table above proves each adapter works on its own. These are about what
// two of them do to each other, which is where a refactor breaks something no
// single-point case can see. A case belongs here only if taking one of its two
// points away would leave nothing to assert — otherwise it is a table row.

/// One tool call the scripted model makes, and the answer to it.
fn calls_the_echo_tool() -> Vec<Reply> {
    vec![
        Reply::Tool {
            id: "call-1",
            name: "ScriptEcho",
            input: serde_json::json!({"say": "anything"}),
        },
        Reply::Text("done"),
    ]
}

/// `tool.around` answering in place of the tool means `tool.result` never
/// sees that answer.
///
/// Neither document says which way this goes, and both readings are
/// defensible: the result point is "the last thing before the model sees it",
/// which argues for running; the ring answered instead of dispatching, so
/// there is no dispatch whose result could be transformed, which argues
/// against. What matters is that it is decided rather than incidental — a
/// script that sanitizes every tool result would otherwise have a hole in it
/// that only appears when some other script starts answering from a cache.
#[tokio::test(flavor = "multi_thread")]
async fn an_answer_from_the_around_ring_skips_the_result_point() {
    let ran = run_session(|_| {
        session(&["use the tool"], calls_the_echo_tool())
            .bind("tool_around.js", "tool.around", "onAround")
            .bind("tool_result.js", "tool.result", "onResult")
    })
    .await;

    assert!(
        ran.holds("SCRIPT-TRACE-AROUND"),
        "the ring's answer never reached the model"
    );
    assert!(
        !ran.holds("SCRIPT-TRACE-RESULT"),
        "the result point ran on an answer that was never dispatched"
    );
    assert!(
        ran.outcomes_at("tool.result").is_empty(),
        "the result point was called, so it was reached and chose to do \
         nothing — a different thing from not being reached: {:?}",
        ran.outcomes_at("tool.result")
    );
}

/// And a refusal is not rewritten either. A denial travels as the tool's own
/// error, and the result point is not offered it.
#[tokio::test(flavor = "multi_thread")]
async fn a_denial_from_the_around_ring_reaches_the_model_unrewritten() {
    let ran = run_session(|_| {
        session(&["use the tool"], calls_the_echo_tool())
            .bind("tool_around_deny.js", "tool.around", "onAround")
            .bind("tool_result.js", "tool.result", "onResult")
    })
    .await;

    assert!(
        ran.holds("SCRIPT-TRACE-DENY"),
        "the model was not told why the call was refused"
    );
    assert!(
        !ran.holds("SCRIPT-TRACE-RESULT"),
        "the result point rewrote a denial it should never have been offered"
    );
    assert!(
        !ran.holds("echo: anything"),
        "the tool ran despite the refusal"
    );
}

/// Two rings on one point: the first binding is the outer one.
///
/// The carrier's document says so, and the order is not something a reader can
/// check from the settings file — a session that installed them the other way
/// round would look identical until the day two scripts disagree.
#[tokio::test(flavor = "multi_thread")]
async fn two_rings_on_one_point_run_in_the_order_they_were_bound() {
    let ran = run_session(|_| {
        session(&["use the tool"], calls_the_echo_tool())
            .bind("tool_around.js", "tool.around", "onAround")
            .bind("tool_around_second.js", "tool.around", "onAround")
    })
    .await;

    assert!(
        ran.holds("SCRIPT-TRACE-AROUND:"),
        "the first binding did not decide, so it is not the outer ring"
    );
    assert!(
        !ran.holds("SCRIPT-TRACE-AROUND-SECOND"),
        "the second binding answered, which means the first never got the call"
    );
}

/// A block one script registers, another script removes.
///
/// The registering points and the assembly point are different mechanisms —
/// one contributes before the session runs, the other edits every turn — and
/// this is the only case where the order between them is observable. A script
/// that could not remove a script-registered block would leave every
/// contribution permanent for the session.
#[tokio::test(flavor = "multi_thread")]
async fn a_block_one_script_registered_can_be_removed_by_another() {
    let with_both = run_session(|_| {
        session(&["hello"], vec![Reply::Text("hi")])
            .bind("prompt_block.js", "prompt.block", "onBlock")
            .bind("prompt_assemble_delete.js", "prompt.assemble", "onAssemble")
    })
    .await;
    assert!(
        !with_both.holds("SCRIPT-TRACE-BLOCK"),
        "the block survived the removal"
    );

    // Without the remover the block is there, so the assertion above is about
    // the removal and not about the block never having been registered.
    let registered_only = run_session(|_| {
        session(&["hello"], vec![Reply::Text("hi")]).bind(
            "prompt_block.js",
            "prompt.block",
            "onBlock",
        )
    })
    .await;
    assert!(
        registered_only.holds("SCRIPT-TRACE-BLOCK"),
        "the block was never in the prompt to begin with"
    );
}

/// A variable one script provides, expanded inside a block another
/// contributed.
///
/// The table's row for `prompt.variable` proves the value appears somewhere in
/// the request. This is the stronger statement, and the one the point is for:
/// it appears *where the placeholder was*, in text a different script wrote.
#[tokio::test(flavor = "multi_thread")]
async fn a_variable_expands_inside_a_block_another_script_contributed() {
    let ran = run_session(|_| {
        session(&["hello"], vec![Reply::Text("hi")])
            .bind("prompt_block_var.js", "prompt.block", "onBlock")
            .bind("prompt_variable.js", "prompt.variable", "onVariable")
    })
    .await;

    assert!(
        ran.holds("trace slot: SCRIPT-TRACE-VARIABLE("),
        "the placeholder was not replaced in the block that held it. \
         Requests:\n{}",
        ran.requests.join("\n---\n")
    );
    assert!(
        !ran.holds("{{script_trace_var}}"),
        "the placeholder is still in the prompt"
    );
}

/// Narrowing the tools a request offers does not gate what may be dispatched.
///
/// `model.request` edits the request; the registry decides what exists. A
/// model that asks for a tool it was not offered — a stale conversation, a
/// second script, a provider that ignored the list — still gets it. Written
/// down because the opposite is the natural assumption, and a script author
/// who reaches for this point as a permission mechanism has picked the wrong
/// one: that is what `tool.around` and the permission gate are for.
#[tokio::test(flavor = "multi_thread")]
async fn withdrawing_a_tool_from_the_request_does_not_gate_its_dispatch() {
    let ran = run_session(|_| {
        session(
            &["use the skill"],
            vec![
                Reply::Tool {
                    id: "call-1",
                    name: "Skill",
                    input: serde_json::json!({"skill": "no-such-skill"}),
                },
                Reply::Text("done"),
            ],
        )
        .bind("model_request_drop_skill.js", "model.request", "onRequest")
    })
    .await;

    assert!(
        !ran.offers("Skill"),
        "the script did not withdraw the tool, so the case is not set up: {:?}",
        ran.tools
    );
    assert!(
        ran.requests.len() >= 2,
        "the call never came back with a result, so the withdrawn tool was \
         not dispatched"
    );
}

/// `tool.result` runs after the hooks, not before them.
///
/// The carrier's document says so — "一个工具结果一次，在所有 hook 之后" — and
/// nothing checked it. The order decides what a script is even looking at: a
/// script that redacts or tags results sees the hook's verdict if it runs
/// second, and the raw tool output if it runs first, which is a different
/// program with the same source.
///
/// A `PostToolUse` hook that blocks replaces the result with its own text, so
/// the two orders leave visibly different strings in the transcript. One
/// assertion tells them apart.
#[tokio::test(flavor = "multi_thread")]
async fn the_result_point_runs_after_the_hooks() {
    let ran = run_session(|_| {
        session(&["use the tool"], calls_the_echo_tool())
            .bind("tool_result.js", "tool.result", "onResult")
            .hooks(serde_json::json!({
                "PostToolUse": [{
                    "type": "command",
                    // Reads the payload and refuses; the message becomes the
                    // result the model is shown.
                    "command": "cat >/dev/null; echo '{\"decision\":\"block\",\"message\":\"HOOK-WAS-HERE\"}'"
                }]
            }))
    })
    .await;

    assert!(
        ran.holds("HOOK-WAS-HERE"),
        "the hook did not run at all, so this proves nothing. Requests:\n{}",
        ran.requests.join("\n---\n")
    );
    assert!(
        ran.holds("SCRIPT-TRACE-RESULT(ScriptEcho) Denied by PostToolUse hook: HOOK-WAS-HERE"),
        "the script did not wrap the hook's verdict, so it ran before the hook \
         rather than after it. Requests:\n{}",
        ran.requests.join("\n---\n")
    );
    assert!(
        !ran.holds("echo: anything"),
        "the tool's own output survived a hook that refused it"
    );
}

/// The message the model is shown next turn and the message in the transcript
/// are the same message.
///
/// `model.message` rewrites an assistant message on its way back. If the
/// rewrite reached only the next request, a resumed or forked session would
/// replay a conversation that never happened — the model would see the
/// original where the live session saw the rewrite, and nothing in either
/// place would say so.
#[tokio::test(flavor = "multi_thread")]
async fn a_rewritten_message_is_the_one_that_gets_logged() {
    let ran = run_session(|_| {
        session(
            &["hello", "and again"],
            vec![Reply::Text("first answer"), Reply::Text("second answer")],
        )
        .bind("model_message.js", "model.message", "onMessage")
        .logged()
    })
    .await;

    assert!(
        ran.holds("SCRIPT-TRACE-MESSAGE first answer"),
        "the next request did not carry the rewrite"
    );
    assert!(
        !ran.log.trim().is_empty(),
        "the run kept no transcript, so this proves nothing"
    );
    assert!(
        ran.log.contains("SCRIPT-TRACE-MESSAGE first answer"),
        "the transcript kept the original while the model was shown the \
         rewrite. Log:\n{}",
        ran.log
    );
}

/// Recall's two halves run on every turn, not once for the session.
///
/// The hook is per user message by contract, and the failure it guards against
/// is subtle: a hook installed once and consulted once would look correct on a
/// one-turn case, which every other case about this point is. The turn number
/// in the ledger is what makes the difference visible.
#[tokio::test(flavor = "multi_thread")]
async fn the_recall_hook_runs_on_every_turn() {
    let ran = run_session(|root| {
        session(
            &["when can I ship this?", "and after that?"],
            // Spare answers: memory work makes model calls of its own, and
            // running out of scripted replies would fail this case for a
            // reason that has nothing to do with the hook.
            vec![
                Reply::Text("on tuesday"),
                Reply::Text("on wednesday"),
                Reply::Text("spare"),
                Reply::Text("spare"),
            ],
        )
        .bind(
            "memory_retrieval.js",
            "memory.retrieval_hook",
            "onRetrieval",
        )
        .memory(seeded_memories(root))
    })
    .await;

    let turns: Vec<u32> = ran
        .ledger
        .records()
        .into_iter()
        .filter(|r| r.point == "memory.retrieval_hook")
        .map(|r| r.turn)
        .collect();
    assert_eq!(
        turns,
        vec![1, 1, 2, 2],
        "expected `before` and `after` on each of two turns, got {turns:?}"
    );
}

/// Echoes its argument, so a `tool.result` script has a result to rewrite.
#[derive(Debug)]
struct ScriptEcho;

#[async_trait::async_trait]
impl Tool for ScriptEcho {
    fn name(&self) -> &str {
        "ScriptEcho"
    }
    fn description(&self) -> &str {
        "Echo the `say` argument back."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"say": {"type": "string"}},
            "required": ["say"]
        })
    }
    fn is_read_only(&self, _: &serde_json::Value) -> bool {
        true
    }
    async fn check_permissions(
        &self,
        _: &serde_json::Value,
        _: &ToolContext,
    ) -> PermissionDecision {
        PermissionDecision::allow()
    }
    async fn prompt(&self, _: &PromptContext) -> String {
        self.description().to_string()
    }
    async fn call(
        &self,
        input: serde_json::Value,
        _ctx: ToolContext,
        _p: ProgressSender,
    ) -> Result<ToolResult, base::error::ToolError> {
        let say = input.get("say").and_then(|v| v.as_str()).unwrap_or("");
        Ok(ToolResult::text(format!("echo: {say}")))
    }
}
