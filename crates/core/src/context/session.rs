//! 会话可变状态 + 文件快照 + 副作用注入接口。

/// Trait for persistent task storage — implemented by the `task` crate.
/// Core uses this trait to avoid a circular dependency on `task`.
pub trait TaskPersist: Send + Sync + std::fmt::Debug {
    fn persist_tasks(&self, tasks: &[serde_json::Value]) -> Result<(), String>;
    fn load_tasks(&self) -> Result<Vec<serde_json::Value>, String>;
}

/// Trait for persistent running-task storage — implemented by the `task` crate.
pub trait RunningTaskPersist: Send + Sync + std::fmt::Debug {
    fn persist_task(&self, task_data: serde_json::Value) -> Result<(), String>;
    fn remove_task(&self, task_id: &str) -> Result<(), String>;
    fn load_stale_tasks(&self) -> Vec<serde_json::Value>;
}

use crate::error::EffectError;
use crate::permission::PermissionMode;
use crate::session::SessionId;
use async_trait::async_trait;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::prompt::{PromptRequest, PromptResponse, SystemMessage};
use super::task::{RunningStatus, RunningTask, TaskType, TodoItem, TodoStatus};

/// 单次会话的可变状态。M1 起始字段集；M2+ 加 messages / cost / file_state 等。
///
/// 多数字段是**裸的** —— 用 `Arc<SessionState>` 共享时不持有内部锁；可变状态
/// （permission_mode / todos）走 `Mutex` 内部可变性。
#[derive(Debug)]
pub struct SessionState {
    pub session_id: SessionId,
    pub cwd: PathBuf,
    pub started_at: time::OffsetDateTime,
    /// 用户级附加可写目录（`additional_directories`）—— Tool 在做路径校验时用
    pub additional_writable_dirs: Vec<PathBuf>,
    /// 权限模式（运行时可变；EnterPlanMode / ExitPlanMode 工具会切）。
    /// 私有 + Mutex 内部可变 —— 调用方走 `permission_mode()` / `set_permission_mode()`。
    permission_mode: std::sync::Mutex<PermissionMode>,
    /// 模型自己维护的 todo 列表（TodoWrite 工具更新；/tasks 命令读取）。
    todos: std::sync::Mutex<Vec<TodoItem>>,
    /// **M4 phase 4**：本会话已激活的 deferred 工具名集合。`Tool::is_deferred()`
    /// 为 true 的工具默认仅暴露 name + description；模型用 `ToolSearch` 命中后
    /// 会被加进这里，下一次 turn 的 build_request 把它升级到 full schema。
    activated_tools: std::sync::Mutex<HashSet<String>>,
    /// **M6**: 结构化任务列表。Task{Create,Get,List,Update,Stop} 工具的后端。
    /// 用 serde_json::Value 存避免跨 crate 类型循环（agix-tools 定义
    /// TaskEntry，agix-core 不能依赖 tools）。caller 序列化进来 / 反序列化
    /// 出去。
    tasks: std::sync::Mutex<Vec<serde_json::Value>>,
    /// **P0 (M31)**: 文件持久化 task store. Some 时所有 add/remove/update 同步
    /// 写到文件系统，支持跨 session resume。None 时退化为纯内存（向后兼容）。
    /// 注入来自 `task` crate，通过 `TaskPersist` trait 避免循环依赖。
    pub task_store: Option<std::sync::Arc<dyn TaskPersist>>,
    /// **P2 (M31)**: 文件持久化 running task store。注入来自 `task` crate。
    /// 通过 `RunningTaskPersist` trait 避免循环依赖。
    pub running_task_store: Option<std::sync::Arc<dyn RunningTaskPersist>>,
    /// **M9**: 后台跑的 sub-agent 任务。AgentTool 在 background=true 时把每个
    /// 任务塞进来；TaskOutput 工具按 id 查；TaskStop 触发 cancel。
    running_tasks: std::sync::Mutex<HashMap<String, std::sync::Arc<RunningTask>>>,
    /// **M11.2**: 本 turn 内的权限拒绝累积。Engine.run_turn 开头 reset，
    /// turn_complete 时打包到事件里。
    permission_denials: std::sync::Mutex<Vec<PermissionDenial>>,
    /// **P1 (M30)**: 文件历史快照。每次 FileWrite/FileEdit/NotebookEdit 改文件
    /// 之前，先把"原内容"快照进来（按 turn 编号 + 文件路径分桶）。`/rewind`
    /// 命令展示并恢复。新文件首次写入时存 `None` 作为"原状态" → restore = 删除
    /// 文件。
    file_snapshots: std::sync::Mutex<FileSnapshotRegistry>,
    /// **P1 (M30)**: 当前 turn 编号。Engine.run_turn 开头 +1；快照记录时取此值。
    current_turn: std::sync::Mutex<u64>,
    /// Last turn number when TodoWrite was called. None if never called within
    /// this session. Used by engine to inject todo reminders if stale for 10+ turns.
    last_todo_write_turn: std::sync::Mutex<Option<u64>>,
    /// **P1.2**: Completed-task count at which the last verification nudge was
    /// injected. Used by engine to avoid re-nudging every turn — only nudge
    /// when the delta since last nudge >= 3.
    last_verification_nudge_count: std::sync::Mutex<u32>,
    /// **P2**: Current active plan text (set by EnterPlanMode, cleared by
    /// ExitPlanMode). Persisted to disk via plan_store for resume support.
    /// Engine injects this into the system reminder on follow-up turns.
    plan_text: std::sync::Mutex<Option<String>>,
    /// **P3**: Current active plan slug (derived from plan text, usable as
    /// filename base). Set/cleared alongside plan_text.
    plan_slug: std::sync::Mutex<Option<String>>,
    /// **P (2026-05-17)**: 项目级授权积累的沙盒放行路径。用户在 Ask 对话框选
    /// "allow for this project" 后，Engine 向此列表添加路径。Bash 工具读取并
    /// 注入 sandbox profile 的 additional_writable。
    sandbox_allow_writes: std::sync::Mutex<Vec<PathBuf>>,
    /// **P (read-before-edit)**: 已读取文件缓存。`FileRead` 成功后记录文件路径
    /// 与当时的 mtime；`FileEdit` / `FileWrite` 的 `validate_input` 用它做
    /// staleness 检测。
    read_cache: std::sync::Mutex<HashMap<PathBuf, FileReadEntry>>,
    /// **P (read dedup)**: 最近一次读取的文件路径，用于连续重复读取检测。
    last_read_path: std::sync::Mutex<Option<PathBuf>>,
    /// What `StructuredOutput` last accepted in the current turn.
    structured_output: std::sync::Mutex<Option<serde_json::Value>>,
}

