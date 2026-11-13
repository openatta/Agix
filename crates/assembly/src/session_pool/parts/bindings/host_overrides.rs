//! 宿主在内存里改过的设置：宿主拿到守护进程自己那一对的设置之后、交给池之前改掉的值，
//! 作为一层叠在池从盘上读入的每份设置上，和托管层在同一个位置。

use base::settings::{LoadedSettings, Settings};
use serde_json::Value;

/// The values the host's settings hold that the same pair read from disk does
/// not: what the host set in memory, as a JSON layer.
#[derive(Debug, Default, Clone)]
pub(in crate::session_pool) struct HostOverrides(Option<Value>);

impl HostOverrides {
    /// What `host` sets differently from `disk`. A key the host removed from a
    /// map is not carried: a layer can only add or replace.
    pub(in crate::session_pool) fn between(disk: &Settings, host: &Settings) -> Self {
        match (serde_json::to_value(disk), serde_json::to_value(host)) {
            (Ok(disk), Ok(host)) => Self(changed(&disk, &host)),
            _ => Self(None),
        }
    }

    /// `settings` with this layer merged over its values.
    pub(in crate::session_pool) fn apply(&self, mut settings: LoadedSettings) -> LoadedSettings {
        let Some(layer) = &self.0 else {
            return settings;
        };
        let Ok(mut merged) = serde_json::to_value(&settings.values) else {
            return settings;
        };
        base::settings::merge_json_values(&mut merged, layer.clone());
        match serde_json::from_value(merged) {
            Ok(values) => settings.values = values,
            Err(e) => tracing::warn!(error = %e, "the host's settings do not apply here; left out"),
        }
        settings
    }
}

fn changed(from: &Value, to: &Value) -> Option<Value> {
    match (from, to) {
        (Value::Object(from), Value::Object(to)) => {
            let layer: serde_json::Map<String, Value> = to
                .iter()
                .filter_map(|(key, value)| {
                    let diff = match from.get(key) {
                        Some(old) => changed(old, value)?,
                        None => value.clone(),
                    };
                    Some((key.clone(), diff))
                })
                .collect();
            (!layer.is_empty()).then_some(Value::Object(layer))
        }
        _ => (from != to).then(|| to.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_what_the_host_changed_is_carried_and_it_lands_on_other_settings() {
        let disk = Settings::defaults_for("m");
        let mut host = disk.clone();
        host.memory_enabled = false;
        host.model.max_tokens = 7;
        let overrides = HostOverrides::between(&disk, &host);
        assert_eq!(
            overrides.0,
            Some(serde_json::json!({"memory_enabled": false, "model": {"max_tokens": 7}}))
        );

        let mut other = Settings::defaults_for("other-model");
        other.language = Some("fr".into());
        let paths = LoadedSettings::new(disk.clone(), Default::default()).paths;
        let applied = overrides.apply(LoadedSettings::new(other, paths));
        assert!(!applied.values.memory_enabled);
        assert_eq!(applied.values.model.max_tokens, 7);
        assert_eq!(applied.values.model.model_name, "other-model");
        assert_eq!(applied.values.language.as_deref(), Some("fr"));

        assert!(HostOverrides::between(&disk, &disk).0.is_none());
    }
}
