//! A coordinator, three workers on different models, jobs inside each
//! worker, and an operator who breaks things and fixes them.
//!
//! The coordinator starts three background workers. Each worker starts one
//! background job-runner per job — tasks inside the worker — and waits for
//! them; a job sleeps for a random while, so they finish in no fixed order.
//! Two faults are planted and repaired from outside, the way an operator
//! would: one job's input is missing (the operator supplies it and tells that
//! worker through `session.messageTask`), and one worker's agent type names a
//! model that does not exist (the operator corrects the file and tells the
//! coordinator, which starts the worker again). The coordinator writes
//! `report.json` once every worker is done.
//!
//! What a pass proves is in the outcome, not in anything the model says: the
//! tokens in the report can only come from jobs that ran (each is derived from
//! an input file at run time), `runs.log` is written by the job scripts
//! themselves, and the proxy saw which model every request asked for.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use live::{LiveRun, Mode};
use serde_json::{json, Value};

const BROKEN_MODEL: &str = "agix-live-no-such-model";

struct Job {
    id: &'static str,
    worker: &'static str,
    secs: u64,
    nonce: String,
}

fn jobs() -> Vec<Job> {
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;
    let plan = [
        ("a1", "worker-a"),
        ("a2", "worker-a"),
        ("a3", "worker-a"),
        ("b1", "worker-b"),
        ("b2", "worker-b"),
        ("b3", "worker-b"),
        ("c1", "worker-c"),
        ("c2", "worker-c"),
    ];
    plan.iter()
        .enumerate()
        .map(|(i, (id, worker))| {
            let mix = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(i as u64 * 1442695040888963407);
            Job {
                id,
                worker,
                secs: 2 + (mix >> 33) % 14,
                nonce: format!("{:012x}", (mix >> 8) & 0xffff_ffff_ffff),
            }
        })
        .collect()
}

fn token_of(nonce: &str) -> String {
    nonce.chars().rev().collect()
}

fn job_script(id: &str, secs: u64) -> String {
    format!(
        "#!/bin/sh\n\
         cd \"$(dirname \"$0\")/..\"\n\
         echo \"{id} start $(date +%s)\" >> runs.log\n\
         sleep {secs}\n\
         if [ ! -f inputs/{id}.dat ]; then\n\
         \x20 echo \"{id} fail $(date +%s)\" >> runs.log\n\
         \x20 echo \"ERROR: missing input file inputs/{id}.dat\" >&2\n\
         \x20 exit 3\n\
         fi\n\
         mkdir -p out\n\
         tok=$(rev < inputs/{id}.dat)\n\
         echo \"$tok\" > out/{id}.txt\n\
         echo \"{id} ok $(date +%s)\" >> runs.log\n\
         echo \"RESULT {id} $tok\"\n"
    )
}

const WORKER_BODY: &str = "\
You are a worker. The message you are given lists job ids. For each job you \
run a background job-runner and collect its result.

1. For every job id, start a sub-agent with the Agent tool: subagent_type \
\"job-runner\", background true, description \"job <id>\", prompt \"Run job \
<id>.\". Start all of them before waiting on any.
2. Wait for them with the TaskWait tool (use ToolSearch to load it if it is \
not in your tool list).
3. When a job-runner reports FAILED, do not inspect or change any file and do \
not rerun the job yet. Keep waiting — for the other jobs with TaskWait, and \
once nothing else is running, by running `sleep 15` with Bash, again and \
again — until a message from the operator says the problem is fixed. Then \
start a new job-runner for that job, exactly as in step 1, and wait for it.
4. Finish only when every job has succeeded. Your final answer is one line \
per job: `<id> <token>`.
";

const RUNNER_BODY: &str = "\
You run exactly one job. Run `sh jobs/<id>.sh` with the Bash tool from the \
project root. If it exits 0, answer `OK <id> <token>`, where token is the \
word after `RESULT <id>` in its output. If it fails, answer `FAILED <id>: \
<the error line it printed>`. Do nothing else: do not read, write or fix any \
file, and do not run anything else.
";