/// **P1 (M30)**: 一条文件快照。把"工具改之前"的状态存下来，用于 `/rewind`。
#[derive(Debug, Clone)]
pub struct FileSnapshot {
    /// 单调递增 id（把不同 turn / 同一 turn 内多次修改区分开）
    pub id: u64,
    /// 这次修改属于哪一 turn（用于 `/rewind to <turn>` 一次回退多文件）
    pub turn: u64,
    pub path: std::path::PathBuf,
    /// 改之前的字节。None = 文件原本不存在（restore = 删除）
    pub before: Option<Vec<u8>>,
    /// 操作来源工具名（"Write" / "Edit" / "NotebookEdit"），仅用于展示
    pub tool_name: String,
    pub recorded_at: time::OffsetDateTime,
}

#[derive(Debug, Default)]
pub struct FileSnapshotRegistry {
    next_id: u64,
    entries: Vec<FileSnapshot>,
    /// 防止"同一 turn 同一文件被改 N 次时 N 个快照"。每条 (turn, path) 只保留
    /// **第一**次的快照（其余 mutation 后续也只是从 first-snapshot 出发）。
    first_seen_per_turn: HashSet<(u64, PathBuf)>,
}

impl FileSnapshotRegistry {
    /// Append a snapshot row (called by FileEdit/FileWrite before mutation).
    pub fn record(
        &mut self,
        turn: u64,
        path: PathBuf,
        before: Option<Vec<u8>>,
        tool_name: impl Into<String>,
    ) {
        let key = (turn, path.clone());
        if !self.first_seen_per_turn.insert(key) {
            // already snapshot'd this file in this turn; later mutations
            // chain on top of the first snapshot
            return;
        }
        self.next_id += 1;
        self.entries.push(FileSnapshot {
            id: self.next_id,
            turn,
            path,
            before,
            tool_name: tool_name.into(),
            recorded_at: time::OffsetDateTime::now_utc(),
        });
    }

    /// All snapshots recorded in this session, in chronological order.
    pub fn entries(&self) -> &[FileSnapshot] {
        &self.entries
    }

    /// All snapshots whose `turn` is strictly greater than `target_turn`.
    /// `/rewind to N` restores these in reverse order (newest first).
    pub fn entries_after(&self, target_turn: u64) -> Vec<&FileSnapshot> {
        self.entries
            .iter()
            .filter(|s| s.turn > target_turn)
            .collect()
    }

