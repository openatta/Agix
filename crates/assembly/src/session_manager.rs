//! In-memory session tracking for daemon.

use std::collections::HashMap;
use std::sync::Arc;

pub struct SessionHandle {
    pub id: String,
}

pub struct SessionManager {
    sessions: HashMap<String, Arc<SessionHandle>>,
}

impl SessionManager {
    pub fn new(_cap: usize) -> Self {
        Self {
            sessions: HashMap::new(),
        }
    }
    pub fn create(&mut self, id: &str) {
        self.sessions.insert(
            id.to_string(),
            Arc::new(SessionHandle { id: id.to_string() }),
        );
    }
    pub fn get(&self, id: &str) -> Option<Arc<SessionHandle>> {
        self.sessions.get(id).cloned()
    }
    pub fn count(&self) -> usize {
        self.sessions.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_starts_empty() {
        let mgr = SessionManager::new(8);
        assert_eq!(mgr.count(), 0);
        assert!(mgr.get("never-existed").is_none());
    }

    #[test]
    fn create_then_get() {
        let mut mgr = SessionManager::new(8);
        mgr.create("sess-1");
        assert!(mgr.get("sess-1").is_some());
        assert_eq!(mgr.count(), 1);
    }
}
