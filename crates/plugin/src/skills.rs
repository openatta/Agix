//! The skills a package ships under `[skills] dir`: one `<name>/SKILL.md` per
//! skill, the layout of the skill directories on disk.
//!
//! Read strictly. Install reads them in the staging directory, discovery
//! reads them again, and a skill that does not pass refuses the whole
//! package — a package that installs with one of its skills quietly missing
//! looks fine until the model reaches for it.

use std::path::{Path, PathBuf};

use base::frozen::frontmatter::{parse_skill_file, split_frontmatter, SKILL_FRONTMATTER_KEYS};
use base::frozen::skill::{SkillEntry, SkillSource};

use crate::manifest::{inside_package, Plugin, PluginError};

const MAX_NAME_CHARS: usize = 64;

/// One skill of a package, as read and checked at load.
#[derive(Debug, Clone)]
pub struct PackageSkill {
    pub entry: SkillEntry,
    /// What the skill expands to: SKILL.md after its frontmatter.
    pub body: String,
    /// The `files:` it bundles, as (declared path, content).
    pub files: Vec<(String, String)>,
}

/// Every skill `plugin` declares, in name order. Empty when the manifest
/// has no `[skills]` section.
pub fn declared_skills(plugin: &Plugin) -> Result<Vec<PackageSkill>, PluginError> {
    let Some(section) = &plugin.manifest.skills else {
        return Ok(Vec::new());
    };
    let dir = &section.dir;
    let schema =
        |what: String| PluginError::Schema(format!("`[skills] dir` `{}` {what}", dir.display()));
    let root = plugin
        .path(dir)
        .map_err(|e| schema(format!("does not resolve: {e}")))?;
    if !root.is_dir() {
        return Err(schema("is not a directory".into()));
    }

    let mut names = Vec::new();
    for entry in std::fs::read_dir(&root)? {
        let file_name = entry?.file_name();
        let Some(name) = file_name.to_str() else {
            return Err(schema(format!(
                "holds `{}`, whose name is not UTF-8",
                file_name.to_string_lossy()
            )));
        };
        if !name.starts_with('.') {
            names.push(name.to_string());
        }
    }
    names.sort();

    names
        .iter()
        .map(|name| {
            read_skill(plugin, dir, name).map_err(|why| {
                PluginError::Schema(format!(
                    "skill `{name}` ({}): {why}",
                    dir.join(name).join("SKILL.md").display()
                ))
            })
        })
        .collect()
}

fn read_skill(plugin: &Plugin, dir: &Path, name: &str) -> Result<PackageSkill, String> {
    check_name(name)?;
    let rel = dir.join(name);
    let skill_dir = plugin.path(&rel).map_err(|e| e.to_string())?;
    if !skill_dir.is_dir() {
        return Err(
            "is not a directory; a package's skills are `<name>/SKILL.md`, one directory each"
                .into(),
        );
    }
    let skill_md = plugin
        .path(&rel.join("SKILL.md"))
        .map_err(|e| format!("no readable SKILL.md: {e}"))?;
    let content = std::fs::read_to_string(&skill_md).map_err(|e| format!("cannot be read: {e}"))?;
    check_frontmatter(name, &content)?;
    let entry = parse_skill_file(&content, name.to_string(), &skill_md, SkillSource::Plugin)
        .ok_or_else(|| "has no `description`".to_string())?;
    let body = split_frontmatter(&content).1.to_string();
    if body.trim().is_empty() {
        return Err("has nothing after its frontmatter".into());
    }
    let files = bundled_files(
        plugin,
        &rel,
        &skill_dir,
        entry.files.as_deref().unwrap_or(&[]),
    )?;
    Ok(PackageSkill { entry, body, files })
}

/// The name a skill is invoked by — `/<name>`, `Skill { skill: name }` — so
/// it is held to what reads unambiguously as one.
fn check_name(name: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let well_formed = chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && name.chars().count() <= MAX_NAME_CHARS;
    if !well_formed {
        return Err(format!(
            "the directory name is not a skill name: 1–{MAX_NAME_CHARS} ASCII letters, digits, \
             `-`, `_` or `.`, starting with a letter or digit"
        ));
    }
    if name.starts_with("mcp__") {
        return Err("`mcp__` names belong to the skills MCP servers' tools become".into());
    }
    Ok(())
}

