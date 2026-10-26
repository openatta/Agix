//! Running plugin-provided hooks.
//!
//! Plugins reach the engine's lifecycle through the dispatcher that already
//! exists — `HookRunner` — rather than through new call sites in the turn
//! loop. This module supplies the backend that dispatcher delegates to for
//! `HookConfig::Wasm`, and is where a plugin's answer is narrowed to what a
//! downloaded package is allowed to say.

use hooks::config::{HookConfig, HookEvent};
use hooks::payload::{HookDecision, HookInput, HookResponse};
use hooks::runner::WasmHookExecutor;
use std::collections::HashMap;
use std::sync::Arc;
use wasm_host::PluginInstance;

/// Dispatches events to the components that subscribed to them.
pub struct WasmEvents {
    /// Plugin name → the instance to ask.
    ///
    /// Keyed by plugin rather than by component because that is the identity
    /// a `HookConfig::Wasm` entry carries. A manifest in which two components
    /// declare `events` is refused when it is parsed
    /// (`plugin::manifest`), so this mapping is unambiguous by construction.
    instances: HashMap<String, Arc<PluginInstance>>,
}

impl WasmEvents {
    pub fn new(instances: HashMap<String, Arc<PluginInstance>>) -> Self {
        Self { instances }
    }

    pub fn is_empty(&self) -> bool {
        self.instances.is_empty()
    }
}

#[async_trait::async_trait]
impl WasmHookExecutor for WasmEvents {
    async fn execute(
        &self,
        plugin: &str,
        payload: &HookInput,
        timeout_ms: Option<u64>,
    ) -> Result<HookResponse, String> {
        let instance = self
            .instances
            .get(plugin)
            .ok_or_else(|| format!("no loaded component for plugin `{plugin}`"))?;

        let payload_json =
            serde_json::to_string(payload).map_err(|e| format!("serialising the event: {e}"))?;

        // The hook entry's own deadline, when it carries one, bounds this
        // call — a hook runs inside a turn that is waiting on it, so it gets
        // no more time than it asked for. Cancellation is not wired: the
        // deadline is what bounds it either way, and a hook is short.
        let cancel = tokio_util::sync::CancellationToken::new();
        let decision = match timeout_ms {
            Some(ms) => {
                let deadline = std::time::Duration::from_millis(ms);
                match tokio::time::timeout(
                    deadline,
                    instance.on_event(&payload.hook_event_name, &payload_json, &cancel),
                )
                .await
                {
                    Ok(r) => r.map_err(|e| e.to_string())?,
                    Err(_) => {
                        return Err(format!("hook did not answer within {ms}ms"));
                    }
                }
            }
            None => instance
                .on_event(&payload.hook_event_name, &payload_json, &cancel)
                .await
                .map_err(|e| e.to_string())?,
        };

        Ok(into_response(decision))
    }
}

/// Narrow a component's answer to what a plugin may say.
///
/// `HookResponse` can also carry `updated_input`, which rewrites a tool's
/// arguments before it runs. That is deliberately unreachable from here.
/// Refusing a call and quietly changing what it does are different powers,
/// and only the first is one a package downloaded from a marketplace gets;
/// the second stays with hooks the user wrote themselves.
fn into_response(decision: wasm_host::bindings::agix::plugin::types::HookDecision) -> HookResponse {
    use wasm_host::bindings::agix::plugin::types::HookDecision as D;
    match decision {
        D::Proceed => HookResponse::default(),
        D::Block(reason) => HookResponse {
            decision: Some(HookDecision::Block),
            message: Some(reason),
            ..Default::default()
        },
        // Context a plugin adds is a message, not a rewrite: it appears
        // alongside what was already there and is attributable to the plugin
        // that said it.
        D::AddContext(text) => HookResponse {
            message: Some(text),
            ..Default::default()
        },
    }
}

