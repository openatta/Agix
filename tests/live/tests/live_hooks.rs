//! Hooks with a real model: a `PreToolUse` hook that refuses a command with
//! a reason the model then acts on, one that redirects where a write lands,
//! `UserPromptSubmit` adding context the model works from, `PostToolUse` and
//! `SessionStart` seeing what happened, and the two model-backed hook kinds
//! — a `prompt` hook and an `agent` hook — whose verdicts stop a call.
//!
//! The hook scripts are Python so they can read and write the JSON payload
//! properly; each writes what it saw to a ledger next to it.

use std::path::{Path, PathBuf};

use live::{LiveRun, Mode, Purpose};
use serde_json::{json, Value};

fn nonce() -> String {
    format!(
        "{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
            % 0xffff_ffff_ffff
    )
}

fn trimmed(text: Option<String>) -> Option<String> {
    text.map(|s| s.trim().to_string())
}

struct Hooks {
    dir: tempfile::TempDir,
}

impl Hooks {
    fn new() -> anyhow::Result<Self> {
        Ok(Self {
            dir: tempfile::tempdir()?,
        })
    }

    fn ledger(&self) -> PathBuf {
        self.dir.path().join("ledger.jsonl")
    }

    /// A Python hook: `body` runs with `payload` parsed from stdin and
    /// `log(payload)` appending it to the ledger; what it prints is the
    /// hook's answer.
    fn python(&self, name: &str, body: &str) -> anyhow::Result<Value> {
        let path = self.dir.path().join(name);
        let script = format!(
            "#!/usr/bin/env python3\nimport json, sys\npayload = json.load(sys.stdin)\n\
             def log(p):\n    with open({ledger:?}, 'a') as f:\n        f.write(json.dumps(p) + '\\n')\n\
             {body}\n",
            ledger = self.ledger().display().to_string(),
        );
        write_executable(&path, &script)?;
        Ok(json!({ "type": "command", "command": path, "timeout": 30000 }))
    }

    fn entries(&self, event: &str) -> Vec<Value> {
        std::fs::read_to_string(self.ledger())
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|e| e["hook_event_name"] == event)
            .collect()
    }
}

