//! GrowthBook-style feature flag client.
//!
//! Remote evaluation with cache-first access, disk persistence,
#![allow(
    clippy::match_like_matches_macro,
    clippy::if_same_then_else,
    clippy::manual_strip,
    clippy::question_mark
)]
//! A/B experiment logging, and killswitch support.
//!
//! Killswitch convention: flags with the `tengu_frond_boric` prefix or
//! `killswitch: true` act as remote emergency disables for other flags.
//! E.g. `tengu_frond_boric_team_mode = true` disables `team_mode`.

use crate::events::{ExperimentExposurePayload, TelemetryEvent};
use crate::handle::TelemetryHandle;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// FlagValue
// ---------------------------------------------------------------------------

/// The evaluated value of a feature flag.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FlagValue {
    Bool(bool),
    Number(f64),
    String(String),
    Json(serde_json::Value),
}

impl FlagValue {
    /// Interpret as a boolean. Returns `None` for non-bool variants.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            FlagValue::Bool(b) => Some(*b),
            _ => None,
        }
    }
}

impl From<bool> for FlagValue {
    fn from(v: bool) -> Self {
        FlagValue::Bool(v)
    }
}

impl From<f64> for FlagValue {
    fn from(v: f64) -> Self {
        FlagValue::Number(v)
    }
}

impl From<String> for FlagValue {
    fn from(v: String) -> Self {
        FlagValue::String(v)
    }
}

// ---------------------------------------------------------------------------
// Targeting rule types
// ---------------------------------------------------------------------------

/// A single targeting rule for feature flag evaluation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetingRule {
    /// Condition expression. Supported forms:
    /// - `"all"` — always matches
    /// - `"userId == <value>"` — matches user ID
    /// - `"attribute[key] == <value>"` — matches attribute
    /// - `"percentRollout(<pct>)"` — percentage rollout by user ID hash
    #[serde(default = "default_condition")]
    pub condition: String,
    /// The value to return when this rule matches.
    pub value: FlagValue,
    /// Optional rollout percentage (0–100) applied on top of the condition.
    #[serde(default)]
    pub coverage: Option<f64>,
}

fn default_condition() -> String {
    "all".to_string()
}

/// A complete feature flag definition (GrowthBook-compatible).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeatureFlagDefinition {
    /// Flag key / name.
    pub key: String,
    /// Default value when no rules match.
    #[serde(default = "default_bool_false")]
    pub default_value: FlagValue,
    /// Ordered list of targeting rules. First match wins.
    #[serde(default)]
    pub rules: Vec<TargetingRule>,
    /// Killswitch: when `true`, this flag's value overrides the targeted flag
    /// to its default-off state (or suppresses its value entirely).
    #[serde(default)]
    pub killswitch: bool,
}

fn default_bool_false() -> FlagValue {
    FlagValue::Bool(false)
}

/// Response from the `GET /api/flags` endpoint (GrowthBook-compatible).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteFlagsResponse {
    #[serde(default)]
    pub flags: HashMap<String, FeatureFlagDefinition>,
    /// Optional feature flags (alternative key used by some deployments).
    #[serde(default)]
    pub features: HashMap<String, FeatureFlagDefinition>,
}

impl RemoteFlagsResponse {
    fn into_flags(self) -> HashMap<String, FeatureFlagDefinition> {
        if !self.flags.is_empty() {
            self.flags
        } else {
            self.features
        }
    }
}

// ---------------------------------------------------------------------------
// FlagContext
// ---------------------------------------------------------------------------

/// Context provided to targeting rule evaluation.
///
/// Passed to `is_enabled()`, `get_value()`, and `log_experiment()`
/// for attribute-targeted feature flags.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FlagContext {
    pub user_id: Option<String>,
    #[serde(default)]
    pub attributes: HashMap<String, String>,
}

impl FlagContext {
    /// Create a context with just a user ID.
    pub fn with_user_id(id: &str) -> Self {
        Self {
            user_id: Some(id.to_string()),
            attributes: HashMap::new(),
        }
    }

    /// Add an attribute to this context (builder-style).
    pub fn with_attribute(mut self, key: &str, value: &str) -> Self {
        self.attributes.insert(key.to_string(), value.to_string());
        self
    }
}

// ---------------------------------------------------------------------------
// FeatureFlagClient
// ---------------------------------------------------------------------------

