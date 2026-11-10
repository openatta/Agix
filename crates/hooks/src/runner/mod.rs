//! `HookRunner` —— 按事件触发已注册的 hook，执行子进程并解析响应。
//!
//! Submodules split the implementation:
//! - `executor` — command, prompt, and HTTP hook execution
//! - `matcher` — hook `if` pattern matching utilities

pub mod executor;
pub mod matcher;

use crate::config::{HookConfig, HookEvent, HooksSettings, OnError};
use crate::payload::{HookDecision, HookInput, HookResponse};
use std::sync::Arc;
use std::time::Duration;

/// 没写 `timeout` 的 hook 最多跑多久（60 秒），四种后端都一样。到时它什么都不拦、
/// 不改，除非它写了 `on_error: "block"`。
pub const DEFAULT_HOOK_TIMEOUT_MS: u64 = 60_000;

#[derive(Debug, Clone)]
pub enum HookOutcome {
    /// hook 跑了，并给出 response
    Ran {
        response: HookResponse,
        stdout: String,
        stderr: String,
        exit_code: Option<i32>,
        /// 跑了但没给出能用的回答：command 退出码不是 0 或 2、回答不是 JSON
        failure: Option<String>,
    },
    /// hook 配置不适用（if 不匹配 / 类型不支持）
    Skipped(&'static str),
    /// 执行失败 / 超时
    Error(String),
}

impl HookOutcome {
    /// Inner response, if any.
    pub fn response(&self) -> Option<&HookResponse> {
        match self {
            HookOutcome::Ran { response, .. } => Some(response),
            _ => None,
        }
    }
    /// True if error.
    pub fn is_error(&self) -> bool {
        matches!(self, HookOutcome::Error(_))
    }
}

/// What a prompt or agent hook's answer amounts to: a model's approval is not
/// the user's, unless the hook says it may stand for it. It read the tool
/// input, which whoever wrote that input can aim at it.
fn judged(mut outcome: HookOutcome, may_approve: bool) -> HookOutcome {
    if let HookOutcome::Ran { response, .. } = &mut outcome {
        if !may_approve && response.decision == Some(HookDecision::Approve) {
            response.decision = None;
        }
    }
    outcome
}

/// 一次 `run` 的聚合结果。便于上层短路 / 统计。
#[derive(Debug, Clone, Default)]
pub struct HookRunResult {
    pub outcomes: Vec<HookOutcome>,
}

impl HookRunResult {
    /// 任意 hook 决议 block → 整体 block
    pub fn blocked(&self) -> Option<&HookResponse> {
        self.outcomes.iter().find_map(|o| match o {
            HookOutcome::Ran { response, .. } if response.decision == Some(HookDecision::Block) => {
                Some(response)
            }
            _ => None,
        })
    }
    /// 任意 hook 决议 approve（取第一条）
    pub fn approved(&self) -> Option<&HookResponse> {
        self.outcomes.iter().find_map(|o| match o {
            HookOutcome::Ran { response, .. }
                if response.decision == Some(HookDecision::Approve) =>
            {
                Some(response)
            }
            _ => None,
        })
    }
    /// 任意 hook 要求中止整个 turn
    pub fn discontinued(&self) -> bool {
        self.outcomes
            .iter()
            .any(|o| matches!(o, HookOutcome::Ran { response, .. } if response.r#continue == Some(false)))
    }
    /// 第一个要求中止的 hook 给出的 `message`
    pub fn discontinue_message(&self) -> Option<&str> {
        self.outcomes.iter().find_map(|o| match o {
            HookOutcome::Ran { response, .. } if response.r#continue == Some(false) => {
                response.message.as_deref()
            }
            _ => None,
        })
    }
    /// 第一个改写 input 的 hook 给出的新 input（按出现顺序）
    pub fn updated_input(&self) -> Option<&serde_json::Value> {
        self.outcomes.iter().find_map(|o| match o {
            HookOutcome::Ran { response, .. } => response.updated_input.as_ref(),
            _ => None,
        })
    }
}

/// Prompt-type hook 执行器。HookRunner 持有 `Option<Arc<dyn PromptHookExecutor>>`；
/// 不注入时 Prompt hook skip 报错 "no executor configured"。
///
/// CLI 注入一个 wrap AnthropicClient 的实现 —— hooks crate 自身**不**依赖
/// agix-anthropic，避免循环依赖（anthropic 依赖 core，hooks 依赖 core，
/// 双方独立扩展）。
#[async_trait::async_trait]
pub trait PromptHookExecutor: Send + Sync {
    /// 喂 prompt + payload，返回模型生成的 JSON HookResponse 文本（caller 解析）。
    async fn execute(
        &self,
        prompt: &str,
        model: Option<&str>,
        payload: &HookInput,
    ) -> Result<String, String>;
}

/// Agent hook executor trait. Consumers (CLI/daemon) implement this to
/// provide sub-agent execution for Agent-type hooks. The executor receives the
/// hook prompt + payload and returns structured JSON: `{"ok": bool, "reason": "..."}`.
#[async_trait::async_trait]
pub trait AgentHookExecutor: Send + Sync {
    /// Execute an agent hook with the given prompt and model.
    /// Returns the agent's JSON response string.
    async fn execute(
        &self,
        prompt: &str,
        model: Option<&str>,
        payload: &HookInput,
    ) -> Result<String, String>;
}

pub struct HookRunner {
    settings: Arc<HooksSettings>,
    default_timeout: Duration,
    /// Prompt hook 执行器；None 时 Prompt hook 报"no executor"
    prompt_executor: Option<Arc<dyn PromptHookExecutor>>,
    /// Agent hook 执行器；None 时 Agent hook 报"no executor"
    agent_executor: Option<Arc<dyn AgentHookExecutor>>,
    /// HTTP hook 的出口
    net: Arc<dyn base::interface::exec::Network>,
    /// Track executed `once` hooks: set of (HookEvent, config_index) that have
    /// already fired and must not fire again in this session.
    once_executed: std::sync::Mutex<std::collections::HashSet<(HookEvent, usize)>>,
    /// Optional file watcher for `FileChanged` hooks.
    /// Kept alive so the underlying notify watcher continues to receive events.
    file_watcher: std::sync::Mutex<Option<crate::watcher::FileWatcher>>,
    /// Pending async rewake hooks — (event, config_index) pairs awaiting a
    /// wake signal. When the wake channel fires, these hooks are re-executed.
    pending_rewakes: std::sync::Mutex<std::collections::HashSet<(HookEvent, usize)>>,
    /// Optional wake channel receiver. Something sends `()` via the
    /// associated sender to signal that background work has completed,
    /// triggering re-execution of pending rewake hooks.
    wake_receiver: std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<()>>>,
    /// Directories prepended to a `Command` hook's subprocess `PATH`, highest
    /// priority first (typically project > scene > global `.agix/hooks/`).
    /// Lets `command` reference a bare script name (e.g. `check.sh`) that
    /// lives in one of these directories via normal shell `PATH` lookup,
    /// without inventing a second hook-script resolution scheme — `command`
    /// is still just a shell string, only the search path changes.
    hooks_search_dirs: Vec<std::path::PathBuf>,
    project_untrusted: bool,
    process: platform::LocalProcess,
}

impl HookRunner {
    /// Construct a new instance.
    pub fn new(settings: HooksSettings) -> Self {
        Self {
            settings: Arc::new(settings),
            default_timeout: Duration::from_millis(DEFAULT_HOOK_TIMEOUT_MS),
            prompt_executor: None,
            agent_executor: None,
            net: Arc::new(platform::LocalNetwork::default()),
            once_executed: std::sync::Mutex::new(std::collections::HashSet::new()),
            file_watcher: std::sync::Mutex::new(None),
            pending_rewakes: std::sync::Mutex::new(std::collections::HashSet::new()),
            wake_receiver: std::sync::Mutex::new(None),
            hooks_search_dirs: Vec::new(),
            project_untrusted: false,
            process: Default::default(),
        }
    }

