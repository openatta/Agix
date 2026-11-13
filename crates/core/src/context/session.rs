//! 会话可变状态 + 副作用注入接口。

use crate::error::EffectError;
use crate::permission::PermissionMode;
use async_trait::async_trait;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::goal::{GoalError, GoalView};
use super::ledger::SessionLedger;
use super::prompt::{PromptRequest, PromptResponse, SystemMessage};
use super::task::{
    RunningStatus, RunningTask, TaskEvent, TaskInfo, TaskType, TodoItem, TodoStatus,
};
use tokio::sync::broadcast;

/// 单次会话的可变状态。
///
/// 字段全部私有，走内部可变性。由设置推出来的部分在 [`SessionEnv`] 里，引擎
/// [`attach`](Self::attach) 时整体替换。
#[derive(Debug)]
pub struct SessionState {
    env: std::sync::RwLock<std::sync::Arc<SessionEnv>>,
    engine: std::sync::RwLock<Option<EngineView>>,
    /// 权限模式（运行时可变；EnterPlanMode / ExitPlanMode 工具会切）。
    /// 私有 + Mutex 内部可变 —— 调用方走 `permission_mode()` / `set_permission_mode()`
    /// / `enter_plan_mode()` / `exit_plan_mode()`。
    permission_mode: std::sync::Mutex<ModeSlot>,
    /// 模型自己维护的 todo 列表（TodoWrite 工具更新；/tasks 命令读取）。
    todos: std::sync::Mutex<Vec<TodoItem>>,
    /// The [`DurableState`] the session log last had.
    recorded_durable: std::sync::Mutex<DurableState>,
    /// 本会话已激活的 deferred 工具名集合。`EngineConfig::deferred_tools`
    /// 里的工具默认仅暴露 name + description；模型用 `ToolSearch` 命中后
    /// 会被加进这里，下一次 turn 的 build_request 把它升级到 full schema。
    activated_tools: std::sync::Mutex<HashSet<String>>,
    /// 后台跑的 sub-agent 任务。AgentTool 在 background=true 时把每个
    /// 任务塞进来；TaskOutput 工具按 id 查；TaskStop 触发 cancel。
    running_tasks: std::sync::Mutex<HashMap<String, std::sync::Arc<RunningTask>>>,
    /// Its tasks' events, for whoever shows this session's tasks — see
    /// [`SessionState::subscribe_tasks`].
    task_events: broadcast::Sender<TaskEvent>,
    /// This session's goal — see [`super::goal`].
    goal: std::sync::Mutex<super::goal::GoalSlot>,
    /// Goal snapshots changed but not yet written to the session log; `None`
    /// records a cleared goal.
    unrecorded_goal_changes: std::sync::Mutex<Vec<Option<super::goal::Goal>>>,
    goal_recording: std::sync::Arc<tokio::sync::Mutex<()>>,
    goal_events: broadcast::Sender<Option<GoalView>>,
    /// What the session's own agent should read at its next step boundary,
    /// each under the key it can be retracted by — see
    /// [`SessionState::post_notice`].
    notices: std::sync::Mutex<Vec<(Option<String>, String)>>,
    /// 已读取文件缓存。`FileRead` 成功后记录文件路径
    /// 与当时的 mtime；`FileEdit` / `FileWrite` 的 `validate_input` 用它做
    /// staleness 检测。
    read_cache: std::sync::Mutex<HashMap<PathBuf, FileReadEntry>>,
    /// 最近一次读取的文件路径，用于连续重复读取检测。
    last_read_path: std::sync::Mutex<Option<PathBuf>>,
    ledger: std::sync::Mutex<SessionLedger>,
    /// The model a host chose for this session since it was built, read by
    /// whichever engine runs the next turn.
    model_choice: std::sync::Mutex<Option<String>>,
}

/// What the engine running a session derives from its settings and scene.
/// Each engine brings its own, and [`SessionState::attach`] replaces the
/// session's whole.
#[derive(Debug, Clone, Default)]
pub struct SessionEnv {
    pub cwd: PathBuf,
    /// 用户级附加可写目录（`additional_directories`）—— Tool 在做路径校验时用
    pub additional_writable_dirs: Vec<PathBuf>,
    /// Directories outside the project the file tools may read without
    /// asking — `sandbox.read_directories`.
    pub read_directories: Vec<PathBuf>,
    /// `sandbox.read_anywhere`.
    pub read_anywhere: bool,
    /// Package and build caches a sandboxed command may write —
    /// `sandbox.cache_directories`; a `~/` path is expanded when the command
    /// runs.
    pub cache_directories: Vec<PathBuf>,
    /// Nobody answers this session's questions or permission prompts — see
    /// `SceneSpec::interactive`.
    pub unattended: bool,
    /// `sandbox.dangerously_disable_sandbox`, for checks made before a
    /// command runs.
    pub sandbox_disabled: bool,
    /// The permission mode the engine is configured to run in.
    pub base_mode: PermissionMode,
}

impl SessionEnv {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            ..Self::default()
        }
    }
}

/// What one engine hangs on the session's state while it runs the session:
/// its [`SessionEnv`], the registry its tool calls dispatch against, and the
/// providers they run on — so that a permission judging a call on this
/// session's behalf (a sub-agent's, judged by its parent) judges the tool that
/// will run, on the machine it will run on.
#[derive(Debug, Clone)]
pub struct EngineAttachment {
    env: std::sync::Arc<SessionEnv>,
    tools: Option<std::sync::Weak<dyn crate::tool::ToolRegistry>>,
    exec: crate::interface::exec::ExecProviders,
    generation: u64,
}

