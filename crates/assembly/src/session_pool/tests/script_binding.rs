//! `script_binding_tests` 原样搬来的用例。

use super::*;
use base::interface::settings::ScriptBinding;
use base::project_trust::ProjectTrust;

#[test]
fn a_relative_binding_does_not_reach_into_an_untrusted_project() {
    let binding = |path: &str| ScriptBinding {
        path: path.into(),
        point: "prompt.block".into(),
        entry: "block".into(),
        timeout_ms: None,
        calls_per_turn: None,
    };
    let mut settings = Settings::defaults_for("m");
    settings.scripts = vec![binding("scripts/x.js"), binding("/opt/agix/y.js")];
    let paths = |settings: &Settings| {
        operator_script_bindings(settings)
            .into_iter()
            .map(|b| b.path)
            .collect::<Vec<_>>()
    };

    settings.project_layer.trust = ProjectTrust::Untrusted;
    assert_eq!(paths(&settings), [PathBuf::from("/opt/agix/y.js")]);
    for trust in [ProjectTrust::Trusted, ProjectTrust::NoProject] {
        settings.project_layer.trust = trust;
        assert_eq!(paths(&settings).len(), 2);
    }
}
