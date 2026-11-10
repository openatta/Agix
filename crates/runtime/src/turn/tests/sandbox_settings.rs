//! 沙箱设置、出口设置如何从 EngineConfig 进入工具上下文。

use crate::turn::tool_env::{exec_providers_from, sandbox_settings_from};

use base::context::config::NetworkModeConfig;
use std::path::PathBuf;

/// The `settings.json` sandbox knobs reach `BashTool` through the tool
/// context.
#[test]
fn tool_context_sandbox_is_taken_from_the_engine_config() {
    let mut config = base::context::EngineConfig::defaults_for("test-model");
    config.sandbox_policy = base::context::config::SandboxPolicyConfig {
        allow_read: vec![PathBuf::from("/tmp/ok")],
        deny_read: Some(vec![PathBuf::from("/tmp/secret")]),
        network_mode: NetworkModeConfig::DenyAll,
        allowed_domains: vec!["api.example.com".into()],
        state_root: None,
        require_enforcement: false,
        unattended_requires_network_policy: true,
        deny_write: Vec::new(),
        allow_write: Vec::new(),
    };

    let s = sandbox_settings_from(&config);

    assert_eq!(s.allow_read, [PathBuf::from("/tmp/ok")]);
    assert_eq!(s.deny_read, [PathBuf::from("/tmp/secret")]);
    assert_eq!(s.network_mode, NetworkModeConfig::DenyAll);
    assert_eq!(s.allowed_domains, ["api.example.com"]);
    assert!(s.unattended_requires_network_policy);
}

/// The same knob, reaching the egress a tool's own HTTP goes out through
/// and not only the sandbox wrapped around a subprocess.
#[test]
fn tool_context_egress_is_taken_from_the_engine_config() {
    use base::interface::exec::Origin;

    let mut config = base::context::EngineConfig::defaults_for("test-model");
    config.sandbox_policy.network_mode = NetworkModeConfig::Allowlist;
    config.sandbox_policy.allowed_domains = vec!["api.example.com".into()];

    let exec = exec_providers_from(&config, None);

    assert!(exec.network.permits("api.example.com", Origin::Agent));
    assert!(
        !exec.network.permits("evil.test", Origin::Agent),
        "a configured allowlist has to reach the tools' egress"
    );
    assert!(
        exec.network.permits("evil.test", Origin::Operator),
        "and stop at what the model chose, not at what the deployment did"
    );
}

/// `None` ("use built-in deny defaults") flattens to an empty vec, which
/// is the same empty-means-defaults convention `tools::bash` reads on the
/// other side — not a request to deny nothing.
#[test]
fn absent_deny_read_flattens_to_empty() {
    let config = base::context::EngineConfig::defaults_for("test-model");
    assert!(config.sandbox_policy.deny_read.is_none());
    assert!(sandbox_settings_from(&config).deny_read.is_empty());
}
