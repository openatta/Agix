//! One task, four sets of scripts, and what is left on disk afterwards.
//!
//! Every other case here binds one or two points and looks at what reached the
//! model. That answers "does this point work" and leaves the question an
//! operator actually asks — *if I bind all of these at once, does my agent
//! still do its job?* — untouched.
//!
//! So this runs a real piece of work: read a file, fix a line, read it back.
//! Real tools, a real working directory, a scripted model for the decisions.
//! The same work runs four times:
//!
//! | profile | bound | what must be true |
//! |---|---|---|
//! | none | nothing | the task finished; this is the baseline |
//! | observers | all nine points, none changing the outcome | the directory is **byte for byte** the baseline's, and every point ran |
//! | intervening | the same, with one ring refusing edits | the directory differs in exactly the way that ring declared |
//! | broken | all nine, every one throwing | the directory is the baseline's again, and every point failed |
//!
//! # Why the ledger is not optional here
//!
//! The observers profile and the broken profile leave *the same directory* —
//! one because every script ran and changed nothing that matters, the other
//! because every script died. Nothing on disk tells them apart, and neither
//! can be told from a run with no scripts at all. The ledger is the only place
//! the difference exists, which is what makes it the load-bearing assertion
//! rather than a diagnostic.
//!
//! # Why no golden file
//!
//! The baseline is computed in the same run as the profiles compared against
//! it, so there is no file to go stale and nothing to regenerate reflexively.
//! What the task itself does is pinned by the scripted replies.

use base::interface::memory::MemoryStore;
use base::interface::scene::AgentScene;
use base::interface::script::ScriptOutcome;
use base::interface::tool::InMemoryToolRegistry;
use base::memory::{DurableMemory, MemoryType};
use std::path::Path;
use std::sync::Arc;
use test_runner::script_session::{drive, Ran, Session};
use test_runner::scripted_model::Reply;

/// The nine points, each bound to a script that takes its answer and leaves
/// the work alone.
///
/// `prompt_block_var.js` is the tenth binding and not a tenth point: a
/// variable expands nowhere unless some block mentions its placeholder, so
/// proving the variable point ran takes a block to hold it.
const OBSERVERS: &[(&str, &str, &str)] = &[
    ("prompt_assemble.js", "prompt.assemble", "onAssemble"),
    ("prompt_block.js", "prompt.block", "onBlock"),
    ("prompt_context.js", "prompt.context", "onContext"),
    ("prompt_block_var.js", "prompt.block", "onBlock"),
    ("prompt_variable.js", "prompt.variable", "onVariable"),
    ("tool_around_observe.js", "tool.around", "onAround"),
    ("tool_result.js", "tool.result", "onResult"),
    (
        "memory_retrieval.js",
        "memory.retrieval_hook",
        "onRetrieval",
    ),
    ("model_request.js", "model.request", "onRequest"),
    ("model_message.js", "model.message", "onMessage"),
];

const POINTS: &[&str] = &[
    "prompt.assemble",
    "prompt.block",
    "prompt.context",
    "prompt.variable",
    "tool.around",
    "tool.result",
    "memory.retrieval_hook",
    "model.request",
    "model.message",
];

fn fixtures() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("tests/")
        .join("fixtures")
        .join("scripts")
}

/// What the profile binds, which is the only thing that differs between runs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Profile {
    /// No scripts at all.
    None,
    /// All nine points, none of them changing what the work does.
    Observers,
    /// The same, except the ring around tool calls refuses edits.
    Intervening,
    /// All nine points, every script throwing.
    Broken,
}

/// What the session's own model is called, as the driver names it. Anything
/// else in a run's calls came from work the turn did on the side.
const MODEL: &str = "script-model";

const FILE: &str = "src/main.py";
const BEFORE: &str = "def main():\n    print(\"Hello, wrold!\")\n\n\nmain()\n";
const AFTER: &str = "def main():\n    print(\"Hello, world!\")\n\n\nmain()\n";

