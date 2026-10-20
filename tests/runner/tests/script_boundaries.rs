//! The promises the script carrier makes when a script misbehaves.
//!
//! Every case here is a sentence already written in `docs/extending_quickjs.md`
//! — a failing script changes nothing, a budget is per turn, a deadline reaches
//! inside the interpreter, a script from outside may only add, one bad binding
//! drops the whole set. They are the reason an operator is willing to run
//! somebody's JavaScript inside their agent, and none of them was checked
//! against a running session.
//!
//! Kept apart from `script_carrier.rs` on purpose. Those cases are about
//! coverage — this point works, and here is the mark that proves it — and a red
//! one usually means a fixture needs updating. A red one here means the engine
//! stopped keeping a promise, and nobody should be able to make it green by
//! regenerating anything.
//!
//! # What is not here
//!
//! A build with no script carrier refuses a `scripts` section rather than
//! ignoring it. That is a compile-time guard in `daemon`, and a test running in
//! a build that *has* the carrier cannot observe the build that does not.

use base::interface::script::{ScriptError, ScriptOutcome};
use base::interface::tool::{
    PermissionDecision, ProgressSender, PromptContext, Tool, ToolContext, ToolResult,
};
use std::sync::Arc;
use test_runner::script_session::{drive, Ran, Session};
use test_runner::scripted_model::Reply;

fn fixtures() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("tests/")
        .join("fixtures")
        .join("scripts")
}

/// The one script that lives outside the project root the cases bind against.
fn outside() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("tests/")
        .join("fixtures")
        .join("scripts_outside")
        .join("add_and_modify.js")
}

fn session(turns: &[&str], replies: Vec<Reply>) -> Session {
    Session::new(fixtures(), turns, replies)
        .scene(Arc::new(scene::scene::coding::CodingScene))
        .tool(Arc::new(Echo) as Arc<dyn Tool>)
}

async fn run(session: Session) -> Ran {
    let tmp = tempfile::tempdir().expect("tempdir");
    drive(tmp.path(), session).await
}

fn calls_guarded() -> Reply {
    Reply::Tool {
        id: "guarded-1",
        name: "Guarded",
        input: serde_json::json!({}),
    }
}

fn calls_echo(id: &'static str, say: &'static str) -> Reply {
    Reply::Tool {
        id,
        name: "Echo",
        input: serde_json::json!({ "say": say }),
    }
}

// ── one bad binding drops the whole set ─────────────────────────────────

/// A `scripts` section is honored whole or not at all.
///
/// Half a configuration is a configuration nobody wrote: the operator reads
/// their settings file, sees five bindings, and has no way to learn that two of
/// them are live. Refusing the set makes the mistake loud at the one moment
/// somebody is in a position to fix it.
#[test]
fn one_binding_that_cannot_be_honored_drops_every_other() {
    let bindings = vec![
        binding("prompt_assemble.js", "prompt.assemble", "onAssemble"),
        binding("tool_result.js", "tool.result", "onResult"),
        // A point that exists in the catalog and has no script adapter.
        binding("tool_result.js", "history.append_observer", "onResult"),
        binding("prompt_block.js", "prompt.block", "onBlock"),
    ];

    let err = script_host::bindings::bind_quickjs(&bindings, &fixtures())
        .expect_err("a binding at an unbindable point must be refused");
    let message = err.to_string();
    assert!(
        message.contains("history.append_observer"),
        "the error must name the binding that is wrong: {message}"
    );
    assert!(
        message.contains("prompt.assemble"),
        "and say what could have been used instead: {message}"
    );
}

/// The same holds for a file that is not there. A path typo is the common
/// version of this, and it must not leave a session half configured.
#[test]
fn a_binding_whose_file_is_missing_drops_every_other() {
    let bindings = vec![
        binding("prompt_assemble.js", "prompt.assemble", "onAssemble"),
        binding("no_such_script.js", "tool.result", "onResult"),
    ];

    let err = script_host::bindings::bind_quickjs(&bindings, &fixtures())
        .expect_err("a missing file must be refused");
    assert!(
        err.to_string().contains("no_such_script.js"),
        "the error must name the file: {err}"
    );
}

// ── authority follows the file ──────────────────────────────────────────