impl EngineAttachment {
    pub fn new(
        env: SessionEnv,
        exec: crate::interface::exec::ExecProviders,
        generation: u64,
    ) -> Self {
        Self {
            env: std::sync::Arc::new(env),
            tools: None,
            exec,
            generation,
        }
    }

    /// Held weakly: the registry's tools may hold this session.
    pub fn with_tools(mut self, tools: &std::sync::Arc<dyn crate::tool::ToolRegistry>) -> Self {
        self.tools = Some(std::sync::Arc::downgrade(tools));
        self
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }
}

#[derive(Debug)]
struct EngineView {
    tools: Option<std::sync::Weak<dyn crate::tool::ToolRegistry>>,
    exec: crate::interface::exec::ExecProviders,
    generation: u64,
    confinement: std::sync::Arc<std::sync::OnceLock<crate::interface::exec::ConfinementReport>>,
}

/// Log namespace and event under which [`DurableState`] is recorded.
pub const SESSION_STATE_NS: &str = "agix.session_state";
pub const SESSION_STATE_EVENT: &str = "change";

/// The part of a session's state that outlives its engine: what the model
/// set up for itself, which an engine brought back from the log must still
/// hold. The rest is derived from settings again or ends with the engine.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DurableState {
    #[serde(default)]
    pub plan: Option<PlanState>,
    #[serde(default)]
    pub todos: Vec<TodoItem>,
    /// The deferred tools `ToolSearch` loaded, sorted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub activated_tools: Vec<String>,
}

/// In plan mode, and where `ExitPlanMode` returns to: `None` is `Default`,
/// for a session that started out in plan mode.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PlanState {
    pub exit_to: Option<PermissionMode>,
}

#[derive(Debug, Default)]
struct ModeSlot {
    current: PermissionMode,
    before_plan: Option<PermissionMode>,
}

impl ModeSlot {
    /// Take up `base`, the mode an engine is configured with, unless in plan
    /// mode: that stays on, exiting to where it would have.
    fn rebase(&mut self, base: PermissionMode) {
        if self.current != PermissionMode::Plan {
            *self = ModeSlot {
                current: base,
                before_plan: None,
            };
        }
    }
}

/// 一次文件读取记录，包含读取时的 mtime。
/// `FileEdit` / `FileWrite` 的 `validate_input` 用它做 staleness 检测。
#[derive(Debug, Clone)]
pub struct FileReadEntry {
    /// 读取时的文件 mtime。
    pub mtime: SystemTime,
    /// 读取时的 offset（FileRead 的 offset 参数）。
    /// `None` 表示整文件读取（offset 未指定）。
    pub offset: Option<usize>,
    /// 读取时的 limit（FileRead 的 limit 参数）。
    /// `None` 表示读至文件末尾。
    pub limit: Option<usize>,
    /// Recorded by `Edit` / `Write` after they changed the file: the model
    /// knows what is in it, but saw a diff rather than the text, so a later
    /// `Read` still returns the content.
    pub written: bool,
}

