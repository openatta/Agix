//! Hook execution methods: command, prompt, HTTP.
//!
//! These are `impl HookRunner` blocks extending the runner with execution logic.
//! Separated from `mod.rs` to keep the struct definition and dispatch clean.

use super::{HookOutcome, HookRunner};
use crate::payload::{HookDecision, HookInput, HookResponse};

use base::interface::exec::{
    ExecError, OutputChunk, OutputStream, Process, ProcessSpec, OUTPUT_GRACE_AFTER_EXIT,
};
use futures::StreamExt;
use std::path::Path;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

impl HookRunner {
    /// Prompt hook：调注入的 PromptHookExecutor，把模型返回的 JSON 解析为
    /// HookResponse。无 executor 时 Skipped + actionable 错误信息。
    pub(super) async fn exec_prompt(
        &self,
        prompt: &str,
        model: Option<&str>,
        input: &HookInput,
        timeout_dur: Duration,
    ) -> HookOutcome {
        let Some(executor) = &self.prompt_executor else {
            return HookOutcome::Skipped(super::NO_PROMPT_EXECUTOR);
        };
        let result =
            match tokio::time::timeout(timeout_dur, executor.execute(prompt, model, input)).await {
                Ok(Ok(text)) => text,
                Ok(Err(e)) => {
                    return HookOutcome::Error(format!("prompt hook executor: {e}"));
                }
                Err(_) => {
                    return HookOutcome::Error(format!(
                        "prompt hook timed out after {}ms",
                        timeout_dur.as_millis()
                    ));
                }
            };
        let (response, failure) = parse_hook_response_lenient(&result);
        HookOutcome::Ran {
            response,
            failure: failure.map(|e| format!("prompt hook answer is not a JSON answer: {e}")),
            stdout: result,
            stderr: String::new(),
            exit_code: Some(0),
        }
    }