/// Client for evaluating GrowthBook-style feature flags.
///
/// Thread-safe via `Arc<Mutex<FeatureFlagClient>>`. Synchronous `is_enabled()`
/// and `get_value()` check an in-memory cache; async `refresh()` fetches from
/// the remote endpoint. A periodic background task drives `refresh()`.
#[derive(Debug)]
pub struct FeatureFlagClient {
    // ── Configuration (seldom changes) ──
    endpoint: String,
    api_key: String,
    refresh_interval: Duration,
    disk_cache_path: Option<PathBuf>,
    sticky: bool,

    // ── Evaluated cache (what callers read) ──
    cache: HashMap<String, FlagValue>,
    killswitches: HashMap<String, bool>,
    cache_timestamp: Instant,

    // ── Raw definitions (for disk persistence) ──
    flags_raw: HashMap<String, FeatureFlagDefinition>,
}

impl FeatureFlagClient {
    /// Killswitch prefix convention: `tengu_frond_boric_<target_flag>`.
    const KILLSWITCH_PREFIX: &'static str = "tengu_frond_boric";

    // ── Construction ──

    /// Create a new feature flag client with the given endpoint and API key.
    pub fn new(endpoint: String, api_key: String) -> Self {
        Self {
            endpoint,
            api_key,
            refresh_interval: default_refresh_interval(),
            disk_cache_path: None,
            sticky: false,
            cache: HashMap::new(),
            killswitches: HashMap::new(),
            cache_timestamp: Instant::now(),
            flags_raw: HashMap::new(),
        }
    }

    /// Set the refresh interval (default: 20 min debug, 6 h release).
    pub fn with_refresh_interval(mut self, interval: Duration) -> Self {
        self.refresh_interval = interval;
        self
    }

    /// Set the disk cache path (e.g. `~/.agix/scenes/<scope>/feature_flags.json`).
    pub fn with_disk_cache(mut self, path: PathBuf) -> Self {
        self.disk_cache_path = Some(path);
        self
    }

    /// Enable sticky mode: once a flag is cached, it won't be re-evaluated
    /// on subsequent refreshes (keep the originally assigned variant).
    pub fn with_sticky(mut self, sticky: bool) -> Self {
        self.sticky = sticky;
        self
    }

    // ── Public API ──

    /// Check if a named feature flag is enabled (true) or disabled (false).
    ///
    /// Killswitches are checked first: if a killswitch targeting this flag is
    /// active, returns `false` regardless of the flag's actual value.
    pub fn is_enabled(&self, flag_name: &str) -> bool {
        // Killswitch check — overrides to false
        if self.is_killswitched(flag_name) {
            tracing::debug!(
                flag = %flag_name,
                "feature flag disabled by active killswitch"
            );
            return false;
        }

        match self.cache.get(flag_name) {
            Some(FlagValue::Bool(true)) => true,
            _ => false,
        }
    }

    /// Get the raw evaluated value of a feature flag, if present.
    ///
    /// Returns `None` when:
    /// - the flag is not found in cache
    /// - an active killswitch suppresses the flag
    pub fn get_value(&self, flag_name: &str) -> Option<FlagValue> {
        if self.is_killswitched(flag_name) {
            tracing::debug!(
                flag = %flag_name,
                "feature flag value suppressed by active killswitch"
            );
            return None;
        }
        self.cache.get(flag_name).cloned()
    }

    /// Evaluate a flag with targeting rules, producing a value plus
    /// exposure information. Returns `(value, matched_rule_index)`.
    ///
    /// The matched rule index is `None` when no rule matched (default applied).
    /// Call `log_experiment()` separately to record the exposure.
    pub fn evaluate_with_context(
        &self,
        flag_name: &str,
        context: &FlagContext,
    ) -> (Option<FlagValue>, Option<usize>) {
        if self.is_killswitched(flag_name) {
            return (None, None);
        }

        let raw = match self.flags_raw.get(flag_name) {
            Some(d) => d,
            None => return (self.cache.get(flag_name).cloned(), None),
        };

        // Evaluate rules in order
        for (idx, rule) in raw.rules.iter().enumerate() {
            if self.match_rule(rule, context) {
                return (Some(rule.value.clone()), Some(idx));
            }
        }

        // No rule matched — use default value
        (Some(raw.default_value.clone()), None)
    }

