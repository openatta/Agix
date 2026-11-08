//! `plugin.toml` schema + the loaded `Plugin` it produces.
//!
//! Everything here is data. Nothing in this crate executes a plugin: a
//! package declares MCP servers, scripts for the host's QuickJS carrier, a
//! declarative scene directory and agent types, and each is turned into what
//! the engine consumes without running anything the package shipped.

use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Plugin API contract versions this build understands.
///
/// A manifest naming anything else is refused rather than loaded on a
/// best-effort basis: a package built against a different version has no
/// meaningful degraded mode.
pub const SUPPORTED_API_VERSIONS: &[&str] = &["0.1"];

/// Top-level sections this build understands, for the unknown-section
/// warning in [`Plugin::load`].
pub const KNOWN_SECTIONS: &[&str] = &["plugin", "mcp", "script", "scene", "agent"];

/// Top-level sections addressed to whoever embeds the engine, not to the
/// engine.
///
/// A host that lets packages contribute to something the engine has no
/// concept of — a user interface, a marketplace listing — needs somewhere in
/// the manifest to put it, and the unknown-section warning was calling every
/// such section a mistake that "contributes nothing". For that section it is
/// the opposite: it is the only thing the host reads.
///
/// A prefix rather than one blessed name (`[[host]]`) because one package can
/// be installed under two hosts, and they would have to share the name.
pub const HOST_SECTION_PREFIX: &str = "x-";

#[derive(Debug, Deserialize, Clone)]
pub struct PluginManifest {
    pub plugin: PluginMeta,
    /// MCP server payloads, native or DSH-bridged.
    #[serde(default)]
    pub mcp: Vec<McpPayload>,
    /// Scripts this package binds to extension points.
    #[serde(default)]
    pub script: Vec<ScriptPayload>,
    /// The scene this plugin owns.
    #[serde(default)]
    pub scene: SceneSection,
    /// Agent types this plugin declares.
    #[serde(default)]
    pub agent: Vec<AgentDef>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct PluginMeta {
    pub name: String,
    pub version: String,
    /// Which [`SUPPORTED_API_VERSIONS`] entry this plugin was built against.
    pub api_version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub homepage: String,
}

/// A script this package binds to an extension point.
///
/// The carrier is the host's, not the package's: a package ships JavaScript
/// and names a point, and the engine that runs it is the one already in the
/// process. Nothing here can widen what the script may do — that follows
/// from the package the file arrived in, and is decided at bind time.
#[derive(Debug, Deserialize, Clone)]
pub struct ScriptPayload {
    /// Extension point id, e.g. `tool.result`. Which ids accept a script is
    /// the carrier's list, not this crate's — a manifest layer that repeated
    /// it would be a second answer to go stale.
    pub point: String,
    /// `<file>:<function>`, the file relative to the plugin root.
    pub entry: String,
    /// Wall clock for one call. Defaults to the carrier's own budget.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// How many times it may run in a turn. Defaults to the carrier's.
    #[serde(default)]
    pub calls_per_turn: Option<u32>,
}

impl ScriptPayload {
    /// The file half of `entry`. Split from the right: a Windows-style
    /// absolute path carries a colon of its own, and such a path is refused
    /// by [`validate`] rather than silently split in the wrong place.
    pub fn file(&self) -> &str {
        self.entry.rsplit_once(':').map_or("", |(f, _)| f)
    }