    /// Builder: the environment a `Command` hook starts from.
    pub fn with_subprocess_env(mut self, env: base::interface::exec::SubprocessEnv) -> Self {
        self.process = platform::LocalProcess::new(env);
        self
    }

    /// Builder: set the directories searched (in order, highest priority
    /// first) when resolving a bare script name in a `Command` hook's `PATH`.
    pub fn with_hooks_search_dirs(mut self, dirs: Vec<std::path::PathBuf>) -> Self {
        self.hooks_search_dirs = dirs;
        self
    }

    /// Builder: the session's project is not trusted (ARCHITECTURE §6.6), so
    /// every hook here came from the user's own tiers and none may start a
    /// program the project supplies under a relative path.
    pub fn with_project_untrusted(mut self, untrusted: bool) -> Self {
        self.project_untrusted = untrusted;
        self
    }

    /// Read-only access to the current settings (for merging plugin hooks etc.).
    pub fn settings(&self) -> &HooksSettings {
        &self.settings
    }

    /// Empty/default instance with no state.
    pub fn empty() -> Self {
        Self::new(HooksSettings::default())
    }

    /// Alias for `empty()` — zero overhead noop runner.
    pub fn noop() -> Self {
        Self::empty()
    }

    /// True when no hooks are registered at all.
    pub fn is_empty(&self) -> bool {
        self.settings.is_empty()
    }

    /// 注入 Prompt hook 执行器。CLI 用 AnthropicClient 包一层注入。
    pub fn with_prompt_executor(mut self, executor: Arc<dyn PromptHookExecutor>) -> Self {
        self.prompt_executor = Some(executor);
        self
    }

    /// 注入 Agent hook 执行器。CLI 用 AgentSpawner 包一层注入。
    pub fn with_agent_executor(mut self, executor: Arc<dyn AgentHookExecutor>) -> Self {
        self.agent_executor = Some(executor);
        self
    }

    /// Post-construction setter for the wake channel receiver.
    /// Works on `&self` (uses interior mutability) so it can be called on
    /// an `Arc<HookRunner>`.
    pub fn set_wake_receiver(&self, rx: tokio::sync::mpsc::UnboundedReceiver<()>) {
        *self
            .wake_receiver
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(rx);
    }

    /// Start watching filesystem paths and fire `HookEvent::FileChanged` hooks
    /// when files are modified, created, or deleted.
    ///
    /// Each change is debounced to `debounce_ms` (default 300ms). The hook
    /// payload includes the changed file path and the change type
    /// (`"created"`, `"modified"`, or `"deleted"`).
    ///
    /// The watcher runs in a background thread and dispatches hook execution
    /// on the current Tokio runtime; each payload starts from `ambient`. A
    /// second call adds `paths` to the running watcher, whose `debounce_ms`
    /// and `ambient` stay what the first call gave — one runner, one watcher,
    /// one `FileChanged` per change however many callers asked.
    ///
    /// Returns an error if the underlying filesystem watcher cannot be started.
    pub fn enable_file_watching(
        self: &Arc<Self>,
        paths: &[std::path::PathBuf],
        debounce_ms: u64,
        ambient: crate::payload::Ambient,
    ) -> Result<(), String> {
        let mut slot = self
            .file_watcher
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot.is_none() {
            *slot = Some(crate::watcher::FileWatcher::start(
                self,
                debounce_ms,
                ambient,
            )?);
        }
        match slot.as_mut() {
            Some(watcher) => watcher.add(paths),
            None => Ok(()),
        }
    }

    /// Add `paths` to the watcher [`enable_file_watching`](Self::enable_file_watching)
    /// started; nothing when it was never started.
    pub fn watch_files(&self, paths: &[std::path::PathBuf]) -> Result<(), String> {
        match self
            .file_watcher
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_mut()
        {
            Some(watcher) => watcher.add(paths),
            None => Ok(()),
        }
    }

    fn net(&self) -> &dyn base::interface::exec::Network {
        &*self.net
    }

    /// True if hooks for event are present.
    pub fn has_hooks_for(&self, event: HookEvent) -> bool {
        self.settings
            .get(&event)
            .map(|v| !v.is_empty())
            .unwrap_or(false)
    }

    /// Fire the `Elicitation` hook event when an MCP tool result is found to
    /// contain an elicitation URL (`mcp://` or `elicitation://`) — i.e. the
    /// server is asking for user attention (e.g. an out-of-band
    /// authorization link). This is the request/notification half of the
    /// pair; see [`Self::run_elicitation_result`] for the response half.
    ///
    /// `ambient` is the session's payload (its id, cwd and permission mode);
    /// `server_name` identifies the MCP server that raised the elicitation.
    /// `url` is the elicitation URL found in its tool result.
    pub async fn run_elicitation(
        &self,
        ambient: &crate::payload::Ambient,
        server_name: &str,
        url: &str,
    ) -> HookRunResult {
        let input = HookInput::from_ambient(ambient, "Elicitation").with_tool(
            format!("mcp_elicitation_{server_name}"),
            serde_json::json!({ "server_name": server_name, "url": url }),
        );
        self.run(HookEvent::Elicitation, &input).await
    }

    /// Fire the `ElicitationResult` hook event with the elicitation context
    /// and user response. This is the response half of the pair started by
    /// [`Self::run_elicitation`] — it fires once a host has actually
    /// collected the user's answer to an elicitation request.
    ///
    /// **Currently has no production caller.** The MCP adapter
    /// (`crates/mcp/src/adapter.rs`) only detects that a server *raised* an
    /// elicitation (see `run_elicitation`) — nothing in this codebase
    /// implements the host-side round-trip that collects a response and
    /// reports it back here. Kept (not deleted) because the shape is right
    /// and a host that does implement that round-trip has a real method to
    /// call; same rationale as `HookEvent::ConfigChange`'s doc comment.
    ///
    /// `ambient` is the session's payload, as for [`Self::run_elicitation`].
    /// `server_name` identifies the MCP server that initiated the elicitation.
    /// `url` is the elicitation URL the user responded to.
    /// `response` is the user's response data (optional).
    pub async fn run_elicitation_result(
        &self,
        ambient: &crate::payload::Ambient,
        server_name: &str,
        url: &str,
        response: Option<&serde_json::Value>,
    ) -> HookRunResult {
        let mut tool_input = serde_json::json!({ "server_name": server_name, "url": url });
        if let Some(r) = response {
            tool_input["response"] = r.clone();
        }
        let mut input = HookInput::from_ambient(ambient, "ElicitationResult")
            .with_tool(format!("mcp_elicitation_{server_name}"), tool_input);
        input.tool_result = response.cloned();
        self.run(HookEvent::ElicitationResult, &input).await
    }