/// What the skill parser would read past without a word: lines it cannot
/// split, values it would take literally, keys it ignores, and a `name` that
/// is not the one the skill is invoked by.
fn check_frontmatter(dir_name: &str, content: &str) -> Result<(), String> {
    let (Some(front), _) = split_frontmatter(content) else {
        return Err("has no frontmatter block between `---` lines".into());
    };
    let mut seen: Vec<String> = Vec::new();
    let mut has_description = false;
    for (index, line) in front.lines().enumerate() {
        let number = index + 1;
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        if line.starts_with([' ', '\t']) {
            return Err(format!(
                "frontmatter line {number} is indented; nested maps and multi-line values are \
                 not supported"
            ));
        }
        let Some((key, value)) = line.split_once(':') else {
            return Err(format!(
                "frontmatter line {number} `{line}` is not `key: value`"
            ));
        };
        let key = key.trim();
        if !SKILL_FRONTMATTER_KEYS.contains(&key) {
            return Err(format!(
                "frontmatter `{key}` is not a field this build reads (it reads: {})",
                SKILL_FRONTMATTER_KEYS.join(", ")
            ));
        }
        let canonical = key.replace(['-', '_'], "").to_ascii_lowercase();
        if seen.contains(&canonical) {
            return Err(format!("frontmatter `{key}` is given twice"));
        }
        seen.push(canonical.clone());

        let raw = value.trim();
        if raw.starts_with('|') || raw.starts_with('>') {
            return Err(format!(
                "frontmatter `{key}` is a block scalar; write it on one line"
            ));
        }
        let value = unquote(raw);
        if value.is_empty() {
            return Err(format!("frontmatter `{key}` has no value"));
        }
        match canonical.as_str() {
            "name" if value != dir_name => {
                return Err(format!(
                    "frontmatter `name` is `{value}`, but the skill's directory is `{dir_name}`"
                ));
            }
            "description" => has_description = true,
            "context" if !matches!(value, "inline" | "fork") => {
                return Err(format!(
                    "frontmatter `context` is `{value}`; it is `inline` or `fork`"
                ));
            }
            "background" | "disablemodelinvocation" | "userinvocable"
                if !matches!(
                    value.to_ascii_lowercase().as_str(),
                    "true" | "false" | "yes" | "no" | "1" | "0"
                ) =>
            {
                return Err(format!(
                    "frontmatter `{key}` is `{value}`; it is `true` or `false`"
                ));
            }
            _ => {}
        }
    }
    if !has_description {
        return Err("frontmatter has no `description`".into());
    }
    Ok(())
}

fn unquote(s: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = s.strip_prefix(quote).and_then(|r| r.strip_suffix(quote)) {
            return inner.trim();
        }
    }
    s
}