/// The `HookConfig` entries a plugin's manifest asks for.
///
/// Event names were validated against the subscribable whitelist when the
/// manifest was parsed; anything that slips through here is dropped rather
/// than trusted, because this is the last point before the dispatcher.
pub fn hook_configs_for(plugin: &plugin::manifest::Plugin) -> Vec<(HookEvent, HookConfig)> {
    let mut out = Vec::new();
    for payload in &plugin.manifest.wasm {
        for name in &payload.events {
            match parse_event(name) {
                Some(event) => out.push((
                    event,
                    HookConfig::Wasm {
                        plugin: plugin.name().to_string(),
                        timeout: Some(payload.capabilities.timeout_ms),
                    },
                )),
                None => tracing::warn!(
                    plugin = %plugin.name(),
                    event = %name,
                    "plugin subscribed to an event this build does not know; ignoring"
                ),
            }
        }
    }
    out
}

fn parse_event(name: &str) -> Option<HookEvent> {
    serde_json::from_value(serde_json::Value::String(name.to_string())).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const HEAD: &str = "[plugin]\nname = \"demo\"\nversion = \"1.0.0\"\napi_version = \"0.1\"\n";

    fn load(root: &Path, body: &str) -> plugin::manifest::Plugin {
        std::fs::write(root.join("plugin.toml"), format!("{HEAD}{body}")).unwrap();
        plugin::manifest::Plugin::load(root, &root.join("plugin.toml")).unwrap()
    }

    #[test]
    fn declared_events_become_hook_entries_naming_the_plugin() {
        let dir = tempfile::tempdir().unwrap();
        let p = load(
            dir.path(),
            r#"
[[wasm]]
component = "p.wasm"
events = ["PreToolUse", "SessionStart"]

[wasm.capabilities]
timeout_ms = 4321
"#,
        );

        let entries = hook_configs_for(&p);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, HookEvent::PreToolUse);
        assert_eq!(entries[1].0, HookEvent::SessionStart);
        match &entries[0].1 {
            HookConfig::Wasm { plugin, timeout } => {
                assert_eq!(plugin, "demo");
                assert_eq!(*timeout, Some(4321), "the plugin's own deadline applies");
            }
            other => panic!("expected a wasm hook, got {other:?}"),
        }
    }

    #[test]
    fn a_plugin_declaring_no_events_subscribes_to_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let p = load(dir.path(), "\n[[wasm]]\ncomponent = \"p.wasm\"\n");
        assert!(hook_configs_for(&p).is_empty());
    }

    #[test]
    fn proceed_says_nothing_at_all() {
        use wasm_host::bindings::agix::plugin::types::HookDecision as D;
        let r = into_response(D::Proceed);
        assert!(r.decision.is_none());
        assert!(r.message.is_none());
        assert!(r.updated_input.is_none());
    }

    #[test]
    fn block_carries_its_reason_to_the_user() {
        use wasm_host::bindings::agix::plugin::types::HookDecision as D;
        let r = into_response(D::Block("policy forbids this".into()));
        assert_eq!(r.decision, Some(HookDecision::Block));
        assert_eq!(r.message.as_deref(), Some("policy forbids this"));
    }

    /// The restriction that separates a plugin from a hook the user wrote:
    /// refusing a call and silently changing what it does are different
    /// powers, and a downloaded package only gets the first.
    #[test]
    fn no_plugin_answer_can_rewrite_a_tools_arguments() {
        use wasm_host::bindings::agix::plugin::types::HookDecision as D;
        for decision in [
            D::Proceed,
            D::Block("no".into()),
            D::AddContext("note".into()),
        ] {
            assert!(
                into_response(decision).updated_input.is_none(),
                "updated_input must be unreachable from a plugin's answer"
            );
        }
    }

    #[test]
    fn added_context_is_a_message_not_a_verdict() {
        use wasm_host::bindings::agix::plugin::types::HookDecision as D;
        let r = into_response(D::AddContext("the demo plugin is active".into()));
        assert!(
            r.decision.is_none(),
            "adding context must not approve or block anything"
        );
        assert_eq!(r.message.as_deref(), Some("the demo plugin is active"));
    }

    #[tokio::test]
    async fn an_event_for_an_unloaded_plugin_is_an_error_not_a_block() {
        let events = WasmEvents::new(HashMap::new());
        assert!(events.is_empty());
        let input = HookInput {
            hook_event_name: "PreToolUse".into(),
            session_id: "s".into(),
            cwd: "/tmp".into(),
            permission_mode: "default".into(),
            ..Default::default()
        };
        let err = events.execute("absent", &input, None).await.unwrap_err();
        assert!(err.contains("absent"), "{err}");
    }
}