    /// Log an experiment exposure to the telemetry pipeline.
    ///
    /// Records which flag was evaluated, which variant the user was assigned,
    /// and the evaluation context (user ID, attributes).
    pub fn log_experiment(
        &self,
        flag_name: &str,
        variant: &str,
        context: &FlagContext,
        telemetry: &TelemetryHandle,
    ) {
        tracing::info!(
            flag = %flag_name,
            variant = %variant,
            user_id = ?context.user_id,
            "experiment exposure logged"
        );

        let event = TelemetryEvent::feature_flag_experiment(
            "global",
            0,
            None,
            ExperimentExposurePayload {
                flag_name: flag_name.to_string(),
                variant: variant.to_string(),
                user_id: context.user_id.clone(),
                attributes: context.attributes.clone(),
            },
        );
        let _ = telemetry.record(event);
    }

    /// Force-refresh flags from the remote endpoint.
    ///
    /// On success, updates the in-memory cache and persists to disk.
    /// On failure, logs a warning and leaves the existing cache intact.
    pub async fn refresh(&mut self) -> Result<(), FeatureFlagError> {
        if self.endpoint.is_empty() || self.api_key.is_empty() {
            return Err(FeatureFlagError::NotConfigured);
        }

        let body = fetch_remote_flags(&self.endpoint, &self.api_key).await?;

        self.apply_flags(body);
        self.cache_timestamp = Instant::now();

        // Persist to disk cache
        if let Err(e) = self.persist_to_disk() {
            tracing::warn!(error = %e, "failed to persist feature flags to disk");
        }

        Ok(())
    }

    /// Load flags from disk cache (no remote HTTP call).
    pub fn load_from_disk(&mut self) -> Result<(), FeatureFlagError> {
        let Some(ref path) = self.disk_cache_path else {
            return Err(FeatureFlagError::NotConfigured);
        };

        let data =
            std::fs::read_to_string(path).map_err(|e| FeatureFlagError::Io(e.to_string()))?;

        let body: RemoteFlagsResponse =
            serde_json::from_str(&data).map_err(|e| FeatureFlagError::Parse(e.to_string()))?;

        self.apply_flags(body);
        self.cache_timestamp = Instant::now();
        Ok(())
    }

    /// Initialize the client on startup: attempt to load from disk cache,
    /// then try a remote refresh. Errors are non-fatal (logged, not returned).
    pub async fn initialize(&mut self) {
        // First, try disk cache
        if self.disk_cache_path.is_some() {
            match self.load_from_disk() {
                Ok(()) => {
                    tracing::info!(
                        path = ?self.disk_cache_path,
                        "feature flags loaded from disk cache"
                    );
                }
                Err(e) => {
                    tracing::debug!(
                        error = %e,
                        path = ?self.disk_cache_path,
                        "no feature flag disk cache found — starting fresh"
                    );
                }
            }
        }

        // Then try remote refresh
        match self.refresh().await {
            Ok(()) => {
                tracing::info!(
                    endpoint = %self.endpoint,
                    flag_count = %self.cache.len(),
                    "feature flags refreshed from remote"
                );
            }
            Err(e) => {
                if self.cache.is_empty() {
                    tracing::warn!(
                        error = %e,
                        "initial feature flag fetch failed — all flags default to false"
                    );
                } else {
                    tracing::warn!(
                        error = %e,
                        "feature flag refresh failed — using disk cache"
                    );
                }
            }
        }
    }

    /// Check if the in-memory cache is stale (elapsed >= refresh_interval).
    pub fn is_cache_stale(&self) -> bool {
        self.cache_timestamp.elapsed() >= self.refresh_interval
    }

    // ── Internal helpers ──

    /// Check whether a killswitch is active for the given flag.
    fn is_killswitched(&self, flag_name: &str) -> bool {
        self.killswitches.get(flag_name).copied().unwrap_or(false)
    }