fn agent_type(description: &str, model: &str, body: &str) -> String {
    format!("---\ndescription: {description}\nmodel: {model}\n---\n{body}")
}

/// `runs.log` as `(job, event, unix seconds)`.
fn runs(log: &str) -> Vec<(String, String, u64)> {
    log.lines()
        .filter_map(|l| {
            let mut parts = l.split_whitespace();
            Some((
                parts.next()?.to_string(),
                parts.next()?.to_string(),
                parts.next()?.parse().ok()?,
            ))
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_coordinator_sees_every_worker_through_planted_faults() -> anyhow::Result<()> {
    let mut run = LiveRun::start(
        "fleet",
        json!({ "execution": { "max_concurrent_subagents": 16 } }),
        Mode::InProcess,
    )
    .await?;
    let cfg = run.config().clone();
    let jobs = jobs();
    let models = [cfg.model_at(0), cfg.model_at(1), cfg.model_at(2)].map(str::to_string);

    for job in &jobs {
        run.write_file(
            &format!("jobs/{}.sh", job.id),
            &job_script(job.id, job.secs),
        )?;
        if job.id != "b2" {
            run.write_file(&format!("inputs/{}.dat", job.id), &job.nonce)?;
        }
    }
    let worker_desc = "Runs a list of jobs, each in its own background job-runner";
    run.write_file(
        ".agix/agents/worker-a.md",
        &agent_type(worker_desc, &models[0], WORKER_BODY),
    )?;
    run.write_file(
        ".agix/agents/worker-b.md",
        &agent_type(worker_desc, &models[1], WORKER_BODY),
    )?;
    run.write_file(
        ".agix/agents/worker-c.md",
        &agent_type(worker_desc, BROKEN_MODEL, WORKER_BODY),
    )?;
    run.write_file(
        ".agix/agents/job-runner.md",
        &agent_type("Runs exactly one job script", &models[0], RUNNER_BODY),
    )?;

    let session = run.session().await?;
    let assignment: BTreeMap<&str, Vec<&str>> = jobs.iter().fold(BTreeMap::new(), |mut m, j| {
        m.entry(j.worker).or_default().push(j.id);
        m
    });
    let roster = assignment
        .iter()
        .map(|(w, ids)| format!("- {w}: {}", ids.join(" ")))
        .collect::<Vec<_>>()
        .join("\n");
    let brief = format!(
        "You coordinate three workers. Their jobs:\n{roster}\n\n\
         1. Start each worker as a background sub-agent right away: Agent tool, \
         subagent_type = the worker's name, background true, description = the \
         worker's name, prompt = \"Run jobs <its job ids>\". Start all three before \
         waiting on any.\n\
         2. Wait for them with the TaskWait tool (use ToolSearch to load it if it \
         is not in your tool list).\n\
         3. If a worker fails, do not restart it on your own and do not touch any \
         file. Keep waiting — with TaskWait while others run, otherwise by running \
         `sleep 15` with Bash, again and again — until the operator tells you it is \
         fixed. Then start it again exactly as in step 1.\n\
         4. When all three have succeeded, write report.json in the project root: \
         {{\"jobs\": {{\"<job id>\": \"<token>\", ...}}, \"restarted\": [<names of \
         workers you started more than once>]}}, taking each token from the \
         worker's answer. Then reply DONE."
    );

    let socket = run.daemon.socket().to_path_buf();
    let mut turns: Vec<tokio::task::JoinHandle<anyhow::Result<rpc_client::TurnEvents>>> =
        Vec::new();
    let start_turn = |message: String, n: usize| {
        let socket = socket.clone();
        let session = session.clone();
        tokio::spawn(async move {
            let mut client = rpc_client::DaemonRpcClient::connect(&socket).await?;
            client
                .session_run_turn(&session, &message, &format!("fleet-main-{n}"), None)
                .await
        })
    };
    turns.push(start_turn(brief, 0));

    let mut operator = run.daemon.connect().await?;
    let deadline = Instant::now() + Duration::from_secs(40 * 60);
    let mut fixed_input = None::<Value>;
    let mut fixed_worker = false;
    let mut told_coordinator = None::<String>;
    let mut nudges = 0;

    let report = loop {
        anyhow::ensure!(
            Instant::now() < deadline,
            "the fleet did not finish in time; runs.log:\n{}",
            run.read_file("runs.log").unwrap_or_default()
        );
        let tasks = run.tasks(&session).await?;
        let of_type = |t: &&Value, ty: &str| t["detail"]["agent_type"] == ty;

        let log = run.read_file("runs.log").unwrap_or_default();
        if fixed_input.is_none() && runs(&log).iter().any(|(j, e, _)| j == "b2" && e == "fail") {
            let b2 = jobs.iter().find(|j| j.id == "b2").unwrap();
            run.write_file("inputs/b2.dat", &b2.nonce)?;
            let worker_b = tasks
                .iter()
                .rfind(|t| of_type(t, "worker-b") && t["status"] == "running")
                .and_then(|t| t["task_id"].as_str())
                .unwrap_or_default()
                .to_string();
            let resp = operator
                .call(
                    "session.messageTask",
                    json!({
                        "session_id": session,
                        "task_id": worker_b,
                        "message": "Operator: the missing input inputs/b2.dat is in place \
                                    now. Rerun job b2."
                    }),
                )
                .await?;
            let result = resp.result.unwrap_or_default();
            eprintln!("[operator] supplied inputs/b2.dat, told {worker_b}: {result}");
            fixed_input = Some(result);
        }

        let c_failed = tasks
            .iter()
            .any(|t| of_type(&t, "worker-c") && t["status"] == "failed");
        if c_failed && !fixed_worker {
            run.write_file(
                ".agix/agents/worker-c.md",
                &agent_type(worker_desc, &models[2], WORKER_BODY),
            )?;
            tokio::time::sleep(Duration::from_secs(2)).await;
            fixed_worker = true;
            let note = "Operator: worker-c's agent type has been fixed (its model name was \
                        wrong). Start worker-c again now."
                .to_string();
            let busy = turns.iter().any(|t| !t.is_finished());
            if busy {
                let resp = operator
                    .call(
                        "session.run_turn",
                        json!({
                            "session_id": session,
                            "message": note,
                            "turn_id": "fleet-operator-steer",
                            "on_busy": "steer"
                        }),
                    )
                    .await?;
                eprintln!("[operator] fixed worker-c, steered: {:?}", resp.result);
                told_coordinator = Some("steer".into());
            } else {
                turns.push(start_turn(note, turns.len()));
                told_coordinator = Some("turn".into());
                eprintln!("[operator] fixed worker-c, told the coordinator in a new turn");
            }
        }

        if turns.iter().all(|t| t.is_finished()) {
            for t in turns.drain(..) {
                let events = t.await??;
                if let Some(err) = events.response.as_ref().and_then(|r| r.error.as_ref()) {
                    anyhow::bail!("a coordinator turn failed: {}", err.message);
                }
                eprintln!("[coordinator] {}", events.text.trim());
            }
            if let Some(text) = run.read_file("report.json") {
                let running = tasks.iter().any(|t| t["status"] == "running");
                if !running {
                    break serde_json::from_str::<Value>(&text)?;
                }
            }
            nudges += 1;
            anyhow::ensure!(nudges <= 6, "the coordinator stopped short six times");
            let note = "Operator: keep coordinating — wait for the workers that are still \
                        running and finish the steps you were given, including report.json."
                .to_string();
            eprintln!("[operator] the coordinator's turn ended early; nudging ({nudges})");
            turns.push(start_turn(note, 100 + nudges));
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    };

    let tasks = run
        .wait_tasks_settled(&session, Duration::from_secs(60))
        .await?;
    let log = run.read_file("runs.log").unwrap_or_default();
    let runs = runs(&log);
    eprintln!("[fleet] report: {report}\n[fleet] runs.log:\n{log}");
    eprintln!(
        "[fleet] nudges {nudges}, worker fix told by {told_coordinator:?}, input fix {fixed_input:?}"
    );
    eprintln!(
        "[fleet] coordinator tasks: {}",
        tasks
            .iter()
            .map(|t| format!(
                "{}:{}[{}]={}",
                t["kind"].as_str().unwrap_or("?"),
                t["detail"]["agent_type"].as_str().unwrap_or("-"),
                t["detail"]["description"].as_str().unwrap_or("-"),
                t["status"].as_str().unwrap_or("?")
            ))
            .collect::<Vec<_>>()
            .join(" ")
    );

    for job in &jobs {
        assert_eq!(
            report["jobs"][job.id].as_str().map(str::trim),
            Some(token_of(&job.nonce).as_str()),
            "the report's token for {} must be the one its run produced: {report}",
            job.id
        );
        assert_eq!(
            run.read_file(&format!("out/{}.txt", job.id))
                .map(|s| s.trim().to_string()),
            Some(token_of(&job.nonce)),
            "{} never produced its output",
            job.id
        );
        assert!(
            runs.iter().any(|(j, e, _)| j == job.id && e == "ok"),
            "{} has no successful run in runs.log",
            job.id
        );
    }

    let b2: Vec<&str> = runs
        .iter()
        .filter(|(j, e, _)| j == "b2" && e != "start")
        .map(|(_, e, _)| e.as_str())
        .collect();
    assert_eq!(b2.first(), Some(&"fail"), "b2 must fail first: {b2:?}");
    assert_eq!(
        b2.last(),
        Some(&"ok"),
        "b2 must succeed after the fix: {b2:?}"
    );
    assert_eq!(
        fixed_input.as_ref().map(|r| r["delivered"].clone()),
        Some(json!(true)),
        "the operator's message must reach worker-b while it waits"
    );

    let worker_c: Vec<&str> = tasks
        .iter()
        .filter(|t| t["detail"]["agent_type"] == "worker-c")
        .map(|t| t["status"].as_str().unwrap_or("?"))
        .collect();
    assert!(
        worker_c.contains(&"failed") && worker_c.last() == Some(&"done"),
        "worker-c fails on its broken model, then succeeds once fixed: {worker_c:?}"
    );
    for worker in ["worker-a", "worker-b", "worker-c"] {
        assert!(
            tasks
                .iter()
                .any(|t| t["detail"]["agent_type"] == worker && t["status"] == "done"),
            "{worker} never finished"
        );
    }

    let exchanges = run.provider.exchanges();
    let asked: HashMap<String, Vec<u16>> = exchanges.iter().fold(HashMap::new(), |mut m, e| {
        m.entry(e.request.model().to_string())
            .or_default()
            .push(e.status);
        m
    });
    eprintln!(
        "[fleet] models asked: {:?}",
        asked
            .iter()
            .map(|(m, s)| (m.clone(), s.len()))
            .collect::<BTreeMap<_, _>>()
    );
    assert!(
        asked
            .get(BROKEN_MODEL)
            .is_some_and(|s| s.iter().all(|c| *c >= 400)),
        "the broken worker's request must reach the endpoint and be refused"
    );
    for model in &models {
        assert!(
            asked.get(model).is_some_and(|s| s.contains(&200)),
            "no successful request asked for {model}"
        );
    }

    let windows: HashMap<&str, (u64, u64)> = jobs
        .iter()
        .filter_map(|j| {
            let start = runs
                .iter()
                .filter(|(id, e, _)| id == j.id && e == "start")
                .map(|r| r.2)
                .next()?;
            let end = runs
                .iter()
                .filter(|(id, e, _)| id == j.id && e != "start")
                .map(|r| r.2)
                .next()?;
            Some((j.id, (start, end)))
        })
        .collect();
    let overlapping = jobs.iter().any(|x| {
        jobs.iter().any(|y| {
            x.worker != y.worker
                && match (windows.get(x.id), windows.get(y.id)) {
                    (Some(a), Some(b)) => a.0 < b.1 && b.0 < a.1,
                    _ => false,
                }
        })
    });
    assert!(
        overlapping,
        "jobs of different workers must run at the same time: {windows:?}"
    );

    run.finish().await;
    Ok(())
}
