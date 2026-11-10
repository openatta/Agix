//! A session's tool policy is decided in one place. The other wiring guards —
//! hook events, telemetry producers, composition-root calls, the delegate
//! spawn site — are in `tests/guards/tests/wiring.rs`.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("daemon crate has a parent (repo root)")
        .to_path_buf()
}

/// A session's tool policy is decided once, in `runtime::tool_policy`, and
/// every path that filters tools reads that. A path that asks the scene
/// directly compiles and passes its tests, and quietly drops the plugin tools
/// the policy admits.
#[test]
fn only_the_tool_policy_asks_the_scene_which_tools_are_permitted() {
    let root = repo_root();
    let src = root.join("crates/runtime/src");
    let mut sites = Vec::new();
    let mut stack = vec![src.clone()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("crates/runtime/src should be readable") {
            let path = entry.unwrap().path();
            if path.is_dir() {
                if path.file_name().is_some_and(|n| n != "tests") {
                    stack.push(path);
                }
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            for (i, line) in text.lines().enumerate() {
                if line.contains(".permits_tool(") {
                    let at = path.strip_prefix(&src).unwrap().display().to_string();
                    sites.push((at, i + 1));
                }
            }
        }
    }

    assert!(
        sites.iter().any(|(f, _)| f == "tool_policy.rs"),
        "the scan found no `.permits_tool(` in tool_policy.rs; the scan or the policy moved"
    );
    let elsewhere: Vec<String> = sites
        .iter()
        .filter(|(f, _)| f != "tool_policy.rs")
        .map(|(f, l)| format!("{f}:{l}"))
        .collect();
    assert!(
        elsewhere.is_empty(),
        "crates/runtime/src asks `Scene::permits_tool` at {elsewhere:?}. Read the session's \
         `ToolPolicy` instead (`self.tooling.tool_policy.permits(..)`), or the tools a \
         plugin admits with `Registrar::admit_tools` disappear on that path."
    );
}
