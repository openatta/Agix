//! 宿主在内存里改过的设置：宿主拿到守护进程自己那一对的设置之后、交给池之前改掉的值，
//! 作为一层叠在池从盘上读入的每份设置上，和托管层在同一个位置；宿主加的权限规则和
//! `max_tokens` 兜底只回到守护进程自己那一对上。

use base::settings::{LoadedRule, LoadedSettings, Settings};
use serde_json::Value;

/// What the host's settings hold that the same pair read from disk does not:
/// the values it set in memory, as a JSON layer, and the rules it added. And
/// the `max_tokens` they hold, which the pair falls back to when no settings
/// file sets one (`--max-tokens`).
#[derive(Debug, Default, Clone)]
pub(in crate::session_pool) struct HostOverrides {
    values: Option<Value>,
    rules: Vec<LoadedRule>,
    max_tokens: Option<u32>,
}

impl HostOverrides {
    /// What `host` sets differently from `disk`. A key the host removed from a
    /// map, or a rule it removed, is not carried: a layer can only add or
    /// replace.
    pub(in crate::session_pool) fn between(disk: &LoadedSettings, host: &LoadedSettings) -> Self {
        let values = match (
            serde_json::to_value(&disk.values),
            serde_json::to_value(&host.values),
        ) {
            (Ok(disk), Ok(host)) => changed(&disk, &host),
            _ => None,
        };
        let on_disk: Vec<_> = disk.rules.iter().map(rule_key).collect();
        let rules = host
            .rules
            .iter()
            .filter(|rule| !on_disk.contains(&rule_key(rule)))
            .cloned()
            .collect();
        Self {
            values,
            rules,
            max_tokens: Some(host.values.model.max_tokens),
        }
    }

    /// `settings` with the values this layer holds merged over its own.
    pub(in crate::session_pool) fn apply(&self, mut settings: LoadedSettings) -> LoadedSettings {
        let Some(layer) = &self.values else {
            return settings;
        };
        let Ok(mut merged) = serde_json::to_value(&settings.values) else {
            return settings;
        };
        base::settings::merge_json_values(&mut merged, layer.clone());
        match serde_json::from_value::<Settings>(merged) {
            Ok(values) => settings.values = values,
            Err(e) => tracing::warn!(error = %e, "the host's settings do not apply here; left out"),
        }
        settings
    }

    /// The host's `max_tokens` where no file set one, [`Self::apply`], and
    /// the rules the host added after them: what the host made of this
    /// daemon's own pair, on that pair read from disk.
    pub(in crate::session_pool) fn apply_to_own(
        &self,
        mut settings: LoadedSettings,
    ) -> LoadedSettings {
        if let Some(max_tokens) = self.max_tokens {
            if settings.values.model.max_tokens == base::settings::DEFAULT_MAX_TOKENS {
                settings.values.model.max_tokens = max_tokens;
            }
        }
        let mut settings = self.apply(settings);
        settings.rules.extend(self.rules.iter().cloned());
        settings
    }
}

fn rule_key(rule: &LoadedRule) -> (Option<Value>, base::interface::config_source::Tier, bool) {
    (
        serde_json::to_value(&rule.rule).ok(),
        rule.tier,
        rule.machine_local,
    )
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
    use base::interface::config_source::Tier;
    use base::settings::PermissionRuleEntry;

    fn loaded(values: Settings) -> LoadedSettings {
        LoadedSettings::new(values, Default::default())
    }

    fn rule(tool: &str) -> LoadedRule {
        LoadedRule::new(
            PermissionRuleEntry {
                tool: tool.into(),
                action: base::permission::RuleBehavior::Allow,
                matching: Default::default(),
            },
            Tier::Project,
            false,
        )
    }

    #[test]
    fn only_what_the_host_changed_is_carried_and_it_lands_on_other_settings() {
        let disk = Settings::defaults_for("m");
        let mut host = disk.clone();
        host.memory_enabled = false;
        host.model.max_tokens = 7;
        let overrides = HostOverrides::between(&loaded(disk.clone()), &loaded(host));
        assert_eq!(
            overrides.values,
            Some(serde_json::json!({"memory_enabled": false, "model": {"max_tokens": 7}}))
        );

        let mut other = Settings::defaults_for("other-model");
        other.language = Some("fr".into());
        let applied = overrides.apply(loaded(other));
        assert!(!applied.values.memory_enabled);
        assert_eq!(applied.values.model.max_tokens, 7);
        assert_eq!(applied.values.model.model_name, "other-model");
        assert_eq!(applied.values.language.as_deref(), Some("fr"));

        let same = HostOverrides::between(&loaded(disk.clone()), &loaded(disk));
        assert!(same.values.is_none());
    }

    #[test]
    fn the_rules_the_host_added_land_only_on_its_own_pair() {
        let mut disk = loaded(Settings::defaults_for("m"));
        disk.rules = vec![rule("Read")];
        let mut host = disk.clone();
        host.rules.push(rule("Bash(touch ruled.txt)"));
        let overrides = HostOverrides::between(&disk, &host);

        let tools = |settings: &LoadedSettings| -> Vec<String> {
            settings.rules.iter().map(|r| r.rule.tool.clone()).collect()
        };
        assert_eq!(
            tools(&overrides.apply_to_own(disk.clone())),
            ["Read", "Bash(touch ruled.txt)"]
        );
        assert_eq!(tools(&overrides.apply(disk)), ["Read"]);
    }
}
