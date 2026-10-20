//! `HookRunner` —— 按事件触发已注册的 hook，执行子进程并解析响应。
//!
//! Submodules split the implementation:
//! - `executor` — command, prompt, and HTTP hook execution
//! - `ssrf` — SSRF guard for HTTP hooks
//! - `matcher` — hook `if` pattern matching utilities

pub mod executor;
pub mod matcher;
pub mod ssrf;

use crate::config::{HookConfig, HookEvent, HooksSettings};
use crate::payload::{HookDecision, HookInput, HookResponse};
use std::sync::Arc;
use std::time::Duration;

/// 默认 hook 超时（10 分钟）。
pub const DEFAULT_HOOK_TIMEOUT_MS: u64 = 600_000;

#[derive(Debug, Clone)]
pub enum HookOutcome {
    /// hook 跑了，并给出 response
    Ran {
        response: HookResponse,
        stdout: String,
        stderr: String,
        exit_code: Option<i32>,
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
/// NOTE: returns `Result<String, String>` — the `String` error type is a legacy
/// pattern inherited from the TS codebase. This is a trait method, so changing
/// the signature would be a breaking API change. When this trait gets a v2,
/// consider returning a proper error type (e.g. `HookExecutorError`).
///
/// CLI 注入一个 wrap AnthropicClient 的实现 —— hooks crate 自身**不**依赖
/// attacode-anthropic，避免循环依赖（anthropic 依赖 core，hooks 依赖 core，
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

/// P1-10: Agent hook executor trait. Consumers (CLI/daemon) implement this to
/// provide sub-agent execution for Agent-type hooks. The executor receives the
/// hook prompt + payload and returns structured JSON: `{"ok": bool, "reason": "..."}`.
#[async_trait::async_trait]
pub trait WasmHookExecutor: Send + Sync {
    /// Ask `plugin` about this event.
    ///
    /// The returned `HookResponse` is deliberately restricted by the
    /// implementation: a plugin may proceed, block with a reason, or add an
    /// attributable note. It may **not** set `updated_input`. Quietly
    /// rewriting another party's tool arguments is a different power from
    /// refusing the call, and it is not one a downloaded package gets — that
    /// capability stays with hooks the user wrote themselves.
    async fn execute(
        &self,
        plugin: &str,
        payload: &HookInput,
        timeout_ms: Option<u64>,
    ) -> Result<crate::payload::HookResponse, String>;
}

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
    /// P1-10: Agent hook 执行器；None 时 Agent hook 报"no executor"
    agent_executor: Option<Arc<dyn AgentHookExecutor>>,
    wasm_executor: Option<Arc<dyn WasmHookExecutor>>,
    /// HTTP hook 的出口；按需 lazy 初始化（绝大多数 session 不用 http hook）
    net: std::sync::OnceLock<std::sync::Arc<dyn base::interface::exec::Network>>,
    /// Track executed `once` hooks: set of (HookEvent, config_index) that have
    /// already fired and must not fire again in this session.
    once_executed: std::sync::Mutex<std::collections::HashSet<(HookEvent, usize)>>,
    /// Optional file watcher for `FileChanged` hooks.
    /// Kept alive so the underlying notify watcher continues to receive events.
    file_watcher: std::sync::Mutex<Option<crate::watcher::FileWatcher>>,
    /// P2: Pending async rewake hooks — (event, config_index) pairs awaiting a
    /// wake signal. When the wake channel fires, these hooks are re-executed.
    pending_rewakes: std::sync::Mutex<std::collections::HashSet<(HookEvent, usize)>>,
    /// P2: Optional wake channel receiver. Something sends `()` via the
    /// associated sender to signal that background work has completed,
    /// triggering re-execution of pending rewake hooks.
    wake_receiver: std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<()>>>,
    /// Directories prepended to a `Command` hook's subprocess `PATH`, highest
    /// priority first (typically project > scene > global `.atta/hooks/`).
    /// Lets `command` reference a bare script name (e.g. `check.sh`) that
    /// lives in one of these directories via normal shell `PATH` lookup,
    /// without inventing a second hook-script resolution scheme — `command`
    /// is still just a shell string, only the search path changes.
    hooks_search_dirs: Vec<std::path::PathBuf>,
}

impl HookRunner {
    /// Construct a new instance.
    pub fn new(settings: HooksSettings) -> Self {
        Self {
            settings: Arc::new(settings),
            default_timeout: Duration::from_millis(DEFAULT_HOOK_TIMEOUT_MS),
            prompt_executor: None,
            agent_executor: None,
            wasm_executor: None,
            net: std::sync::OnceLock::new(),
            once_executed: std::sync::Mutex::new(std::collections::HashSet::new()),
            file_watcher: std::sync::Mutex::new(None),
            pending_rewakes: std::sync::Mutex::new(std::collections::HashSet::new()),
            wake_receiver: std::sync::Mutex::new(None),
            hooks_search_dirs: Vec::new(),
        }
    }