/// A script from outside the project may add to the prompt and may not rewrite
/// it — and the refusal costs it only the edit it was not allowed to make.
///
/// This is the one point where provenance has anything to bite on, and the
/// interesting half is not the refusal: it is that the same pass still applies
/// everything it was allowed to do. Dropping the whole pass would punish an
/// author for one overreach, and — worse — would make "this script is being
/// held back" indistinguishable from "this script does nothing".
#[tokio::test(flavor = "multi_thread")]
async fn a_script_from_outside_the_project_may_add_and_may_not_rewrite() {
    let path = outside();
    let ran = run(session(&["hello"], vec![Reply::Text("hi")])
        .bind("prompt_block.js", "prompt.block", "onBlock")
        .bind(
            path.to_str().expect("fixture path is utf-8"),
            "prompt.assemble",
            "onAssemble",
        ))
    .await;

    assert!(
        ran.holds("SCRIPT-TRACE-OUTSIDE-ADD"),
        "the block it was allowed to add never reached the model"
    );
    assert!(
        !ran.holds("SCRIPT-TRACE-OUTSIDE-EDIT"),
        "a script from outside the project rewrote blocks it does not own"
    );

    let outcomes = ran.outcomes_at("prompt.assemble");
    assert!(
        outcomes
            .iter()
            .any(|o| matches!(o, ScriptOutcome::Refused { .. })),
        "nothing was recorded as refused, so a reader cannot tell this script \
         apart from one that did nothing: {outcomes:?}"
    );
    assert!(
        outcomes.contains(&ScriptOutcome::Applied),
        "the permitted half of the pass was dropped along with the refused \
         half: {outcomes:?}"
    );
}

/// The same script, from inside the project, is allowed both halves — so the
/// case above is about where the file lives and not about the script.
///
/// The project here is a directory of its own rather than the fixture tree,
/// because "inside" is decided against the root the bindings were resolved
/// from: to move a script inside, move the root.
#[tokio::test(flavor = "multi_thread")]
async fn the_operators_own_script_may_rewrite_what_it_likes() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path();
    std::fs::copy(outside(), project.join("add_and_modify.js")).expect("copy into the project");

    let session = Session::new(project, &["hello"], vec![Reply::Text("hi")])
        .tool(Arc::new(Echo) as Arc<dyn Tool>)
        .bind(
            fixtures().join("prompt_block.js").to_str().unwrap(),
            "prompt.block",
            "onBlock",
        )
        .bind("add_and_modify.js", "prompt.assemble", "onAssemble");
    let ran = drive(project, session).await;

    assert!(
        ran.holds("SCRIPT-TRACE-OUTSIDE-EDIT"),
        "a script the operator wrote was not allowed to rewrite the prompt"
    );
    assert!(
        !ran.outcomes_at("prompt.assemble")
            .iter()
            .any(|o| matches!(o, ScriptOutcome::Refused { .. })),
        "the operator's own script was refused something"
    );
}

// ── budgets ─────────────────────────────────────────────────────────────

/// A script that never returns is stopped, and the turn goes on without it.
///
/// The carrier's timeout cannot do this on its own: abandoning a future leaves
/// the interpreter spinning on the thread it was called from, and the
/// synchronous points have no future to abandon in the first place. The
/// deadline has to reach inside the interpreter, and this is where that claim
/// is checked rather than believed.
#[tokio::test(flavor = "multi_thread")]
async fn a_script_that_never_returns_costs_its_own_call_and_no_more() {
    let ran = run(session(
        &["use the tool"],
        vec![calls_echo("call-1", "anything"), Reply::Text("done")],
    )
    .bind("broken/hangs.js", "tool.result", "boom")
    .within_ms(50))
    .await;

    assert!(
        ran.holds("echo: anything"),
        "the result never reached the model, so the stuck script took the turn \
         down with it"
    );
    let outcomes = ran.outcomes_at("tool.result");
    assert!(
        outcomes.iter().any(|o| matches!(
            o,
            ScriptOutcome::Failed {
                error: ScriptError::TimedOut { .. }
            }
        )),
        "the script was not stopped by its deadline: {outcomes:?}"
    );
}