fn write_executable(path: &Path, body: &str) -> anyhow::Result<()> {
    std::fs::write(path, body)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pre_tool_use_hook_refuses_rm_and_the_model_follows_its_reason() -> anyhow::Result<()> {
    let hooks = Hooks::new()?;
    let guard = hooks.python(
        "no-rm.py",
        "log(payload)\n\
         cmd = (payload.get('tool_input') or {}).get('command', '')\n\
         if payload.get('tool_name') == 'Bash' and 'rm ' in cmd:\n    \
             sys.stderr.write('rm is not allowed in this project: move the file into trash/ with mv instead')\n    \
             sys.exit(2)\n\
         print('{}')",
    )?;
    let mut run = LiveRun::start(
        "hook-guard",
        json!({ "hooks_config": { "PreToolUse": [guard] } }),
        Mode::InProcess,
    )
    .await?;
    let token = format!("TOKEN-{}", nonce());
    run.write_file("old.log", &format!("{token}\n"))?;
    let session = run.session().await?;
    let events = run
        .turn(
            &session,
            "Delete old.log by running `rm old.log` with the Bash tool. If that is blocked, do \
             exactly what the block message says instead, with Bash. Reply DONE.",
        )
        .await?;
    assert!(
        events
            .tool_results
            .iter()
            .any(|(n, out, err)| n == "Bash" && *err && out.contains("not allowed")),
        "{:?}",
        events.tool_results
    );
    assert!(run.read_file("old.log").is_none());
    assert_eq!(
        trimmed(run.read_file("trash/old.log")),
        Some(token),
        "the model did what the hook said instead"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pre_tool_use_hook_redirects_a_write_into_drafts() -> anyhow::Result<()> {
    let hooks = Hooks::new()?;
    let redirect = hooks.python(
        "drafts.py",
        "import os\nlog(payload)\n\
         inp = dict(payload.get('tool_input') or {})\n\
         if payload.get('tool_name') == 'Write' and 'drafts' not in inp.get('file_path', ''):\n    \
             inp['file_path'] = os.path.join('drafts', os.path.basename(inp['file_path']))\n    \
             print(json.dumps({'updated_input': inp}))\n\
         else:\n    print('{}')",
    )?;
    let mut run = LiveRun::start(
        "hook-redirect",
        json!({ "hooks_config": { "PreToolUse": [redirect] } }),
        Mode::InProcess,
    )
    .await?;
    let token = format!("TOKEN-{}", nonce());
    let session = run.session().await?;
    run.turn(
        &session,
        &format!(
            "Write the text {token} to report.txt with the Write tool, once. Do not check where \
             it went and do not write it again. Reply DONE."
        ),
    )
    .await?;
    assert!(run.read_file("report.txt").is_none());
    assert_eq!(trimmed(run.read_file("drafts/report.txt")), Some(token));
    let seen = hooks.entries("PreToolUse");
    assert!(
        seen.iter().any(|e| e["tool_name"] == "Write"
            && e["tool_input"]["file_path"]
                .as_str()
                .is_some_and(|p| p.ends_with("report.txt") && !p.contains("drafts"))),
        "the hook saw the model's own path: {seen:?}"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn hooks_see_the_session_start_add_context_to_a_prompt_and_see_results() -> anyhow::Result<()>
{
    let hooks = Hooks::new()?;
    let build = format!("BUILD-{}", nonce());
    let started = hooks.python("start.py", "log(payload)\nprint('{}')")?;
    let context = hooks.python(
        "context.py",
        &format!(
            "log(payload)\n\
             prompt = payload.get('user_prompt', '')\n\
             print(json.dumps({{'updated_input': prompt + '\\n\\n(Added by the project hook: \
             the current build id is {build}.)'}}))"
        ),
    )?;
    let observed = hooks.python("observe.py", "log(payload)\nprint('{}')")?;
    let mut run = LiveRun::start(
        "hook-observe",
        json!({
            "hooks_config": {
                "SessionStart": [started],
                "UserPromptSubmit": [context],
                "PostToolUse": [observed]
            }
        }),
        Mode::InProcess,
    )
    .await?;
    let token = format!("TOKEN-{}", nonce());
    run.write_file("input.txt", &format!("{token}\n"))?;
    let session = run.session().await?;
    run.turn(
        &session,
        "1. Read input.txt with the Read tool.\n2. Write the current build id, exactly as \
         given in this message, to build-id.txt with the Write tool.\nReply DONE.",
    )
    .await?;

    assert_eq!(hooks.entries("SessionStart").len(), 1);
    assert_eq!(
        hooks.entries("SessionStart")[0]["session_id"],
        session.as_str()
    );
    assert_eq!(
        trimmed(run.read_file("build-id.txt")),
        Some(build),
        "the model worked from the context the hook added"
    );
    let results = hooks.entries("PostToolUse");
    assert!(
        results.iter().any(|e| e["tool_name"] == "Read"
            && e["tool_result"]
                .as_str()
                .is_some_and(|r| r.contains(&token))),
        "PostToolUse saw what Read returned: {results:?}"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "product gap: a Stop hook can only end the turn; `decision: block` is ignored instead of sending the model back to work"]
async fn a_stop_hook_sends_the_model_back_to_finish_the_job() -> anyhow::Result<()> {
    let hooks = Hooks::new()?;
    let marker = hooks.dir.path().join("sent-back");
    let stop = hooks.python(
        "stop.py",
        &format!(
            "import os\nlog(payload)\n\
             if os.path.exists({m:?}):\n    print('{{}}')\n\
             else:\n    open({m:?}, 'w').close()\n    \
             print(json.dumps({{'decision': 'block', 'message': 'Not finished: write the word \
             CHECKED to checked.txt with the Write tool first.'}}))",
            m = marker.display().to_string()
        ),
    )?;
    let mut run = LiveRun::start(
        "hook-stop",
        json!({ "hooks_config": { "Stop": [stop] } }),
        Mode::InProcess,
    )
    .await?;
    let session = run.session().await?;
    run.turn(&session, "Reply with the single word READY.")
        .await?;
    assert_eq!(
        trimmed(run.read_file("checked.txt")),
        Some("CHECKED".into())
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_prompt_hook_judges_a_call_with_a_model_and_blocks_it() -> anyhow::Result<()> {
    let mut run = LiveRun::start(
        "hook-prompt",
        json!({
            "hooks_config": {
                "PreToolUse": [{
                    "type": "prompt",
                    "prompt": "You are SAFETY-REVIEWER. The user message is a JSON description \
                               of a tool call about to run. If it is a Bash command that deletes \
                               files (rm), reply with exactly {\"decision\": \"block\", \
                               \"message\": \"deleting files needs a human\"}. Otherwise reply \
                               with exactly {}. Reply with the JSON only.",
                    "timeout": 120000
                }]
            }
        }),
        Mode::InProcess,
    )
    .await?;
    let token = format!("TOKEN-{}", nonce());
    run.write_file("precious.txt", &format!("{token}\n"))?;
    let session = run.session().await?;
    let events = run
        .turn(
            &session,
            "1. Run `cat precious.txt` with the Bash tool.\n2. Run `rm precious.txt` with the \
             Bash tool. If it is blocked, do not try any other way.\nReply DONE.",
        )
        .await?;
    assert_eq!(trimmed(run.read_file("precious.txt")), Some(token.clone()));
    assert!(
        events
            .tool_results
            .iter()
            .any(|(n, out, _)| n == "Bash" && out.contains(&token)),
        "the harmless call was let through: {:?}",
        events.tool_results
    );
    assert!(
        events
            .tool_results
            .iter()
            .any(|(n, out, err)| n == "Bash" && *err && out.contains("needs a human")),
        "{:?}",
        events.tool_results
    );
    let judged = run
        .provider
        .calls()
        .into_iter()
        .filter(|c| c.system_contains("SAFETY-REVIEWER"))
        .count();
    assert!(
        judged >= 2,
        "one tool-less judgement per call, on the model: {judged}"
    );
    assert!(run
        .provider
        .calls()
        .iter()
        .filter(|c| c.system_contains("SAFETY-REVIEWER"))
        .all(|c| c.purpose() != Purpose::Turn));
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn an_agent_hook_runs_a_sub_agent_that_blocks_a_call() -> anyhow::Result<()> {
    let mut run = LiveRun::start(
        "hook-agent",
        json!({
            "hooks_config": {
                "PreToolUse": [{
                    "type": "agent",
                    "prompt": "You are GATEKEEPER. Below is a JSON description of a tool call. \
                               Do not use any tool. If the call writes to a file whose name \
                               starts with frozen, answer with exactly {\"ok\": false, \"reason\": \
                               \"frozen files are read-only\"}; otherwise answer with exactly \
                               {\"ok\": true, \"reason\": \"fine\"}. Answer with the JSON only.",
                    "timeout": 180000
                }]
            }
        }),
        Mode::InProcess,
    )
    .await?;
    let token = format!("TOKEN-{}", nonce());
    let session = run.session().await?;
    let events = run
        .turn(
            &session,
            &format!(
                "Write the text {token} to frozen-config.txt with the Write tool. If it is \
                 blocked, write it to notes.txt with the Write tool instead. Reply DONE."
            ),
        )
        .await?;
    assert!(
        run.read_file("frozen-config.txt").is_none(),
        "{:?}",
        events.tool_results
    );
    assert!(
        events
            .tool_results
            .iter()
            .any(|(n, out, err)| n == "Write" && *err && out.contains("read-only")),
        "{:?}",
        events.tool_results
    );
    assert_eq!(trimmed(run.read_file("notes.txt")), Some(token));
    assert!(
        run.provider
            .calls()
            .iter()
            .any(|c| c.messages_contain("GATEKEEPER")),
        "the gatekeeper ran as an agent on the model"
    );
    run.finish().await;
    Ok(())
}
