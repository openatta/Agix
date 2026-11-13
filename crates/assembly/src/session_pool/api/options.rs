//! 会话的 `options` 领域类型。serde 形状会写进会话日志的 `agix.session/spec` 条目，
//! 改字段名或默认值就是改持久化格式。

use super::AskKind;

/// Optional per-session configuration passed via `session.create` or
/// `session.run_turn` params.
///
/// Taken when the session is created and kept with it: every rebuild of the
/// session's engine — a settings change, an idle eviction, a daemon restart —
/// applies the same options again. Options sent with a later call to an
/// existing session are ignored.
#[derive(Debug, Clone, Default, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct SessionOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recorder: Option<RecorderOptions>,
    /// Where this session's kept time starts (the date in its prompt, its log's timestamps).
    /// Omitted: the machine's clock — except in replay, where it starts when the recording did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock: Option<ClockOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub telemetry: Option<TelemetryOptions>,
    /// This session's permission mode. `None` takes the settings'
    /// `permission_mode`. A session may always ask for a stricter mode than
    /// the settings'; a looser one is clamped unless the settings set
    /// `allow_client_permission_override`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<base::settings::PermissionMode>,
    /// Allow/deny/ask rules for the `RuleSetPermission` this session's
    /// `permission_mode` (if set) constructs. Ignored when `permission_mode`
    /// is `None`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub permission_rules: Vec<base::permission::PermissionRule>,
    /// JSON Schema the session's structured output must satisfy — see
    /// `runtime::agent::Builder::output_schema`.
    pub output_schema: Option<serde_json::Value>,
    /// The provider this session's conversation goes to, by its id in
    /// `providers`. `None` follows the settings' routing. Sub-agents,
    /// compaction and the other routed tasks keep the settings' routing
    /// either way.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// The conversation's model. `None` takes the provider's `default_model`
    /// when `provider` is given, the settings' model otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// A system-prompt append / override for this session's own agent, over
    /// the settings files'. Its delegates do not get it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<runtime::agent::SessionPrompt>,
    /// The kinds of `prompt` frame this session's host answers: `permission`
    /// always, `question` when listed. Names this daemon does not know are
    /// ignored rather than voiding the options.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prompt_types: Vec<String>,
}

/// Record or replay this session's LLM calls.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
pub struct RecorderOptions {
    pub mode: String, // "record" | "replay" | "rerun"
    /// Directory name for the recording under `dir`. Omitted, the session id
    /// is used — which is what production recording wants, and what a replay
    /// must therefore override with the name of the recording it wants back.
    #[serde(default)]
    pub name: Option<String>,
    /// Absolute path to the recordings root. Omitted, the daemon's own root is
    /// used — `settings.recorder.root` if configured, else
    /// `ConfigPaths::global_recordings_dir()`.
    #[serde(default)]
    pub dir: Option<String>,
    /// On replay, whether a live request that differs from the recorded one
    /// fails the call (`true`) or only warns (`false`).
    #[serde(default)]
    pub strict: bool,
}

#[derive(
    Debug, Clone, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize, schemars::JsonSchema,
)]
#[serde(default)]
pub struct ClockOptions {
    /// RFC 3339, e.g. `2026-10-28T09:00:00Z`. The clock reads this when the session is built
    /// and runs on at wall-clock speed.
    pub start: String,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
pub struct TelemetryOptions {
    pub output: String, // absolute path to telemetry output file
}

impl SessionOptions {
    /// Whether the host answers the model's questions (`AskUserQuestion`).
    pub fn answers_questions(&self) -> bool {
        self.prompt_types
            .iter()
            .any(|t| t == AskKind::Question.as_str())
    }
}