    /// Builder: set the directories searched (in order, highest priority
    /// first) when resolving a bare script name in a `Command` hook's `PATH`.
    pub fn with_hooks_search_dirs(mut self, dirs: Vec<std::path::PathBuf>) -> Self {
        self.hooks_search_dirs = dirs;
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

    /// Builder: set default timeout.
    pub fn with_default_timeout(mut self, timeout: Duration) -> Self {
        self.default_timeout = timeout;
        self
    }

    /// 注入 Prompt hook 执行器。CLI 用 AnthropicClient 包一层注入。
    pub fn with_prompt_executor(mut self, executor: Arc<dyn PromptHookExecutor>) -> Self {
        self.prompt_executor = Some(executor);
        self
    }

    /// P1-10: 注入 Agent hook 执行器。CLI 用 AgentSpawner 包一层注入。
    /// Wire the backend that runs plugin-provided hooks. Absent in a build
    /// without the plugin subsystem, where `HookConfig::Wasm` entries are
    /// skipped with an explanation rather than silently ignored.
    pub fn with_wasm_executor(mut self, executor: Arc<dyn WasmHookExecutor>) -> Self {
        self.wasm_executor = Some(executor);
        self
    }

    pub fn with_agent_executor(mut self, executor: Arc<dyn AgentHookExecutor>) -> Self {
        self.agent_executor = Some(executor);
        self
    }

    /// P2: Builder-style setter for the wake channel receiver.
    /// The receiver is used by `check_rewakes()` to detect when background
    /// async work has completed and pending rewake hooks should be re-executed.
    pub fn with_wake_receiver(mut self, rx: tokio::sync::mpsc::UnboundedReceiver<()>) -> Self {
        self.wake_receiver = std::sync::Mutex::new(Some(rx));
        self
    }

    /// P2: Post-construction setter for the wake channel receiver.
    /// Works on `&self` (uses interior mutability) so it can be called on
    /// an `Arc<HookRunner>`.
    pub fn set_wake_receiver(&self, rx: tokio::sync::mpsc::UnboundedReceiver<()>) {
        *self.wake_receiver.lock().unwrap() = Some(rx);
    }

    /// Register a hook config at runtime (e.g. from plugin install).
    /// Uses `Arc::make_mut` to clone-on-write the inner settings.
    pub fn register_hook(&mut self, event: HookEvent, config: HookConfig) {
        let settings = Arc::make_mut(&mut self.settings);
        settings.entry(event).or_default().push(config);
    }

    /// Start watching filesystem paths and fire `HookEvent::FileChanged` hooks
    /// when files are modified, created, or deleted.
    ///
    /// Each change is debounced to `debounce_ms` (default 300ms). The hook
    /// payload includes the changed file path and the change type
    /// (`"created"`, `"modified"`, or `"deleted"`).
    ///
    /// The watcher runs in a background thread and dispatches hook execution
    /// on the current Tokio runtime. Calling this multiple times replaces the
    /// previous watcher — only the latest set of watched paths is active.
    ///
    /// Returns an error if the underlying filesystem watcher cannot be started.
    pub fn enable_file_watching(
        self: &Arc<Self>,
        paths: &[std::path::PathBuf],
        debounce_ms: u64,
    ) -> Result<(), String> {
        let mut watcher = crate::watcher::FileWatcher::new();
        watcher.watch_paths(paths, Arc::clone(self), debounce_ms)?;
        *self.file_watcher.lock().unwrap() = Some(watcher);
        Ok(())
    }

    fn net(&self) -> &dyn base::interface::exec::Network {
        &**self.net.get_or_init(|| {
            std::sync::Arc::new(base::interface::exec::local::LocalNetwork::default())
        })
    }

    /// Send this runner's HTTP hooks out through a given egress.
    ///
    /// Returns whether it took. The cell fills itself with the default egress
    /// the first time a hook fires, so a host that installs its own after that
    /// keeps the default — and a deployment that thinks it configured its
    /// egress and did not should hear about it rather than find out from a
    /// request that should not have left.
    #[must_use = "an egress that did not take is a policy that is not applied"]
    pub fn set_network(&self, net: std::sync::Arc<dyn base::interface::exec::Network>) -> bool {
        if self.net.set(net).is_err() {
            tracing::error!(
                "the hook runner's egress was already in use; the one just supplied is ignored"
            );
            return false;
        }
        true
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
    /// `server_name` identifies the MCP server that raised the elicitation.
    /// `url` is the elicitation URL found in its tool result.
    pub async fn run_elicitation(&self, server_name: &str, url: &str) -> HookRunResult {
        let mut tool_input = serde_json::Map::new();
        tool_input.insert(
            "server_name".into(),
            serde_json::Value::String(server_name.into()),
        );
        tool_input.insert("url".into(), serde_json::Value::String(url.into()));
        let input = crate::payload::HookInput {
            hook_event_name: "Elicitation".into(),
            session_id: String::new(),
            cwd: String::new(),
            permission_mode: "default".into(),
            tool_name: Some(format!("mcp_elicitation_{server_name}")),
            tool_input: Some(serde_json::Value::Object(tool_input)),
            tool_use_id: None,
            tool_result: None,
            is_error: None,
            user_prompt: None,
            ..Default::default()
        };
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
    /// `server_name` identifies the MCP server that initiated the elicitation.
    /// `url` is the elicitation URL the user responded to.
    /// `response` is the user's response data (optional).
    pub async fn run_elicitation_result(
        &self,
        server_name: &str,
        url: &str,
        response: Option<&serde_json::Value>,
    ) -> HookRunResult {
        let mut tool_input = serde_json::Map::new();
        tool_input.insert(
            "server_name".into(),
            serde_json::Value::String(server_name.into()),
        );
        tool_input.insert("url".into(), serde_json::Value::String(url.into()));
        if let Some(r) = response {
            tool_input.insert("response".into(), r.clone());
        }
        let input = crate::payload::HookInput {
            hook_event_name: "ElicitationResult".into(),
            session_id: String::new(),
            cwd: String::new(),
            permission_mode: "default".into(),
            tool_name: Some(format!("mcp_elicitation_{server_name}")),
            tool_input: Some(serde_json::Value::Object(tool_input)),
            tool_use_id: None,
            tool_result: response.cloned(),
            is_error: None,
            user_prompt: None,
            ..Default::default()
        };
        self.run(HookEvent::ElicitationResult, &input).await
    }

    /// 跑某事件的所有 hook。短路：第一个 `continue=false` 之后不再跑。
    /// 跑所有 hook **in parallel** (was: sequential stop-on-block).
    /// Aligns with TS `AsyncHookRegistry`'s `Promise.all` model.
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

        // Filter out already-executed `once` hooks so they don't fire again.
        let once_done = {
            let once_set = self.once_executed.lock().unwrap();
            configs
                .iter()
                .enumerate()
                .filter(|(idx, cfg)| {
                    let is_once = matches!(
                        cfg,
                        HookConfig::Command {
                            once: Some(true),
                            ..
                        }
                    );
                    !is_once || !once_set.contains(&(event, *idx))
                })
                .map(|(idx, _)| idx)
                .collect::<Vec<usize>>()
        }; // MutexGuard dropped here

        // Spawn each hook as an indexed future; collect into a Vec keyed by
        // registration index so the order in HookRunResult.outcomes is stable
        // regardless of completion order.
        let mut futures = futures::stream::FuturesUnordered::new();
        for &idx in &once_done {
            let cfg = configs[idx].clone();
            let input = input.clone();
            futures.push(async move { (idx, self.run_one(&cfg, &input).await) });
        }

        let mut indexed: Vec<(usize, HookOutcome)> = Vec::with_capacity(once_done.len());
        use futures::StreamExt;
        while let Some(item) = futures.next().await {
            indexed.push(item);
        }
        indexed.sort_by_key(|(i, _)| *i);

        // P2: Check for rewake signals in hook responses.
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
                            self.pending_rewakes.lock().unwrap().insert((event, idx));
                        }
                    }
                }
            }
        }

        // Mark executed `once` hooks so they don't fire on subsequent run() calls.
        let mut once_set = self.once_executed.lock().unwrap();
        for &idx in &once_done {
            once_set.insert((event, idx));
        }

        HookRunResult {
            outcomes: indexed.into_iter().map(|(_, o)| o).collect(),
        }
    }

    /// Run a single hook config, dispatching to the appropriate executor.
    /// `pub(super)` so that tests in submodules can call this directly if needed.
    pub(super) async fn run_one(&self, cfg: &HookConfig, input: &HookInput) -> HookOutcome {
        match cfg {
            HookConfig::Command {
                command,
                shell,
                timeout,
                if_pattern,
                only_on_error,
                ..
            } => {
                if let Some(pattern) = if_pattern {
                    if !matcher::if_matches(pattern, input) {
                        return HookOutcome::Skipped("if pattern did not match");
                    }
                }
                if only_on_error.unwrap_or(false) && input.is_error != Some(true) {
                    return HookOutcome::Skipped("only_on_error=true but tool was not in error");
                }
                let timeout_dur = timeout
                    .map(Duration::from_millis)
                    .unwrap_or(self.default_timeout);
                let shell = shell.as_deref().unwrap_or("bash");
                self.exec_command(shell, command, input, timeout_dur).await
            }
            HookConfig::Prompt {
                prompt,
                timeout,
                model,
            } => {
                self.exec_prompt(prompt, model.as_deref(), input, *timeout)
                    .await
            }
            HookConfig::Http {
                url,
                headers,
                timeout,
            } => self.exec_http(url, headers, input, *timeout).await,
            HookConfig::Agent {
                prompt,
                timeout,
                model,
            } => {
                self.exec_agent(prompt, model.as_deref(), input, *timeout)
                    .await
            }
            HookConfig::Wasm { plugin, timeout } => self.exec_wasm(plugin, input, *timeout).await,
        }
    }

    /// Hand the event to an installed plugin's component.
    ///
    /// Delegated to an injected executor for the same reason the prompt and
    /// agent backends are: this crate must not depend on wasmtime, or the
    /// hook dispatcher could not exist in a build with the plugin subsystem
    /// compiled out.
    async fn exec_wasm(
        &self,
        plugin: &str,
        input: &HookInput,
        timeout_ms: Option<u64>,
    ) -> HookOutcome {
        let Some(ref executor) = self.wasm_executor else {
            return HookOutcome::Skipped(
                "wasm hook: no WasmHookExecutor configured — plugins are unavailable in this build",
            );
        };
        match executor.execute(plugin, input, timeout_ms).await {
            Ok(response) => HookOutcome::Ran {
                stdout: response.message.clone().unwrap_or_default(),
                response,
                stderr: String::new(),
                exit_code: Some(0),
            },
            // A plugin that fails to answer is a plugin that has not
            // objected. Treating a fault as a block would let a broken
            // package halt every tool call in the session.
            Err(e) => {
                tracing::warn!(plugin = %plugin, error = %e, "wasm hook failed; proceeding");
                HookOutcome::Skipped("wasm hook: the plugin failed to answer")
            }
        }
    }

    /// P1-10: Execute an Agent-type hook. Delegates to the injected AgentHookExecutor.
    /// If no executor is available, returns Skipped with an explanation.
    async fn exec_agent(
        &self,
        prompt: &str,
        model: Option<&str>,
        input: &HookInput,
        _timeout_ms: Option<u64>,
    ) -> HookOutcome {
        let Some(ref executor) = self.agent_executor else {
            return HookOutcome::Skipped(
                "agent hook: no AgentHookExecutor configured — wire one at CLI/daemon startup",
            );
        };
        match executor.execute(prompt, model, input).await {
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
                    }
                } else {
                    HookOutcome::Ran {
                        response: crate::payload::HookResponse {
                            message: Some(response.clone()),
                            ..Default::default()
                        },
                        stdout: response,
                        stderr: String::new(),
                        exit_code: Some(0),
                    }
                }
            }
            Err(e) => HookOutcome::Error(format!("agent hook execution failed: {e}")),
        }
    }

    /// P2: Check for wake signals and re-execute any pending rewake hooks.
    ///
    /// Drains the wake receiver; if a wake signal (or channel close) is
    /// detected, re-runs all hooks in `pending_rewakes` with a minimal
    /// `WakeEvent` input and returns their responses.
    ///
    /// If no wake signal is available, the pending set is preserved so the
    /// hooks can be re-executed on a later call.
    pub async fn check_rewakes(&self) -> Vec<HookResponse> {
        // Drain the pending set under lock
        let pending: Vec<(HookEvent, usize)> = {
            let mut set = self.pending_rewakes.lock().unwrap();
            if set.is_empty() {
                return Vec::new();
            }
            set.drain().collect()
        };

        // Check for a wake signal on the channel
        let should_wake = {
            let mut rx_guard = self.wake_receiver.lock().unwrap();
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
            let mut set = self.pending_rewakes.lock().unwrap();
            set.extend(pending);
            return Vec::new();
        }

        // Build a minimal WakeEvent input for hook re-execution
        let wake_input = HookInput {
            hook_event_name: "WakeEvent".into(),
            session_id: String::new(),
            cwd: String::new(),
            permission_mode: "default".into(),
            tool_name: None,
            tool_input: None,
            tool_use_id: None,
            tool_result: None,
            is_error: None,
            user_prompt: None,
            ..Default::default()
        };

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
                },
                HookConfig::Command {
                    command: "echo peer-still-runs".into(),
                    shell: None,
                    timeout: None,
                    if_pattern: None,
                    only_on_error: None,
                    once: None,
                    async_rewake: None,
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
                },
                HookConfig::Command {
                    command: "echo second".into(),
                    shell: None,
                    timeout: None,
                    if_pattern: None,
                    only_on_error: None,
                    once: None,
                    async_rewake: None,
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
            "atta-hooks-test-{}-{}",
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

    fn tempfile_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "atta-hooks-test-{}-{}",
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

    /// Regression: `FileWatcher`/`enable_file_watching` existed fully
    /// implemented (debounced `notify` watcher → dispatches `FileChanged` on
    /// this runner) with zero production callers — `Builder::build()` never
    /// called it, so a configured `FileChanged` hook silently never fired.
    /// This exercises the real watcher end-to-end: modify a watched file,
    /// confirm the hook actually runs.
    #[tokio::test]
    async fn file_changed_hook_fires_on_watched_file_modification() {
        use std::io::Write;

        let unique = format!(
            "{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let path = std::env::temp_dir().join(format!("atta_hooks_watch_test_{unique}.txt"));
        let marker = std::env::temp_dir().join(format!("atta_hooks_watch_test_{unique}.marker"));
        std::fs::write(&path, "initial").unwrap();
        let _ = std::fs::remove_file(&marker);

        let mut s: HooksSettings = HashMap::new();
        s.insert(
            HookEvent::FileChanged,
            vec![HookConfig::Command {
                command: format!("echo fired >> {}", marker.display()),
                shell: None,
                timeout: None,
                if_pattern: None,
                only_on_error: None,
                once: None,
                async_rewake: None,
            }],
        );
        let r = Arc::new(HookRunner::new(s));
        r.enable_file_watching(std::slice::from_ref(&path), 50)
            .expect("watcher starts");

        // Let the watcher thread register with the OS before mutating the file.
        tokio::time::sleep(Duration::from_millis(200)).await;
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            writeln!(f, "changed").unwrap();
        }
        // Debounce window (50ms) + async hook dispatch + shell exec.
        tokio::time::sleep(Duration::from_millis(1500)).await;

        assert!(
            marker.exists(),
            "FileChanged hook should have fired and written the marker file"
        );

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&marker);
    }
}