/// A script that allocates without bound fails its call, and a second script
/// at another point is untouched.
///
/// The ceiling is per runtime and a runtime is per call, so one script's
/// appetite is its own problem. Given a generous clock so that what stops it is
/// the ceiling rather than the deadline — the case is about memory.
#[tokio::test(flavor = "multi_thread")]
async fn a_script_that_eats_memory_fails_alone() {
    let ran = run(session(&["hello"], vec![Reply::Text("hi")])
        .bind("broken/eats_memory.js", "prompt.context", "boom")
        .within_ms(5_000)
        .bind("prompt_block.js", "prompt.block", "onBlock"))
    .await;

    assert!(
        ran.holds("SCRIPT-TRACE-BLOCK"),
        "the other script's block is missing, so one script's appetite cost \
         another its contribution"
    );
    // Specifically not a timeout: with five seconds to spend, a call that
    // ended anyway ended because the runtime stopped it.
    let outcomes = ran.outcomes_at("prompt.context");
    assert!(
        outcomes.iter().any(|o| matches!(
            o,
            ScriptOutcome::Failed {
                error: ScriptError::Failed(_)
            }
        )),
        "the runaway script was not stopped by the memory ceiling: {outcomes:?}"
    );
}

/// The call quota is per turn, and the turn after it is a fresh one.
///
/// A point that fires once per tool call is where a pathological turn becomes a
/// pathological bill, so the budget is the reason such a point can be offered
/// to a script at all. The half that is easy to get wrong is the reset: a
/// counter that only ever climbs makes a per-session limit wearing a per-turn
/// name, and the script goes quiet in the middle of a long session with nothing
/// to say why.
#[tokio::test(flavor = "multi_thread")]
async fn the_quota_stops_a_turn_and_the_next_turn_starts_over() {
    let ran = run(session(
        &["three calls", "one more"],
        vec![
            Reply::Tools(vec![
                ("c1", "Echo", serde_json::json!({"say": "one"})),
                ("c2", "Echo", serde_json::json!({"say": "two"})),
                ("c3", "Echo", serde_json::json!({"say": "three"})),
            ]),
            Reply::Text("done"),
            calls_echo("c4", "four"),
            Reply::Text("done again"),
        ],
    )
    .bind("tool_result_quota.js", "tool.result", "onResult")
    .calls_per_turn(2))
    .await;

    let first_turn: Vec<ScriptOutcome> = ran
        .records_at("tool.result")
        .into_iter()
        .filter(|r| r.turn == 1)
        .map(|r| r.outcome)
        .collect();
    assert_eq!(
        first_turn.len(),
        3,
        "three results, so three calls to the point: {first_turn:?}"
    );
    assert_eq!(
        first_turn
            .iter()
            .filter(|o| **o == ScriptOutcome::Applied)
            .count(),
        2,
        "the quota was two: {first_turn:?}"
    );
    assert!(
        first_turn.iter().any(|o| matches!(
            o,
            ScriptOutcome::Failed {
                error: ScriptError::QuotaExhausted { .. }
            }
        )),
        "the third call was not refused by the quota: {first_turn:?}"
    );

    let second_turn: Vec<ScriptOutcome> = ran
        .records_at("tool.result")
        .into_iter()
        .filter(|r| r.turn == 2)
        .map(|r| r.outcome)
        .collect();
    assert_eq!(
        second_turn,
        vec![ScriptOutcome::Applied],
        "the next turn did not start with a fresh budget: {second_turn:?}"
    );
}

// ── the ring sits outside the gate ──────────────────────────────────────

