//! 按 (项目, 场景) 缓存的 MCP manager：每一对一把更新锁，连接与列工具只挡住同一对。

use super::*;

/// The pool's centrally-connected MCP managers, one slot per
/// `(project, scene)`. The map's own lock is only ever held to find, add or
/// drop a slot. Connecting, reconnecting and listing tools happen under the
/// slot's update lock: a server that never answers holds up its own pair and
/// no other, while two callers on one pair still connect once and a late
/// result is never written over by a change computed before it.
#[derive(Default)]
pub(super) struct McpCache {
    slots: std::sync::Mutex<HashMap<ProjectSceneKey, Arc<McpSlot>>>,
}

#[derive(Default)]
pub(super) struct McpSlot {
    manager: std::sync::Mutex<Option<Arc<McpManager>>>,
    update: AsyncMutex<()>,
}

/// A slot held for a change: whatever is computed from [`Self::current`] and
/// [`Self::set`] before the guard drops cannot interleave with another one.
pub(super) struct McpSlotUpdate<'a> {
    slot: &'a McpSlot,
    _held: tokio::sync::MutexGuard<'a, ()>,
}

impl McpCache {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<ProjectSceneKey, Arc<McpSlot>>> {
        self.slots
            .lock()
            .expect("the MCP cache is never held across a panic")
    }

    /// The pair's manager as it is now, without waiting for a change in
    /// progress.
    pub(super) fn current(&self, key: &ProjectSceneKey) -> Option<Arc<McpManager>> {
        self.existing(key)?.current()
    }

    pub(super) fn slot(&self, key: &ProjectSceneKey) -> Arc<McpSlot> {
        self.lock().entry(key.clone()).or_default().clone()
    }

    pub(super) fn existing(&self, key: &ProjectSceneKey) -> Option<Arc<McpSlot>> {
        self.lock().get(key).cloned()
    }

    pub(super) fn slots(&self) -> Vec<(ProjectSceneKey, Arc<McpSlot>)> {
        self.lock()
            .iter()
            .map(|(key, slot)| (key.clone(), slot.clone()))
            .collect()
    }

    /// Drop the slots `keep` says no to. A change still running on one
    /// finishes on a slot nobody looks up any more; the next caller on that
    /// pair connects anew.
    pub(super) fn retain(&self, mut keep: impl FnMut(&ProjectSceneKey) -> bool) {
        self.lock().retain(|key, _| keep(key));
    }

    #[cfg(test)]
    pub(super) fn connected_pairs(&self) -> usize {
        self.lock()
            .values()
            .filter(|slot| slot.current().is_some())
            .count()
    }
}

impl McpSlot {
    pub(super) fn current(&self) -> Option<Arc<McpManager>> {
        self.manager
            .lock()
            .expect("an MCP slot is never held across a panic")
            .clone()
    }

    pub(super) async fn update(&self) -> McpSlotUpdate<'_> {
        McpSlotUpdate {
            _held: self.update.lock().await,
            slot: self,
        }
    }
}

impl McpSlotUpdate<'_> {
    pub(super) fn current(&self) -> Option<Arc<McpManager>> {
        self.slot.current()
    }

    pub(super) fn set(&self, manager: Arc<McpManager>) {
        *self
            .slot
            .manager
            .lock()
            .expect("an MCP slot is never held across a panic") = Some(manager);
    }
}