    /// The exported function `entry` names.
    pub fn function(&self) -> &str {
        self.entry.rsplit_once(':').map_or("", |(_, f)| f)
    }
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum McpKind {
    /// A standard MCP server; `config` points at its server config JSON.
    Native,
    /// A DeepSeek-Harness plugin; `entry` points at its JS entry, loaded by
    /// the `agix-dsh-bridge` process which speaks MCP on its behalf.
    Dsh,
}

#[derive(Debug, Deserialize, Clone)]
pub struct McpPayload {
    pub name: String,
    pub kind: McpKind,
    /// `kind = "native"`: server config JSON, relative to the plugin root.
    #[serde(default)]
    pub config: Option<PathBuf>,
    /// `kind = "dsh"`: JS entry module, relative to the plugin root.
    #[serde(default)]
    pub entry: Option<PathBuf>,
    /// Environment variable names to pass through to the server process.
    #[serde(default)]
    pub env: Vec<String>,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct SceneSection {
    /// The scene this plugin owns, as a declarative scene directory
    /// (`scene.json` + `prompt.md`) relative to the plugin root. Its id must
    /// be `plugin:<name>`.
    #[serde(default)]
    pub dir: Option<PathBuf>,
}

/// A plugin-declared agent type.
///
/// `permission_mode` stays a string here and is parsed when the definition
/// becomes an engine agent type ([`crate::agents`]), so one bad value costs
/// that field rather than the whole manifest.
#[derive(Debug, Deserialize, Clone)]
pub struct AgentDef {
    pub name: String,
    pub description: String,
    /// Markdown system prompt, relative to the plugin root.
    pub prompt: PathBuf,
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    #[serde(default)]
    pub disallowed_tools: Vec<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub permission_mode: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub max_turns: Option<u32>,
    /// Scene to run this agent's sub-agents in, e.g. `plugin:<name>`.
    #[serde(default)]
    pub scene: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("toml parse: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("plugin schema: {0}")]
    Schema(String),
    #[error("homograph: {0}")]
    Homograph(String),
    #[error("checksum: {0}")]
    Checksum(String),
    #[error("unsupported api_version `{found}` (this build supports {supported})")]
    ApiVersion { found: String, supported: String },
}

#[derive(Debug, Clone)]
pub struct Plugin {
    pub root: PathBuf,
    pub manifest: PluginManifest,
}

impl Plugin {
    pub fn load(root: &Path, manifest_path: &Path) -> Result<Self, PluginError> {
        let raw = std::fs::read_to_string(manifest_path)?;
        refuse_retired_sections(&raw)?;
        let manifest: PluginManifest = toml::from_str(&raw)?;
        validate(&manifest)?;
        warn_unknown_sections(&raw, &manifest.plugin.name);
        Ok(Plugin {
            root: root.to_path_buf(),
            manifest,
        })
    }

    pub fn name(&self) -> &str {
        &self.manifest.plugin.name
    }

    /// Scene id this plugin's own scene registers under, if it declares one.
    pub fn scene_id(&self) -> Option<String> {
        self.manifest
            .scene
            .dir
            .is_some()
            .then(|| format!("plugin:{}", self.name()))
    }