/// A refusal from the around ring happens before anyone is asked to approve
/// the call.
///
/// The ordering is written down — the ring is outside the permission gate —
/// and it is the whole reason a script can be used as a policy at all: a
/// refusal that arrived *after* the question would still have interrupted the
/// operator to ask about a call that was never going to run, and a `respond`
/// would have answered from a cache the gate had already approved.
///
/// Every other case here runs with permissions bypassed, so this is also the
/// only one that would notice the ring being moved inside the gate.
#[tokio::test(flavor = "multi_thread")]
async fn a_refusal_lands_before_the_permission_question_is_asked() {
    let refused = run(session(
        &["use the tool"],
        vec![calls_guarded(), Reply::Text("done")],
    )
    .asking()
    .tool(Arc::new(Guarded) as Arc<dyn Tool>)
    .bind("tool_around_deny_guarded.js", "tool.around", "onAround"))
    .await;

    assert!(
        refused.prompts.is_empty(),
        "the operator was asked about a call the script had already refused: \
         {:?}",
        refused.prompts
    );
    assert!(
        refused.holds("SCRIPT-TRACE-DENY-GUARDED"),
        "the model was not told why the call was refused"
    );

    // Without the script the same call does reach the gate — so the assertion
    // above is about the ring and not about a tool that never asks.
    let asked = run(session(
        &["use the tool"],
        vec![calls_guarded(), Reply::Text("done")],
    )
    .asking()
    .tool(Arc::new(Guarded) as Arc<dyn Tool>))
    .await;
    assert_eq!(
        asked.prompts,
        vec!["Guarded".to_string()],
        "the tool did not ask for approval, so the case proves nothing"
    );
}

// ── shapes a point cannot act on ────────────────────────────────────────

/// A message rewrite of the wrong length is discarded whole.
///
/// There is no way to know which of the returned blocks is the extra one, so
/// there is no honest partial application — and a message where one block came
/// from the script and the rest did not is the kind of thing nobody downstream
/// can reason about.
#[tokio::test(flavor = "multi_thread")]
async fn a_message_rewrite_of_the_wrong_length_changes_nothing() {
    let ran = run(session(
        &["hello", "and again"],
        vec![Reply::Text("first answer"), Reply::Text("second answer")],
    )
    .bind("broken/wrong_shape.js", "model.message", "onMessage"))
    .await;

    assert!(
        !ran.holds("SCRIPT-TRACE-EXTRA-BLOCK"),
        "an answer with the wrong number of blocks was applied anyway"
    );
    assert!(
        ran.holds("first answer"),
        "the original message did not survive"
    );
    let outcomes = ran.outcomes_at("model.message");
    assert!(
        outcomes
            .iter()
            .any(|o| matches!(o, ScriptOutcome::NoChange { detail: Some(_) })),
        "the ledger does not say why nothing changed: {outcomes:?}"
    );
}

/// A variable whose value is not a string leaves its placeholder in the prompt.
///
/// Deliberately not blanked: an unresolved placeholder is a bug to see, and a
/// script that blanked its own placeholder on the way down would make its
/// failure look like a successful empty answer. A script that does want it gone
/// says so with `""`.
#[tokio::test(flavor = "multi_thread")]
async fn a_variable_that_is_not_a_string_leaves_its_placeholder_alone() {
    let ran = run(session(&["hello"], vec![Reply::Text("hi")])
        .bind("prompt_block_var.js", "prompt.block", "onBlock")
        .bind("broken/wrong_shape.js", "prompt.variable", "onVariable"))
    .await;

    assert!(
        ran.holds("trace slot: {{script_trace_var}}"),
        "the placeholder was replaced by something, or the block that holds it \
         never arrived. Requests:\n{}",
        ran.requests.join("\n---\n")
    );
}

/// A contribution cannot take a name the engine already contributes.
///
/// Blocks are addressed by name and the first match wins, so a contribution
/// that took `rules` would quietly swallow every edit meant for the real one —
/// including the refusals that keep an outside script away from it.
#[tokio::test(flavor = "multi_thread")]
async fn a_contribution_cannot_take_a_kernel_block_name() {
    let ran = run(session(&["hello"], vec![Reply::Text("hi")]).bind(
        "broken/kernel_name.js",
        "prompt.block",
        "onBlock",
    ))
    .await;

    assert!(
        !ran.holds("SCRIPT-TRACE-KERNEL-NAME"),
        "a script registered itself under a name the engine owns"
    );
    let outcomes = ran.outcomes_at("prompt.block");
    assert!(
        outcomes
            .iter()
            .any(|o| matches!(o, ScriptOutcome::NoChange { detail: Some(_) })),
        "the ledger does not say why the block is missing: {outcomes:?}"
    );
}

// ── when a script is read, and what a binding checks ────────────────────