/// Read the file, fix the typo, read it back, report.
///
/// Written out rather than left to a model because the point of the case is
/// the *scripts*: a task that varied between profiles could not tell a script
/// that broke the work from a model that chose differently.
fn the_task(workdir: &Path) -> Vec<Reply> {
    let path = workdir.join(FILE);
    let path = path.to_string_lossy().to_string();
    vec![
        Reply::Tool {
            id: "read-1",
            name: "Read",
            input: serde_json::json!({ "file_path": path }),
        },
        Reply::Tool {
            id: "edit-1",
            name: "Edit",
            input: serde_json::json!({
                "file_path": path,
                "old_string": "wrold",
                "new_string": "world",
            }),
        },
        Reply::Tool {
            id: "read-2",
            name: "Read",
            input: serde_json::json!({ "file_path": path }),
        },
        Reply::Text("fixed the typo"),
        // Memory work reaches for the small model at the end of a turn. Its
        // answer is not part of the task, but a scripted model that ran out of
        // replies would panic on a background thread and say nothing useful.
        Reply::Text("(background)"),
    ]
}

struct Outcome {
    ran: Ran,
    /// Every file under the working directory, path and content, sorted.
    disk: String,
}

async fn run(profile: Profile) -> Outcome {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let workdir = root.join("workdir");
    std::fs::create_dir_all(workdir.join("src")).expect("src");
    std::fs::create_dir_all(workdir.join(".agix")).expect(".agix");
    std::fs::write(workdir.join(FILE), BEFORE).expect("write the file");

    let registry = Arc::new(InMemoryToolRegistry::new());
    tools::register_builtin_tools(&registry);

    let memory = Arc::new(MemoryStore::new(
        root.join("mem-user"),
        root.join("mem-local"),
    ));
    seed_memories(&memory);

    let mut session = Session::new(
        fixtures(),
        &["fix the typo in src/main.py"],
        the_task(&workdir),
    )
    .scene(Arc::new(scene::scene::coding::CodingScene) as Arc<dyn AgentScene>)
    .in_project(&workdir)
    .memory(memory);
    for tool in registry.list() {
        session = session.tool(tool);
    }

    session = match profile {
        Profile::None => session,
        Profile::Observers => bind_all(session, OBSERVERS),
        Profile::Intervening => {
            let swapped: Vec<(&str, &str, &str)> = OBSERVERS
                .iter()
                .map(|(path, point, entry)| {
                    if *point == "tool.around" {
                        ("tool_around_deny_edit.js", *point, *entry)
                    } else {
                        (*path, *point, *entry)
                    }
                })
                .collect();
            bind_all(session, &swapped)
        }
        Profile::Broken => {
            let mut s = session;
            for point in POINTS {
                s = s.bind("broken/throws.js", point, "boom");
            }
            s
        }
    };

    let ran = drive(root, session).await;
    Outcome {
        ran,
        disk: snapshot(&workdir),
    }
}

fn bind_all(mut session: Session, bindings: &[(&str, &str, &str)]) -> Session {
    for (path, point, entry) in bindings {
        session = session.bind(path, point, entry);
    }
    session
}