    /// Look up a single snapshot by id (None if not found).
    pub fn entry_by_id(&self, id: u64) -> Option<&FileSnapshot> {
        self.entries.iter().find(|s| s.id == id)
    }

    /// Distinct turn numbers that have any snapshot, ascending.
    pub fn turns_with_snapshots(&self) -> Vec<u64> {
        let mut out: Vec<u64> = self.entries.iter().map(|s| s.turn).collect();
        out.sort();
        out.dedup();
        out
    }
}

/// **P (read-before-edit)**: 一次文件读取记录，包含读取时的 mtime。
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
}

/// 权限拒绝记录（M11.2）。一次工具调用因权限被拒时记一行。
#[derive(Debug, Clone, serde::Serialize)]
pub struct PermissionDenial {
    pub tool_name: String,
    pub tool_use_id: String,
    pub reason: String,
}

impl SessionState {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            session_id: SessionId::new(),
            cwd,
            started_at: time::OffsetDateTime::now_utc(),
            additional_writable_dirs: Vec::new(),
            permission_mode: std::sync::Mutex::new(PermissionMode::Default),
            todos: std::sync::Mutex::new(Vec::new()),
            activated_tools: std::sync::Mutex::new(HashSet::new()),
            tasks: std::sync::Mutex::new(Vec::new()),
            task_store: None,
            running_task_store: None,
            running_tasks: std::sync::Mutex::new(HashMap::new()),
            permission_denials: std::sync::Mutex::new(Vec::new()),
            file_snapshots: std::sync::Mutex::new(FileSnapshotRegistry::default()),
            current_turn: std::sync::Mutex::new(0),
            last_todo_write_turn: std::sync::Mutex::new(None),
            last_verification_nudge_count: std::sync::Mutex::new(0),
            plan_text: std::sync::Mutex::new(None),
            plan_slug: std::sync::Mutex::new(None),
            sandbox_allow_writes: std::sync::Mutex::new(Vec::new()),
            read_cache: std::sync::Mutex::new(HashMap::new()),
            last_read_path: std::sync::Mutex::new(None),
            structured_output: std::sync::Mutex::new(None),
        }
    }

    // ---- P1 (M30): file-history snapshot ----

    /// Increment + return the new turn number. Engine.run_turn calls this once
    /// per turn (right at the start) so subsequent snapshots tag with the
    /// new turn id.
    pub fn bump_turn(&self) -> u64 {
        let mut t = self.current_turn.lock().unwrap_or_else(|e| e.into_inner());
        *t += 1;
        *t
    }

    pub fn current_turn(&self) -> u64 {
        *self.current_turn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Snapshot a file's pre-mutation state. If the file doesn't exist, store
    /// `None` (restore = delete). Re-records on the same (turn, path) are
    /// no-ops — the first snapshot in a turn is the canonical "before" state.
    pub fn snapshot_file(&self, path: &std::path::Path, tool_name: impl Into<String>) {
        let turn = self.current_turn();
        let before = match std::fs::read(path) {
            Ok(bytes) => Some(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => return, // unreadable for some other reason; don't fail the tool
        };
        self.file_snapshots
            .lock()
            .unwrap()
            .record(turn, path.to_path_buf(), before, tool_name);
    }

    /// Read-only access to the snapshot registry (for `/rewind` listing).
    pub fn file_snapshots_snapshot(&self) -> Vec<FileSnapshot> {
        self.file_snapshots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entries()
            .to_vec()
    }

    pub fn file_snapshot_turns(&self) -> Vec<u64> {
        self.file_snapshots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .turns_with_snapshots()
    }

    /// Restore a single snapshot by id. Returns the path that was restored.
    pub fn restore_snapshot(&self, id: u64) -> Result<PathBuf, std::io::Error> {
        let snap = {
            let reg = self
                .file_snapshots
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            reg.entry_by_id(id).cloned().ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::NotFound, "snapshot not found")
            })?
        };
        match &snap.before {
            Some(bytes) => std::fs::write(&snap.path, bytes)?,
            None => match std::fs::remove_file(&snap.path) {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            },
        }
        Ok(snap.path)
    }

    /// Restore every snapshot taken **after** `target_turn`. Used by
    /// `/rewind to <turn>` — bring the FS back to whatever it was at the end
    /// of `target_turn`. Restores newest → oldest so file-A-modified-twice
    /// ends up at the older state, not the intermediate one.
    pub fn restore_to_turn(&self, target_turn: u64) -> Result<Vec<PathBuf>, std::io::Error> {
        let to_restore: Vec<FileSnapshot> = {
            let reg = self
                .file_snapshots
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            reg.entries_after(target_turn)
                .into_iter()
                .cloned()
                .collect()
        };
        let mut restored = Vec::new();
        // newest first — apply older snapshots last so they win on duplicate paths
        for snap in to_restore.into_iter().rev() {
            match &snap.before {
                Some(bytes) => std::fs::write(&snap.path, bytes)?,
                None => {
                    if let Err(e) = std::fs::remove_file(&snap.path) {
                        if e.kind() != std::io::ErrorKind::NotFound {
                            return Err(e);
                        }
                    }
                }
            }
            if !restored.iter().any(|p| p == &snap.path) {
                restored.push(snap.path);
            }
        }
        Ok(restored)
    }

    // ---- M11.2: permission denials per turn ----
    /// Append a snapshot row (called by FileEdit/FileWrite before mutation).
    pub fn record_denial(
        &self,
        tool_name: impl Into<String>,
        tool_use_id: impl Into<String>,
        reason: impl Into<String>,
    ) {
        self.permission_denials
            .lock()
            .unwrap()
            .push(PermissionDenial {
                tool_name: tool_name.into(),
                tool_use_id: tool_use_id.into(),
                reason: reason.into(),
            });
    }

    pub fn drain_denials(&self) -> Vec<PermissionDenial> {
        std::mem::take(
            &mut *self
                .permission_denials
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
        )
    }

    /// P2: Check if any tool has been denied 3+ times in this session.
    /// Returns the tool name and count if the escalation threshold is met.
    pub fn check_denial_escalation(&self) -> Option<(String, usize)> {
        let denials = self
            .permission_denials
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for d in denials.iter() {
            *counts.entry(d.tool_name.clone()).or_insert(0) += 1;
        }
        counts.into_iter().find(|(_, c)| *c >= 3)
    }

    // ---- M9: running tasks (background sub-agent tracking) ----

    /// 注册一个新的 running task；返回 Arc 让 caller 持着写 output。
    /// 如果 running_task_store 已配置，同步 persist 初始状态。
    pub fn register_running_task(&self, task_id: impl Into<String>) -> std::sync::Arc<RunningTask> {
        let task_id = task_id.into();
        let task = std::sync::Arc::new(RunningTask::new(task_id.clone(), TaskType::Agent));
        self.running_tasks
            .lock()
            .unwrap()
            .insert(task_id, task.clone());
        // TODO: running_task_store persistence — downcast in task crate layer
        task
    }

    /// Persist a running task's current state to disk (fire-and-forget).
    /// TODO: running_task_store persistence — downcast in task crate layer
    pub fn persist_running_task(&self, _task: &RunningTask) {
        // Stub: task crate injects concrete store and handles this.
    }

    /// Remove a running task's persisted file (fire-and-forget).
    /// TODO: running_task_store persistence — downcast in task crate layer
    pub fn remove_running_task_persistence(&self, _task_id: &str) {
        // Stub: task crate injects concrete store and handles this.
    }

    /// Load stale running tasks from disk (crash recovery).
    /// Returns tasks that survived a process restart, with status set to
    /// `Failed("process restarted")`.
    /// TODO: Load stale running tasks from disk (crash recovery).
    /// Needs concrete RunningTaskStore type from `task` crate.
    /// The store is injected via `with_running_task_store` as `Arc<dyn Any>`.
    /// Downcast to the concrete type in the `task` crate's integration layer.
    pub fn load_stale_running_tasks(&self) -> Vec<serde_json::Value> {
        // Stub: task crate injects concrete store and handles this.
        Vec::new()
    }

    /// Rehydrate a stale `RunningTask` from disk data and register it.
    /// Tasks flagged `Failed("process restarted")` are injected into
    /// `self.running_tasks` so `TaskOutput` can report the outcome.
    /// Rehydrate a stale `RunningTask` from disk data and register it.
    /// TODO: Needs concrete RunningTaskData from `task` crate.
    /// Caller (in task crate) should handle this via the public fields.
    pub fn insert_running_task(&self, task_id: String, task: std::sync::Arc<RunningTask>) {
        self.running_tasks.lock().unwrap().insert(task_id, task);
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
            .unwrap()
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    v.status.lock().unwrap_or_else(|e| e.into_inner()).clone(),
                )
            })
            .collect()
    }

    /// 主动取消一个 running task；返回是否找到。caller 后续调用 find_running_task
    /// 看 status 应该会变成 Cancelled。
    pub fn cancel_running_task(&self, task_id: &str) -> bool {
        if let Some(task) = self
            .running_tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(task_id)
        {
            task.cancel.cancel();
            true
        } else {
            false
        }
    }

    /// **M6**: Task 工具用 —— 用 generic helpers 让 agix-tools 操作 tasks
    /// 而不引 cycle。`T: Serialize + DeserializeOwned + Clone` 即可。
    /// 如果 task_store 已设，同时异步写到文件系统（fire-and-forget）。
    pub fn add_task<T: serde::Serialize>(&self, task: T) {
        if let Ok(v) = serde_json::to_value(&task) {
            // TODO: task_store persistence — downcast from Arc<dyn Any> in task crate layer
            self.tasks.lock().unwrap_or_else(|e| e.into_inner()).push(v);
        }
    }

    /// 按 id 从内存缓存删除任务。如果 task_store 已设，同时删文件。
    pub fn remove_task(&self, id: &str) -> bool {
        let mut found = false;
        {
            let mut tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(pos) = tasks
                .iter()
                .position(|v| v.get("id").and_then(|i| i.as_str()) == Some(id))
            {
                tasks.remove(pos);
                found = true;
            }
        }
        if found {
            // TODO: task_store persistence — downcast from Arc<dyn Any> in task crate layer
        }
        found
    }

    /// Set a file-backed task store for persistence.
    /// Inject a file-backed task store for persistence.
    /// The store type-erased via `Arc<dyn Any>` (core cannot depend on task crate).
    /// Inject a file-backed task store for persistence (implements [`TaskPersist`]).
    pub fn with_task_store(mut self, store: std::sync::Arc<dyn TaskPersist>) -> Self {
        self.task_store = Some(store);
        self
    }

    /// Inject a file-backed running task store for crash recovery (implements [`RunningTaskPersist`]).
    pub fn with_running_task_store(
        mut self,
        store: std::sync::Arc<dyn RunningTaskPersist>,
    ) -> Self {
        self.running_task_store = Some(store);
        self
    }

    pub fn tasks<T: serde::de::DeserializeOwned>(&self) -> Vec<T> {
        self.tasks
            .lock()
            .unwrap()
            .iter()
            .filter_map(|v| serde_json::from_value(v.clone()).ok())
            .collect()
    }

    pub fn find_task<T: serde::de::DeserializeOwned>(&self, id: &str) -> Option<T> {
        self.tasks
            .lock()
            .unwrap()
            .iter()
            .find(|v| v.get("id").and_then(|i| i.as_str()) == Some(id))
            .and_then(|v| serde_json::from_value(v.clone()).ok())
    }

    /// 找 id 匹配的 task，调 `mutate` 改它，返回更新后的 task。`mutate` 看到
    /// 的是反序列化后的 T；写回时再序列化。
    /// 如果 task_store 已设，同时异步写到文件系统。
    pub fn update_task<T, F>(&self, id: &str, mutate: F) -> Option<T>
    where
        T: serde::Serialize + serde::de::DeserializeOwned,
        F: FnOnce(&mut T),
    {
        let mut tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
        for slot in tasks.iter_mut() {
            if slot.get("id").and_then(|i| i.as_str()) == Some(id) {
                let mut t: T = serde_json::from_value(slot.clone()).ok()?;
                mutate(&mut t);
                let updated = serde_json::to_value(&t).ok()?;
                // TODO: task_store persistence — downcast from Arc<dyn Any> in task crate layer
                *slot = updated;
                return Some(t);
            }
        }
        None
    }

    /// Builder：设初始权限模式（CLI 启动时从 settings/CLI 注入）。
    pub fn with_permission_mode(self, mode: PermissionMode) -> Self {
        *self
            .permission_mode
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = mode;
        self
    }

    /// 读当前模式（PermissionMode 是 Copy，返回值就行）。
    pub fn permission_mode(&self) -> PermissionMode {
        *self
            .permission_mode
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// 切换模式（EnterPlanMode / ExitPlanMode / `/permissions` 命令）。
    pub fn set_permission_mode(&self, mode: PermissionMode) {
        *self
            .permission_mode
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = mode;
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

    /// Record that TodoWrite was called on the current turn. Called by engine
    /// when it processes TodoWrite tool results.
    pub fn record_todo_write_turn(&self, turn: u64) {
        *self
            .last_todo_write_turn
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(turn);
    }

    /// Returns the turn number since last TodoWrite, or None if never called
    /// or current_turn is not advanced enough to compute.
    pub fn turns_since_todo_write(&self) -> Option<u64> {
        let guard = self
            .last_todo_write_turn
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let last = (*guard)?;
        let current = *self.current_turn.lock().unwrap_or_else(|e| e.into_inner());
        Some(current.saturating_sub(last))
    }

    /// Read + update the verification nudge counter atomically. Returns the
    /// difference between current and last-nudged completed-task count.
    pub fn check_verification_nudge(&self, completed_count: u32) -> u32 {
        let mut last = self
            .last_verification_nudge_count
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let delta = completed_count.saturating_sub(*last);
        if delta >= 3 {
            *last = completed_count;
        }
        delta
    }

    /// Set the active plan text (called by EnterPlanMode tool).
    pub fn set_plan_text(&self, plan: String) {
        *self.plan_text.lock().unwrap_or_else(|e| e.into_inner()) = Some(plan);
    }

    /// Clear the active plan text (called by ExitPlanMode tool).
    pub fn clear_plan_text(&self) {
        *self.plan_text.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Read the current plan text, if any.
    pub fn plan_text(&self) -> Option<String> {
        self.plan_text
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Set the active plan slug (called by EnterPlanMode tool).
    pub fn set_plan_slug(&self, slug: String) {
        *self.plan_slug.lock().unwrap_or_else(|e| e.into_inner()) = Some(slug);
    }

    /// Clear the active plan slug (called by ExitPlanMode tool).
    pub fn clear_plan_slug(&self) {
        *self.plan_slug.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Read the current plan slug, if any.
    pub fn plan_slug(&self) -> Option<String> {
        self.plan_slug
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// **P (2026-05-17)**: 添加一个沙盒放写路径（项目级授权时调用）。
    pub fn add_sandbox_allow_write(&self, path: PathBuf) {
        self.sandbox_allow_writes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(path);
    }

    /// 读取所有沙盒放写路径。
    pub fn sandbox_allow_writes(&self) -> Vec<PathBuf> {
        self.sandbox_allow_writes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// **P (read-before-edit)**: 记录一次文件读取。`FileRead` 工具成功后调用。
    /// 存入读取时的 mtime 与偏移范围，供后续去重（相同范围 + 不变 mtime）与
    /// Edit/Write staleness 检测。
    pub fn record_read(&self, path: &Path) {
        self.record_read_with_range(path, None, None);
    }

    /// Record a file read with offset/limit for precise dedup; also stores the mtime.
    pub fn record_read_with_range(&self, path: &Path, offset: Option<usize>, limit: Option<usize>) {
        if let Ok(meta) = std::fs::metadata(path) {
            if let Ok(mtime) = meta.modified() {
                self.read_cache
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(
                        path.to_path_buf(),
                        FileReadEntry {
                            mtime,
                            offset,
                            limit,
                        },
                    );
                *self
                    .last_read_path
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some(path.to_path_buf());
            }
        }
    }

    /// **P (read-before-edit)**: 检查文件是否已读且未过期。
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

    /// **P (read dedup)**: 文件是否已读且未变化。用于 Read 工具跳过重复读取。
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
            entry.mtime == current_mtime && entry.offset == offset && entry.limit == limit
        })
    }

    /// **P (read dedup)**: 给定路径是否与最近一次读取的文件一致。
    /// 仅检查路径相等；mtime 一致性由调用方通过 `check_read_dedup` 保证。
    pub fn is_consecutive_read(&self, path: &Path) -> bool {
        let last = self
            .last_read_path
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        last.as_ref().is_some_and(|p| p == path)
    }

    pub fn set_structured_output(&self, value: serde_json::Value) {
        *self
            .structured_output
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(value);
    }

    pub fn take_structured_output(&self) -> Option<serde_json::Value> {
        self.structured_output
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
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

    /// 状态汇总：(总数, pending, in_progress, completed)
    pub fn todos_summary(&self) -> (usize, usize, usize, usize) {
        let v = self.todos.lock().unwrap_or_else(|e| e.into_inner());
        let mut p = 0;
        let mut i = 0;
        let mut c = 0;
        for t in v.iter() {
            match t.status {
                TodoStatus::Pending => p += 1,
                TodoStatus::InProgress => i += 1,
                TodoStatus::Completed => c += 1,
            }
        }
        (v.len(), p, i, c)
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
