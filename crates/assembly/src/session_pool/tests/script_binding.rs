//! `script_binding_tests` 原样搬来的用例。

use super::*;
use base::project_trust::ProjectTrust;
use base::settings::ScriptBinding;

#[test]
fn a_relative_binding_does_not_reach_into_an_untrusted_project() {
    let binding = |path: &str| ScriptBinding {
        path: path.into(),
        point: "prompt.block".into(),
        entry: "block".into(),
        timeout_ms: None,
        calls_per_turn: None,
    };
    let mut values = Settings::defaults_for("m");
    values.scripts = vec![binding("scripts/x.js"), binding("/opt/agix/y.js")];
    let paths = |trust: ProjectTrust| {
        let settings = base::settings::LoadedSettings::new(
            values.clone(),
            base::settings::PathSettings::default(),
        )
        .with_project_layer(base::project_trust::ProjectLayer {
            trust,
            ..Default::default()
        });
        operator_script_bindings(&settings)
            .into_iter()
            .map(|b| b.path)
            .collect::<Vec<_>>()
    };

    assert_eq!(
        paths(ProjectTrust::Untrusted),
        [PathBuf::from("/opt/agix/y.js")]
    );
    for trust in [ProjectTrust::Trusted, ProjectTrust::NoProject] {
        assert_eq!(paths(trust).len(), 2);
    }
}