    /// 并行跑某事件的所有 hook。
    /// Failures of individual hooks are isolated — one slow / crashing hook
    /// doesn't block the others.
    ///
    /// Merge semantics (computed by `HookRunResult` accessors):
    /// - any hook with `decision: "block"` → engine treats as deny
    /// - any hook with `continue: false` → engine aborts the turn
    /// - first hook (in registration order) with `updated_input` wins for
    ///   input rewriting; conflicting modifications from later hooks are
    ///   ignored deterministically
    ///
    /// Outcomes are collected in **registration order** (not completion order)
    /// so `updated_input()` is deterministic.
    pub async fn run(&self, event: HookEvent, input: &HookInput) -> HookRunResult {
        let configs = match self.settings.get(&event) {
            Some(v) => v,
            None => return HookRunResult::default(),
        };

        let pending = {
            let once_set = self
                .once_executed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            configs
                .iter()
                .enumerate()
                .filter(|(idx, cfg)| !cfg.is_once() || !once_set.contains(&(event, *idx)))
                .map(|(idx, _)| idx)
                .collect::<Vec<usize>>()
        };

        // Spawn each hook as an indexed future; collect into a Vec keyed by
        // registration index so the order in HookRunResult.outcomes is stable
        // regardless of completion order.
        let mut futures = futures::stream::FuturesUnordered::new();
        for &idx in &pending {
            let cfg = configs[idx].clone();
            let input = input.clone();
            futures.push(async move { (idx, self.run_one(&cfg, &input).await) });
        }

        let mut indexed: Vec<(usize, HookOutcome)> = Vec::with_capacity(pending.len());
        use futures::StreamExt;
        while let Some(item) = futures.next().await {
            indexed.push(item);
        }
        indexed.sort_by_key(|(i, _)| *i);

        for (idx, outcome) in &indexed {
            if let HookOutcome::Error(error) = outcome {
                if configs[*idx].on_error() == OnError::Allow {
                    tracing::warn!(
                        event = ?event,
                        hook = %configs[*idx].describe(),
                        %error,
                        "hook failed; it neither blocks nor rewrites anything"
                    );
                }
            }
        }

        // Check for rewake signals in hook responses.
        // If a hook has `async_rewake: true` in its config AND returns
        // `rewake: true` in its response, add it to the pending rewake set
        // for later re-execution when a wake signal fires.
        for &(idx, ref outcome) in &indexed {
            if let HookOutcome::Ran { ref response, .. } = outcome {
                if response.rewake == Some(true) {
                    if let Some(cfg) = configs.get(idx) {
                        let is_async_rewake = matches!(
                            cfg,
                            HookConfig::Command {
                                async_rewake: Some(true),
                                ..
                            }
                        );
                        if is_async_rewake {
                            self.pending_rewakes
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .insert((event, idx));
                        }
                    }
                }
            }
        }

        let mut once_set = self
            .once_executed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (idx, outcome) in &indexed {
            if configs[*idx].is_once() && matches!(outcome, HookOutcome::Ran { .. }) {
                once_set.insert((event, *idx));
            }
        }
        drop(once_set);

        HookRunResult {
            outcomes: indexed
                .into_iter()
                .map(|(idx, outcome)| match configs[idx].on_error() {
                    OnError::Allow => outcome,
                    OnError::Block => block_on_failure(event, &configs[idx], outcome),
                })
                .collect(),
        }
    }

    /// Run a single hook config, dispatching to the appropriate executor.
    /// `pub(super)` so that tests in submodules can call this directly if needed.
    pub(super) async fn run_one(&self, cfg: &HookConfig, input: &HookInput) -> HookOutcome {
        if let Err(why) = cfg.applies_to(input) {
            return HookOutcome::Skipped(why);
        }
        match cfg {
            HookConfig::Command {
                command,
                shell,
                timeout,
                ..
            } => {
                let shell = shell.as_deref().unwrap_or("bash");
                self.exec_command(shell, command, input, self.timeout_of(*timeout))
                    .await
            }
            HookConfig::Prompt {
                prompt,
                timeout,
                model,
                may_approve,
                ..
            } => judged(
                self.exec_prompt(prompt, model.as_deref(), input, self.timeout_of(*timeout))
                    .await,
                *may_approve,
            ),
            HookConfig::Http {
                url,
                headers,
                allow_local,
                timeout,
                ..
            } => {
                self.exec_http(url, headers, *allow_local, input, self.timeout_of(*timeout))
                    .await
            }
            HookConfig::Agent {
                prompt,
                timeout,
                model,
                may_approve,
                ..
            } => judged(
                self.exec_agent(prompt, model.as_deref(), input, self.timeout_of(*timeout))
                    .await,
                *may_approve,
            ),
        }
    }

    fn timeout_of(&self, configured_ms: Option<u64>) -> Duration {
        configured_ms
            .map(Duration::from_millis)
            .unwrap_or(self.default_timeout)
    }

    /// Execute an Agent-type hook. Delegates to the injected AgentHookExecutor.
    /// If no executor is available, returns Skipped with an explanation.
    async fn exec_agent(
        &self,
        prompt: &str,
        model: Option<&str>,
        input: &HookInput,
        timeout: Duration,
    ) -> HookOutcome {
        let Some(ref executor) = self.agent_executor else {
            return HookOutcome::Skipped(NO_AGENT_EXECUTOR);
        };
        let answered =
            match tokio::time::timeout(timeout, executor.execute(prompt, model, input)).await {
                Ok(answered) => answered,
                Err(_) => {
                    return HookOutcome::Error(format!(
                        "agent hook timed out after {}ms",
                        timeout.as_millis()
                    ))
                }
            };
        match answered {
            Ok(response) => {
                // Parse JSON response: {"ok": bool, "reason": "..."}
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&response) {
                    let ok = parsed.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
                    let reason = parsed.get("reason").and_then(|v| v.as_str()).unwrap_or("");
                    let hook_response = crate::payload::HookResponse {
                        decision: if ok {
                            Some(crate::payload::HookDecision::Approve)
                        } else {
                            Some(crate::payload::HookDecision::Block)
                        },
                        message: Some(reason.to_string()),
                        ..Default::default()
                    };
                    HookOutcome::Ran {
                        response: hook_response,
                        stdout: response,
                        stderr: String::new(),
                        exit_code: Some(0),
                        failure: None,
                    }
                } else {
                    HookOutcome::Ran {
                        response: crate::payload::HookResponse {
                            message: Some(response.clone()),
                            ..Default::default()
                        },
                        failure: Some(format!(
                            "agent hook answer is not JSON: {}",
                            executor::truncate(&response, 200)
                        )),
                        stdout: response,
                        stderr: String::new(),
                        exit_code: Some(0),
                    }
                }
            }
            Err(e) => HookOutcome::Error(format!("agent hook execution failed: {e}")),
        }
    }

    /// Check for wake signals and re-execute any pending rewake hooks.
    ///
    /// Drains the wake receiver; if a wake signal (or channel close) is
    /// detected, re-runs all hooks in `pending_rewakes` with a `WakeEvent`
    /// input built on `ambient` (the session's id, cwd and permission mode)
    /// and returns their responses.
    ///
    /// If no wake signal is available, the pending set is preserved so the
    /// hooks can be re-executed on a later call. Signals that arrive while no
    /// hook is waiting are discarded: work that finished before a hook asked
    /// to be woken is not what it is waiting for.
    pub async fn check_rewakes(&self, ambient: HookInput) -> Vec<HookResponse> {
        let pending: Vec<(HookEvent, usize)> = {
            let mut set = self
                .pending_rewakes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if set.is_empty() {
                drop(set);
                if let Some(rx) = self
                    .wake_receiver
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .as_mut()
                {
                    while rx.try_recv().is_ok() {}
                }
                return Vec::new();
            }
            set.drain().collect()
        };

        // Check for a wake signal on the channel
        let should_wake = {
            let mut rx_guard = self
                .wake_receiver
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match rx_guard.as_mut() {
                Some(rx) => match rx.try_recv() {
                    Ok(()) => true,
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => false,
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                        // Channel closed — treat as wake so pending hooks don't
                        // remain stuck forever.
                        true
                    }
                },
                None => false,
            }
        };

        if !should_wake {
            // No wake signal yet — restore the pending set for next time
            let mut set = self
                .pending_rewakes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            set.extend(pending);
            return Vec::new();
        }

        let wake_input = HookInput::lifecycle(
            "WakeEvent",
            ambient.session_id,
            ambient.cwd,
            ambient.permission_mode,
        );

        // Re-execute each pending hook with the wake input
        let mut responses = Vec::new();
        for (event, idx) in pending {
            let configs = match self.settings.get(&event) {
                Some(v) => v,
                None => continue,
            };
            let cfg = match configs.get(idx) {
                Some(c) => c,
                None => continue,
            };
            let outcome = self.run_one(cfg, &wake_input).await;
            if let HookOutcome::Ran { response, .. } = outcome {
                responses.push(response);
            }
        }

        responses
    }
}

const NO_PROMPT_EXECUTOR: &str = "prompt-type hook needs a PromptHookExecutor injected";
const NO_AGENT_EXECUTOR: &str =
    "agent hook: no AgentHookExecutor configured — wire one at CLI/daemon startup";