    /// Resolve a manifest-relative path against this plugin's root.
    pub fn path(&self, rel: &Path) -> PathBuf {
        self.root.join(rel)
    }
}

/// Say so when a manifest carries a section this build does not read.
///
/// A warning rather than `deny_unknown_fields`, because the two failure modes
/// are not symmetric: refusing would turn every package built against a
/// *newer* Core into an install failure, while a typo — `[[scripts]]` for
/// `[[script]]` — silently produces a package that contributes nothing and
/// looks installed. Naming the section costs the forward-compatible case
/// nothing and is the only signal the typo ever gets.
///
/// [`HOST_SECTION_PREFIX`] is the third case: a section neither this build nor
/// any build reads, because it was never addressed to the engine.
fn warn_unknown_sections(raw: &str, plugin: &str) {
    for section in unknown_sections(raw) {
        tracing::warn!(
            plugin,
            section = section.as_str(),
            known = KNOWN_SECTIONS.join(", "),
            "plugin.toml declares a section this build does not read; it contributes nothing"
        );
    }
}

/// Sections [`warn_unknown_sections`] would name, split out so the decision
/// can be tested without a tracing subscriber.
fn unknown_sections(raw: &str) -> Vec<String> {
    let Ok(toml::Value::Table(table)) = raw.parse::<toml::Value>() else {
        return Vec::new();
    };
    table
        .keys()
        .filter(|key| !key.starts_with(HOST_SECTION_PREFIX))
        .filter(|key| !KNOWN_SECTIONS.contains(&key.as_str()))
        .cloned()
        .collect()
}

/// Refused rather than warned about like an unknown section: these were written
/// for something this build removed, so the package would install and the part
/// its author cared about would never run.
fn refuse_retired_sections(raw: &str) -> Result<(), PluginError> {
    let Ok(toml::Value::Table(table)) = raw.parse::<toml::Value>() else {
        return Ok(());
    };
    if table.contains_key("wasm") {
        return Err(PluginError::Schema(
            "`[[wasm]]` is not supported: Agix runs no WebAssembly components. Extend the \
             engine with a Rust plugin or a `[[script]]`; content for the embedding host \
             goes in an `x-` section"
                .into(),
        ));
    }
    if table
        .get("scene")
        .and_then(toml::Value::as_table)
        .is_some_and(|scene| scene.contains_key("own"))
    {
        return Err(PluginError::Schema(
            "`[scene.own]` is not supported; declare the scene as a directory with \
             `[scene] dir` (`scene.json` + `prompt.md`)"
                .into(),
        ));
    }
    Ok(())
}

fn validate(m: &PluginManifest) -> Result<(), PluginError> {
    if m.plugin.name.trim().is_empty() {
        return Err(PluginError::Schema("plugin.name must not be empty".into()));
    }
    if !SUPPORTED_API_VERSIONS.contains(&m.plugin.api_version.as_str()) {
        return Err(PluginError::ApiVersion {
            found: m.plugin.api_version.clone(),
            supported: SUPPORTED_API_VERSIONS.join(", "),
        });
    }
    for s in &m.script {
        if s.point.trim().is_empty() {
            return Err(PluginError::Schema(
                "a `[[script]]` declares no `point`".into(),
            ));
        }
        let Some((file, function)) = s.entry.rsplit_once(':') else {
            return Err(PluginError::Schema(format!(
                "script entry `{}` is not `<file>:<function>`",
                s.entry
            )));
        };
        if file.is_empty() || function.is_empty() {
            return Err(PluginError::Schema(format!(
                "script entry `{}` is not `<file>:<function>`",
                s.entry
            )));
        }
        // The path is resolved against the package's own directory, so
        // anything that could leave it is refused here rather than relied on
        // being caught later — the later check is in a different crate, and a
        // package that reaches outside itself is the one case where "it
        // probably fails anyway" is not good enough.
        let path = Path::new(file);
        if path.is_absolute() || path.components().any(|c| c.as_os_str() == "..") {
            return Err(PluginError::Schema(format!(
                "script entry `{}` must name a file inside the package",
                s.entry
            )));
        }
    }

    if let Some(dir) = &m.scene.dir {
        if dir.is_absolute() || dir.components().any(|c| c.as_os_str() == "..") {
            return Err(PluginError::Schema(format!(
                "`[scene] dir` `{}` must name a directory inside the package",
                dir.display()
            )));
        }
    }

    for s in &m.mcp {
        match s.kind {
            McpKind::Native if s.config.is_none() => {
                return Err(PluginError::Schema(format!(
                    "mcp `{}` is native but declares no `config`",
                    s.name
                )));
            }
            McpKind::Dsh if s.entry.is_none() => {
                return Err(PluginError::Schema(format!(
                    "mcp `{}` is dsh but declares no `entry`",
                    s.name
                )));
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load_str(toml_str: &str) -> Result<PluginManifest, PluginError> {
        refuse_retired_sections(toml_str)?;
        let m: PluginManifest = toml::from_str(toml_str)?;
        validate(&m)?;
        Ok(m)
    }

    const MINIMAL: &str = r#"
[plugin]
name = "p"
version = "1.0.0"
api_version = "0.1"
"#;

    /// A host section is not a typo, and the warning that calls it one is
    /// wrong about the only part of the manifest that host reads.
    #[test]
    fn a_host_section_is_not_reported_as_a_section_nobody_reads() {
        let manifest = format!(
            "{MINIMAL}\n[[x-ui]]\npanel = \"panel.js\"\n\n[x-marketplace]\nlisting = \"listed\"\n"
        );
        assert!(
            unknown_sections(&manifest).is_empty(),
            "`x-` sections are addressed to the host, not to this build"
        );
    }

    /// Reserving a prefix must not cost the typo its warning — that is what
    /// the whole check exists for.
    #[test]
    fn a_misspelled_engine_section_is_still_reported() {
        let manifest = format!("{MINIMAL}\n[[scripts]]\nentry = \"main.js\"\n");
        assert_eq!(unknown_sections(&manifest), vec!["scripts".to_string()]);
    }

    #[test]
    fn a_scene_dir_is_the_plugins_one_scene() {
        let m = load_str(&format!("{MINIMAL}\n[scene]\ndir = \"scene\"\n")).unwrap();
        assert_eq!(m.scene.dir.as_deref(), Some(Path::new("scene")));

        for outside in ["../elsewhere", "/abs/scene"] {
            let e = load_str(&format!("{MINIMAL}\n[scene]\ndir = \"{outside}\"\n"))
                .unwrap_err()
                .to_string();
            assert!(e.contains("inside the package"), "{outside}: {e}");
        }
    }

    #[test]
    fn a_manifest_may_declare_no_payloads_at_all() {
        let m = load_str(MINIMAL).unwrap();
        assert_eq!(m.plugin.name, "p");
        assert!(m.mcp.is_empty());
        assert!(m.agent.is_empty());
        assert!(m.scene.dir.is_none());
    }

    #[test]
    fn full_manifest_round_trips() {
        let m = load_str(
            r#"
[plugin]
name = "github-tools"
version = "1.2.0"
api_version = "0.1"
description = "GitHub tools"

[[mcp]]
name = "github"
kind = "native"
config = "mcp/github.json"

[[mcp]]
name = "pr-helper"
kind = "dsh"
entry = "dist/index.js"

[[script]]
point = "tool.result"
entry = "scripts/annotate.js:onResult"

[scene]
dir = "scene"

[[agent]]
name = "pr-reviewer"
description = "Reviews PRs"
prompt = "agents/reviewer.md"
allowed_tools = ["Read"]
permission_mode = "plan"
max_turns = 30
scene = "plugin:github-tools"
"#,
        )
        .unwrap();

        assert_eq!(m.mcp.len(), 2);
        assert_eq!(m.mcp[0].kind, McpKind::Native);
        assert_eq!(m.mcp[1].kind, McpKind::Dsh);
        assert_eq!(m.script[0].point, "tool.result");
        assert_eq!(m.scene.dir.as_deref(), Some(Path::new("scene")));
        assert_eq!(m.agent[0].scene.as_deref(), Some("plugin:github-tools"));
        assert_eq!(m.agent[0].permission_mode.as_deref(), Some("plan"));
    }

    #[test]
    fn empty_name_is_refused() {
        let err = load_str(
            r#"
[plugin]
name = ""
version = "1.0.0"
api_version = "0.1"
"#,
        )
        .unwrap_err();
        assert!(matches!(err, PluginError::Schema(_)));
    }

    /// No silent downgrade: a plugin built against a version this binary
    /// doesn't implement is refused with the reason, not loaded on a
    /// best-effort basis.
    #[test]
    fn an_unsupported_api_version_is_refused_with_the_supported_set() {
        let err = load_str(
            r#"
[plugin]
name = "p"
version = "1.0.0"
api_version = "9.9"
"#,
        )
        .unwrap_err();
        match err {
            PluginError::ApiVersion { found, supported } => {
                assert_eq!(found, "9.9");
                assert!(supported.contains("0.1"));
            }
            other => panic!("expected ApiVersion, got {other:?}"),
        }
    }

    #[test]
    fn a_missing_api_version_is_a_parse_error_not_a_default() {
        let err = load_str(
            r#"
[plugin]
name = "p"
version = "1.0.0"
"#,
        )
        .unwrap_err();
        assert!(matches!(err, PluginError::Toml(_)));
    }

    #[test]
    fn a_script_payload_splits_into_a_file_and_a_function() {
        let m = load_str(&format!(
            "{MINIMAL}\n[[script]]\npoint = \"tool.result\"\nentry = \"a/b.js:onResult\"\n"
        ))
        .unwrap();
        assert_eq!(m.script[0].file(), "a/b.js");
        assert_eq!(m.script[0].function(), "onResult");
        assert_eq!(m.script[0].timeout_ms, None);
    }

    /// The manifest layer does not know which points accept a script — that
    /// is the carrier's list. It knows only that `entry` has to say which
    /// file and which function, because neither half can be guessed.
    #[test]
    fn a_script_entry_that_is_not_file_colon_function_is_refused() {
        for entry in ["onResult", "a.js:", ":onResult"] {
            let err = load_str(&format!(
                "{MINIMAL}\n[[script]]\npoint = \"tool.result\"\nentry = \"{entry}\"\n"
            ))
            .unwrap_err();
            assert!(
                matches!(&err, PluginError::Schema(m) if m.contains("<file>:<function>")),
                "`{entry}`: {err}"
            );
        }
    }

    /// A package's script has to be the package's. Refused at the manifest
    /// rather than left to the bind, because "it would probably fail anyway"
    /// is not the standard for a file that runs in the host's process.
    #[test]
    fn a_script_entry_that_reaches_outside_the_package_is_refused() {
        for entry in ["../../etc/evil.js:go", "/etc/evil.js:go"] {
            let err = load_str(&format!(
                "{MINIMAL}\n[[script]]\npoint = \"tool.result\"\nentry = \"{entry}\"\n"
            ))
            .unwrap_err();
            assert!(
                matches!(&err, PluginError::Schema(m) if m.contains("inside the package")),
                "`{entry}`: {err}"
            );
        }
    }

    /// An unknown section does not fail the load: a package built against a
    /// newer Core has to stay installable on an older one. The warning is
    /// what a typo gets, and this is the check that the load still succeeds.
    #[test]
    fn an_unknown_section_is_ignored_rather_than_refused() {
        let m = load_str(&format!("{MINIMAL}\n[[ui]]\nbundle = \"dist/index.js\"\n")).unwrap();
        assert_eq!(m.plugin.name, "p");
        assert!(m.script.is_empty());
    }

    #[test]
    fn a_wasm_section_is_refused_with_where_to_go_instead() {
        let err = load_str(&format!("{MINIMAL}\n[[wasm]]\ncomponent = \"p.wasm\"\n"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("[[wasm]]"), "{err}");
        assert!(err.contains("`x-`"), "{err}");
    }

    #[test]
    fn an_inline_scene_is_refused_in_favour_of_a_scene_directory() {
        let err = load_str(&format!(
            "{MINIMAL}\n[scene.own]\nname = \"Own\"\nprompt = \"scene/prompt.md\"\n"
        ))
        .unwrap_err()
        .to_string();
        assert!(err.contains("[scene] dir"), "{err}");
    }

    #[test]
    fn each_mcp_kind_requires_its_own_path_field() {
        for (body, missing) in [
            (
                r#"
[[mcp]]
name = "s"
kind = "native"
"#,
                "config",
            ),
            (
                r#"
[[mcp]]
name = "s"
kind = "dsh"
"#,
                "entry",
            ),
        ] {
            let err = load_str(&format!("{MINIMAL}{body}")).unwrap_err();
            assert!(err.to_string().contains(missing), "{err}");
        }
    }

    #[test]
    fn scene_id_is_namespaced_and_only_present_when_a_scene_is_declared() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("plugin.toml"), MINIMAL).unwrap();
        let p = Plugin::load(dir.path(), &dir.path().join("plugin.toml")).unwrap();
        assert_eq!(p.scene_id(), None);

        std::fs::write(
            dir.path().join("plugin.toml"),
            format!("{MINIMAL}\n[scene]\ndir = \"scene\"\n"),
        )
        .unwrap();
        let p = Plugin::load(dir.path(), &dir.path().join("plugin.toml")).unwrap();
        assert_eq!(p.scene_id().as_deref(), Some("plugin:p"));
    }
}
