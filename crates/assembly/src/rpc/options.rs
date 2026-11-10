//! 会话的 `options`：字段定义，以及逐字段宽松读取、读不懂的跳过并报告的读法。

use serde::Serialize;
use serde_json::Value;

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

impl SessionOptions {
    /// Every field `options` may carry.
    pub const FIELDS: &'static [&'static str] = &[
        "recorder",
        "clock",
        "telemetry",
        "permission_mode",
        "permission_rules",
        "output_schema",
        "provider",
        "model",
        "prompt",
        "prompt_types",
    ];

    /// The fields whose value is a list. Each entry is read on its own, so
    /// one bad entry is left out without the rest — a bad allow rule must not
    /// take the deny rules beside it along.
    pub const LIST_FIELDS: &'static [&'static str] = &["permission_rules", "prompt_types"];

    /// Not a session option but an instruction about reading them: with it,
    /// what [`Self::from_params`] would leave out makes the call fail.
    pub const STRICT: &'static str = "strict";

    /// `params.options` as far as it can be read, and what in it could not
    /// be: a field this daemon does not know, one whose value does not
    /// parse, or one entry of a list field. Each field is read on its own, so
    /// one bad field leaves the rest in force.
    pub fn from_params(params: &Value) -> (Option<Self>, Vec<IgnoredOption>) {
        Self::from_options(params.get("options"))
    }

    /// [`Self::from_params`] on the `options` value itself.
    pub fn from_options(options: Option<&Value>) -> (Option<Self>, Vec<IgnoredOption>) {
        let fields = match options {
            None | Some(Value::Null) => return (None, Vec::new()),
            Some(Value::Object(fields)) => fields,
            Some(_) => {
                return (
                    None,
                    vec![IgnoredOption {
                        field: None,
                        error: "options must be an object".to_string(),
                    }],
                )
            }
        };
        let mut ignored = Vec::new();
        let mut readable = serde_json::Map::new();
        for (name, value) in fields {
            if name == Self::STRICT {
                if !value.is_boolean() {
                    ignored.push(IgnoredOption {
                        field: Some(name.clone()),
                        error: "must be a boolean".to_string(),
                    });
                }
                continue;
            }
            if !Self::FIELDS.contains(&name.as_str()) {
                ignored.push(IgnoredOption {
                    field: Some(name.clone()),
                    error: "unknown field".to_string(),
                });
                continue;
            }
            let value = match value {
                Value::Array(entries) if Self::LIST_FIELDS.contains(&name.as_str()) => {
                    Value::Array(Self::readable_entries(name, entries, &mut ignored))
                }
                _ => value.clone(),
            };
            match Self::read_alone(name, &value) {
                Ok(()) => {
                    readable.insert(name.clone(), value);
                }
                Err(error) => ignored.push(IgnoredOption {
                    field: Some(name.clone()),
                    error,
                }),
            }
        }
        if readable.is_empty() {
            return (None, ignored);
        }
        match serde_json::from_value(Value::Object(readable)) {
            Ok(options) => (Some(options), ignored),
            Err(e) => {
                ignored.push(IgnoredOption {
                    field: None,
                    error: e.to_string(),
                });
                (None, ignored)
            }
        }
    }

    /// Whether `params.options` asks for strict reading. A `strict` that is
    /// not a boolean counts as asking: it was meant as something.
    pub fn strict_requested(params: &Value) -> bool {
        Self::strict_in(params.get("options"))
    }

    fn strict_in(options: Option<&Value>) -> bool {
        options
            .and_then(|o| o.get(Self::STRICT))
            .is_some_and(|v| v != &Value::Bool(false))
    }

    fn read_alone(name: &str, value: &Value) -> Result<(), String> {
        let alone = Value::Object([(name.to_string(), value.clone())].into_iter().collect());
        serde_json::from_value::<Self>(alone)
            .map(drop)
            .map_err(|e| e.to_string())
    }

    fn readable_entries(
        name: &str,
        entries: &[Value],
        ignored: &mut Vec<IgnoredOption>,
    ) -> Vec<Value> {
        let mut kept = Vec::new();
        for (index, entry) in entries.iter().enumerate() {
            match Self::read_alone(name, &Value::Array(vec![entry.clone()])) {
                Ok(()) => kept.push(entry.clone()),
                Err(error) => ignored.push(IgnoredOption {
                    field: Some(format!("{name}[{index}]")),
                    error,
                }),
            }
        }
        kept
    }

    /// Whether the host answers the model's questions (`AskUserQuestion`).
    pub fn answers_questions(&self) -> bool {
        self.prompt_types.iter().any(|t| t == PROMPT_TYPE_QUESTION)
    }
}

/// A field of `options` the daemon left out, as the response's
/// `ignored_options` reports it. `field` is `None` when `options` as a whole
/// could not be read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct IgnoredOption {
    pub field: Option<String>,
    pub error: String,
}

/// `options` as a method's params carry it: any JSON, read field by field by
/// [`read_options`] rather than refused whole.
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)]
#[serde(transparent)]
pub struct RawOptions(pub Value);

impl schemars::JsonSchema for RawOptions {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "RawOptions".into()
    }

    fn inline_schema() -> bool {
        true
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        generator.subschema_for::<SessionOptions>()
    }
}