/// A failed `on_error: "block"` hook, turned into the block it stands for.
///
/// A hook whose backend this host never wired is a failure here too: a hook
/// written to fail closed must not open because nothing could run it.
fn block_on_failure(event: HookEvent, cfg: &HookConfig, outcome: HookOutcome) -> HookOutcome {
    let (why, stdout, stderr, exit_code) = match outcome {
        HookOutcome::Error(error) => (error, String::new(), String::new(), None),
        HookOutcome::Ran {
            failure: Some(failure),
            stdout,
            stderr,
            exit_code,
            ..
        } => (failure, stdout, stderr, exit_code),
        HookOutcome::Skipped(why) if why == NO_PROMPT_EXECUTOR || why == NO_AGENT_EXECUTOR => {
            (why.to_string(), String::new(), String::new(), None)
        }
        other => return other,
    };
    let message = format!("{} failed and its on_error is block: {why}", cfg.describe());
    tracing::warn!(event = ?event, %message, "hook failed; blocking");
    HookOutcome::Ran {
        response: HookResponse {
            decision: Some(HookDecision::Block),
            message: Some(message),
            ..Default::default()
        },
        stdout,
        stderr,
        exit_code,
        failure: Some(why),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::HookEvent;
    use serde_json::json;
    use std::collections::HashMap;

    fn input_for_bash(cmd: &str) -> HookInput {
        HookInput {
            hook_event_name: "PreToolUse".into(),
            session_id: "test".into(),
            cwd: "/tmp".into(),
            permission_mode: "default".into(),
            tool_name: Some("Bash".into()),
            tool_input: Some(json!({"command": cmd})),
            tool_use_id: Some("toolu_01".into()),
            tool_result: None,
            is_error: None,
            user_prompt: None,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn empty_runner_returns_no_outcomes() {
        let r = HookRunner::empty();
        let result = r.run(HookEvent::PreToolUse, &input_for_bash("ls")).await;
        assert!(result.outcomes.is_empty());
    }

    #[tokio::test]
    async fn has_hooks_for_works() {
        let mut s: HooksSettings = HashMap::new();
        s.insert(HookEvent::SessionStart, vec![]);
        s.insert(
            HookEvent::PreToolUse,
            vec![HookConfig::Command {
                command: "echo".into(),
                shell: None,
                timeout: None,
                if_pattern: None,
                only_on_error: None,
                once: None,
                async_rewake: None,
                on_error: OnError::Allow,
            }],
        );
        let r = HookRunner::new(s);
        assert!(r.has_hooks_for(HookEvent::PreToolUse));
        assert!(!r.has_hooks_for(HookEvent::SessionStart)); // empty list
        assert!(!r.has_hooks_for(HookEvent::Stop));
    }

    #[tokio::test]
    async fn only_on_error_filters() {
        let mut s: HooksSettings = HashMap::new();
        s.insert(
            HookEvent::PostToolUse,
            vec![HookConfig::Command {
                command: r#"echo '{"message":"ouch"}'"#.into(),
                shell: None,
                timeout: None,
                if_pattern: None,
                only_on_error: Some(true),
                once: None,
                async_rewake: None,
                on_error: OnError::Allow,
            }],
        );
        let r = HookRunner::new(s);

        let mut input = input_for_bash("ls");
        input.is_error = Some(false);
        let r1 = r.run(HookEvent::PostToolUse, &input).await;
        assert!(matches!(&r1.outcomes[0], HookOutcome::Skipped(_)));

        input.is_error = Some(true);
        let r2 = r.run(HookEvent::PostToolUse, &input).await;
        assert!(matches!(&r2.outcomes[0], HookOutcome::Ran { .. }));
    }

    #[tokio::test]
    async fn unsupported_hook_types_are_skipped() {
        let mut s: HooksSettings = HashMap::new();
        s.insert(
            HookEvent::PreToolUse,
            vec![HookConfig::Prompt {
                prompt: "is this safe?".into(),
                timeout: None,
                model: None,
                if_pattern: None,
                on_error: OnError::Allow,
                may_approve: false,
            }],
        );
        let r = HookRunner::new(s);
        let result = r.run(HookEvent::PreToolUse, &input_for_bash("ls")).await;
        assert!(matches!(&result.outcomes[0], HookOutcome::Skipped(_)));
    }

    #[tokio::test]
    async fn discontinue_signals_block_but_other_hooks_still_run() {
        let mut s: HooksSettings = HashMap::new();
        s.insert(
            HookEvent::PreToolUse,
            vec![
                HookConfig::Command {
                    command: r#"echo '{"continue":false,"message":"abort"}'"#.into(),
                    shell: None,
                    timeout: None,
                    if_pattern: None,
                    only_on_error: None,
                    once: None,
                    async_rewake: None,
                    on_error: OnError::Allow,
                },
                HookConfig::Command {
                    command: "echo peer-still-runs".into(),
                    shell: None,
                    timeout: None,
                    if_pattern: None,
                    only_on_error: None,
                    once: None,
                    async_rewake: None,
                    on_error: OnError::Allow,
                },
            ],
        );
        let r = HookRunner::new(s);
        let result = r.run(HookEvent::PreToolUse, &input_for_bash("ls")).await;
        assert_eq!(result.outcomes.len(), 2);
        assert!(result.discontinued());
        match &result.outcomes[1] {
            HookOutcome::Ran { stdout, .. } => assert!(stdout.contains("peer-still-runs")),
            other => panic!("expected hook 2 to have run; got {other:?}"),
        }
    }

    #[tokio::test]
    async fn parallel_hooks_outcomes_in_registration_order() {
        let mut s: HooksSettings = HashMap::new();
        s.insert(
            HookEvent::PreToolUse,
            vec![
                HookConfig::Command {
                    command: "sleep 0.2; echo first".into(),
                    shell: None,
                    timeout: None,
                    if_pattern: None,
                    only_on_error: None,
                    once: None,
                    async_rewake: None,
                    on_error: OnError::Allow,
                },
                HookConfig::Command {
                    command: "echo second".into(),
                    shell: None,
                    timeout: None,
                    if_pattern: None,
                    only_on_error: None,
                    once: None,
                    async_rewake: None,
                    on_error: OnError::Allow,
                },
            ],
        );
        let r = HookRunner::new(s);
        let result = r.run(HookEvent::PreToolUse, &input_for_bash("ls")).await;
        assert_eq!(result.outcomes.len(), 2);
        match (&result.outcomes[0], &result.outcomes[1]) {
            (HookOutcome::Ran { stdout: s0, .. }, HookOutcome::Ran { stdout: s1, .. }) => {
                assert!(s0.contains("first"));
                assert!(s1.contains("second"));
            }
            other => panic!("unexpected outcomes: {other:?}"),
        }
    }

    #[tokio::test]
    async fn updated_input_is_propagated() {
        let mut s: HooksSettings = HashMap::new();
        s.insert(
            HookEvent::PreToolUse,
            vec![HookConfig::Command {
                command: r#"echo '{"updated_input":{"command":"echo redirected"}}'"#.into(),
                shell: None,
                timeout: None,
                if_pattern: None,
                only_on_error: None,
                once: None,
                async_rewake: None,
                on_error: OnError::Allow,
            }],
        );
        let r = HookRunner::new(s);
        let result = r.run(HookEvent::PreToolUse, &input_for_bash("ls")).await;
        let new = result.updated_input().expect("expected updated_input");
        assert_eq!(new["command"], "echo redirected");
    }

    #[tokio::test]
    async fn if_pattern_filters_by_tool() {
        let mut s: HooksSettings = HashMap::new();
        s.insert(
            HookEvent::PreToolUse,
            vec![HookConfig::Command {
                command: r#"echo '{"decision":"block","message":"git push blocked"}'"#.into(),
                shell: None,
                timeout: None,
                if_pattern: Some("Bash(git push:*)".into()),
                only_on_error: None,
                once: None,
                async_rewake: None,
                on_error: OnError::Allow,
            }],
        );
        let r = HookRunner::new(s);

        // 不匹配 → skipped
        let r1 = r.run(HookEvent::PreToolUse, &input_for_bash("ls")).await;
        assert!(matches!(&r1.outcomes[0], HookOutcome::Skipped(_)));
        assert!(r1.blocked().is_none());

        // 匹配 → blocked
        let r2 = r
            .run(
                HookEvent::PreToolUse,
                &input_for_bash("git push origin main"),
            )
            .await;
        assert!(r2.blocked().is_some());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn command_hook_resolves_bare_script_name_via_hooks_search_dirs() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "agix-hooks-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("greet.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\necho '{\"message\":\"hi from greet.sh\"}'\n",
        )
        .unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();

        let mut s: HooksSettings = HashMap::new();
        s.insert(
            HookEvent::PreToolUse,
            vec![HookConfig::Command {
                // Bare filename, no path — must resolve via PATH.
                command: "greet.sh".into(),
                shell: None,
                timeout: None,
                if_pattern: None,
                only_on_error: None,
                once: None,
                async_rewake: None,
                on_error: OnError::Allow,
            }],
        );
        let r = HookRunner::new(s).with_hooks_search_dirs(vec![dir.clone()]);
        let result = r.run(HookEvent::PreToolUse, &input_for_bash("ls")).await;
        let _ = std::fs::remove_dir_all(&dir);
        match &result.outcomes[0] {
            HookOutcome::Ran { stdout, .. } => assert!(stdout.contains("hi from greet.sh")),
            other => panic!("expected the bare script name to resolve via PATH, got {other:?}"),
        }
    }

    /// In a project nobody trusted, a hook written in the user's own settings
    /// must not start a program the repository put under a relative path.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_untrusted_project_does_not_supply_a_hooks_relative_program() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile_dir();
        let project = root.join("project");
        let marker = project.join("ran");
        std::fs::create_dir_all(project.join("scripts")).unwrap();
        let script = project.join("scripts").join("check.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let hooks_dir = root.join("hooks");
        std::fs::create_dir_all(&hooks_dir).unwrap();
        let own = hooks_dir.join("own.sh");
        std::fs::write(&own, "#!/bin/sh\necho '{\"message\":\"own\"}'\n").unwrap();
        std::fs::set_permissions(&own, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut input = input_for_bash("ls");
        input.cwd = project.display().to_string();
        let runner = |command: &str, on_error: OnError, untrusted: bool| {
            let mut s: HooksSettings = HashMap::new();
            s.insert(
                HookEvent::PreToolUse,
                vec![HookConfig::Command {
                    command: command.into(),
                    shell: None,
                    timeout: None,
                    if_pattern: None,
                    only_on_error: None,
                    once: None,
                    async_rewake: None,
                    on_error,
                }],
            );
            HookRunner::new(s)
                .with_hooks_search_dirs(vec![hooks_dir.clone()])
                .with_project_untrusted(untrusted)
        };

        for command in [
            "./scripts/check.sh",
            "scripts/check.sh",
            "true && cd scripts && ./check.sh",
            "env X=1 scripts/check.sh",
            "bash scripts/check.sh",
            "sh -e ./scripts/check.sh",
            "bash -c './scripts/check.sh'",
            ". scripts/check.sh",
            "echo scripts/check.sh",
        ] {
            let result = runner(command, OnError::Allow, true)
                .run(HookEvent::PreToolUse, &input)
                .await;
            assert!(!marker.exists(), "`{command}` ran the project's script");
            match &result.outcomes[0] {
                HookOutcome::Error(e) => assert!(e.contains("not trusted"), "`{command}`: {e}"),
                other => panic!("`{command}`: expected a hook failure, got {other:?}"),
            }
        }

        let blocked = runner("./scripts/check.sh", OnError::Block, true)
            .run(HookEvent::PreToolUse, &input)
            .await;
        assert!(blocked.blocked().is_some(), "{:?}", blocked.outcomes);

        for command in [
            "own.sh",
            "/bin/echo '{}'",
            "echo scripts/missing.sh",
            "true",
        ] {
            let result = runner(command, OnError::Allow, true)
                .run(HookEvent::PreToolUse, &input)
                .await;
            assert!(
                matches!(result.outcomes[0], HookOutcome::Ran { .. }),
                "`{command}`: {:?}",
                result.outcomes
            );
        }

        runner("./scripts/check.sh", OnError::Allow, false)
            .run(HookEvent::PreToolUse, &input)
            .await;
        assert!(marker.exists(), "a trusted project's script runs as before");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// An interpreter the user's hook starts by name is not the project's
    /// file, but the script it is handed is.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_untrusted_project_does_not_hand_a_hook_its_files() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile_dir();
        let project = root.join("project");
        let marker = project.join("ran");
        let scripts = project.join("scripts");
        std::fs::create_dir_all(&scripts).unwrap();
        let touch = format!("touch '{}'", marker.display());
        std::fs::write(
            scripts.join("check.awk"),
            format!("BEGIN {{ system(\"{touch}\") }}\n"),
        )
        .unwrap();
        for entry in ["check.py", "__main__.py"] {
            std::fs::write(
                scripts.join(entry),
                format!("open('{}', 'w')\n", marker.display()),
            )
            .unwrap();
        }
        std::fs::write(
            scripts.join("check.js"),
            format!("require('fs').writeFileSync('{}', '')\n", marker.display()),
        )
        .unwrap();
        std::fs::write(scripts.join("check.sh"), format!("{touch}\n")).unwrap();
        let hooks_dir = root.join("hooks");
        std::fs::create_dir_all(&hooks_dir).unwrap();
        for (dir, body) in [
            (&hooks_dir, "#!/bin/sh\nexit 0\n".to_string()),
            (&project, format!("#!/bin/sh\n{touch}\n")),
        ] {
            let own = dir.join("own.sh");
            std::fs::write(&own, body).unwrap();
            std::fs::set_permissions(&own, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let mut input = input_for_bash("ls");
        input.cwd = project.display().to_string();
        let runner = |command: &str, untrusted: bool| {
            let mut s: HooksSettings = HashMap::new();
            s.insert(
                HookEvent::PreToolUse,
                vec![HookConfig::Command {
                    command: command.into(),
                    shell: None,
                    timeout: None,
                    if_pattern: None,
                    only_on_error: None,
                    once: None,
                    async_rewake: None,
                    on_error: OnError::Allow,
                }],
            );
            HookRunner::new(s)
                .with_hooks_search_dirs(vec![hooks_dir.clone()])
                .with_project_untrusted(untrusted)
        };

        for command in [
            "awk -f scripts/check.awk",
            "python3 scripts/check.py",
            "python3 -W ignore scripts/check.py",
            "node scripts/check.js",
            "node --require=./scripts/check.js -e 0",
            "cd scripts && python3 check.py",
            "cat scripts/check.sh | sh",
            "sh < scripts/check.sh",
            "sh scripts/check.s?",
            "for f in scripts/check.*; do python3 \"$f\"; done",
            "python3 scripts",
            "node -r./scripts/check.js -e 0",
        ] {
            let result = runner(command, true)
                .run(HookEvent::PreToolUse, &input)
                .await;
            assert!(!marker.exists(), "`{command}` ran the project's script");
            match &result.outcomes[0] {
                HookOutcome::Error(e) => assert!(e.contains("not trusted"), "`{command}`: {e}"),
                other => panic!("`{command}`: expected a hook failure, got {other:?}"),
            }
        }

        for command in [
            "own.sh",
            "true --flag ./scripts/missing.py /etc/hosts",
            "awk -f /dev/null",
            "echo log >> scripts/check.sh.log",
        ] {
            let result = runner(command, true)
                .run(HookEvent::PreToolUse, &input)
                .await;
            assert!(
                matches!(result.outcomes[0], HookOutcome::Ran { .. }),
                "`{command}`: {:?}",
                result.outcomes
            );
            assert!(!marker.exists(), "`{command}` ran the project's script");
        }

        runner("awk -f scripts/check.awk", false)
            .run(HookEvent::PreToolUse, &input)
            .await;
        assert!(marker.exists(), "a trusted project's script runs as before");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `$PWD` is the project the hook runs in, however the path is quoted.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_untrusted_project_is_not_reached_through_pwd() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile_dir();
        let project = root.join("project");
        let marker = project.join("ran");
        std::fs::create_dir_all(project.join("scripts")).unwrap();
        let script = project.join("scripts").join("check.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut input = input_for_bash("ls");
        input.cwd = project.display().to_string();
        let runner = |command: &str, untrusted: bool| {
            let mut s: HooksSettings = HashMap::new();
            s.insert(
                HookEvent::PreToolUse,
                vec![HookConfig::Command {
                    command: command.into(),
                    shell: None,
                    timeout: None,
                    if_pattern: None,
                    only_on_error: None,
                    once: None,
                    async_rewake: None,
                    on_error: OnError::Allow,
                }],
            );
            HookRunner::new(s).with_project_untrusted(untrusted)
        };

        for command in [
            "$PWD/scripts/check.sh",
            "${PWD}/scripts/check.sh",
            "\"$PWD\"/scripts/check.sh",
            "sh -c '$PWD/scripts/check.sh'",
        ] {
            let result = runner(command, true)
                .run(HookEvent::PreToolUse, &input)
                .await;
            assert!(!marker.exists(), "`{command}` ran the project's script");
            match &result.outcomes[0] {
                HookOutcome::Error(e) => assert!(e.contains("not trusted"), "`{command}`: {e}"),
                other => panic!("`{command}`: expected a hook failure, got {other:?}"),
            }
        }

        let result = runner("test -d \"$HOME\" && cd \"$HOME\"", true)
            .run(HookEvent::PreToolUse, &input)
            .await;
        assert!(
            matches!(result.outcomes[0], HookOutcome::Ran { .. }),
            "{:?}",
            result.outcomes
        );

        runner("$PWD/scripts/check.sh", false)
            .run(HookEvent::PreToolUse, &input)
            .await;
        assert!(marker.exists(), "a trusted project's script runs as before");
        let _ = std::fs::remove_dir_all(&root);
    }

    fn one_command_hook(command: &str, timeout_ms: u64) -> HookRunner {
        let mut s: HooksSettings = HashMap::new();
        s.insert(
            HookEvent::PreToolUse,
            vec![HookConfig::Command {
                command: command.into(),
                shell: None,
                timeout: Some(timeout_ms),
                if_pattern: None,
                only_on_error: None,
                once: None,
                async_rewake: None,
                on_error: OnError::Allow,
            }],
        );
        HookRunner::new(s)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_block_is_honored_when_the_hook_leaves_a_background_process_behind() {
        let dir = tempfile_dir();
        let r = one_command_hook(
            &format!(
                r#"sleep 30 & echo $! > '{}/pid'; echo '{{"decision":"block","message":"no"}}'"#,
                dir.display()
            ),
            20_000,
        );
        let started = std::time::Instant::now();
        let result = r.run(HookEvent::PreToolUse, &input_for_bash("ls")).await;
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the hook waited on its background sleep: {:?}",
            started.elapsed()
        );
        assert_eq!(
            result.blocked().and_then(|r| r.message.clone()).as_deref(),
            Some("no"),
            "{:?}",
            result.outcomes
        );
        let pid: i32 = std::fs::read_to_string(dir.join("pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        // SAFETY: signal 0 only asks whether the pid exists.
        while unsafe { libc::kill(pid, 0) } == 0 {
            assert!(std::time::Instant::now() < deadline, "the sleep survived");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_hook_that_outlives_its_timeout_is_stopped_and_reported() {
        let r = one_command_hook("sleep 30", 300);
        let started = std::time::Instant::now();
        let result = r.run(HookEvent::PreToolUse, &input_for_bash("ls")).await;
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(result.outcomes[0].is_error(), "{:?}", result.outcomes);
        assert!(result.blocked().is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_command_hook_starts_from_the_configured_environment() {
        if std::env::var_os("CARGO_MANIFEST_DIR").is_none() {
            return;
        }
        let probe = "printenv CARGO_MANIFEST_DIR || echo absent";
        let stdout_of = |r: HookRunResult| match &r.outcomes[0] {
            HookOutcome::Ran { stdout, .. } => stdout.trim().to_string(),
            other => panic!("{other:?}"),
        };
        let inherited = one_command_hook(probe, 10_000)
            .run(HookEvent::PreToolUse, &input_for_bash("ls"))
            .await;
        assert_eq!(stdout_of(inherited), env!("CARGO_MANIFEST_DIR"));

        let allowlist = base::interface::exec::SubprocessEnv {
            mode: base::interface::exec::SubprocessEnvMode::Allowlist,
            allow: vec![],
        };
        let narrowed = one_command_hook(probe, 10_000)
            .with_subprocess_env(allowlist)
            .run(HookEvent::PreToolUse, &input_for_bash("ls"))
            .await;
        assert_eq!(stdout_of(narrowed), "absent");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_command_hook_runs_in_the_directory_its_payload_names() {
        let dir = tempfile_dir();
        let mut input = input_for_bash("ls");
        input.cwd = dir.to_string_lossy().into_owned();
        let result = one_command_hook("pwd -P", 10_000)
            .run(HookEvent::PreToolUse, &input)
            .await;
        let expected = std::fs::canonicalize(&dir).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        match &result.outcomes[0] {
            HookOutcome::Ran { stdout, .. } => {
                assert_eq!(std::path::Path::new(stdout.trim()), expected)
            }
            other => panic!("{other:?}"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_payload_without_a_usable_cwd_still_runs_the_hook() {
        let mut input = input_for_bash("ls");
        input.cwd = "/definitely/not/a/dir".into();
        let result = one_command_hook("echo ran", 10_000)
            .run(HookEvent::PreToolUse, &input)
            .await;
        match &result.outcomes[0] {
            HookOutcome::Ran { stdout, .. } => assert_eq!(stdout.trim(), "ran"),
            other => panic!("{other:?}"),
        }
    }

    fn tempfile_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agix-hooks-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn command_hook_without_search_dirs_does_not_resolve_bare_script_name() {
        let mut s: HooksSettings = HashMap::new();
        s.insert(
            HookEvent::PreToolUse,
            vec![HookConfig::Command {
                command: "definitely-not-a-real-command-xyz".into(),
                shell: None,
                timeout: None,
                if_pattern: None,
                only_on_error: None,
                once: None,
                async_rewake: None,
                on_error: OnError::Allow,
            }],
        );
        let r = HookRunner::new(s); // no hooks_search_dirs configured
        let result = r.run(HookEvent::PreToolUse, &input_for_bash("ls")).await;
        match &result.outcomes[0] {
            HookOutcome::Ran { exit_code, .. } => assert_ne!(*exit_code, Some(0)),
            other => panic!("expected a failing exit code, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn http_hook_with_unreachable_url_returns_error() {
        let mut s: HooksSettings = HashMap::new();
        s.insert(
            HookEvent::PreToolUse,
            vec![HookConfig::Http {
                url: "http://127.0.0.1:1/no-such".into(),
                headers: HashMap::new(),
                timeout: Some(500),
                if_pattern: None,
                on_error: OnError::Allow,
                allow_local: false,
            }],
        );
        let r = HookRunner::new(s);
        let result = r.run(HookEvent::PreToolUse, &input_for_bash("ls")).await;
        assert_eq!(result.outcomes.len(), 1);
        match &result.outcomes[0] {
            HookOutcome::Error(msg) => {
                assert!(
                    msg.starts_with("http hook") || msg.starts_with("SSRF guard"),
                    "error should be tagged http hook or SSRF guard, got: {msg}"
                );
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    fn command(command: &str) -> HookConfig {
        HookConfig::Command {
            command: command.into(),
            shell: None,
            timeout: Some(10_000),
            if_pattern: None,
            only_on_error: None,
            once: None,
            async_rewake: None,
            on_error: OnError::Allow,
        }
    }

    fn on_pre_tool_use(hooks: Vec<HookConfig>) -> HookRunner {
        HookRunner::new(HashMap::from([(HookEvent::PreToolUse, hooks)]))
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_exit_code_decides_whatever_stdout_holds() {
        let blocked = on_pre_tool_use(vec![command("echo 'not json {{'; echo NO-RM >&2; exit 2")])
            .run(HookEvent::PreToolUse, &input_for_bash("rm x"))
            .await;
        assert_eq!(
            blocked.blocked().and_then(|r| r.message.as_deref()),
            Some("NO-RM"),
            "{:?}",
            blocked.outcomes
        );

        let failed = on_pre_tool_use(vec![command("echo boom >&2; exit 1")])
            .run(HookEvent::PreToolUse, &input_for_bash("ls"))
            .await;
        assert!(failed.blocked().is_none(), "{:?}", failed.outcomes);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_once_hook_is_not_used_up_by_the_calls_its_if_skips() {
        let dir = tempfile_dir();
        let marker = dir.join("ran");
        let hook = HookConfig::Command {
            command: format!("echo x >> '{}'", marker.display()),
            shell: None,
            timeout: Some(10_000),
            if_pattern: Some("Bash(git push:*)".into()),
            only_on_error: None,
            once: Some(true),
            async_rewake: None,
            on_error: OnError::Allow,
        };
        let r = on_pre_tool_use(vec![hook]);
        for cmd in ["ls", "git push origin", "git push origin", "ls"] {
            r.run(HookEvent::PreToolUse, &input_for_bash(cmd)).await;
        }
        let ran = std::fs::read_to_string(&marker).unwrap_or_default();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(ran.lines().count(), 1, "{ran:?}");
    }

    struct Answering(&'static str);

    #[async_trait::async_trait]
    impl PromptHookExecutor for Answering {
        async fn execute(&self, _: &str, _: Option<&str>, _: &HookInput) -> Result<String, String> {
            Ok(self.0.to_string())
        }
    }

    #[async_trait::async_trait]
    impl AgentHookExecutor for Answering {
        async fn execute(&self, _: &str, _: Option<&str>, _: &HookInput) -> Result<String, String> {
            Ok(self.0.to_string())
        }
    }

    /// A judge model reads the tool input, which whoever wrote it can aim at
    /// the judge; its "fine" is not the user's. Only a hook that says so may
    /// answer the permission prompt with it. A refusal stands either way.
    #[tokio::test]
    async fn a_judge_models_approval_counts_only_where_the_hook_allows_it() {
        let run = |config: serde_json::Value| async move {
            let config: HookConfig = serde_json::from_value(config).unwrap();
            on_pre_tool_use(vec![config])
                .with_prompt_executor(Arc::new(Answering(r#"{"decision":"approve"}"#)))
                .with_agent_executor(Arc::new(Answering(r#"{"ok":true}"#)))
                .run(HookEvent::PreToolUse, &input_for_bash("ls"))
                .await
        };
        for kind in ["prompt", "agent"] {
            let unsaid = run(serde_json::json!({"type": kind, "prompt": "judge"})).await;
            assert!(unsaid.approved().is_none(), "{kind}: {:?}", unsaid.outcomes);
            assert!(unsaid.blocked().is_none(), "{kind}: {:?}", unsaid.outcomes);

            let allowed = run(serde_json::json!({
                "type": kind, "prompt": "judge", "may_approve": true
            }))
            .await;
            assert!(
                allowed.approved().is_some(),
                "{kind}: {:?}",
                allowed.outcomes
            );
        }

        let refused = on_pre_tool_use(vec![serde_json::from_value(serde_json::json!({
            "type": "agent", "prompt": "judge"
        }))
        .unwrap()])
        .with_agent_executor(Arc::new(Answering(r#"{"ok":false,"reason":"no"}"#)))
        .run(HookEvent::PreToolUse, &input_for_bash("ls"))
        .await;
        assert!(refused.blocked().is_some(), "{:?}", refused.outcomes);
    }

    #[tokio::test]
    async fn every_backend_honors_its_if() {
        let r = on_pre_tool_use(vec![
            HookConfig::Prompt {
                prompt: "judge".into(),
                timeout: None,
                model: None,
                if_pattern: Some("Write|Edit".into()),
                on_error: OnError::Allow,
                may_approve: false,
            },
            HookConfig::Agent {
                prompt: "judge".into(),
                timeout: None,
                model: None,
                if_pattern: Some("Write".into()),
                on_error: OnError::Allow,
                may_approve: false,
            },
            HookConfig::Http {
                url: "http://203.0.113.1/hook".into(),
                headers: HashMap::new(),
                timeout: Some(100),
                if_pattern: Some("Write".into()),
                on_error: OnError::Allow,
                allow_local: false,
            },
        ])
        .with_prompt_executor(Arc::new(Answering(r#"{"decision":"block"}"#)))
        .with_agent_executor(Arc::new(Answering(r#"{"ok":false}"#)));

        let bash = r.run(HookEvent::PreToolUse, &input_for_bash("ls")).await;
        assert!(
            bash.outcomes
                .iter()
                .all(|o| matches!(o, HookOutcome::Skipped(_))),
            "{:?}",
            bash.outcomes
        );

        let mut write = input_for_bash("ls");
        write.tool_name = Some("Write".into());
        let written = r.run(HookEvent::PreToolUse, &write).await;
        assert!(written.blocked().is_some(), "{:?}", written.outcomes);
    }

    struct Slow;

    #[async_trait::async_trait]
    impl AgentHookExecutor for Slow {
        async fn execute(&self, _: &str, _: Option<&str>, _: &HookInput) -> Result<String, String> {
            tokio::time::sleep(Duration::from_secs(30)).await;
            Ok(r#"{"ok":false}"#.into())
        }
    }

    #[tokio::test]
    async fn an_agent_hook_is_held_to_its_timeout() {
        let r = on_pre_tool_use(vec![HookConfig::Agent {
            prompt: "judge".into(),
            timeout: Some(200),
            model: None,
            if_pattern: None,
            on_error: OnError::Allow,
            may_approve: false,
        }])
        .with_agent_executor(Arc::new(Slow));
        let started = std::time::Instant::now();
        let result = r.run(HookEvent::PreToolUse, &input_for_bash("ls")).await;
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(result.outcomes[0].is_error(), "{:?}", result.outcomes);
        assert!(result.blocked().is_none());
    }

    fn blocking_on_error(mut hook: HookConfig) -> HookConfig {
        match &mut hook {
            HookConfig::Command { on_error, .. }
            | HookConfig::Prompt { on_error, .. }
            | HookConfig::Http { on_error, .. }
            | HookConfig::Agent { on_error, .. } => *on_error = OnError::Block,
        }
        hook
    }

    fn block_message(result: &HookRunResult) -> String {
        result
            .blocked()
            .and_then(|r| r.message.clone())
            .unwrap_or_else(|| panic!("not blocked: {:?}", result.outcomes))
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_failing_command_hook_with_on_error_block_blocks() {
        let mut timing_out = command("sleep 30");
        if let HookConfig::Command { timeout, .. } = &mut timing_out {
            *timeout = Some(300);
        }
        for (hook, said) in [
            (timing_out, "timed out"),
            (command("echo boom >&2; exit 1"), "exited 1: boom"),
            (command("echo 'not json'"), "not a JSON answer"),
        ] {
            let result = on_pre_tool_use(vec![blocking_on_error(hook)])
                .run(HookEvent::PreToolUse, &input_for_bash("ls"))
                .await;
            let message = block_message(&result);
            assert!(
                message.contains("command `") && message.contains(said),
                "{message}"
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn on_error_block_leaves_a_working_hook_alone() {
        let quiet = on_pre_tool_use(vec![blocking_on_error(command("true"))])
            .run(HookEvent::PreToolUse, &input_for_bash("ls"))
            .await;
        assert!(quiet.blocked().is_none(), "{:?}", quiet.outcomes);

        let refused = on_pre_tool_use(vec![blocking_on_error(command("echo NO >&2; exit 2"))])
            .run(HookEvent::PreToolUse, &input_for_bash("ls"))
            .await;
        assert_eq!(block_message(&refused), "NO");

        let mut skipped = blocking_on_error(command("exit 1"));
        if let HookConfig::Command { if_pattern, .. } = &mut skipped {
            *if_pattern = Some("Write".into());
        }
        let skipped = on_pre_tool_use(vec![skipped])
            .run(HookEvent::PreToolUse, &input_for_bash("ls"))
            .await;
        assert!(skipped.blocked().is_none(), "{:?}", skipped.outcomes);
    }

    #[tokio::test]
    async fn failing_prompt_http_and_agent_hooks_with_on_error_block_block() {
        let prompt = || HookConfig::Prompt {
            prompt: "judge".into(),
            timeout: None,
            model: None,
            if_pattern: None,
            on_error: OnError::Block,
            may_approve: false,
        };
        let agent = |timeout| HookConfig::Agent {
            prompt: "judge".into(),
            timeout,
            model: None,
            if_pattern: None,
            on_error: OnError::Block,
            may_approve: false,
        };

        let unparsable = on_pre_tool_use(vec![prompt()])
            .with_prompt_executor(Arc::new(Answering("sure, looks fine")))
            .run(HookEvent::PreToolUse, &input_for_bash("ls"))
            .await;
        assert!(block_message(&unparsable).contains("prompt hook"));

        let unwired = on_pre_tool_use(vec![prompt()])
            .run(HookEvent::PreToolUse, &input_for_bash("ls"))
            .await;
        assert!(block_message(&unwired).contains("PromptHookExecutor"));

        let unreachable = on_pre_tool_use(vec![HookConfig::Http {
            url: "http://127.0.0.1:1/no-such".into(),
            headers: HashMap::new(),
            timeout: Some(500),
            if_pattern: None,
            on_error: OnError::Block,
            allow_local: false,
        }])
        .run(HookEvent::PreToolUse, &input_for_bash("ls"))
        .await;
        assert!(block_message(&unreachable).contains("http `http://127.0.0.1:1/no-such`"));

        let timed_out = on_pre_tool_use(vec![agent(Some(200))])
            .with_agent_executor(Arc::new(Slow))
            .run(HookEvent::PreToolUse, &input_for_bash("ls"))
            .await;
        assert!(block_message(&timed_out).contains("timed out"));

        let errored = on_pre_tool_use(vec![agent(None)])
            .with_agent_executor(Arc::new(Failing))
            .run(HookEvent::PreToolUse, &input_for_bash("ls"))
            .await;
        assert!(block_message(&errored).contains("JUDGE-UNAVAILABLE"));

        let rambling = on_pre_tool_use(vec![agent(None)])
            .with_agent_executor(Arc::new(Answering("I think it is fine")))
            .run(HookEvent::PreToolUse, &input_for_bash("ls"))
            .await;
        assert!(block_message(&rambling).contains("not JSON"));

        let approving = on_pre_tool_use(vec![agent(None)])
            .with_agent_executor(Arc::new(Answering(r#"{"ok":true}"#)))
            .run(HookEvent::PreToolUse, &input_for_bash("ls"))
            .await;
        assert!(approving.blocked().is_none(), "{:?}", approving.outcomes);
    }

    #[test]
    fn a_hook_without_a_timeout_gets_a_minute() {
        assert_eq!(
            HookRunner::empty().timeout_of(None),
            Duration::from_secs(60)
        );
        assert_eq!(
            HookRunner::empty().timeout_of(Some(1500)),
            Duration::from_millis(1500)
        );
    }

    #[derive(Clone, Default)]
    struct Captured(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_hook_that_errors_says_so_in_the_log() {
        let logs = Captured::default();
        let writer = logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let r = on_pre_tool_use(vec![HookConfig::Agent {
            prompt: "judge".into(),
            timeout: None,
            model: None,
            if_pattern: None,
            on_error: OnError::Allow,
            may_approve: false,
        }])
        .with_agent_executor(Arc::new(Failing));
        let result = r.run(HookEvent::PreToolUse, &input_for_bash("ls")).await;
        assert!(result.outcomes[0].is_error());

        let logged = String::from_utf8_lossy(
            &logs
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
        .into_owned();
        assert!(
            logged.contains("WARN")
                && logged.contains("PreToolUse")
                && logged.contains("agent hook")
                && logged.contains("JUDGE-UNAVAILABLE"),
            "{logged}"
        );
    }

    struct Failing;

    #[async_trait::async_trait]
    impl AgentHookExecutor for Failing {
        async fn execute(&self, _: &str, _: Option<&str>, _: &HookInput) -> Result<String, String> {
            Err("JUDGE-UNAVAILABLE".into())
        }
    }

    /// Rewrites `file` until the hook writing to `marker` has run: the OS
    /// watcher registers on its own schedule, so one early write can be lost.
    async fn edit_until_seen(file: &std::path::Path, marker: &std::path::Path) -> String {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        let mut edits = 0;
        loop {
            edits += 1;
            std::fs::write(file, format!("edit {edits}")).unwrap();
            let waited = std::time::Instant::now();
            while waited.elapsed() < Duration::from_secs(1) {
                if let Ok(seen) = std::fs::read_to_string(marker) {
                    if !seen.is_empty() {
                        return seen;
                    }
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "no FileChanged after {edits} edits of {}",
                file.display()
            );
        }
    }

    fn recording_file_changed(marker: &std::path::Path) -> Arc<HookRunner> {
        Arc::new(HookRunner::new(HashMap::from([(
            HookEvent::FileChanged,
            vec![command(&format!("cat >> '{}'", marker.display()))],
        )])))
    }

    fn ambient_of(session: &'static str) -> crate::payload::Ambient {
        Arc::new(move || HookInput::lifecycle("", session, "/the/project", "plan"))
    }

    /// A configured `FileChanged` hook fires through the real watcher, and its
    /// payload is the session's.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn file_changed_hook_fires_on_watched_file_modification() {
        let dir = tempfile_dir();
        let path = dir.join("watched.txt");
        let marker = dir.join("seen.jsonl");
        std::fs::write(&path, "initial").unwrap();

        let r = recording_file_changed(&marker);
        r.enable_file_watching(std::slice::from_ref(&path), 50, ambient_of("root-sess"))
            .expect("watcher starts");

        let seen = edit_until_seen(&path, &marker).await;
        let first: serde_json::Value =
            serde_json::from_str(seen.lines().next().unwrap_or_default()).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(first["hook_event_name"], "FileChanged");
        assert_eq!(first["session_id"], "root-sess");
        assert_eq!(first["cwd"], "/the/project");
        assert_eq!(first["permission_mode"], "plan");
        assert_eq!(
            std::path::Path::new(first["tool_input"]["file_path"].as_str().unwrap())
                .file_name()
                .unwrap(),
            "watched.txt"
        );
    }

    /// A second caller adds to the one watcher rather than starting another,
    /// so one change still fires one hook.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn a_second_watch_request_adds_files_to_the_same_watcher() {
        let dir = tempfile_dir();
        let first = dir.join("first.md");
        let second = dir.join("second.md");
        let marker = dir.join("seen.jsonl");
        std::fs::write(&first, "a").unwrap();
        std::fs::write(&second, "b").unwrap();

        let r = recording_file_changed(&marker);
        r.watch_files(std::slice::from_ref(&second))
            .expect("nothing to add to before the watcher starts");
        r.enable_file_watching(std::slice::from_ref(&first), 50, ambient_of("s"))
            .expect("watcher starts");
        r.enable_file_watching(std::slice::from_ref(&first), 50, ambient_of("other"))
            .expect("same file again");
        r.watch_files(std::slice::from_ref(&second))
            .expect("second file");

        let seen = edit_until_seen(&second, &marker).await;
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            seen.lines().all(|l| l.contains("\"session_id\":\"s\"")),
            "the first caller's ambient is the watcher's: {seen}"
        );
    }
}