impl SessionState {
    /// A state no engine is attached to yet: its [`env`](Self::env) is empty
    /// until one is.
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self {
            env: Default::default(),
            engine: std::sync::RwLock::new(None),
            permission_mode: std::sync::Mutex::new(ModeSlot::default()),
            todos: std::sync::Mutex::new(Vec::new()),
            recorded_durable: std::sync::Mutex::new(DurableState::default()),
            activated_tools: std::sync::Mutex::new(HashSet::new()),
            running_tasks: std::sync::Mutex::new(HashMap::new()),
            task_events: tokio::sync::broadcast::channel(256).0,
            notices: std::sync::Mutex::new(Vec::new()),
            goal: std::sync::Mutex::new(super::goal::GoalSlot::default()),
            unrecorded_goal_changes: std::sync::Mutex::new(Vec::new()),
            goal_recording: Default::default(),
            goal_events: tokio::sync::broadcast::channel(64).0,
            read_cache: std::sync::Mutex::new(HashMap::new()),
            last_read_path: std::sync::Mutex::new(None),
            ledger: Default::default(),
            model_choice: std::sync::Mutex::new(None),
        }
    }

    /// A state working in `cwd`, as a test needs one: no engine attached.
    pub fn for_tests(cwd: PathBuf) -> Self {
        Self::for_tests_in(SessionEnv::new(cwd))
    }

    /// A state with `env`, as a test needs one: no engine attached.
    pub fn for_tests_in(env: SessionEnv) -> Self {
        let state = Self::new();
        state.mode_slot().rebase(env.base_mode);
        *write(&state.env) = std::sync::Arc::new(env);
        state
    }

    /// Hang `engine` on this state: its [`SessionEnv`] replaces the one
    /// before, and the permission mode becomes the one it is configured with
    /// unless the session is in plan mode.
    pub fn attach(&self, engine: EngineAttachment) {
        let EngineAttachment {
            env,
            tools,
            exec,
            generation,
        } = engine;
        self.mode_slot().rebase(env.base_mode);
        *write(&self.env) = env;
        *write(&self.engine) = Some(EngineView {
            tools,
            exec,
            generation,
            confinement: Default::default(),
        });
    }

    /// Take off the engine attached as `generation`; one attached since stays.
    /// The env stays too: what that engine left running still works where it
    /// did.
    pub fn detach(&self, generation: u64) {
        let mut engine = write(&self.engine);
        if engine.as_ref().is_some_and(|e| e.generation == generation) {
            *engine = None;
        }
    }

    pub fn env(&self) -> std::sync::Arc<SessionEnv> {
        read(&self.env).clone()
    }

    /// Register work this session is running, under `cancel` — the token the
    /// work itself observes, so stopping the task stops the work.
    ///
    /// Its start is announced at once, except a shell command's or a
    /// monitor's: most of those finish in milliseconds, and a list that
    /// flickered with each one would hide the ones worth seeing. Those are
    /// announced only if still running a second later, and a task never
    /// announced ends without a word. From then on what they print goes out
    /// as `task_progress`, at most once a second, and once more as they end.
    pub fn register_task(
        &self,
        task_id: impl Into<String>,
        task_type: TaskType,
        cancel: tokio_util::sync::CancellationToken,
        detail: serde_json::Value,
    ) -> std::sync::Arc<RunningTask> {
        let task_id = task_id.into();
        let task =
            std::sync::Arc::new(RunningTask::with_cancel(task_id.clone(), task_type, cancel));
        *task.detail.lock().unwrap_or_else(|e| e.into_inner()) = detail;
        task.report_to(self.task_events.clone());
        self.running_tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(task_id, task.clone());
        let quiet = matches!(task_type, TaskType::Shell | TaskType::Monitor);
        if !quiet {
            self.announce(&task);
        } else if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let (task, events) = (task.clone(), self.task_events.clone());
            runtime.spawn(async move {
                let a_second = std::time::Duration::from_secs(1);
                tokio::select! {
                    _ = tokio::time::sleep(a_second) => {}
                    _ = task.wait_ended() => return,
                }
                if task.has_ended() {
                    return;
                }
                Self::announce_on(&events, &task);
                loop {
                    task.report_progress();
                    tokio::select! {
                        _ = tokio::time::sleep(a_second) => {}
                        _ = task.wait_ended() => return,
                    }
                }
            });
        }
        task
    }

    fn announce(&self, task: &RunningTask) {
        Self::announce_on(&self.task_events, task);
    }

    fn announce_on(events: &broadcast::Sender<TaskEvent>, task: &RunningTask) {
        if !task
            .announced
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            let _ = events.send(TaskEvent::Started { task: task.info() });
        }
    }

    /// Record that a task's work is over, having ended with `status`: its
    /// output closes and `task_finished` goes out. A task already ended —
    /// stopped by `cancel_running_task`, say, before its own code got here —
    /// keeps the ending it has. Finished tasks beyond the newest few are
    /// forgotten.
    pub fn finish_task(&self, task_id: &str, status: RunningStatus) {
        let Some(task) = self.find_running_task(task_id) else {
            return;
        };
        Self::mark_ended(&task, status);
        if Self::close_on(&self.task_events, &task) {
            self.forget_old_tasks();
        }
    }

    /// Set the task's end and `status`, unless it has ended already.
    fn mark_ended(task: &RunningTask, status: RunningStatus) {
        {
            let mut ended = task.ended_ms.lock().unwrap_or_else(|e| e.into_inner());
            if ended.is_some() {
                return;
            }
            *ended = Some(super::task::unix_ms());
            let mut current = task.status.lock().unwrap_or_else(|e| e.into_inner());
            if matches!(*current, RunningStatus::Running) {
                *current = status;
            }
        }
        task.ended.notify_waiters();
    }

    /// Close the task's output and announce `task_finished`; `false` if it
    /// was closed already.
    fn close_on(events: &broadcast::Sender<TaskEvent>, task: &RunningTask) -> bool {
        if !task.close_output() {
            return false;
        }
        if task.announced.load(std::sync::atomic::Ordering::SeqCst) {
            let _ = events.send(TaskEvent::Finished { task: task.info() });
        }
        true
    }

    /// How long stopped work has to wind down and reach its own
    /// [`finish_task`](Self::finish_task) before its output is closed for it;
    /// longer than the 10 s a stopped program gets to wrap up.
    const STOPPED_WORK_GRACE: std::time::Duration = std::time::Duration::from_secs(15);

    /// Tell this session's agent something it did not ask for in this turn,
    /// such as a background sub-agent having finished. A running turn reads
    /// it at its next step boundary; otherwise the next turn opens with it.
    ///
    /// A notice under `key` can be [retracted](Self::retract_notice) before
    /// it is read: one about a task the agent has since waited for tells it
    /// nothing new.
    pub fn post_notice(&self, key: Option<String>, text: impl Into<String>) {
        self.notices
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((key, text.into()));
    }

    /// Withdraw the unread notice under `key`; `false` if there was none —
    /// never posted, or already read.
    pub fn retract_notice(&self, key: &str) -> bool {
        let mut notices = self.notices.lock().unwrap_or_else(|e| e.into_inner());
        let before = notices.len();
        notices.retain(|(k, _)| k.as_deref() != Some(key));
        notices.len() != before
    }

    pub fn take_notices(&self) -> Vec<String> {
        std::mem::take(&mut *self.notices.lock().unwrap_or_else(|e| e.into_inner()))
            .into_iter()
            .map(|(_, text)| text)
            .collect()
    }

    pub fn ledger(&self) -> std::sync::MutexGuard<'_, SessionLedger> {
        lock(&self.ledger)
    }

    /// The model the next turn asks for, when a host chose one; `None` leaves
    /// the engine on the model it was built with.
    pub fn model_choice(&self) -> Option<String> {
        lock(&self.model_choice).clone()
    }

    pub fn set_model_choice(&self, model: Option<String>) {
        *lock(&self.model_choice) = model;
    }

    /// Put back what the session log recorded: the goal, disarmed, and the
    /// durable state. For a state no turn has run on yet.
    pub fn restore_from_log(&self, goal: Option<super::goal::Goal>, durable: Option<DurableState>) {
        self.restore_goal(goal);
        if let Some(durable) = durable {
            self.restore_durable_state(durable);
        }
    }

    const FINISHED_TASKS_KEPT: usize = 20;

    fn forget_old_tasks(&self) {
        let mut tasks = self.running_tasks.lock().unwrap_or_else(|e| e.into_inner());
        let mut finished: Vec<(i64, String)> = tasks
            .values()
            .filter(|t| t.output_closed())
            .filter_map(|t| {
                let ended = (*t.ended_ms.lock().unwrap_or_else(|e| e.into_inner()))?;
                Some((ended, t.task_id.clone()))
            })
            .collect();
        if finished.len() <= Self::FINISHED_TASKS_KEPT {
            return;
        }
        finished.sort();
        for (_, id) in finished
            .iter()
            .take(finished.len() - Self::FINISHED_TASKS_KEPT)
        {
            tasks.remove(id);
        }
    }

    /// Every task this session knows — running, and the most recently ended —
    /// oldest first.
    pub fn task_infos(&self) -> Vec<TaskInfo> {
        let mut tasks: Vec<std::sync::Arc<RunningTask>> = self
            .running_tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect();
        tasks.sort_by_key(|t| t.started_ms);
        tasks.iter().map(|t| t.info()).collect()
    }

    /// Its tasks' events as they happen.
    pub fn subscribe_tasks(&self) -> broadcast::Receiver<TaskEvent> {
        self.task_events.subscribe()
    }

    /// The goal as hosts and the model see it.
    pub fn goal_view(&self) -> Option<GoalView> {
        lock(&self.goal).view()
    }

    pub fn goal_create(
        &self,
        objective: String,
        max_rounds: Option<u32>,
        origin: super::goal::TurnOrigin,
    ) -> Result<GoalView, GoalError> {
        let mut slot = lock(&self.goal);
        slot.create(objective, max_rounds, origin)?;
        self.goal_changed(&slot, true).ok_or(GoalError::NoGoal)
    }

    pub fn goal_update(
        &self,
        goal_id: &str,
        revision: u64,
        action: super::goal::GoalAction,
        origin: super::goal::TurnOrigin,
    ) -> Result<GoalView, GoalError> {
        let mut slot = lock(&self.goal);
        slot.update(goal_id, revision, action, origin)?;
        self.goal_changed(&slot, true).ok_or(GoalError::NoGoal)
    }

    pub fn goal_clear(
        &self,
        origin: super::goal::TurnOrigin,
    ) -> Result<(), super::goal::GoalError> {
        let mut slot = lock(&self.goal);
        slot.clear(origin)?;
        self.goal_changed(&slot, true);
        Ok(())
    }

    /// Stop automatic rounds, keeping the goal as it is.
    pub fn goal_disarm(&self) {
        let mut slot = lock(&self.goal);
        if slot.disarm() {
            self.goal_changed(&slot, false);
        }
    }

    /// The next automatic round — see
    /// [`super::goal::GoalSlot::next_round`].
    pub fn goal_next_round(&self) -> Option<super::goal::GoalRound> {
        let mut slot = lock(&self.goal);
        let (round, changed) = slot.next_round();
        if changed {
            self.goal_changed(&slot, true);
        }
        round
    }

    /// Start the outstanding round `turn_id` — see
    /// [`super::goal::GoalSlot::claim_round`].
    pub fn goal_claim_round(&self, turn_id: &str) -> bool {
        let mut slot = lock(&self.goal);
        let claimed = slot.claim_round(turn_id);
        if claimed {
            self.goal_changed(&slot, true);
        }
        claimed
    }

    /// Give up the outstanding round `turn_id` without running it.
    pub fn goal_abandon_round(&self, turn_id: &str) {
        lock(&self.goal).abandon_round(turn_id);
    }

    /// Put back the goal a session log recorded, disarmed.
    fn restore_goal(&self, goal: Option<super::goal::Goal>) {
        *lock(&self.goal) = super::goal::GoalSlot::restored(goal);
    }

    /// Goal changes the session log does not have yet, oldest first, for the
    /// caller to write while it holds what this returns. Until then every
    /// other caller waits, so the log gets the changes in the order they were
    /// made, and a caller handed none knows none are still being written.
    pub async fn unrecorded_goal_changes(&self) -> super::goal::UnrecordedGoalChanges {
        let writing = self.goal_recording.clone().lock_owned().await;
        super::goal::UnrecordedGoalChanges {
            changes: std::mem::take(&mut *lock(&self.unrecorded_goal_changes)),
            _writing: writing,
        }
    }

    /// The goal's view each time it changes.
    pub fn subscribe_goal(&self) -> broadcast::Receiver<Option<GoalView>> {
        self.goal_events.subscribe()
    }

    fn goal_changed(&self, slot: &super::goal::GoalSlot, record: bool) -> Option<GoalView> {
        if record {
            lock(&self.unrecorded_goal_changes).push(slot.goal.clone());
        }
        let view = slot.view();
        let _ = self.goal_events.send(view.clone());
        view
    }

    /// 找一个 running task。返回 Arc 让 caller 读快照（output / status）。
    pub fn find_running_task(&self, task_id: &str) -> Option<std::sync::Arc<RunningTask>> {
        self.running_tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(task_id)
            .cloned()
    }

    /// 列出所有 running tasks 的 (id, status) 快照。
    pub fn list_running_tasks(&self) -> Vec<(String, RunningStatus)> {
        self.running_tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    v.status.lock().unwrap_or_else(|e| e.into_inner()).clone(),
                )
            })
            .collect()
    }

    /// Ids of the background work still running — sub-agents, teammates,
    /// programs, monitors; not a foreground shell command — sorted.
    pub fn running_background_task_ids(&self) -> Vec<String> {
        self.running_background_tasks()
            .iter()
            .map(|task| task.task_id.clone())
            .collect()
    }

    /// The background work still running, as
    /// [`running_background_task_ids`](Self::running_background_task_ids)
    /// picks it, sorted by id.
    pub fn running_background_tasks(&self) -> Vec<std::sync::Arc<RunningTask>> {
        let mut tasks: Vec<std::sync::Arc<RunningTask>> = self
            .running_tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .filter(|task| {
                !matches!(task.task_type, TaskType::Shell)
                    && matches!(
                        *task.status.lock().unwrap_or_else(|e| e.into_inner()),
                        RunningStatus::Running
                    )
            })
            .cloned()
            .collect();
        tasks.sort_by(|a, b| a.task_id.cmp(&b.task_id));
        tasks
    }

    /// Stop a running task: cancel what it runs under and record it as
    /// stopped. `false` if there is no such task or it had already ended.
    ///
    /// `task_finished` waits for the work to wind down — what it prints on
    /// the way out is part of its output — up to [`Self::STOPPED_WORK_GRACE`].
    pub fn cancel_running_task(&self, task_id: &str) -> bool {
        let Some(task) = self.find_running_task(task_id) else {
            return false;
        };
        if task.has_ended() {
            return false;
        }
        task.cancel.cancel();
        Self::mark_ended(&task, RunningStatus::Cancelled);
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                let events = self.task_events.clone();
                runtime.spawn(async move {
                    tokio::time::sleep(Self::STOPPED_WORK_GRACE).await;
                    Self::close_on(&events, &task);
                });
            }
            Err(_) => {
                Self::close_on(&self.task_events, &task);
            }
        }
        true
    }

    /// Builder：设初始权限模式（CLI 启动时从 settings/CLI 注入）。
    pub fn with_permission_mode(self, mode: PermissionMode) -> Self {
        self.set_permission_mode(mode);
        self
    }

    fn mode_slot(&self) -> std::sync::MutexGuard<'_, ModeSlot> {
        self.permission_mode
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// 读当前模式（PermissionMode 是 Copy，返回值就行）。
    pub fn permission_mode(&self) -> PermissionMode {
        self.mode_slot().current
    }

    /// 直接设模式，并忘掉进入计划模式前的那个。
    pub fn set_permission_mode(&self, mode: PermissionMode) {
        *self.mode_slot() = ModeSlot {
            current: mode,
            before_plan: None,
        };
    }

    /// 进计划模式，记下进入前的模式给 [`exit_plan_mode`](Self::exit_plan_mode)。
    /// 已经在计划模式里时什么都不改。
    pub fn enter_plan_mode(&self) {
        let mut slot = self.mode_slot();
        if slot.current != PermissionMode::Plan {
            slot.before_plan = Some(slot.current);
            slot.current = PermissionMode::Plan;
        }
    }

    /// 退出计划模式，回到进入前的模式；会话一开始就在计划模式里的回到 `Default`。
    pub fn exit_plan_mode(&self) -> PermissionMode {
        let mut slot = self.mode_slot();
        slot.current = slot.before_plan.take().unwrap_or_default();
        slot.current
    }

    pub fn durable_state(&self) -> DurableState {
        let plan = {
            let slot = self.mode_slot();
            (slot.current == PermissionMode::Plan).then_some(PlanState {
                exit_to: slot.before_plan,
            })
        };
        let mut activated_tools: Vec<String> = self.activated_tools().into_iter().collect();
        activated_tools.sort();
        DurableState {
            plan,
            todos: self.todos(),
            activated_tools,
        }
    }

    /// The durable state, when it differs from what the log last had. The
    /// caller writes it; from here on it counts as recorded.
    pub fn unrecorded_durable_state(&self) -> Option<DurableState> {
        let now = self.durable_state();
        let mut recorded = lock(&self.recorded_durable);
        if *recorded == now {
            return None;
        }
        *recorded = now.clone();
        Some(now)
    }

    /// Put back what the session log recorded. Outside plan mode the session
    /// keeps the mode it was built with.
    fn restore_durable_state(&self, state: DurableState) {
        if let Some(plan) = state.plan {
            *self.mode_slot() = ModeSlot {
                current: PermissionMode::Plan,
                before_plan: plan.exit_to,
            };
        }
        lock(&self.todos).clone_from(&state.todos);
        self.activate_tools(state.activated_tools.iter().cloned());
        *lock(&self.recorded_durable) = state;
    }

    /// 读取当前 todo 列表（克隆给调用方）
    pub fn todos(&self) -> Vec<TodoItem> {
        self.todos.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// TodoWrite 工具用：一次性替换整个列表。
    /// 如果全部 completed 则自动清空。
    pub fn set_todos(&self, items: Vec<TodoItem>) {
        let all_completed = items.iter().all(|t| t.status == TodoStatus::Completed);
        if all_completed && !items.is_empty() {
            *self.todos.lock().unwrap_or_else(|e| e.into_inner()) = Vec::new();
        } else {
            *self.todos.lock().unwrap_or_else(|e| e.into_inner()) = items;
        }
    }

    /// 记录一次文件读取。`FileRead` 工具成功后调用。
    /// 存入读取时的 mtime 与偏移范围，供后续去重（相同范围 + 不变 mtime）与
    /// Edit/Write staleness 检测。
    pub fn record_read(&self, path: &Path) {
        self.record_read_with_range(path, None, None);
    }

    /// Record a file read with offset/limit for precise dedup; also stores the mtime.
    pub fn record_read_with_range(&self, path: &Path, offset: Option<usize>, limit: Option<usize>) {
        if self.remember_file(path, offset, limit, false) {
            *self
                .last_read_path
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(path.to_path_buf());
        }
    }

    /// Record that `Edit` / `Write` just changed `path`, so the next edit of
    /// it does not ask for a `Read` first or call the session's own change
    /// an outside one.
    pub fn record_write(&self, path: &Path) {
        self.remember_file(path, None, None, true);
    }

    fn remember_file(
        &self,
        path: &Path,
        offset: Option<usize>,
        limit: Option<usize>,
        written: bool,
    ) -> bool {
        let Some(mtime) = std::fs::metadata(path).and_then(|m| m.modified()).ok() else {
            return false;
        };
        self.read_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                path.to_path_buf(),
                FileReadEntry {
                    mtime,
                    offset,
                    limit,
                    written,
                },
            );
        true
    }

    /// 检查文件是否已读且未过期。
    ///
    /// - 文件不存在于磁盘 → 跳过检查（返回 None）
    /// - 文件已读且 mtime 一致 → 返回 None（通过）
    /// - 文件未读 → 返回 "must be read" 错误信息
    /// - 文件已读但 mtime 不同 → 返回 "modified since read" 错误信息
    pub fn check_read_staleness(&self, path: &Path) -> Option<&'static str> {
        let meta = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(_) => return None, // file doesn't exist — skip staleness check
        };
        let current_mtime = match meta.modified() {
            Ok(t) => t,
            Err(_) => return None, // can't determine mtime — skip check
        };
        let cache = self.read_cache.lock().unwrap_or_else(|e| e.into_inner());
        match cache.get(path) {
            None => Some("File must be read before editing. Use the Read tool first."),
            Some(entry) if entry.mtime != current_mtime => {
                Some("File has been modified since it was read. Re-read it first.")
            }
            _ => None, // fresh
        }
    }

    /// 文件是否已读且未变化。用于 Read 工具跳过重复读取。
    /// 与 `check_read_staleness` 不同——后者给 Edit/Write 用，返回错误信息；
    /// 这个给 Read 自己用，只回答 bool。
    pub fn check_read_dedup(&self, path: &Path) -> bool {
        self.check_read_dedup_with_range(path, None, None)
    }

    /// Check read dedup with explicit offset/limit range.
    /// Only deduplicates when both the mtime AND the exact offset/limit range match.
    pub fn check_read_dedup_with_range(
        &self,
        path: &Path,
        offset: Option<usize>,
        limit: Option<usize>,
    ) -> bool {
        let meta = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(_) => return false,
        };
        let current_mtime = match meta.modified() {
            Ok(t) => t,
            Err(_) => return false,
        };
        let cache = self.read_cache.lock().unwrap_or_else(|e| e.into_inner());
        cache.get(path).is_some_and(|entry| {
            !entry.written
                && entry.mtime == current_mtime
                && entry.offset == offset
                && entry.limit == limit
        })
    }

    /// 给定路径是否与最近一次读取的文件一致。
    /// 仅检查路径相等；mtime 一致性由调用方通过 `check_read_dedup` 保证。
    pub fn is_consecutive_read(&self, path: &Path) -> bool {
        let last = self
            .last_read_path
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        last.as_ref().is_some_and(|p| p == path)
    }

    /// The providers the attached engine's tool calls run on, if one is
    /// attached.
    pub fn exec(&self) -> Option<crate::interface::exec::ExecProviders> {
        read(&self.engine).as_ref().map(|e| e.exec.clone())
    }

    /// What the attached engine's sandbox holds a command to: `probe`d the
    /// first time it is asked, the same answer after until another engine
    /// attaches. With none attached nothing is kept.
    pub fn confinement(
        &self,
        probe: impl FnOnce() -> crate::interface::exec::ConfinementReport,
    ) -> crate::interface::exec::ConfinementReport {
        let kept = read(&self.engine).as_ref().map(|e| e.confinement.clone());
        match kept {
            Some(kept) => *kept.get_or_init(probe),
            None => probe(),
        }
    }

    /// The tool this session's calls named `name` dispatch to, if the
    /// attached engine's registry holds it.
    pub fn tool(&self, name: &str) -> Option<std::sync::Arc<dyn crate::tool::Tool>> {
        let tools = read(&self.engine).as_ref()?.tools.as_ref()?.upgrade()?;
        tools.get(name)
    }

    /// 当前已激活的 deferred 工具名（克隆 set；调用方只读）。
    pub fn activated_tools(&self) -> HashSet<String> {
        self.activated_tools
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// ToolSearch 命中后调用：把工具加进 activated 集合。
    /// 多次调用 idempotent；空集合也允许。
    pub fn activate_tools<I>(&self, names: I)
    where
        I: IntoIterator<Item = String>,
    {
        let mut set = self
            .activated_tools
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for name in names {
            set.insert(name);
        }
    }
}

