//! Hook execution methods: command, prompt, HTTP.
//!
//! These are `impl HookRunner` blocks extending the runner with execution logic.
//! Separated from `mod.rs` to keep the struct definition and dispatch clean.

use super::ssrf::ssrf_check_url;
use super::{HookOutcome, HookRunner};
use crate::payload::{HookDecision, HookInput, HookResponse};

use base::interface::exec::{
    ExecError, OutputChunk, OutputStream, Process, ProcessSpec, OUTPUT_GRACE_AFTER_EXIT,
};
use futures::StreamExt;
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
        timeout_ms: Option<u64>,
    ) -> HookOutcome {
        let Some(executor) = &self.prompt_executor else {
            return HookOutcome::Skipped(
                "prompt-type hook needs a PromptHookExecutor injected (CLI does this for Auto mode)",
            );
        };
        let timeout_dur = timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(self.default_timeout);
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
        // 模型可能用 markdown 包 JSON；尽力解
        let response = parse_hook_response_lenient(&result);
        HookOutcome::Ran {
            response,
            stdout: result,
            stderr: String::new(),
            exit_code: Some(0),
        }
    }

    /// HTTP hook：POST JSON payload 到 url，期望返回 JSON HookResponse。
    /// 自定义 headers 加进 request；status >= 400 → Error；body 解析失败 → 默认空 response。
    ///
    /// **SSRF guard**: rejects requests to private/reserved IP ranges (127.0.0.0/8,
    /// 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16, ::1) before connecting.
    pub(super) async fn exec_http(
        &self,
        url: &str,
        headers: &std::collections::HashMap<String, String>,
        input: &HookInput,
        timeout_ms: Option<u64>,
    ) -> HookOutcome {
        // SSRF guard: block private/reserved IPs before connecting
        if let Err(e) = ssrf_check_url(url).await {
            return HookOutcome::Error(e.to_string());
        }

        let timeout_dur = timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(self.default_timeout);
        let payload = match serde_json::to_vec(input) {
            Ok(p) => p,
            Err(e) => return HookOutcome::Error(format!("http hook payload: {e}")),
        };
        let mut req = base::interface::exec::HttpRequest::post(url)
            .header("content-type", "application/json")
            .body(payload);
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
        let response = parse_hook_response_lenient(&body);
        HookOutcome::Ran {
            response,
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
        let mut spec = ProcessSpec::new(shell, ".").args(["-c", command]);

        // Let `command` reference a bare script name (e.g. `check.sh`) that
        // lives in `.atta/hooks/` by prepending the configured search dirs
        // to this subprocess's PATH — normal shell command lookup does the
        // rest, no custom resolution syntax needed.
        if !self.hooks_search_dirs.is_empty() {
            let existing = std::env::var_os("PATH").unwrap_or_default();
            let mut search: Vec<std::path::PathBuf> = self.hooks_search_dirs.clone();
            search.extend(std::env::split_paths(&existing));
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
        let output = child.output().expect("the stream is taken once");
        let stop_reading = CancellationToken::new();
        let mut collect = tokio::spawn(collect_output(output, stop_reading.clone()));

        let status = tokio::select! {
            _ = tokio::time::sleep(timeout) => {
                child.kill().await;
                stop_reading.cancel();
                warn!(command, ?timeout, "command hook timed out; it does not block or rewrite anything");
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

        // 解析 stdout
        let response = if stdout.trim().is_empty() {
            // exit != 0 + 空 stdout → 视为 block，message 用 stderr
            if !status.success {
                debug!(
                    ?exit_code,
                    %stderr,
                    "hook exited non-zero with empty stdout; treating as block"
                );
                HookResponse {
                    decision: Some(HookDecision::Block),
                    message: Some(if stderr.is_empty() {
                        format!("hook exited {exit_code:?}")
                    } else {
                        stderr.trim().to_string()
                    }),
                    ..Default::default()
                }
            } else {
                HookResponse::default()
            }
        } else {
            match serde_json::from_str::<HookResponse>(stdout.trim()) {
                Ok(r) => r,
                Err(e) => {
                    warn!(stdout = %stdout, error = %e, "hook stdout not valid JSON; default response");
                    HookResponse::default()
                }
            }
        };

        HookOutcome::Ran {
            response,
            stdout,
            stderr,
            exit_code,
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
/// 解析失败时返回 default response（continue=None, decision=None）。
fn parse_hook_response_lenient(s: &str) -> HookResponse {
    let trimmed = s.trim();
    let candidate = if let Some(rest) = trimmed.strip_prefix("```json") {
        rest.trim_start_matches('\n').trim_end_matches("```").trim()
    } else if let Some(rest) = trimmed.strip_prefix("```") {
        rest.trim_start_matches('\n').trim_end_matches("```").trim()
    } else {
        trimmed
    };
    serde_json::from_str(candidate).unwrap_or_default()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{head}…")
    }
}

// ---- Tests for parse_hook_response_lenient ----

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hook_response_handles_plain_json() {
        let r = parse_hook_response_lenient(r#"{"decision":"block"}"#);
        assert_eq!(r.decision, Some(HookDecision::Block));
    }

    #[test]
    fn parse_hook_response_handles_markdown_block() {
        let r = parse_hook_response_lenient("```json\n{\"continue\":false}\n```");
        assert_eq!(r.r#continue, Some(false));
    }

    #[test]
    fn parse_hook_response_garbage_returns_default() {
        let r = parse_hook_response_lenient("not json at all{{{");
        assert_eq!(r.decision, None);
        assert_eq!(r.r#continue, None);
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
        let r = parse_hook_response_lenient("```\n{\"decision\":\"approve\"}\n```");
        assert_eq!(r.decision, Some(HookDecision::Approve));
    }

    #[test]
    fn parse_hook_response_empty_string_returns_default() {
        let r = parse_hook_response_lenient("");
        assert_eq!(r.decision, None);
    }

    #[test]
    fn parse_hook_response_whitespace_only() {
        let r = parse_hook_response_lenient("   \n  ");
        assert_eq!(r.decision, None);
    }
}