    /// HTTP hook：POST JSON payload 到 url，期望返回 JSON HookResponse。
    /// 自定义 headers 加进 request；status >= 400 → Error；body 解析失败 → 默认空 response。
    ///
    /// 每一跳都不许落到本机或内网地址（`HttpRequest::public_only`）：url 写在
    /// 一份仓库也能带来的 settings.json 里。`allow_local` 只放开 url 自己那个
    /// 主机与端口。
    pub(super) async fn exec_http(
        &self,
        url: &str,
        headers: &std::collections::HashMap<String, String>,
        allow_local: bool,
        input: &HookInput,
        timeout_dur: Duration,
    ) -> HookOutcome {
        let payload = match serde_json::to_vec(input) {
            Ok(p) => p,
            Err(e) => return HookOutcome::Error(format!("http hook payload: {e}")),
        };
        let req = base::interface::exec::HttpRequest::post(url)
            .header("content-type", "application/json")
            .body(payload);
        let mut req = if allow_local {
            req.public_only_but_target()
        } else {
            req.public_only()
        };
        for (k, v) in headers {
            req = req.header(k, v);
        }
        // `Origin::Operator`: an HTTP hook endpoint is written in settings.json
        // by whoever runs this engine, not chosen by the model.
        let sending = self
            .net()
            .send(req, base::interface::exec::Origin::Operator);
        let resp = match tokio::time::timeout(timeout_dur, sending).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => return HookOutcome::Error(format!("http hook send: {e}")),
            Err(_elapsed) => {
                return HookOutcome::Error(format!(
                    "http hook send: no response within {}ms",
                    timeout_dur.as_millis()
                ))
            }
        };
        let status = resp.status;
        let body = resp.body_text();
        if !(200..300).contains(&status) {
            return HookOutcome::Error(format!("http hook {status}: {}", truncate(&body, 500)));
        }
        let (response, failure) = parse_hook_response_lenient(&body);
        HookOutcome::Ran {
            response,
            failure: failure.map(|e| format!("http hook body is not a JSON answer: {e}")),
            stdout: body,
            stderr: String::new(),
            exit_code: Some(0),
        }
    }

    pub(super) async fn exec_command(
        &self,
        shell: &str,
        command: &str,
        input: &HookInput,
        timeout: Duration,
    ) -> HookOutcome {
        let cwd = Some(Path::new(&input.cwd))
            .filter(|dir| !input.cwd.is_empty() && dir.is_dir())
            .unwrap_or(Path::new("."));
        if self.project_untrusted {
            if let Some(found) = project_supplied(shell, command, cwd) {
                return HookOutcome::Error(found.refusal());
            }
        }
        let mut spec = ProcessSpec::new(shell, cwd).args(["-c", command]);

        // Let `command` reference a bare script name (e.g. `check.sh`) that
        // lives in `.agix/hooks/` by prepending the configured search dirs
        // to this subprocess's PATH — normal shell command lookup does the
        // rest, no custom resolution syntax needed.
        if !self.hooks_search_dirs.is_empty() || self.project_untrusted {
            let existing = std::env::var_os("PATH").unwrap_or_default();
            let mut search: Vec<std::path::PathBuf> = self.hooks_search_dirs.clone();
            search.extend(
                std::env::split_paths(&existing)
                    .filter(|dir| !self.project_untrusted || dir.is_absolute()),
            );
            if let Ok(joined) = std::env::join_paths(search) {
                spec.env
                    .push(("PATH".into(), joined.to_string_lossy().into_owned()));
            }
        }

        match serde_json::to_vec(input) {
            Ok(mut payload) => {
                payload.push(b'\n');
                spec.stdin = Some(payload);
            }
            Err(e) => {
                warn!("hook input serialize failed: {e}");
                spec.stdin = Some(Vec::new());
            }
        }

        let mut child = match self.process.spawn(spec, CancellationToken::new()).await {
            Ok(c) => c,
            Err(e) => return HookOutcome::Error(format!("hook spawn failed: {e}")),
        };
        #[allow(clippy::expect_used)]
        let output = child.output().expect("the stream is taken once");
        let stop_reading = CancellationToken::new();
        let mut collect = tokio::spawn(collect_output(output, stop_reading.clone()));

        let status = tokio::select! {
            _ = tokio::time::sleep(timeout) => {
                child.kill().await;
                stop_reading.cancel();
                return HookOutcome::Error(format!("hook timed out after {:?}", timeout));
            }
            r = child.wait() => match r {
                Ok(s) => s,
                Err(e) => {
                    stop_reading.cancel();
                    return HookOutcome::Error(format!("hook wait failed: {e}"));
                }
            },
        };
        let (stdout, stderr) = tokio::select! {
            r = &mut collect => r.unwrap_or_default(),
            _ = tokio::time::sleep(OUTPUT_GRACE_AFTER_EXIT) => {
                child.kill().await;
                stop_reading.cancel();
                (&mut collect).await.unwrap_or_default()
            }
        };
        let exit_code = status.code;
        let (response, failure) = response_for_exit(command, exit_code, &stdout, &stderr);

        HookOutcome::Ran {
            response,
            stdout,
            stderr,
            exit_code,
            failure,
        }
    }
}

/// What a command hook would take from the project it runs in.
#[derive(Debug, PartialEq, Eq)]
enum ProjectSupplied {
    /// A program started by a path relative to where the hook runs: `shell`
    /// itself, or a command's program (`./x`, `bin/x`, after `cd`, inside
    /// `sh -c`, behind `env` and the other wrappers).
    Program(String),
    /// A word naming a file that is there under the hook's working directory
    /// or a directory a `cd` in the command goes to: the script an interpreter
    /// runs (`python3 x.py`, `awk -f x.awk`, `. env.sh`), an option's value
    /// (`--require=./x.js`), what `<` feeds in.
    File(String),
}