/// A script is read when the session is built, and not again.
///
/// Skills and agent types both watch their files and reload; scripts
/// deliberately do not, and the difference is the kind of thing that gets
/// "fixed" by someone who noticed the inconsistency rather than the reason.
/// The reason is that a binding is a decision about what code runs inside the
/// agent, and a session that silently picked up an edited file would be
/// running code nobody chose for it.
#[tokio::test(flavor = "multi_thread")]
async fn a_script_edited_mid_session_does_not_take_effect_until_the_next_one() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path();
    let script = project.join("house.js");
    std::fs::write(
        &script,
        r#"function onAssemble(blocks) {
             blocks.push({ name: "house", content: "SCRIPT-TRACE-FIRST" });
             return blocks;
           }"#,
    )
    .unwrap();

    let edited = script.clone();
    let session = Session::new(
        project,
        &["one", "two"],
        vec![Reply::Text("a"), Reply::Text("b")],
    )
    .tool(Arc::new(Echo) as Arc<dyn Tool>)
    .bind("house.js", "prompt.assemble", "onAssemble")
    .between_turns(move |_turn, _root| {
        std::fs::write(
            &edited,
            r#"function onAssemble(blocks) {
                     blocks.push({ name: "house", content: "SCRIPT-TRACE-SECOND" });
                     return blocks;
                   }"#,
        )
        .unwrap();
    });
    let ran = drive(project, session).await;

    assert!(
        ran.holds("SCRIPT-TRACE-FIRST"),
        "the script never ran at all"
    );
    assert!(
        !ran.holds("SCRIPT-TRACE-SECOND"),
        "an edit to the file changed a session that had already read it"
    );
}

/// A file that is not JavaScript binds cleanly and fails at every call.
///
/// Binding checks that the point exists and that the file can be read. It does
/// not evaluate the file, so a syntax error is not one of the failures the
/// all-or-nothing rule covers: the session starts, every binding is installed,
/// and each one fails when it is first called. That is a different shape of
/// bad day from a typo'd path — the operator gets a session that runs and
/// quietly does none of what they configured — so it is worth having written
/// down rather than discovered.
#[tokio::test(flavor = "multi_thread")]
async fn a_file_that_is_not_javascript_binds_and_then_fails_every_call() {
    let bindings = vec![
        binding("broken/syntax_error.js", "prompt.assemble", "boom"),
        binding("prompt_block.js", "prompt.block", "onBlock"),
    ];
    assert!(
        script_host::bindings::bind_quickjs(&bindings, &fixtures()).is_ok(),
        "binding reads the file; it does not evaluate it"
    );

    let ran = run(session(&["hello"], vec![Reply::Text("hi")])
        .bind("broken/syntax_error.js", "prompt.assemble", "boom")
        .bind("prompt_block.js", "prompt.block", "onBlock"))
    .await;

    assert!(
        ran.outcomes_at("prompt.assemble")
            .iter()
            .any(|o| matches!(o, ScriptOutcome::Failed { .. })),
        "the broken file did not fail at the point it was bound to: {:?}",
        ran.outcomes_at("prompt.assemble")
    );
    assert!(
        ran.holds("SCRIPT-TRACE-BLOCK"),
        "the other binding was dropped along with the broken one, although \
         binding had already accepted the set"
    );
}

/// Two bindings of one file get two budgets.
///
/// The quota belongs to a binding, not to a file — one line of configuration
/// is one thing being budgeted. If they shared, the first point to be called
/// would spend the whole allowance and the second would look like a script
/// that had been bound but never ran.
#[tokio::test(flavor = "multi_thread")]
async fn two_bindings_of_one_file_have_budgets_of_their_own() {
    let ran = run(session(
        &["twice"],
        vec![
            Reply::Tools(vec![
                ("c1", "Echo", serde_json::json!({"say": "one"})),
                ("c2", "Echo", serde_json::json!({"say": "two"})),
            ]),
            Reply::Text("done"),
        ],
    )
    .bind("two_points.js", "tool.around", "onAround")
    .calls_per_turn(1)
    .bind("two_points.js", "tool.result", "onResult")
    .calls_per_turn(1))
    .await;

    for point in ["tool.around", "tool.result"] {
        let outcomes = ran.outcomes_at(point);
        assert_eq!(
            outcomes.first(),
            Some(&ScriptOutcome::Applied),
            "`{point}` was already out of budget on its first call, so the \
             two bindings are sharing one: {outcomes:?}"
        );
        assert!(
            outcomes.iter().any(|o| matches!(
                o,
                ScriptOutcome::Failed {
                    error: ScriptError::QuotaExhausted { .. }
                }
            )),
            "`{point}` never hit its own quota, so this proves nothing: \
             {outcomes:?}"
        );
    }
}