/// 副作用注入：工具想问用户、想发 OS 通知、想往 transcript 加 system message
/// 时走这里。Engine / TUI / CLI / 测试各自实现一份。
#[async_trait]
pub trait ToolEffects: Send + Sync {
    /// 向用户问问题。headless / 非交互场景返回 `EffectError::NotInteractive`，
    /// 由权限闸把它视为 deny。
    async fn ask_user(&self, request: PromptRequest) -> Result<PromptResponse, EffectError>;

    /// OS 级通知（iTerm2 / Kitty / Ghostty / 终端 bell）；headless 阶段无操作。
    fn os_notify(&self, _message: &str, _kind: &str) {}

    /// 往 transcript 追加一条 UI-only 的 system message（不进 API）。
    fn append_system_message(&self, _msg: SystemMessage) {}
}

/// 啥都不干的 ToolEffects：测试 / 一次性 print 模式 / 启动时占位。
pub struct NoopEffects;

#[async_trait]
impl ToolEffects for NoopEffects {
    async fn ask_user(&self, _: PromptRequest) -> Result<PromptResponse, EffectError> {
        Err(EffectError::NotInteractive)
    }
}

impl crate::interface::tool::RunningTasksCallback for SessionState {
    fn find(&self, tid: &str) -> Option<(String, Vec<String>, RunningStatus)> {
        let task = self.find_running_task(tid)?;
        let output = task.output_text();
        let log = task
            .events_log
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let status = task
            .status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        Some((output, log, status))
    }