    /// Apply raw flag definitions to internal state, re-evaluating all flags.
    fn apply_flags(&mut self, response: RemoteFlagsResponse) {
        let raw = response.into_flags();

        if self.sticky && !self.cache.is_empty() {
            // Sticky mode: only update flags that aren't already cached.
            // New flags are added, existing flags keep their assigned value.
            for (key, def) in raw {
                if !self.flags_raw.contains_key(&key) {
                    self.flags_raw.insert(key.clone(), def.clone());
                    self.evaluate_and_cache_one(key, def);
                } else if !self.cache.contains_key(&key) {
                    self.flags_raw.insert(key.clone(), def.clone());
                    self.evaluate_and_cache_one(key, def);
                }
                // else: already cached, skip
            }
            return;
        }

        // Non-sticky: full replacement
        self.flags_raw = raw;
        // Build killswitch map BEFORE removing prefix flags (so rebuild_cache
        // doesn't need to search for them)
        let mut new_killswitches: HashMap<String, bool> = HashMap::new();
        let killswitch_keys: Vec<String> = self
            .flags_raw
            .iter()
            .filter(|(key, def)| {
                if key.starts_with(Self::KILLSWITCH_PREFIX) {
                    let target = key[Self::KILLSWITCH_PREFIX.len()..].trim_start_matches('_');
                    let enabled = def.default_value.as_bool().unwrap_or(true);
                    new_killswitches.insert(target.to_string(), enabled);
                    true // remove from flags_raw
                } else if def.killswitch {
                    let enabled = def.default_value.as_bool().unwrap_or(true);
                    new_killswitches.insert(key.to_string(), enabled);
                    true // remove from flags_raw
                } else {
                    false // keep
                }
            })
            .map(|(key, _)| key.clone())
            .collect();
        for key in &killswitch_keys {
            self.flags_raw.remove(key);
        }
        self.killswitches = new_killswitches;
        self.rebuild_cache();
    }

    /// Get the endpoint URL (used by spawn.rs for initialization).
    pub(crate) fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Get the API key (used by spawn.rs for initialization).
    pub(crate) fn api_key(&self) -> &str {
        &self.api_key
    }

    /// Get the disk cache path (used by spawn.rs for initialization).
    pub(crate) fn disk_cache_path(&self) -> Option<&std::path::Path> {
        self.disk_cache_path.as_deref()
    }

    /// Check if sticky mode is enabled (used by spawn.rs for initialization).
    pub(crate) fn sticky(&self) -> bool {
        self.sticky
    }

    /// Rebuild the evaluated cache from raw definitions.
    /// Killswitches should have been built by `apply_flags` already.
    fn rebuild_cache(&mut self) {
        let mut new_cache: HashMap<String, FlagValue> = HashMap::new();

        // Killswitch prefix flags and killswitch-attribute flags have been
        // removed by `apply_flags` already — just evaluate what's left.
        for (key, def) in &self.flags_raw {
            debug_assert!(
                !key.starts_with(Self::KILLSWITCH_PREFIX) && !def.killswitch,
                "killswitch flags should have been removed before rebuild_cache"
            );
            let value = self.evaluate_flag(def);
            new_cache.insert(key.clone(), value);
        }

        self.cache = new_cache;
    }

    /// Evaluate a single flag definition to its effective value (no context).
    /// For simple flags without targeting, this returns the default value.
    fn evaluate_flag(&self, def: &FeatureFlagDefinition) -> FlagValue {
        // No rules? Return default.
        if def.rules.is_empty() {
            return def.default_value.clone();
        }

        // Evaluate rules without context — only "all" and percentage rules apply.
        for rule in &def.rules {
            if self.condition_matches_no_context(&rule.condition) {
                if let Some(coverage) = rule.coverage {
                    // Percentage rollout: use hash of flag key for stable assignment
                    if self.rollout_hit(&def.key, coverage) {
                        return rule.value.clone();
                    }
                } else {
                    return rule.value.clone();
                }
            }
        }

        def.default_value.clone()
    }

    /// Match a targeting rule against a context.
    fn match_rule(&self, rule: &TargetingRule, context: &FlagContext) -> bool {
        let cond = rule.condition.trim();

        // "all" — always matches
        if cond.eq_ignore_ascii_case("all") {
            return true;
        }

        // "userId == <value>"
        if let Some(val) = cond
            .strip_prefix("userId ==")
            .or_else(|| cond.strip_prefix("userId=="))
        {
            let val = val.trim().trim_matches('"').trim_matches('\'');
            return context.user_id.as_deref() == Some(val);
        }

        // "attribute[key] == <value>"
        if let Some(rest) = cond.strip_prefix("attribute[") {
            if let Some(bracket_end) = rest.find(']') {
                let attr_key = rest[..bracket_end].trim();
                let after = rest[bracket_end + 1..].trim();
                if let Some(val) = after
                    .strip_prefix("==")
                    .or_else(|| after.strip_prefix("== "))
                {
                    let val = val.trim().trim_matches('"').trim_matches('\'');
                    return context.attributes.get(attr_key).map(|s| s.as_str()) == Some(val);
                }
            }
        }

        // "percentRollout(<pct>)" — without user context, falls through
        false
    }