impl ProjectSupplied {
    fn refusal(&self) -> String {
        match self {
            Self::Program(word) => format!(
                "hook program `{word}` is a relative path and the project is not trusted: \
                 it would run the project's file of that name; write an absolute path, \
                 or trust the project"
            ),
            Self::File(word) => format!(
                "hook command names the project's file `{word}` and the project is not \
                 trusted: the hook would run or read the project's copy; write an absolute \
                 path, or trust the project"
            ),
        }
    }
}

/// Reads `command` as the hook `if` reads it. Which interpreter takes which
/// argument as code is not guessed: any argument that is a file in the
/// project counts. A program spelled through a variable is not judged here.
fn project_supplied(shell: &str, command: &str, cwd: &Path) -> Option<ProjectSupplied> {
    use permissions::invocation::{program_name, traced_invocations, Invocation};
    if shell.contains('/') && is_relative(shell) {
        return Some(ProjectSupplied::Program(shell.to_string()));
    }
    let mut argvs = Vec::new();
    // `segments` drops a command's leading `NAME=value` words.
    let mut operands: Vec<String> = permissions::shell::lex(command)
        .iter()
        .filter(|word| permissions::shell::is_assignment(&word.text))
        .flat_map(|word| word.text.split_whitespace())
        .map(str::to_string)
        .collect();
    for segment in permissions::shell::segments(command) {
        let words = permissions::shell::lex(&segment.source);
        operands.extend(read_from(&words));
        for traced in traced_invocations(&segment.source) {
            operands.extend(
                traced
                    .assignments
                    .iter()
                    .flat_map(|assignment| assignment.split_whitespace())
                    .map(str::to_string),
            );
            match traced.invocation {
                Invocation::Runs(argv) => argvs.push(argv),
                Invocation::Unknown => operands.extend(
                    without_redirections(&words)
                        .skip(1)
                        .map(|word| word.text.clone()),
                ),
            }
        }
    }
    if let Some(program) = argvs
        .iter()
        .filter_map(|argv| argv.first())
        .find(|program| program.contains('/') && is_relative(program))
    {
        return Some(ProjectSupplied::Program(program.clone()));
    }

    let mut bases = vec![cwd.to_path_buf()];
    for argv in &argvs {
        if !matches!(program_name(&argv[0]), "cd" | "pushd") {
            continue;
        }
        let Some(dir) = argv[1..].iter().find(|arg| !arg.starts_with('-')) else {
            continue;
        };
        if is_relative(dir) {
            let reached: Vec<_> = bases.iter().map(|base| base.join(dir)).collect();
            bases.extend(reached);
            bases.truncate(MAX_CD_BASES);
        } else if Path::new(dir).starts_with(cwd) {
            bases.push(dir.into());
        }
    }

    argvs
        .iter()
        .flat_map(|argv| argv.iter().skip(1))
        .chain(&operands)
        .find(|word| {
            paths_in(word).any(|path| {
                !path.is_empty()
                    && is_relative(path)
                    && bases.iter().any(|base| base.join(path).is_file())
            })
        })
        .map(|word| ProjectSupplied::File(word.clone()))
}

const MAX_CD_BASES: usize = 32;

/// The paths a word may name: itself, and what follows each `=` of an
/// `--opt=value` or `NAME=--opt=value`. A short option's attached value
/// (`-r./x`) is not split off.
fn paths_in(word: &str) -> impl Iterator<Item = &str> {
    let whole = (!word.starts_with('-')).then_some(word);
    let values = word.match_indices('=').map(|(at, _)| &word[at + 1..]);
    whole.into_iter().chain(values)
}

fn without_redirections(
    words: &[permissions::shell::Word],
) -> impl Iterator<Item = &permissions::shell::Word> {
    let mut target_next = false;
    words.iter().filter(move |word| {
        if word.redirect {
            target_next = true;
            false
        } else {
            !std::mem::take(&mut target_next)
        }
    })
}