/// Two memories, so the recall point has something to be called about.
fn seed_memories(store: &MemoryStore) {
    let memory = |name: &str, content: &str| DurableMemory {
        name: name.to_string(),
        description: "seeded by the task profile cases".into(),
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

/// Everything the tools left behind, as one comparable string.
///
/// `.agix` is skipped: it is the project's configuration, not the work, and
/// nothing in these profiles writes to it.
fn snapshot(workdir: &Path) -> String {
    fn walk(dir: &Path, base: &Path, out: &mut Vec<(String, String)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut paths: Vec<_> = entries.flatten().map(|e| e.path()).collect();
        paths.sort();
        for path in paths {
            let relative = path.strip_prefix(base).unwrap_or(&path).to_path_buf();
            if relative.starts_with(".agix") {
                continue;
            }
            if path.is_dir() {
                walk(&path, base, out);
            } else {
                out.push((
                    relative.to_string_lossy().to_string(),
                    std::fs::read_to_string(&path).unwrap_or_else(|_| "<binary>".into()),
                ));
            }
        }
    }
    let mut files = Vec::new();
    walk(workdir, workdir, &mut files);
    files
        .into_iter()
        .map(|(path, content)| format!("{path}\n{content}"))
        .collect::<Vec<_>>()
        .join("\n----\n")
}

/// The baseline: the work gets done with no scripts anywhere.
#[tokio::test(flavor = "multi_thread")]
async fn the_task_finishes_with_nothing_bound() {
    let out = run(Profile::None).await;
    assert!(
        out.disk.contains(AFTER),
        "the task did not do its work, so every profile compared against it \
         proves nothing:\n{}",
        out.disk
    );
    assert_eq!(
        out.ran.calls_to(MODEL),
        4,
        "the task took a different path than the one it was written for"
    );
}

/// Nine points bound, and the work comes out identical.
///
/// This is the case with the widest reach: every adapter is installed at once,
/// on a real task, and any one of them quietly changing what a tool does or
/// what a turn decides shows up here as a directory that no longer matches.
#[tokio::test(flavor = "multi_thread")]
async fn observers_at_every_point_leave_the_work_untouched() {
    let baseline = run(Profile::None).await;
    let observed = run(Profile::Observers).await;

    assert_eq!(
        observed.disk, baseline.disk,
        "a script changed the work while claiming to observe it"
    );

    for point in POINTS {
        let outcomes = observed.ran.outcomes_at(point);
        assert!(
            !outcomes.is_empty(),
            "`{point}` was never called, so this profile is not what it says \
             it is"
        );
        assert!(
            outcomes.contains(&ScriptOutcome::Applied),
            "`{point}` never took its script's answer: {outcomes:?}"
        );
        assert!(
            !outcomes
                .iter()
                .any(|o| matches!(o, ScriptOutcome::Failed { .. })),
            "`{point}` failed during a run that is supposed to be clean: \
             {outcomes:?}"
        );
    }
}

/// One ring refuses edits, and that refusal is the only difference.
///
/// The point is not that a denial works — a boundary case covers that — but
/// that it costs exactly what it said it would. A profile that also lost the
/// reads, or that ended the turn early, would be an intervention with
/// consequences its author did not declare.
#[tokio::test(flavor = "multi_thread")]
async fn an_intervening_script_changes_exactly_what_it_declared() {
    let baseline = run(Profile::None).await;
    let intervened = run(Profile::Intervening).await;

    assert!(
        intervened.disk.contains(BEFORE),
        "the edit went through despite being refused:\n{}",
        intervened.disk
    );
    assert_ne!(
        intervened.disk, baseline.disk,
        "the refusal changed nothing, so the case is not set up"
    );
    assert_eq!(
        paths(&intervened.disk),
        paths(&baseline.disk),
        "the refusal cost more than the edit: the files are not the same set"
    );
    assert!(
        intervened.ran.holds("SCRIPT-TRACE-NO-EDITS"),
        "the model was not told why the edit did not happen"
    );
    assert_eq!(
        intervened.ran.calls_to(MODEL),
        4,
        "the turn took a different shape, so the refusal cost more than the \
         call it refused"
    );
}

/// Nine points bound to scripts that all throw, and the work still comes out
/// exactly as it does with nothing bound.
///
/// The directory this leaves is identical to the observers profile's, which is
/// the whole reason the ledger is asserted on: on disk, "every script ran
/// harmlessly" and "every script died" are the same run.
#[tokio::test(flavor = "multi_thread")]
async fn scripts_that_all_throw_leave_the_work_exactly_as_it_was() {
    let baseline = run(Profile::None).await;
    let broken = run(Profile::Broken).await;

    assert_eq!(
        broken.disk, baseline.disk,
        "a script that threw still changed the work"
    );

    for point in POINTS {
        let outcomes = broken.ran.outcomes_at(point);
        assert!(
            !outcomes.is_empty(),
            "`{point}` was never called, so nothing here says the adapter was \
             reached at all"
        );
        assert!(
            outcomes
                .iter()
                .all(|o| matches!(o, ScriptOutcome::Failed { .. })),
            "`{point}` recorded something other than a failure: {outcomes:?}"
        );
    }
}

/// A policy script follows the work into a sub-agent; a prompt script does
/// not.
///
/// Delegation is where a rule quietly stops applying. The model asks for a
/// sub-agent, that sub-agent builds a session of its own, and every ring the
/// operator put around tool calls is gone from it — so "no edits in this
/// project" becomes "no edits unless you ask someone else to do it", decided
/// by the model rather than by anyone.
///
/// The other half of the case is the line itself: a prompt contribution is
/// written against the prompt of the session that bound it, and a delegate has
/// its own scene and its own prompt. So the assembly mark must reach the
/// parent's request and not the delegate's.
#[tokio::test(flavor = "multi_thread")]
async fn a_delegate_inherits_the_policy_scripts_and_not_the_prompt_ones() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let workdir = root.join("workdir");
    std::fs::create_dir_all(workdir.join("src")).expect("src");
    std::fs::create_dir_all(workdir.join(".agix")).expect(".agix");
    std::fs::write(workdir.join(FILE), BEFORE).expect("write the file");
    let path = workdir.join(FILE).to_string_lossy().to_string();

    let registry = Arc::new(InMemoryToolRegistry::new());
    tools::register_builtin_tools(&registry);

    // The queue is shared, and the sub-agent's calls land inside the parent's
    // `Agent` call — so this is the order the two sessions consume it in.
    let replies = vec![
        Reply::Tool {
            id: "spawn-1",
            name: "Agent",
            input: serde_json::json!({
                "prompt": "DELEGATED-TASK: fix the typo in src/main.py",
                "subagent_type": "general-purpose",
            }),
        },
        Reply::Tool {
            id: "edit-1",
            name: "Edit",
            input: serde_json::json!({
                "file_path": path,
                "old_string": "wrold",
                "new_string": "world",
            }),
        },
        Reply::Text("I could not edit the file"),
        Reply::Text("the delegate reported back"),
        Reply::Text("(background)"),
    ];

    let mut session = Session::new(fixtures(), &["delegate the typo fix"], replies)
        .scene(Arc::new(scene::scene::coding::CodingScene) as Arc<dyn AgentScene>)
        .in_project(&workdir)
        .bind("tool_around_deny_edit.js", "tool.around", "onAround")
        .bind("prompt_assemble.js", "prompt.assemble", "onAssemble");
    for tool in registry.list() {
        session = session.tool(tool);
    }

    let ran = drive(root, session).await;

    assert!(
        std::fs::read_to_string(workdir.join(FILE))
            .expect("read the file")
            .contains("wrold"),
        "the delegate's edit went through, so the ring around tool calls did \
         not follow it"
    );

    let delegate_requests: Vec<&String> = ran
        .requests
        .iter()
        .filter(|r| r.contains("DELEGATED-TASK"))
        .collect();
    assert!(
        !delegate_requests.is_empty(),
        "no request carried the delegated task, so no sub-agent ran and this \
         case proves nothing"
    );
    assert!(
        delegate_requests
            .iter()
            .all(|r| !r.contains("SCRIPT-TRACE-ASSEMBLE")),
        "the parent's prompt pass was applied to the delegate's own prompt"
    );
    assert!(
        ran.holds("SCRIPT-TRACE-ASSEMBLE"),
        "the prompt pass did not reach the parent either, so the case is not \
         set up"
    );
}

/// The file names in a snapshot, without their contents.
fn paths(disk: &str) -> Vec<&str> {
    disk.split("\n----\n")
        .filter_map(|entry| entry.lines().next())
        .collect()
}