/// A binding that names a function the file does not have.
///
/// The two halves of the carrier answer this differently, and both are worth
/// pinning: a registering point makes its identity call while binding, so it
/// registers nothing at all; an intercepting point only finds out when it is
/// first called, and then keeps finding out.
#[tokio::test(flavor = "multi_thread")]
async fn a_binding_that_names_a_function_the_file_does_not_have_fails_at_its_point() {
    let ran = run(session(
        &["use the tool"],
        vec![calls_echo("call-1", "anything"), Reply::Text("done")],
    )
    .bind("tool_result.js", "tool.result", "onNoSuchFunction")
    .bind("prompt_block.js", "prompt.block", "onNoSuchFunction"))
    .await;

    assert!(
        ran.holds("echo: anything"),
        "the tool result was lost along with the script that could not run"
    );
    assert!(
        ran.outcomes_at("tool.result")
            .iter()
            .any(|o| matches!(o, ScriptOutcome::Failed { .. })),
        "the missing entry did not fail the call: {:?}",
        ran.outcomes_at("tool.result")
    );
    assert!(
        !ran.holds("SCRIPT-TRACE-BLOCK"),
        "a block was registered by a function that does not exist"
    );
}

/// Reordering is not something this point can express, and asking for it does
/// not rewrite anything.
///
/// A block's position comes from where it was registered, not from where it
/// sits in the returned array. Handing the same blocks back in another order
/// therefore asks for nothing — except where several blocks share a name, in
/// which case the reordering reads as an edit to each of them, and an edit
/// nothing can address is refused rather than applied to whichever one came
/// first.
#[tokio::test(flavor = "multi_thread")]
async fn handing_the_blocks_back_in_another_order_rewrites_nothing() {
    let ran = run(session(&["hello"], vec![Reply::Text("hi")])
        .bind("prompt_block.js", "prompt.block", "onBlock")
        .bind(
            "prompt_assemble_reverse.js",
            "prompt.assemble",
            "onAssemble",
        ))
    .await;

    assert!(
        ran.holds("SCRIPT-TRACE-BLOCK"),
        "the block is gone, so the pass did more than reorder"
    );
    let outcomes = ran.outcomes_at("prompt.assemble");
    assert!(
        !outcomes.contains(&ScriptOutcome::Applied),
        "a reordering pass was charged as an edit: {outcomes:?}"
    );
}

fn binding(path: &str, point: &str, entry: &str) -> base::interface::settings::ScriptBinding {
    base::interface::settings::ScriptBinding {
        path: path.into(),
        point: point.into(),
        entry: entry.into(),
        timeout_ms: None,
        calls_per_turn: None,
    }
}

/// A tool that always wants approval, so a case can watch for the question.
#[derive(Debug)]
struct Guarded;

#[async_trait::async_trait]
impl Tool for Guarded {
    fn name(&self) -> &str {
        "Guarded"
    }
    fn description(&self) -> &str {
        "Does nothing, but insists on being approved first."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    fn is_read_only(&self, _: &serde_json::Value) -> bool {
        false
    }
    async fn check_permissions(
        &self,
        _: &serde_json::Value,
        _: &ToolContext,
    ) -> PermissionDecision {
        PermissionDecision::ask("this tool always asks")
    }
    async fn prompt(&self, _: &PromptContext) -> String {
        self.description().to_string()
    }
    async fn call(
        &self,
        _input: serde_json::Value,
        _ctx: ToolContext,
        _p: ProgressSender,
    ) -> Result<ToolResult, base::error::ToolError> {
        Ok(ToolResult::text("the guarded tool ran"))
    }
}

/// Echoes its argument, so the result point has a result to work on.
#[derive(Debug)]
struct Echo;

#[async_trait::async_trait]
impl Tool for Echo {
    fn name(&self) -> &str {
        "Echo"
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