/// The files a simple command's `<` / `<>` redirections read. What `>`
/// writes to is not handed to anything, and a here-doc's delimiter or a
/// here-string is not a file.
fn read_from(words: &[permissions::shell::Word]) -> Vec<String> {
    words
        .windows(2)
        .filter(|pair| {
            pair[0].redirect
                && matches!(
                    pair[0]
                        .text
                        .trim_start_matches(|c: char| c.is_ascii_digit()),
                    "<" | "<>"
                )
        })
        .map(|pair| pair[1].text.clone())
        .collect()
}

fn is_relative(word: &str) -> bool {
    !word.starts_with('/') && !word.starts_with('~')
}

/// The exit code a command hook blocks with, whatever it printed.
const BLOCKING_EXIT: i32 = 2;

/// What a command hook's exit says, and why it failed when it did.
///
/// `2` blocks, with stderr as the reason (stdout when stderr is empty); its
/// stdout is not read as JSON. `0` hands stdout over as the JSON answer. Any
/// other exit, a signal included, is the hook failing: it lets the call
/// through, as a hook that crashed or timed out does.
fn response_for_exit(
    command: &str,
    exit_code: Option<i32>,
    stdout: &str,
    stderr: &str,
) -> (HookResponse, Option<String>) {
    match exit_code {
        Some(0) if stdout.trim().is_empty() => (HookResponse::default(), None),
        Some(0) => match serde_json::from_str::<HookResponse>(stdout.trim()) {
            Ok(response) => (response, None),
            Err(e) => {
                warn!(command, stdout = %stdout, error = %e, "hook stdout is not a JSON answer; it is ignored");
                (
                    HookResponse::default(),
                    Some(format!(
                        "stdout is not a JSON answer ({e}): {}",
                        truncate(stdout.trim(), 200)
                    )),
                )
            }
        },
        Some(BLOCKING_EXIT) => {
            let reason = [stderr.trim(), stdout.trim()]
                .into_iter()
                .find(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("hook exited {BLOCKING_EXIT}"));
            debug!(command, %reason, "hook exited 2; blocking");
            (
                HookResponse {
                    decision: Some(HookDecision::Block),
                    message: Some(reason),
                    ..Default::default()
                },
                None,
            )
        }
        other => {
            warn!(
                command,
                exit_code = ?other,
                stderr = %stderr.trim(),
                "hook exited neither 0 nor 2; it neither blocks nor rewrites anything"
            );
            let exit = match other {
                Some(code) => format!("exited {code}"),
                None => "was killed by a signal".to_string(),
            };
            let stderr = stderr.trim();
            let failure = if stderr.is_empty() {
                exit
            } else {
                format!("{exit}: {}", truncate(stderr, 200))
            };
            (HookResponse::default(), Some(failure))
        }
    }
}

async fn collect_output(
    mut output: futures::stream::BoxStream<'static, Result<OutputChunk, ExecError>>,
    stop: CancellationToken,
) -> (String, String) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    loop {
        let next = tokio::select! {
            biased;
            _ = stop.cancelled() => None,
            n = output.next() => n,
        };
        let Some(Ok(chunk)) = next else { break };
        match chunk.stream {
            OutputStream::Stdout => out.extend(chunk.bytes),
            OutputStream::Stderr => err.extend(chunk.bytes),
        }
    }
    (
        String::from_utf8_lossy(&out).into_owned(),
        String::from_utf8_lossy(&err).into_owned(),
    )
}

/// 解析 hook 响应文本为 HookResponse。模型可能 wrap 在 ``` ```json``` 里；
/// 空文本是什么都不说；解析失败时返回 default response 与失败原因。
fn parse_hook_response_lenient(s: &str) -> (HookResponse, Option<String>) {
    let trimmed = s.trim();
    let candidate = if let Some(rest) = trimmed.strip_prefix("```json") {
        rest.trim_start_matches('\n').trim_end_matches("```").trim()
    } else if let Some(rest) = trimmed.strip_prefix("```") {
        rest.trim_start_matches('\n').trim_end_matches("```").trim()
    } else {
        trimmed
    };
    if candidate.is_empty() {
        return (HookResponse::default(), None);
    }
    match serde_json::from_str(candidate) {
        Ok(response) => (response, None),
        Err(e) => (
            HookResponse::default(),
            Some(format!("{e}: {}", truncate(candidate, 200))),
        ),
    }
}