    fn cancel(&self, tid: &str) -> bool {
        self.cancel_running_task(tid)
    }
}

/// A task that lasts as long as this value: registered on creation, ended on
/// drop — stopped if its token was cancelled, done otherwise. For work bounded
/// by one call, like a shell command.
pub struct TaskScope {
    session: std::sync::Arc<SessionState>,
    task: std::sync::Arc<RunningTask>,
}

impl TaskScope {
    pub fn new(
        session: std::sync::Arc<SessionState>,
        task_id: impl Into<String>,
        task_type: TaskType,
        cancel: tokio_util::sync::CancellationToken,
        detail: serde_json::Value,
    ) -> Self {
        let task = session.register_task(task_id, task_type, cancel, detail);
        Self { session, task }
    }

    pub fn task(&self) -> &std::sync::Arc<RunningTask> {
        &self.task
    }
}

impl Drop for TaskScope {
    fn drop(&mut self) {
        let status = if self.task.cancel.is_cancelled() {
            RunningStatus::Cancelled
        } else {
            RunningStatus::Completed
        };
        self.session.finish_task(&self.task.task_id, status);
    }
}

/// A task whose holder says how it ended: registered on creation, ended by
/// [`conclude`](Self::conclude). One dropped without concluding — its work
/// panicked, was dropped from a queue, or its holder simply went away — ends
/// as stopped, and the parent hears so when the lease was taken
/// [notifying](Self::notifying) it. No task held by a lease is left running.
pub struct TaskLease {
    session: std::sync::Arc<SessionState>,
    task: std::sync::Arc<RunningTask>,
    stopped_notice: Option<StoppedNotice>,
    concluded: bool,
}