    /// Match a condition without user context (for no-context evaluation).
    fn condition_matches_no_context(&self, condition: &str) -> bool {
        let cond = condition.trim();
        cond.eq_ignore_ascii_case("all") || cond.starts_with("percentRollout(")
    }

    /// Stable rollout check using a hash of the flag key.
    /// Returns `true` if this flag's hash falls within the rollout percentage.
    fn rollout_hit(&self, key: &str, coverage: f64) -> bool {
        if coverage >= 100.0 {
            return true;
        }
        if coverage <= 0.0 {
            return false;
        }

        // Simple deterministic hash for stable rollout assignment
        let hash = simple_hash(key);
        let pct = (hash % 100) as f64;
        pct < coverage
    }

    /// Persist raw flag definitions to the disk cache.
    fn persist_to_disk(&self) -> Result<(), FeatureFlagError> {
        let Some(ref path) = self.disk_cache_path else {
            return Err(FeatureFlagError::NotConfigured);
        };

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| FeatureFlagError::Io(e.to_string()))?;
        }

        let wrapper = RemoteFlagsResponse {
            flags: self.flags_raw.clone(),
            features: HashMap::new(),
        };

        let data = serde_json::to_string_pretty(&wrapper)
            .map_err(|e| FeatureFlagError::Parse(e.to_string()))?;

        // Atomic write: write to temp then rename
        let tmp_path = path.with_extension("json.tmp");
        std::fs::write(&tmp_path, &data).map_err(|e| FeatureFlagError::Io(e.to_string()))?;
        std::fs::rename(&tmp_path, path).map_err(|e| FeatureFlagError::Io(e.to_string()))?;

        Ok(())
    }

    /// Evaluate a single flag definition and insert it into the cache.
    fn evaluate_and_cache_one(&mut self, key: String, def: FeatureFlagDefinition) {
        if key.starts_with(Self::KILLSWITCH_PREFIX) || def.killswitch {
            return;
        }
        let value = self.evaluate_flag(&def);
        self.cache.insert(key, value);
    }

    /// Swap the full runtime state (cache, killswitches, flags_raw, cache_timestamp)
    /// from another client into `self`. Used by the telemetry pipeline to initialise
    /// the shared client from a temporary client without Send issues.
    pub(crate) fn swap_state(&mut self, other: &mut Self) {
        std::mem::swap(&mut self.cache, &mut other.cache);
        std::mem::swap(&mut self.killswitches, &mut other.killswitches);
        std::mem::swap(&mut self.flags_raw, &mut other.flags_raw);
        std::mem::swap(&mut self.cache_timestamp, &mut other.cache_timestamp);
    }
}

// ---------------------------------------------------------------------------
// Simple hash
// ---------------------------------------------------------------------------

/// A simple, deterministic hash for stable rollout assignment.
/// Not cryptographically secure — only used for percentage rollouts.
fn simple_hash(s: &str) -> u64 {
    let mut hash: u64 = 5381;
    for b in s.bytes() {
        hash = hash.wrapping_mul(33).wrapping_add(b as u64);
    }
    hash
}

// ---------------------------------------------------------------------------
// FeatureFlagError
// ---------------------------------------------------------------------------

/// Errors that can occur during feature flag operations.
#[derive(Debug, Clone, thiserror::Error)]
pub enum FeatureFlagError {
    /// Client not configured (missing endpoint or API key).
    #[error("feature flag client is not configured")]
    NotConfigured,

    /// HTTP error during remote fetch.
    #[error("HTTP error: {0}")]
    Http(String),

    /// Parse error (invalid JSON response).
    #[error("parse error: {0}")]
    Parse(String),

    /// I/O error (disk persistence).
    #[error("I/O error: {0}")]
    Io(String),
}

// ---------------------------------------------------------------------------
// Background refresh
// ---------------------------------------------------------------------------

