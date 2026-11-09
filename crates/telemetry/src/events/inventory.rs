//! The event kinds as data, for the inventory in `docs/telemetry.md`.
//!
//! What each kind means is declared once, beside its constructor in
//! `event_kinds!`; its fields are read off the payload type's JSON schema.
//! [`render_markdown`] turns both into the table, and
//! `crates/telemetry/tests/producers.rs` fails when the document and the code
//! disagree.

use serde_json::Value;

/// Which file an event is written to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recorded {
    /// The session's own `telemetry/<session-id>.jsonl`.
    Session,
    /// The daemon's `telemetry/pool.jsonl`: things that happen to no session.
    Pool,
    /// Either, depending on where it was emitted.
    Both,
}

impl Recorded {
    fn label(self) -> &'static str {
        match self {
            Recorded::Session => "会话",
            Recorded::Pool => "池",
            Recorded::Both => "会话 / 池",
        }
    }
}

pub struct KindInfo {
    pub kind: &'static str,
    pub when: &'static str,
    pub recorded: Recorded,
    pub schema: fn() -> schemars::Schema,
}

impl KindInfo {
    /// The payload's fields, sorted: `name`, `name?` when it can be absent or
    /// `null`, and the values an enum field takes.
    pub fn fields(&self) -> Vec<String> {
        let schema = (self.schema)();
        let root = schema.as_value();
        let required: Vec<&str> = root["required"]
            .as_array()
            .map(|names| names.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let Some(properties) = root["properties"].as_object() else {
            return Vec::new();
        };
        let mut names: Vec<&String> = properties.keys().collect();
        names.sort();
        names
            .into_iter()
            .map(|name| {
                let optional = if required.contains(&name.as_str()) {
                    ""
                } else {
                    "?"
                };
                let values = enum_values(root, &properties[name]);
                if values.is_empty() {
                    format!("`{name}{optional}`")
                } else {
                    format!("`{name}{optional}`({})", values.join(" / "))
                }
            })
            .collect()
    }
}

fn enum_values(root: &Value, property: &Value) -> Vec<String> {
    let resolved = match property["$ref"].as_str() {
        Some(reference) => reference
            .strip_prefix("#/$defs/")
            .map(|name| &root["$defs"][name])
            .unwrap_or(property),
        None => property,
    };
    let quoted = |v: &Value| v.as_str().map(|s| format!("`{s}`"));
    if let Some(values) = resolved["enum"].as_array() {
        return values.iter().filter_map(quoted).collect();
    }
    resolved["oneOf"]
        .as_array()
        .map(|variants| {
            variants
                .iter()
                .filter_map(|v| v.get("const").and_then(quoted))
                .collect()
        })
        .unwrap_or_default()
}

/// The inventory table of `docs/telemetry.md`.
pub fn render_markdown() -> String {
    let mut out = String::new();
    out.push_str("| `type` | 何时发 | 记在哪 | 字段(信封之外) |\n");
    out.push_str("|---|---|---|---|\n");
    for info in crate::EventPayload::inventory() {
        out.push_str(&format!(
            "| `{}` | {} | {} | {} |\n",
            info.kind,
            info.when.trim(),
            info.recorded.label(),
            info.fields().join("、"),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_inventory_has_every_kind_in_order() {
        let kinds: Vec<&str> = crate::EventPayload::inventory()
            .iter()
            .map(|i| i.kind)
            .collect();
        assert_eq!(kinds, crate::EventPayload::KINDS);
    }

    #[test]
    fn every_kind_has_fields_and_says_when() {
        for info in crate::EventPayload::inventory() {
            assert!(!info.when.trim().is_empty(), "{}", info.kind);
            assert!(!info.fields().is_empty(), "{}", info.kind);
        }
    }

    #[test]
    fn enum_fields_list_their_values_and_optional_fields_say_so() {
        let tool_execution = crate::EventPayload::inventory()
            .into_iter()
            .find(|i| i.kind == "tool_execution")
            .unwrap();
        let fields = tool_execution.fields().join(" ");
        assert!(
            fields.contains("`outcome`(`approved` / `denied` / `succeeded`"),
            "{fields}"
        );
        assert!(fields.contains("`error_message?`"), "{fields}");
        assert!(fields.contains("`tool_name`"), "{fields}");
    }
}