pub(super) fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{head}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project_with(files: &[&str]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agix-hooks-project-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        for file in files {
            let path = dir.join(file);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap_or_default();
            }
            std::fs::write(path, "").unwrap_or_default();
        }
        dir
    }

    #[test]
    fn relative_programs_are_told_apart_from_absolute_and_looked_up_ones() {
        let project = project_with(&["x.sh", "env.sh", "check.sh"]);
        for (shell, command, word) in [
            ("bash", "./x.sh", "./x.sh"),
            ("bash", "ls && bin/x", "bin/x"),
            ("bash", "exec ../x", "../x"),
            ("bash", "sudo -u me ./x", "./x"),
            ("bash", "sh -c 'cd sub; ./y'", "./y"),
            ("./my-shell", "true", "./my-shell"),
        ] {
            assert_eq!(
                project_supplied(shell, command, &project),
                Some(ProjectSupplied::Program(word.into())),
                "`{command}`"
            );
        }
        for command in [
            "/opt/x.sh",
            "~/bin/x.sh",
            "check.sh",
            "echo ./x",
            "bash -c 'echo hi'",
            "bash -s < /dev/null",
            "sh /etc/x.sh",
            "$HOME/bin/x.sh",
            "python3 -m pytest",
        ] {
            assert_eq!(
                project_supplied("bash", command, &project),
                None,
                "`{command}`"
            );
        }
        let _ = std::fs::remove_dir_all(&project);
    }

    #[test]
    fn any_argument_that_is_a_file_in_the_project_is_the_projects() {
        let project = project_with(&[
            "x.sh",
            "env.sh",
            "scripts/x.py",
            "scripts/hook.js",
            "sub/deeper/y.rb",
        ]);
        for (command, word) in [
            ("python3 scripts/x.py", "scripts/x.py"),
            (
                "python3 -X dev -W ignore scripts/x.py --fix",
                "scripts/x.py",
            ),
            ("node scripts/hook.js", "scripts/hook.js"),
            ("deno run -A scripts/hook.js", "scripts/hook.js"),
            ("awk -f x.sh", "x.sh"),
            ("zsh -o pipefail x.sh", "x.sh"),
            ("sh -- x.sh", "x.sh"),
            ("source env.sh", "env.sh"),
            ("env A=1 timeout 5 perl x.sh", "x.sh"),
            (
                "node --require=./scripts/hook.js -e 0",
                "--require=./scripts/hook.js",
            ),
            (
                "PYTHONSTARTUP=scripts/x.py python3 -i",
                "PYTHONSTARTUP=scripts/x.py",
            ),
            (
                "NODE_OPTIONS='--require ./scripts/hook.js' node -e 0",
                "./scripts/hook.js",
            ),
            ("cat x.sh | sh", "x.sh"),
            ("sh < x.sh", "x.sh"),
            ("python3 0<scripts/x.py", "scripts/x.py"),
            ("cd scripts && python3 x.py", "x.py"),
            ("cd sub; cd deeper; ruby y.rb", "y.rb"),
            ("sh -c 'cd scripts; node hook.js'", "hook.js"),
            ("$RUNNER scripts/x.py", "scripts/x.py"),
            ("echo $(cat x.sh)", "x.sh"),
        ] {
            assert_eq!(
                project_supplied("bash", command, &project),
                Some(ProjectSupplied::File(word.into())),
                "`{command}`"
            );
        }
        for command in [
            "python3 scripts/missing.py",
            "python3 --version",
            "python3 -m pytest -q",
            "node -e 0",
            "true --flag -- . .. scripts sub/deeper",
            "python3 /opt/x.py",
            "python3 ~/x.py",
            "echo done >> x.sh",
            "echo done > env.sh 2>&1",
            "check.sh x",
        ] {
            assert_eq!(
                project_supplied("bash", command, &project),
                None,
                "`{command}`"
            );
        }
        let _ = std::fs::remove_dir_all(&project);
    }

    #[test]
    fn parse_hook_response_handles_plain_json() {
        let (r, failure) = parse_hook_response_lenient(r#"{"decision":"block"}"#);
        assert_eq!(failure, None);
        assert_eq!(r.decision, Some(HookDecision::Block));
    }

    #[test]
    fn parse_hook_response_handles_markdown_block() {
        let (r, _) = parse_hook_response_lenient("```json\n{\"continue\":false}\n```");
        assert_eq!(r.r#continue, Some(false));
    }

    #[test]
    fn parse_hook_response_garbage_returns_default() {
        let (r, failure) = parse_hook_response_lenient("not json at all{{{");
        assert_eq!(r.decision, None);
        assert_eq!(r.r#continue, None);
        assert!(failure.is_some());
    }

    #[test]
    fn truncate_short_string() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_long_string() {
        let result = truncate("hello world this is a long string", 10);
        assert!(result.starts_with("hello worl"));
        assert!(result.ends_with('…'));
    }

    #[test]
    fn parse_hook_response_handles_markdown_without_lang() {
        let (r, _) = parse_hook_response_lenient("```\n{\"decision\":\"approve\"}\n```");
        assert_eq!(r.decision, Some(HookDecision::Approve));
    }

    #[test]
    fn parse_hook_response_empty_string_returns_default() {
        let (r, failure) = parse_hook_response_lenient("");
        assert_eq!(r.decision, None);
        assert_eq!(failure, None, "saying nothing is not a failure");
    }

    #[test]
    fn parse_hook_response_whitespace_only() {
        let (r, failure) = parse_hook_response_lenient("   \n  ");
        assert_eq!(r.decision, None);
        assert_eq!(failure, None);
    }

    #[test]
    fn exit_2_blocks_whatever_stdout_says() {
        for stdout in ["", "not json {{", r#"{"decision":"approve"}"#] {
            let (r, failure) = response_for_exit("h", Some(2), stdout, "NO-RM\n");
            assert_eq!(failure, None);
            assert_eq!(r.decision, Some(HookDecision::Block), "stdout {stdout:?}");
            assert_eq!(r.message.as_deref(), Some("NO-RM"));
        }
    }

    #[test]
    fn exit_2_without_stderr_gives_stdout_as_the_reason() {
        let (r, _) = response_for_exit("h", Some(2), "plain words\n", "");
        assert_eq!(r.decision, Some(HookDecision::Block));
        assert_eq!(r.message.as_deref(), Some("plain words"));
        let (r, _) = response_for_exit("h", Some(2), "", "");
        assert_eq!(r.message.as_deref(), Some("hook exited 2"));
    }

    #[test]
    fn any_other_failure_lets_the_call_through() {
        for code in [Some(1), Some(127), None] {
            for stdout in ["", "oops", r#"{"decision":"block"}"#] {
                let (r, failure) = response_for_exit("h", code, stdout, "boom");
                assert_eq!(r.decision, None, "{code:?} {stdout:?}");
                assert_eq!(r.r#continue, None);
                assert!(failure.is_some_and(|f| f.contains("boom")), "{code:?}");
            }
        }
    }

    #[test]
    fn exit_0_reads_stdout_as_the_answer() {
        let (r, failure) =
            response_for_exit("h", Some(0), r#"{"decision":"block","message":"m"}"#, "");
        assert_eq!(r.decision, Some(HookDecision::Block));
        assert_eq!(failure, None);
        let (r, failure) = response_for_exit("h", Some(0), "", "x");
        assert_eq!((r.decision, failure), (None, None));
        let (r, failure) = response_for_exit("h", Some(0), "garbage", "");
        assert_eq!(r.decision, None);
        assert!(failure.is_some_and(|f| f.contains("garbage")));
    }
}
