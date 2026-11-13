//! Every settings.json example in the README and `docs/` — fenced `json` /
//! `jsonc` blocks and inline `{…}` or `"key": …` spans whose top-level keys
//! are all settings keys — must pass the generated settings schema and load
//! without a value left out; the few the docs show on purpose as ignored must
//! be refused by both. An editor flagging a documented example, or the loader
//! quietly dropping one the schema accepts, is the same mismatch.

#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};

use base::interface::config_source::{InMemoryLayers, Tier};
use base::settings::{settings_json_schema, unknown_settings_keys, LoadedSettings};
use serde_json::Value;

struct Example {
    at: String,
    value: Value,
}

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn documents() -> Vec<PathBuf> {
    let mut files = vec![repo().join("README.md")];
    let mut docs: Vec<PathBuf> = std::fs::read_dir(repo().join("docs"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|e| e == "md"))
        .collect();
    docs.sort();
    files.extend(docs);
    files
}

fn without_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            match c {
                '\\' => out.extend(chars.next()),
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match (c, chars.peek()) {
            ('"', _) => {
                in_string = true;
                out.push(c);
            }
            ('/', Some('/')) => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            _ => out.push(c),
        }
    }
    out
}

fn is_settings(value: &Value, keys: &[String]) -> bool {
    value.as_object().is_some_and(|object| {
        !object.is_empty()
            && object
                .keys()
                .all(|k| k == "$schema" || keys.iter().any(|known| known == k))
    })
}

fn examples(keys: &[String]) -> Vec<Example> {
    let mut found = Vec::new();
    for file in documents() {
        let name = file
            .strip_prefix(repo())
            .unwrap_or(&file)
            .display()
            .to_string();
        let text = std::fs::read_to_string(&file).unwrap();
        let mut lines = text.lines().enumerate();
        while let Some((number, line)) = lines.next() {
            let trimmed = line.trim_start();
            if let Some(lang) = trimmed.strip_prefix("```") {
                let body: Vec<&str> = lines
                    .by_ref()
                    .map(|(_, l)| l)
                    .take_while(|l| !l.trim_start().starts_with("```"))
                    .collect();
                if matches!(lang.trim(), "json" | "jsonc") {
                    if let Ok(value) =
                        serde_json::from_str::<Value>(&without_comments(&body.join("\n")))
                    {
                        if is_settings(&value, keys) {
                            found.push(Example {
                                at: format!("{name}:{}", number + 1),
                                value,
                            });
                        }
                    }
                }
                continue;
            }
            for span in line.split('`').skip(1).step_by(2) {
                let span = span.trim();
                let candidate = if span.starts_with('{') {
                    span.to_string()
                } else if span.starts_with('"') {
                    format!("{{{span}}}")
                } else {
                    continue;
                };
                if let Ok(value) = serde_json::from_str::<Value>(&candidate) {
                    if is_settings(&value, keys) {
                        found.push(Example {
                            at: format!("{name}:{}", number + 1),
                            value,
                        });
                    }
                }
            }
        }
    }
    found
}

/// What the loader makes of one file on its own: the values it left out.
fn left_out_by_loader(example: &Example) -> Vec<String> {
    let source =
        InMemoryLayers::new().with(Tier::Global, false, &example.at, example.value.clone());
    let dir = PathBuf::from("/nonexistent");
    let settings = LoadedSettings::load_from(
        &source,
        dir.join("global"),
        dir.join("scene"),
        dir.join("project/.agix"),
        "code",
        "model",
    );
    let mut left_out: Vec<String> = settings
        .ignored
        .into_iter()
        .map(|ignored| format!("{}: {}", ignored.key, ignored.error))
        .collect();
    left_out.extend(
        unknown_settings_keys(&example.value)
            .into_iter()
            .map(|key| format!("unknown key {key}")),
    );
    left_out
}

#[test]
fn every_documented_settings_example_is_valid_and_loads_whole() {
    let schema = settings_json_schema();
    let keys: Vec<String> = schema["properties"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    let validator = jsonschema::validator_for(&schema).unwrap();
    let examples = examples(&keys);
    for expected in ["README.md", "docs/migrating_to_0.3.md", "docs/hooks.md"] {
        assert!(
            examples.iter().any(|e| e.at.starts_with(expected)),
            "no settings example found in {expected}; the extraction is broken"
        );
    }
    // Values the docs show on purpose as what gets ignored.
    let counterexamples = [
        serde_json::json!({"language": 5}),
        serde_json::json!({"permission_mode": "plann"}),
    ];
    // `plugin.list`'s result, which shares a settings key and is no settings file.
    let not_settings = [serde_json::json!({"plugins": []})];

    let mut wrong = Vec::new();
    for example in examples.iter().filter(|e| !not_settings.contains(&e.value)) {
        let schema_errors: Vec<String> = validator
            .iter_errors(&example.value)
            .map(|error| format!("{error} at {}", error.instance_path()))
            .collect();
        let left_out = left_out_by_loader(example);
        if counterexamples.contains(&example.value) {
            if schema_errors.is_empty() || left_out.is_empty() {
                wrong.push(format!(
                    "{}: a counterexample, but schema errors {schema_errors:?}, loader left out {left_out:?}",
                    example.at
                ));
            }
        } else if !schema_errors.is_empty() || !left_out.is_empty() {
            wrong.push(format!(
                "{}: {}: schema errors {schema_errors:?}, loader left out {left_out:?}",
                example.at, example.value
            ));
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

#[test]
fn the_schema_accepts_what_the_loader_starts_from() {
    let validator = jsonschema::validator_for(&settings_json_schema()).unwrap();
    for minimal in [
        serde_json::json!({}),
        serde_json::json!({"model": {"model_name": "m"}}),
        serde_json::json!({"sandbox": {"network_mode": "deny_all"}}),
        serde_json::json!({"execution": {"max_turns": 10}}),
    ] {
        let errors: Vec<String> = validator
            .iter_errors(&minimal)
            .map(|e| e.to_string())
            .collect();
        assert!(errors.is_empty(), "{minimal}: {errors:?}");
    }
    for refused in [
        serde_json::json!({"permission_mode": "bubble"}),
        serde_json::json!({"permission_rules": [{"tool": "Bash"}]}),
        serde_json::json!({"model": {"max_tokens": "many"}}),
    ] {
        assert!(!validator.is_valid(&refused), "{refused} passed the schema");
    }
}