type StoppedNotice = Box<dyn Fn(&RunningTask) -> String + Send + Sync>;

impl TaskLease {
    pub fn new(
        session: std::sync::Arc<SessionState>,
        task_id: impl Into<String>,
        task_type: TaskType,
        cancel: tokio_util::sync::CancellationToken,
        detail: serde_json::Value,
    ) -> Self {
        let task = session.register_task(task_id, task_type, cancel, detail);
        Self {
            session,
            task,
            stopped_notice: None,
            concluded: false,
        }
    }

    /// The notice the parent is posted when the lease is dropped unconcluded.
    pub fn notifying(
        mut self,
        notice: impl Fn(&RunningTask) -> String + Send + Sync + 'static,
    ) -> Self {
        self.stopped_notice = Some(Box::new(notice));
        self
    }

    pub fn task(&self) -> &std::sync::Arc<RunningTask> {
        &self.task
    }

    /// End the task with `status`. `notice` is posted to the session first,
    /// so whoever wakes on the task's end finds it there to withdraw; a task
    /// already stopped from outside gets none, since whoever stopped it knows.
    pub fn conclude(mut self, status: RunningStatus, notice: Option<String>) {
        self.concluded = true;
        if let Some(notice) = notice.filter(|_| !self.task.has_ended()) {
            self.session
                .post_notice(Some(self.task.task_id.clone()), notice);
        }
        self.session.finish_task(&self.task.task_id, status);
    }
}

