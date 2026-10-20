//! Short-lived cache of MCP tool results, keyed by (server, tool, arguments).
//!
//! Only consulted for tools whose server declares them both read-only and
//! idempotent (see `McpToolAdapter`); a repeated call to anything else must
//! reach the server, since it may change state or answer differently. One
//! cache is shared by every adapter a manager builds, in memory only.

use crate::client::McpCallResult;
use std::collections::HashMap;
use std::time::{Duration, Instant};

const CACHE_TTL: Duration = Duration::from_secs(30);
const MAX_CACHE_ENTRIES: usize = 100;

#[derive(Debug, Clone)]
struct CacheEntry {
    result: McpCallResult,
    created_at: Instant,
}

#[derive(Debug, Default)]
pub struct McpOutputCache {
    entries: HashMap<String, CacheEntry>,
}

impl McpOutputCache {
    pub fn new() -> Self {
        Self::default()
    }

    fn key(server: &str, tool: &str, args: &serde_json::Map<String, serde_json::Value>) -> String {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        server.hash(&mut h);
        tool.hash(&mut h);
        if let Ok(serialized) = serde_json::to_string(args) {
            serialized.hash(&mut h);
        }
        format!("mcp_cache_{}_{server}_{tool}", h.finish())
    }

    /// The stored result, unless missing or older than the TTL.
    pub fn get(
        &self,
        server: &str,
        tool: &str,
        args: &serde_json::Map<String, serde_json::Value>,
    ) -> Option<McpCallResult> {
        let entry = self.entries.get(&Self::key(server, tool, args))?;
        if entry.created_at.elapsed() > CACHE_TTL {
            return None;
        }
        Some(entry.result.clone())
    }

    /// Store a successful result, evicting the oldest entry when full. Error
    /// results are not stored: a failure is no reason to skip the next call.
    pub fn put(
        &mut self,
        server: &str,
        tool: &str,
        args: &serde_json::Map<String, serde_json::Value>,
        result: McpCallResult,
    ) {
        if result.is_error {
            return;
        }
        if self.entries.len() >= MAX_CACHE_ENTRIES {
            if let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, v)| v.created_at)
                .map(|(k, _)| k.clone())
            {
                self.entries.remove(&oldest);
            }
        }
        self.entries.insert(
            Self::key(server, tool, args),
            CacheEntry {
                result,
                created_at: Instant::now(),
            },
        );
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Drop expired entries; returns how many went.
    pub fn evict_expired(&mut self) -> usize {
        let before = self.entries.len();
        self.entries
            .retain(|_, v| v.created_at.elapsed() <= CACHE_TTL);
        before - self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::McpContent;

    fn make_result(text: &str) -> McpCallResult {
        McpCallResult {
            content: vec![McpContent::Text(text.to_string())],
            is_error: false,
            meta: None,
        }
    }

    fn make_args(s: &str) -> serde_json::Map<String, serde_json::Value> {
        let mut m = serde_json::Map::new();
        m.insert("q".to_string(), serde_json::Value::String(s.to_string()));
        m
    }

    #[test]
    fn cache_hit_returns_stored_result() {
        let mut cache = McpOutputCache::new();
        let args = make_args("test");
        cache.put("github", "search_issues", &args, make_result("cached"));
        let result = cache.get("github", "search_issues", &args);
        assert!(result.is_some());
        match &result.unwrap().content[0] {
            McpContent::Text(s) => assert_eq!(s, "cached"),
            _ => panic!("expected text"),
        }
    }

    #[test]
    fn cache_miss_returns_none() {
        let cache = McpOutputCache::new();
        let args = make_args("test");
        assert!(cache.get("github", "search_issues", &args).is_none());
    }

    #[test]
    fn different_args_produce_different_keys() {
        let mut cache = McpOutputCache::new();
        let args1 = make_args("test1");
        let args2 = make_args("test2");
        cache.put("github", "search", &args1, make_result("one"));
        cache.put("github", "search", &args2, make_result("two"));
        let r1 = cache.get("github", "search", &args1);
        let r2 = cache.get("github", "search", &args2);
        match &r1.unwrap().content[0] {
            McpContent::Text(s) => assert_eq!(s, "one"),
            _ => panic!("expected text"),
        }
        match &r2.unwrap().content[0] {
            McpContent::Text(s) => assert_eq!(s, "two"),
            _ => panic!("expected text"),
        }
    }

    #[test]
    fn error_results_are_never_stored() {
        let mut cache = McpOutputCache::new();
        let mut failed = make_result("boom");
        failed.is_error = true;
        cache.put("s", "t", &make_args("x"), failed);
        assert!(cache.is_empty());
    }

    #[test]
    fn cache_clear_removes_all() {
        let mut cache = McpOutputCache::new();
        cache.put("s", "t", &make_args("x"), make_result("y"));
        assert_eq!(cache.len(), 1);
        cache.clear();
        assert!(cache.is_empty());
    }

    #[test]
    fn evict_expired_keeps_fresh_entries() {
        let mut cache = McpOutputCache::new();
        cache.put("s", "t", &make_args("x"), make_result("y"));
        assert_eq!(cache.evict_expired(), 0);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn a_full_cache_drops_its_oldest_entry() {
        let mut cache = McpOutputCache::new();
        for i in 0..MAX_CACHE_ENTRIES {
            cache.put("s", "t", &make_args(&i.to_string()), make_result("v"));
        }
        cache.put("s", "t", &make_args("newest"), make_result("v"));
        assert_eq!(cache.len(), MAX_CACHE_ENTRIES);
        assert!(cache.get("s", "t", &make_args("newest")).is_some());
    }
}