/// `options` as [`SessionOptions::from_options`] reads it, with a warning
/// for each field it left out — or, when the options ask for `strict`, an
/// `INVALID_PARAMS` error naming them instead.
pub fn read_options(
    method: &str,
    options: Option<&RawOptions>,
) -> Result<(Option<SessionOptions>, Vec<IgnoredOption>), super::RpcError> {
    let options = options.map(|raw| &raw.0);
    let (read, ignored) = SessionOptions::from_options(options);
    if !ignored.is_empty() && SessionOptions::strict_in(options) {
        let fields: Vec<&str> = ignored
            .iter()
            .map(|i| i.field.as_deref().unwrap_or("options"))
            .collect();
        return Err(super::RpcError {
            code: super::codes::INVALID_PARAMS,
            message: format!("options that do not read: {}", fields.join(", ")),
            data: Some(serde_json::json!({ "invalid_options": ignored })),
        });
    }
    for left_out in &ignored {
        tracing::warn!(
            method,
            field = left_out.field.as_deref().unwrap_or("options"),
            error = %left_out.error,
            "ignored part of `options`"
        );
    }
    Ok((read, ignored))
}

/// `prompt_type` of a `prompt` frame that asks whether a tool call may run.
pub const PROMPT_TYPE_PERMISSION: &str = "permission";
/// `prompt_type` of a `prompt` frame that carries the model's question.
pub const PROMPT_TYPE_QUESTION: &str = "question";

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_listed_option_fields_are_exactly_the_struct_s() {
        let every_field = SessionOptions {
            recorder: Some(serde_json::from_value(serde_json::json!({"mode": "record"})).unwrap()),
            clock: Some(ClockOptions {
                start: "2026-10-28T09:00:00Z".into(),
            }),
            telemetry: Some(TelemetryOptions {
                output: "/tmp/t.jsonl".into(),
            }),
            permission_mode: Some(base::settings::PermissionMode::Plan),
            permission_rules: vec![serde_json::from_value(serde_json::json!({
                "source": "session", "behavior": "allow", "tool_name": "Read"
            }))
            .unwrap()],
            output_schema: Some(serde_json::json!({})),
            provider: Some("p".into()),
            model: Some("m".into()),
            prompt: Some(Default::default()),
            prompt_types: vec![PROMPT_TYPE_QUESTION.into()],
        };
        let serialized = serde_json::to_value(&every_field).unwrap();
        let mut names: Vec<&str> = serialized
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        let mut listed = SessionOptions::FIELDS.to_vec();
        names.sort_unstable();
        listed.sort_unstable();
        assert_eq!(names, listed);

        let mut lists: Vec<&str> = serialized
            .as_object()
            .unwrap()
            .iter()
            .filter(|(_, v)| v.is_array())
            .map(|(k, _)| k.as_str())
            .collect();
        let mut listed = SessionOptions::LIST_FIELDS.to_vec();
        lists.sort_unstable();
        listed.sort_unstable();
        assert_eq!(lists, listed, "a list field is not read entry by entry");
    }

    #[test]
    fn a_bad_entry_of_a_list_field_is_left_out_alone() {
        let params = serde_json::json!({"options": {
            "permission_rules": [
                {"source": "session", "behavior": "allwo", "tool_name": "Read"},
                {"source": "session", "behavior": "deny", "tool_name": "Bash"},
            ],
            "prompt_types": [7, "question"],
        }});
        let (options, ignored) = SessionOptions::from_params(&params);
        let options = options.unwrap();
        assert_eq!(options.permission_rules.len(), 1);
        assert_eq!(options.permission_rules[0].tool_name, "Bash");
        assert!(options.answers_questions());
        let mut fields: Vec<_> = ignored.iter().map(|i| i.field.as_deref()).collect();
        fields.sort();
        assert_eq!(
            fields,
            [Some("permission_rules[0]"), Some("prompt_types[0]")]
        );

        let (_, ignored) =
            SessionOptions::from_params(&serde_json::json!({"options": {"permission_rules": {}}}));
        assert_eq!(ignored[0].field.as_deref(), Some("permission_rules"));
    }

    #[test]
    fn strict_is_read_but_not_kept_as_an_option() {
        let on = serde_json::json!({"options": {"strict": true, "model": "m"}});
        let (options, ignored) = SessionOptions::from_params(&on);
        assert!(ignored.is_empty(), "{ignored:?}");
        assert_eq!(options.unwrap().model.as_deref(), Some("m"));
        assert!(SessionOptions::strict_requested(&on));

        let off = serde_json::json!({"options": {"strict": false}});
        assert!(!SessionOptions::strict_requested(&off));
        assert!(!SessionOptions::strict_requested(&serde_json::json!({})));

        let odd = serde_json::json!({"options": {"strict": "yes"}});
        assert!(SessionOptions::strict_requested(&odd));
        let (_, ignored) = SessionOptions::from_params(&odd);
        assert_eq!(ignored[0].field.as_deref(), Some("strict"));
    }

    #[test]
    fn a_bad_option_field_is_reported_and_the_rest_still_apply() {
        let params = serde_json::json!({"options": {
            "permission_mode": "plann",
            "model": "m",
            "permision_rules": [],
        }});
        let (options, mut ignored) = SessionOptions::from_params(&params);
        assert_eq!(options.unwrap().model.as_deref(), Some("m"));
        ignored.sort_by(|a, b| a.field.cmp(&b.field));
        let fields: Vec<_> = ignored.iter().map(|i| i.field.as_deref()).collect();
        assert_eq!(fields, [Some("permision_rules"), Some("permission_mode")]);
        assert_eq!(ignored[0].error, "unknown field");
        assert!(ignored[1].error.contains("plann"), "{:?}", ignored[1]);

        let (options, ignored) = SessionOptions::from_params(&serde_json::json!({}));
        assert!(options.is_none() && ignored.is_empty());
        let (options, ignored) = SessionOptions::from_params(&serde_json::json!({"options": 3}));
        assert!(options.is_none());
        assert_eq!(ignored[0].field, None);
    }
}