/// The `files:` a skill appends to its expansion, read now so they are
/// disclosed with the rest of what it puts in front of the model.
fn bundled_files(
    plugin: &Plugin,
    rel: &Path,
    skill_dir: &Path,
    declared: &[String],
) -> Result<Vec<(String, String)>, String> {
    declared
        .iter()
        .map(|file| {
            let path = PathBuf::from(file);
            if !inside_package(&path) {
                return Err(format!(
                    "`files` entry `{file}` must name a file inside the skill's directory"
                ));
            }
            let resolved = plugin
                .path(&rel.join(&path))
                .map_err(|e| format!("`files` entry `{file}`: {e}"))?;
            if !resolved.starts_with(skill_dir) {
                return Err(format!(
                    "`files` entry `{file}` resolves outside the skill's directory"
                ));
            }
            let text = std::fs::read_to_string(&resolved)
                .map_err(|e| format!("`files` entry `{file}` cannot be read: {e}"))?;
            Ok((file.clone(), text))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEAD: &str = "[plugin]\nname = \"demo\"\nversion = \"1.0.0\"\napi_version = \"0.1\"\n";

    fn package(files: &[(&str, &str)]) -> (tempfile::TempDir, Plugin) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("plugin.toml"),
            format!("{HEAD}\n[skills]\ndir = \"skills\"\n"),
        )
        .unwrap();
        for (path, body) in files {
            let path = dir.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
        let plugin = Plugin::load(dir.path(), &dir.path().join("plugin.toml")).unwrap();
        (dir, plugin)
    }

    fn refusal(files: &[(&str, &str)]) -> String {
        let (_dir, plugin) = package(files);
        declared_skills(&plugin)
            .expect_err("the package is refused")
            .to_string()
    }

    #[test]
    fn each_skill_directory_is_read_as_a_plugin_skill() {
        let (_dir, plugin) = package(&[
            (
                "skills/tour/SKILL.md",
                "---\nname: tour\ndescription: Walk through it\nallowed-tools: [\"Bash(ls:*)\"]\nfiles: [notes.md]\n---\nShow the tools.\n",
            ),
            ("skills/tour/notes.md", "the notes"),
            (
                "skills/audit/SKILL.md",
                "---\ndescription: \"Audit it\"\n---\nAudit.\n",
            ),
        ]);
        let skills = declared_skills(&plugin).unwrap();
        let names: Vec<&str> = skills.iter().map(|s| s.entry.name.as_str()).collect();
        assert_eq!(names, ["audit", "tour"]);
        let tour = &skills[1];
        assert_eq!(tour.entry.source, SkillSource::Plugin);
        assert_eq!(tour.entry.description, "Walk through it");
        assert_eq!(tour.body, "Show the tools.\n");
        assert_eq!(
            tour.files,
            [("notes.md".to_string(), "the notes".to_string())]
        );
        assert!(tour.entry.path.ends_with("skills/tour/SKILL.md"));
    }

    #[test]
    fn a_package_without_a_skills_section_has_none_even_with_a_skills_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("plugin.toml"), HEAD).unwrap();
        std::fs::create_dir_all(dir.path().join("skills/tour")).unwrap();
        std::fs::write(
            dir.path().join("skills/tour/SKILL.md"),
            "---\ndescription: d\n---\nbody\n",
        )
        .unwrap();
        let plugin = Plugin::load(dir.path(), &dir.path().join("plugin.toml")).unwrap();
        assert!(declared_skills(&plugin).unwrap().is_empty());
    }

    /// Each refusal names the skill and the item, because the person reading
    /// it has a package of several and one line of output.
    #[test]
    fn a_skill_that_does_not_pass_refuses_the_package_naming_skill_and_item() {
        let ok = ("skills/fine/SKILL.md", "---\ndescription: d\n---\nbody\n");
        for (skill_md, says) in [
            ("no frontmatter at all\n", "no frontmatter"),
            ("---\nname: bad\n---\nbody\n", "no `description`"),
            (
                "---\ndescription: d\nname: other\n---\nbody\n",
                "`name` is `other`",
            ),
            (
                "---\ndescription: d\ncontext: elsewhere\n---\nbody\n",
                "`context`",
            ),
            (
                "---\ndescription: d\nbackground: maybe\n---\nbody\n",
                "`background`",
            ),
            (
                "---\ndescription: d\ndesription: typo\n---\nbody\n",
                "`desription` is not",
            ),
            (
                "---\ndescription: d\nmetadata:\n  a: b\n---\nbody\n",
                "not a field",
            ),
            ("---\ndescription: |\n  long\n---\nbody\n", "block scalar"),
            (
                "---\ndescription: d\n  indented: x\n---\nbody\n",
                "indented",
            ),
            (
                "---\ndescription: d\nallowed-tools:\n---\nbody\n",
                "`allowed-tools` has no value",
            ),
            (
                "---\ndescription: d\nallowed_tools: [A]\nallowed-tools: [B]\n---\nbody\n",
                "twice",
            ),
            ("---\ndescription: d\n---\n\n", "nothing after"),
            (
                "---\ndescription: d\nfiles: [../../plugin.toml]\n---\nbody\n",
                "`files` entry",
            ),
            (
                "---\ndescription: d\nfiles: [missing.md]\n---\nbody\n",
                "`files` entry `missing.md`",
            ),
        ] {
            let err = refusal(&[ok, ("skills/bad/SKILL.md", skill_md)]);
            assert!(err.contains("skill `bad`"), "{skill_md:?}: {err}");
            assert!(
                err.contains(says),
                "{skill_md:?}: expected `{says}` in {err}"
            );
        }
    }

    #[test]
    fn what_is_not_a_skill_directory_is_refused_rather_than_skipped() {
        let ok = ("skills/fine/SKILL.md", "---\ndescription: d\n---\nbody\n");
        let err = refusal(&[ok, ("skills/README.md", "# skills")]);
        assert!(err.contains("skill `README.md`"), "{err}");
        let err = refusal(&[ok, ("skills/empty/notes.md", "x")]);
        assert!(
            err.contains("skill `empty`") && err.contains("SKILL.md"),
            "{err}"
        );
        let err = refusal(&[("skills/mcp__x__y/SKILL.md", "---\ndescription: d\n---\nb\n")]);
        assert!(err.contains("`mcp__`"), "{err}");
        let err = refusal(&[("skills/-dash/SKILL.md", "---\ndescription: d\n---\nb\n")]);
        assert!(err.contains("not a skill name"), "{err}");
    }

    #[test]
    fn a_missing_skills_directory_refuses_the_package() {
        let err = refusal(&[]);
        assert!(err.contains("`[skills] dir` `skills`"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn a_skill_that_links_out_of_the_package_is_refused() {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(
            outside.path().join("SKILL.md"),
            "---\ndescription: d\n---\nnot the package's\n",
        )
        .unwrap();
        let (dir, plugin) = package(&[("skills/fine/SKILL.md", "---\ndescription: d\n---\nb\n")]);
        std::os::unix::fs::symlink(outside.path(), dir.path().join("skills/away")).unwrap();
        let err = declared_skills(&plugin).unwrap_err().to_string();
        assert!(
            err.contains("skill `away`") && err.contains("outside the package"),
            "{err}"
        );

        std::fs::remove_file(dir.path().join("skills/away")).unwrap();
        std::fs::create_dir_all(dir.path().join("skills/away")).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("SKILL.md"),
            dir.path().join("skills/away/SKILL.md"),
        )
        .unwrap();
        let err = declared_skills(&plugin).unwrap_err().to_string();
        assert!(err.contains("outside the package"), "{err}");
    }
}