/// Spawn a periodic background task that refreshes feature flags.
///
/// The task sleeps for `interval` between refreshes, regardless of whether the
/// previous refresh succeeded or failed. The common pattern is:
///
/// ```ignore
/// let client = Arc::new(std::sync::Mutex::new(FeatureFlagClient::new(...)));
/// let refresh_handle = spawn_refresh_task(client.clone(), Duration::from_secs(1200));
/// ```
pub fn spawn_refresh_task(
    client: std::sync::Arc<std::sync::Mutex<FeatureFlagClient>>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        // Skip the first tick (immediate) — the caller does initial refresh.
        ticker.tick().await;

        loop {
            ticker.tick().await;

            // Check if stale (brief lock)
            let needs_refresh = {
                let guard = match client.lock() {
                    Ok(g) => g,
                    Err(_) => continue,
                };
                guard.is_cache_stale()
            };

            if !needs_refresh {
                continue;
            }

            // Snapshot config outside lock for async HTTP call
            let (endpoint, api_key, disk_path) = {
                let guard = match client.lock() {
                    Ok(g) => g,
                    Err(_) => continue,
                };
                (
                    guard.endpoint.clone(),
                    guard.api_key.clone(),
                    guard.disk_cache_path.clone(),
                )
            };

            // HTTP call outside the lock
            let result = fetch_remote_flags(&endpoint, &api_key).await;

            match result {
                Ok(response) => {
                    let mut guard = match client.lock() {
                        Ok(g) => g,
                        Err(_) => continue,
                    };
                    guard.apply_flags(response);
                    guard.cache_timestamp = Instant::now();
                    if let Err(e) = guard.persist_to_disk() {
                        tracing::warn!(error = %e, "failed to persist feature flags during background refresh");
                    }
                    tracing::debug!(
                        flag_count = guard.cache.len(),
                        "feature flags refreshed in background"
                    );
                }
                Err(e) => {
                    // Try disk fallback on HTTP failure
                    if let Some(path) = disk_path {
                        tracing::warn!(error = %e, path = %path.display(), "feature flag remote refresh failed, trying disk fallback");
                        let mut guard = match client.lock() {
                            Ok(g) => g,
                            Err(_) => continue,
                        };
                        let _ = guard.load_from_disk();
                    } else {
                        tracing::warn!(error = %e, "feature flag remote refresh failed");
                    }
                }
            }
        }
    })
}