impl Drop for TaskLease {
    fn drop(&mut self) {
        if self.concluded {
            return;
        }
        if let Some(notice) = self.stopped_notice.as_ref() {
            if !self.task.has_ended() {
                self.session
                    .post_notice(Some(self.task.task_id.clone()), notice(&self.task));
            }
        }
        self.session
            .finish_task(&self.task.task_id, RunningStatus::Cancelled);
    }
}

fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn read<T>(l: &std::sync::RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    l.read().unwrap_or_else(|e| e.into_inner())
}

fn write<T>(l: &std::sync::RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    l.write().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn todo(content: &str) -> TodoItem {
        TodoItem {
            content: content.into(),
            status: TodoStatus::Pending,
            active_form: content.into(),
        }
    }

    #[test]
    fn plan_mode_and_todos_come_back_from_what_the_log_recorded() {
        let before = SessionState::for_tests("/tmp".into())
            .with_permission_mode(PermissionMode::AcceptEdits);
        assert_eq!(before.unrecorded_durable_state(), None);
        before.enter_plan_mode();
        before.set_todos(vec![todo("step one")]);
        let recorded = before.unrecorded_durable_state().expect("a change");
        assert_eq!(before.unrecorded_durable_state(), None);
        let logged = serde_json::to_value(&recorded).unwrap();
        assert_eq!(
            logged,
            serde_json::json!({
                "plan": { "exit_to": "acceptEdits" },
                "todos": [{ "content": "step one", "status": "pending", "active_form": "step one" }]
            })
        );

        let after = SessionState::for_tests("/tmp".into())
            .with_permission_mode(PermissionMode::AcceptEdits);
        after.restore_durable_state(serde_json::from_value(logged).unwrap());
        assert_eq!(after.permission_mode(), PermissionMode::Plan);
        assert_eq!(after.todos(), vec![todo("step one")]);
        assert_eq!(after.unrecorded_durable_state(), None);
        assert_eq!(after.exit_plan_mode(), PermissionMode::AcceptEdits);
        assert!(after.unrecorded_durable_state().is_some());
    }

    #[test]
    fn the_tools_tool_search_loaded_come_back_from_the_log() {
        let before = SessionState::for_tests("/tmp".into());
        before.activate_tools(["WebFetch".to_string(), "NotebookEdit".to_string()]);
        let logged =
            serde_json::to_value(before.unrecorded_durable_state().expect("a change")).unwrap();
        assert_eq!(
            logged["activated_tools"],
            serde_json::json!(["NotebookEdit", "WebFetch"])
        );

        let after = SessionState::for_tests("/tmp".into());
        after.restore_durable_state(serde_json::from_value(logged).unwrap());
        assert!(after.activated_tools().contains("WebFetch"));
        assert_eq!(after.unrecorded_durable_state(), None);
    }

    #[test]
    fn a_session_restored_out_of_plan_mode_keeps_the_mode_it_was_built_with() {
        let after =
            SessionState::for_tests("/tmp".into()).with_permission_mode(PermissionMode::Default);
        after.restore_durable_state(serde_json::from_value(serde_json::json!({})).unwrap());
        assert_eq!(after.permission_mode(), PermissionMode::Default);
        assert!(after.todos().is_empty());
    }

    fn attachment(cwd: &str, base_mode: PermissionMode, generation: u64) -> EngineAttachment {
        EngineAttachment::new(
            SessionEnv {
                base_mode,
                ..SessionEnv::new(cwd.into())
            },
            crate::interface::exec::ExecProviders::in_process(),
            generation,
        )
    }

    #[test]
    fn an_attached_engine_brings_its_env_and_mode_but_plan_mode_stays() {
        let state = SessionState::new();
        state.attach(attachment("/one", PermissionMode::AcceptEdits, 1));
        assert_eq!(state.env().cwd, PathBuf::from("/one"));
        assert_eq!(state.permission_mode(), PermissionMode::AcceptEdits);

        state.attach(attachment("/two", PermissionMode::Default, 2));
        assert_eq!(state.env().cwd, PathBuf::from("/two"));
        assert_eq!(state.permission_mode(), PermissionMode::Default);

        state.set_permission_mode(PermissionMode::AcceptEdits);
        state.enter_plan_mode();
        state.attach(attachment("/three", PermissionMode::BypassPermissions, 3));
        assert_eq!(state.permission_mode(), PermissionMode::Plan);
        assert_eq!(state.exit_plan_mode(), PermissionMode::AcceptEdits);
    }

    #[test]
    fn an_engine_detaches_only_itself() {
        let state = SessionState::new();
        state.attach(attachment("/one", PermissionMode::Default, 1));
        state.attach(attachment("/two", PermissionMode::Default, 2));
        state.detach(1);
        assert!(state.exec().is_some(), "the engine attached since stays");
        state.detach(2);
        assert!(state.exec().is_none());
        assert_eq!(state.env().cwd, PathBuf::from("/two"));
    }

    fn leased(session: &std::sync::Arc<SessionState>, id: &str) -> TaskLease {
        TaskLease::new(
            session.clone(),
            id,
            TaskType::Agent,
            tokio_util::sync::CancellationToken::new(),
            serde_json::json!({}),
        )
        .notifying(|task| format!("{} stopped", task.task_id))
    }

    #[test]
    fn a_concluded_lease_ends_its_task_as_it_says() {
        let session = std::sync::Arc::new(SessionState::for_tests("/tmp".into()));
        leased(&session, "t1").conclude(
            RunningStatus::Failed("boom".into()),
            Some("t1 failed".into()),
        );
        let task = session.find_running_task("t1").expect("kept after it ends");
        assert_eq!(
            *task.status.lock().unwrap(),
            RunningStatus::Failed("boom".into())
        );
        assert_eq!(session.take_notices(), ["t1 failed"]);
    }

    #[test]
    fn a_lease_dropped_unconcluded_ends_its_task_stopped_and_says_so() {
        let session = std::sync::Arc::new(SessionState::for_tests("/tmp".into()));
        drop(leased(&session, "t2"));
        let task = session.find_running_task("t2").expect("kept after it ends");
        assert!(task.has_ended());
        assert_eq!(*task.status.lock().unwrap(), RunningStatus::Cancelled);
        assert_eq!(session.take_notices(), ["t2 stopped"]);
    }

    #[test]
    fn a_task_stopped_from_outside_gets_no_notice_from_its_lease() {
        let session = std::sync::Arc::new(SessionState::for_tests("/tmp".into()));
        let lease = leased(&session, "t3");
        session.finish_task("t3", RunningStatus::Cancelled);
        lease.conclude(RunningStatus::Completed, Some("t3 done".into()));
        assert!(session.take_notices().is_empty());
    }
}