const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// Fetch flags from the remote endpoint (no lock held).
async fn fetch_remote_flags(
    endpoint: &str,
    api_key: &str,
) -> Result<RemoteFlagsResponse, FeatureFlagError> {
    if endpoint.is_empty() || api_key.is_empty() {
        return Err(FeatureFlagError::NotConfigured);
    }

    let url = format!("{}/api/flags", endpoint.trim_end_matches('/'));
    let net = base::interface::exec::local::LocalNetwork::default();
    // `Origin::Operator`: the flag endpoint is deployment configuration.
    let req = base::interface::exec::HttpRequest::get(&url)
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Accept", "application/json");
    let sending =
        base::interface::exec::Network::send(&net, req, base::interface::exec::Origin::Operator);

    let resp = match tokio::time::timeout(FETCH_TIMEOUT, sending).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => return Err(FeatureFlagError::Http(format!("request failed: {e}"))),
        Err(_elapsed) => {
            return Err(FeatureFlagError::Http(format!(
                "request failed: no response within {}s",
                FETCH_TIMEOUT.as_secs()
            )))
        }
    };

    if !(200..300).contains(&resp.status) {
        return Err(FeatureFlagError::Http(format!(
            "status {}: {}",
            resp.status,
            resp.body_text()
        )));
    }

    serde_json::from_slice(&resp.body)
        .map_err(|e| FeatureFlagError::Parse(format!("failed to parse response: {e}")))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn default_refresh_interval() -> Duration {
    if cfg!(debug_assertions) {
        Duration::from_secs(20 * 60) // 20 minutes in debug
    } else {
        Duration::from_secs(6 * 3600) // 6 hours in release
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_value_from_bool() {
        let v: FlagValue = true.into();
        assert_eq!(v.as_bool(), Some(true));
    }

    #[test]
    fn flag_value_as_bool_none_for_number() {
        let v = FlagValue::Number(42.0);
        assert_eq!(v.as_bool(), None);
    }

    #[test]
    fn is_enabled_returns_false_for_missing_flag() {
        let client = FeatureFlagClient::new("".into(), "".into());
        assert!(!client.is_enabled("nonexistent"));
    }

    #[test]
    fn is_enabled_checks_cache() {
        let mut client = FeatureFlagClient::new("".into(), "".into());
        client
            .cache
            .insert("test_flag".into(), FlagValue::Bool(true));
        assert!(client.is_enabled("test_flag"));
    }

    #[test]
    fn is_enabled_returns_false_for_killswitched_flag() {
        let mut client = FeatureFlagClient::new("".into(), "".into());
        client
            .cache
            .insert("test_flag".into(), FlagValue::Bool(true));
        client.killswitches.insert("test_flag".into(), true);
        assert!(!client.is_enabled("test_flag"));
    }

    #[test]
    fn get_value_none_for_killswitched_flag() {
        let mut client = FeatureFlagClient::new("".into(), "".into());
        client
            .cache
            .insert("test_flag".into(), FlagValue::String("hello".into()));
        client.killswitches.insert("test_flag".into(), true);
        assert!(client.get_value("test_flag").is_none());
    }

    #[test]
    fn apply_flags_handles_killswitch_prefix() {
        let mut client = FeatureFlagClient::new("".into(), "".into());
        let mut flags = HashMap::new();

        // A killswitch flag
        flags.insert(
            "tengu_frond_boric_some_feature".into(),
            FeatureFlagDefinition {
                key: "tengu_frond_boric_some_feature".into(),
                default_value: FlagValue::Bool(true),
                rules: vec![],
                killswitch: false,
            },
        );

        // A normal flag
        flags.insert(
            "some_feature".into(),
            FeatureFlagDefinition {
                key: "some_feature".into(),
                default_value: FlagValue::Bool(true),
                rules: vec![],
                killswitch: false,
            },
        );

        let resp = RemoteFlagsResponse {
            flags,
            features: HashMap::new(),
        };
        client.apply_flags(resp);

        // Killswitch is active for "some_feature"
        assert!(client.is_killswitched("some_feature"));
        // "some_feature" itself should not be in the cache (it's killswitched)
        assert!(!client.is_enabled("some_feature"));
    }

    #[test]
    fn apply_flags_handles_killswitch_attribute() {
        let mut client = FeatureFlagClient::new("".into(), "".into());
        let mut flags = HashMap::new();

        flags.insert(
            "other_feature".into(),
            FeatureFlagDefinition {
                key: "other_feature".into(),
                default_value: FlagValue::Bool(true),
                rules: vec![],
                killswitch: true, // This flag is a killswitch for itself
            },
        );

        let resp = RemoteFlagsResponse {
            flags,
            features: HashMap::new(),
        };
        client.apply_flags(resp);

        // "other_feature" is killswitched and should not be in cache
        assert!(!client.is_enabled("other_feature"));
    }

    #[test]
    fn evaluate_with_context_no_rules_returns_default() {
        let mut client = FeatureFlagClient::new("".into(), "".into());
        let mut flags = HashMap::new();
        flags.insert(
            "my_flag".into(),
            FeatureFlagDefinition {
                key: "my_flag".into(),
                default_value: FlagValue::Bool(true),
                rules: vec![],
                killswitch: false,
            },
        );
        let resp = RemoteFlagsResponse {
            flags,
            features: HashMap::new(),
        };
        client.apply_flags(resp);

        let ctx = FlagContext::with_user_id("user1");
        let (val, rule_idx) = client.evaluate_with_context("my_flag", &ctx);
        assert_eq!(val, Some(FlagValue::Bool(true)));
        assert_eq!(rule_idx, None);
    }

    #[test]
    fn percent_rollout_hit() {
        let client = FeatureFlagClient::new("".into(), "".into());
        // "stable_key" should always produce the same hash
        let hit = client.rollout_hit("stable_key", 100.0);
        assert!(hit);

        let miss = client.rollout_hit("stable_key", 0.0);
        assert!(!miss);
    }

    #[test]
    fn simple_hash_is_stable() {
        assert_eq!(simple_hash("hello"), simple_hash("hello"));
        assert_ne!(simple_hash("hello"), simple_hash("world"));
    }

    #[test]
    fn condition_matches_all() {
        let client = FeatureFlagClient::new("".into(), "".into());
        assert!(client.condition_matches_no_context("all"));
        assert!(client.condition_matches_no_context("ALL"));
    }

    #[test]
    fn condition_matches_percent_rollout() {
        let client = FeatureFlagClient::new("".into(), "".into());
        assert!(client.condition_matches_no_context("percentRollout(50)"));
    }

    #[test]
    fn flag_context_builder() {
        let ctx = FlagContext::with_user_id("alice")
            .with_attribute("plan", "enterprise")
            .with_attribute("region", "us-east");
        assert_eq!(ctx.user_id, Some("alice".into()));
        assert_eq!(ctx.attributes.get("plan"), Some(&"enterprise".into()));
        assert_eq!(ctx.attributes.get("region"), Some(&"us-east".into()));
    }

    #[test]
    fn is_cache_stale_initial_not_stale() {
        let client = FeatureFlagClient::new("".into(), "".into());
        assert!(!client.is_cache_stale());
    }

    #[test]
    fn new_client_defaults() {
        let client = FeatureFlagClient::new("https://flags.example.com".into(), "key123".into());
        assert!(!client.is_enabled("anything"));
        assert!(client.get_value("anything").is_none());
    }

    #[test]
    fn remote_flags_response_into_flags_uses_flags_first() {
        let mut flags = HashMap::new();
        flags.insert(
            "a".into(),
            FeatureFlagDefinition {
                key: "a".into(),
                default_value: FlagValue::Bool(true),
                rules: vec![],
                killswitch: false,
            },
        );
        let mut features = HashMap::new();
        features.insert(
            "b".into(),
            FeatureFlagDefinition {
                key: "b".into(),
                default_value: FlagValue::Bool(true),
                rules: vec![],
                killswitch: false,
            },
        );
        let resp = RemoteFlagsResponse {
            flags: flags.clone(),
            features,
        };
        let result = resp.into_flags();
        assert!(result.contains_key("a"));
        assert!(!result.contains_key("b"));
    }

    #[test]
    fn remote_flags_response_into_flags_falls_back_to_features() {
        let mut features = HashMap::new();
        features.insert(
            "c".into(),
            FeatureFlagDefinition {
                key: "c".into(),
                default_value: FlagValue::Bool(false),
                rules: vec![],
                killswitch: false,
            },
        );
        let resp = RemoteFlagsResponse {
            flags: HashMap::new(),
            features,
        };
        let result = resp.into_flags();
        assert!(result.contains_key("c"));
    }

    #[test]
    fn sticky_mode_preserves_cached_values() {
        let mut client = FeatureFlagClient::new("".into(), "".into());
        client.sticky = true;

        // First load: flag is true
        let mut flags = HashMap::new();
        flags.insert(
            "my_flag".into(),
            FeatureFlagDefinition {
                key: "my_flag".into(),
                default_value: FlagValue::Bool(true),
                rules: vec![],
                killswitch: false,
            },
        );
        client.apply_flags(RemoteFlagsResponse {
            flags,
            features: HashMap::new(),
        });
        assert!(client.is_enabled("my_flag"));

        // Second load: flag is now false
        let mut flags2 = HashMap::new();
        flags2.insert(
            "my_flag".into(),
            FeatureFlagDefinition {
                key: "my_flag".into(),
                default_value: FlagValue::Bool(false),
                rules: vec![],
                killswitch: false,
            },
        );
        client.apply_flags(RemoteFlagsResponse {
            flags: flags2,
            features: HashMap::new(),
        });

        // Sticky mode: should still be true
        assert!(client.is_enabled("my_flag"));
    }

    #[test]
    fn non_sticky_mode_updates_values() {
        let mut client = FeatureFlagClient::new("".into(), "".into());
        client.sticky = false;

        // First load: flag is true
        let mut flags = HashMap::new();
        flags.insert(
            "my_flag".into(),
            FeatureFlagDefinition {
                key: "my_flag".into(),
                default_value: FlagValue::Bool(true),
                rules: vec![],
                killswitch: false,
            },
        );
        client.apply_flags(RemoteFlagsResponse {
            flags,
            features: HashMap::new(),
        });
        assert!(client.is_enabled("my_flag"));

        // Second load: flag is now false
        let mut flags2 = HashMap::new();
        flags2.insert(
            "my_flag".into(),
            FeatureFlagDefinition {
                key: "my_flag".into(),
                default_value: FlagValue::Bool(false),
                rules: vec![],
                killswitch: false,
            },
        );
        client.apply_flags(RemoteFlagsResponse {
            flags: flags2,
            features: HashMap::new(),
        });

        // Non-sticky: should be updated to false
        assert!(!client.is_enabled("my_flag"));
    }
}
