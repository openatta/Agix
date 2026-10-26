//! Skill loading, expansion, and activation.
//!
//! Skills are loaded from three tiers: `~/.agix/skills/<name>/SKILL.md`
//! (global default, shared by every scene), `~/.agix/scenes/<scope>/skills/<name>/SKILL.md`
//! (scene-specific override — same name wins over the global default; `scope`
//! identifies the product instance, no default), and
//! `<cwd>/.agents/skills/<name>/SKILL.md` (project-level — this is the
//! external fact standard also scanned by Codex). They can be invoked via
//! slash commands (`/<name> [args]`) or conditionally activated when
//! matching file paths are touched.

use std::path::{Path, PathBuf};

use super::frontmatter::{parse_skill_file, split_frontmatter};
use super::regex::Regex;

/// 单个 Skill：从 `<dir>/<skill_name>/SKILL.md` 的 YAML frontmatter 抽出。
#[derive(Debug, Clone)]
pub struct SkillEntry {
    /// 目录名（如 `summarize-pr`）；当 frontmatter 给 `name` 时取 frontmatter 值
    pub name: String,
    /// frontmatter `description`；不存在则取 markdown 第一行
    pub description: String,
    /// frontmatter `when_to_use`；可选
    pub when_to_use: Option<String>,
    pub source: SkillSource,
    /// SKILL.md 的物理路径（用于 `/skills` 命令展示）
    pub path: PathBuf,
    /// Hint shown to user for arguments (e.g. "commit message")
    pub argument_hint: Option<String>,
    /// Restrict tools this skill can invoke (whitelist of tool names)
    pub allowed_tools: Option<Vec<String>>,
    /// Tools removed from the pool while this skill is active (denylist,
    /// applied before `allowed_tools`). Clears on the next user message,
    /// same lifecycle as `allowed_tools`.
    pub disallowed_tools: Option<Vec<String>>,
    /// Named positional arguments (`arguments: [issue, branch]`) for `$name`
    /// substitution in the skill body, in addition to the existing
    /// positional `$1`..`$9`/`$ARGUMENTS` forms.
    pub arguments: Option<Vec<String>>,
    /// Override model for this skill
    pub model: Option<String>,
    /// Effort/thinking-mode override while this skill is active. Maps onto
    /// `base::interface::settings::ThinkingMode` — Agix has no separate
    /// "effort" axis, so this is not a new concept, just this field's name.
    pub effort: Option<String>,
    /// Execution context: "fork" (sub-agent) or "inline" (default)
    pub context: Option<String>,
    /// Which agent type to run as when `context: "fork"` is set. Defaults to
    /// `general-purpose` when unset.
    pub agent: Option<String>,
    /// Only meaningful with `context: "fork"`. `false` waits for the forked
    /// result in the invoking turn instead of running it in the background.
    /// Default: `true` (background).
    pub background: Option<bool>,
    /// If true, model cannot invoke this skill directly (only user via slash)
    pub disable_model_invocation: bool,
    /// If false, skill is hidden from user-facing slash command list
    pub user_invocable: bool,
    /// Glob patterns for conditional activation (auto-load when matching files are touched)
    pub paths: Option<Vec<String>>,
    /// Version string for the skill
    pub version: Option<String>,
    /// **P1 **: Reference files bundled with the skill. Each entry is a path
    /// relative to the skill directory. When the skill is invoked, these files
    /// are read and their content is injected into the skill context.
    pub files: Option<Vec<String>>,
    /// **P2 **: Hook event names this skill subscribes to. When the skill is
    /// loaded, these hooks are registered with the HookRunner.
    pub hooks: Option<Vec<String>>,
}

impl Default for SkillEntry {
    fn default() -> Self {
        Self {
            name: String::new(),
            description: String::new(),
            when_to_use: None,
            source: SkillSource::User,
            path: PathBuf::new(),
            argument_hint: None,
            allowed_tools: None,
            disallowed_tools: None,
            arguments: None,
            model: None,
            effort: None,
            context: None,
            agent: None,
            background: None,
            disable_model_invocation: false,
            user_invocable: true,
            paths: None,
            version: None,
            files: None,
            hooks: None,
        }
    }
}

impl SkillEntry {
    /// 读 SKILL.md，剥 frontmatter，返回 markdown body（trim 后）。
    /// 给 `/<skill-name>` slash 调用时把 body 作为 user prompt 模板用。
    ///
    /// 特殊路径 `(bundled:<name>)` 走内置 skill 的 in-memory body 而非读盘。
    pub async fn read_body(&self) -> std::io::Result<String> {
        let path_str = self.path.to_string_lossy();
        if let Some(name) = path_str
            .strip_prefix("(bundled:")
            .and_then(|s| s.strip_suffix(")"))
        {
            // TODO: bundled skill resolution -- needs injection from skills crate
            // (core cannot depend on skills). For now, bundled prefixes always
            // fall through to the NotFound path below.
            if path_str.starts_with("(bundled:") {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("bundled skill resolver not configured for: {name}"),
                ));
            }
        }
        let content = tokio::fs::read_to_string(&self.path).await?;
        let (_front, body) = split_frontmatter(&content);
        Ok(body.trim().to_string())
    }

    /// Resolves `self.files` (paths relative to the skill's folder) to
    /// absolute paths. `self.path` is the SKILL.md file itself, so the
    /// skill folder is `self.path.parent()`. Returns an empty `Vec` if
    /// `files` is `None`/empty.
    ///
    /// Purely mechanical path joining — no existence checks or
    /// traversal/symlink-escape validation happen here. Callers that need
    /// those (e.g. `SkillTool`, which must surface a visible error for any
    /// declared file that escapes the skill folder) run them separately via
    /// `permissions::path_safety`, which this `core` crate deliberately does
    /// not depend on.
    pub fn resolve_bundled_files(&self) -> Vec<PathBuf> {
        let Some(files) = &self.files else {
            return Vec::new();
        };
        let Some(skill_dir) = self.path.parent() else {
            return Vec::new();
        };
        files.iter().map(|f| skill_dir.join(f)).collect()
    }
}

/// Skill 来源 -- user / project；同名时后入者覆盖先入者（见下方扫描顺序）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillSource {
    /// `~/.agix/skills/`（全局默认）或 `~/.agix/scenes/<scope>/skills/`（scene 覆盖）
    User,
    /// `<cwd>/.agents/skills/`（外部事实标准，Codex 也扫描这个路径）
    Project,
    /// Loaded from a plugin's manifest (`~/.agix/plugins/<name>/SKILL.md` or
    /// `~/.agix/scenes/<scope>/plugins/<name>/SKILL.md`)
    Plugin,
}

/// 从三层扫 SKILL.md：全局 `~/.agix/skills/` -> scene 覆盖
/// `~/.agix/scenes/<scope>/skills/` -> 项目级 `<cwd>/.agents/skills/`（外部事实标准）。
/// 同名时后扫到的覆盖先扫到的（`scan_skills_dir` 结果直接 extend 进 `Vec`，
/// 调用方按"先入者优先"处理 slash 分发，所以顺序是：scene 覆盖需要排在它要
/// 覆盖的全局条目**之前**，让 scene 版本先命中）。
async fn collect_skills(paths: &crate::paths::ConfigPaths, cwd: &Path) -> Vec<SkillEntry> {
    let global_dir = paths.global_skills_dir();
    let scene_dir = paths.user_skills_dir();
    let project_dir = cwd.join(".agents").join("skills");
    let mut all = Vec::new();
    all.extend(scan_skills_dir(&scene_dir, SkillSource::User).await);
    all.extend(scan_skills_dir(&global_dir, SkillSource::User).await);
    all.extend(scan_skills_dir(&project_dir, SkillSource::Project).await);
    all
}

async fn scan_skills_dir(dir: &Path, source: SkillSource) -> Vec<SkillEntry> {
    let mut out = Vec::new();
    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(e) => e,
        Err(_) => return out,
    };
    let mut subdirs: Vec<PathBuf> = Vec::new();
    while let Ok(Some(e)) = entries.next_entry().await {
        let p = e.path();
        if p.is_dir() {
            subdirs.push(p);
        }
    }
    subdirs.sort();
    for d in subdirs {
        let skill_md = d.join("SKILL.md");
        let Ok(content) = tokio::fs::read_to_string(&skill_md).await else {
            continue;
        };
        let dir_name = d
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "(unnamed)".to_string());
        if let Some(entry) = parse_skill_file(&content, dir_name, &skill_md, source) {
            out.push(entry);
        }
    }
    out
}

/// Public helper used by plugin loader to convert a SKILL.md path into
/// a [`SkillEntry`]. Reads the file, parses YAML frontmatter, falls back to
/// first markdown body line for description.
pub async fn load_skill_from_path(path: &Path, source: SkillSource) -> Option<SkillEntry> {
    let content = tokio::fs::read_to_string(path).await.ok()?;
    let dir_name = path
        .parent()
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "plugin-skill".to_string());
    parse_skill_file(&content, dir_name, path, source)
}

/// 公开的"只扫 skills"入口 -- 给 CLI/TUI 启动时构造 skill 列表用，避免重跑
/// FrozenContext::collect 里的 git 子命令。
///
/// 把 5 个内置 bundled skills 追加到列表末尾。disk 上同名 skill 优先
/// （因为先入列表，slash 命中第一个）。
pub async fn load_session_skills(cwd: &Path, paths: &crate::paths::ConfigPaths) -> Vec<SkillEntry> {
    // Disk skills are loaded first (take priority for slash commands).
    // Callers should use collect_skills_with_bundled() to append bundled skills.
    collect_skills(paths, cwd).await
}

/// Like [`load_session_skills`] but appends bundled skills after disk skills.
/// Callers in the `runtime` or `skills` crate pass in bundled skill entries
/// (e.g. from `skills::bundled::bundled_skills()`).
///
/// Disk skills appear first, so same-name slash commands hit the user's version.
/// Bundled skills that don't exist on disk are appended last.
pub async fn load_session_skills_with_bundled(
    cwd: &Path,
    bundled: Vec<SkillEntry>,
    paths: &crate::paths::ConfigPaths,
) -> Vec<SkillEntry> {
    let mut all = load_session_skills(cwd, paths).await;
    let disk_names: std::collections::HashSet<String> =
        all.iter().map(|s| s.name.clone()).collect();
    for s in bundled {
        if !disk_names.contains(&s.name) {
            all.push(s);
        }
    }
    all
}

/// T2.1: Activate conditional skills whose `paths` globs match the given file paths.
///
/// Conditional skills are those with a `paths` frontmatter field listing glob
/// patterns. When a tool touches files matching those patterns, the skill becomes
/// available to the model. Activation is session-persistent (once activated,
/// stays active).
pub fn activate_conditional_skills(
    all_skills: &mut Vec<SkillEntry>,
    conditional_skills: &mut Vec<SkillEntry>,
    affected_paths: &[&str],
) {
    if conditional_skills.is_empty() || affected_paths.is_empty() {
        return;
    }
    let mut activated_indices: Vec<usize> = Vec::new();
    for (i, skill) in conditional_skills.iter().enumerate() {
        if let Some(ref patterns) = skill.paths {
            let mut matched = false;
            for path_str in affected_paths {
                for pat in patterns {
                    // Simple glob matching: supports * and ** wildcards
                    if simple_glob_match(pat, path_str) {
                        matched = true;
                        break;
                    }
                }
                if matched {
                    break;
                }
            }
            if matched {
                activated_indices.push(i);
            }
        }
    }
    // Move activated skills from conditional to active (reverse order for safe removal)
    for &i in activated_indices.iter().rev() {
        let skill = conditional_skills.remove(i);
        all_skills.push(skill);
    }
}

/// Simple glob matching supporting `*` (single-segment) and `**` (multi-segment) wildcards.
/// Used for conditional skill path matching (T2.1).
fn simple_glob_match(pattern: &str, path: &str) -> bool {
    let pattern = pattern.trim();
    if pattern.is_empty() {
        return false;
    }
    // Literal substring match
    if !pattern.contains('*') {
        return path.contains(pattern);
    }
    // Convert glob to check-each-segment
    let pat_segments: Vec<&str> = pattern.split('/').collect();
    let path_segments: Vec<&str> = path.split('/').collect();

    fn match_segments(pat: &[&str], p: &[&str], pi: usize, si: usize) -> bool {
        if pi >= pat.len() {
            return si >= p.len();
        }
        if si >= p.len() {
            // Only ** can match empty
            return pat[pi..].iter().all(|s| *s == "**");
        }
        match pat[pi] {
            "**" => {
                // ** matches zero or more segments
                match_segments(pat, p, pi + 1, si) // zero segments
                    || match_segments(pat, p, pi + 1, si + 1) // one segment
                    || match_segments(pat, p, pi, si + 1) // more segments
            }
            "*" => match_segments(pat, p, pi + 1, si + 1),
            literal => {
                if literal == p[si] {
                    match_segments(pat, p, pi + 1, si + 1)
                } else if literal.contains('*') {
                    // Handle partial wildcards like "*.rs" or "foo*.txt"
                    let re_str = format!("^{}$", regex_escape(literal).replace("\\*", ".*"));
                    if let Ok(re) = Regex::new(&re_str) {
                        re.is_match(p[si]) && match_segments(pat, p, pi + 1, si + 1)
                    } else {
                        false
                    }
                } else {
                    false
                }
            }
        }
    }

    match_segments(&pat_segments, &path_segments, 0, 0)
}

fn regex_escape(s: &str) -> String {
    let mut escaped = String::new();
    for c in s.chars() {
        if ".^$+?()[]{}|\\".contains(c) {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

/// 尝试把 `/<skill-name> [args]` 这类 slash 输入展开成 user prompt：
///
/// - `line` 不以 `/` 开头 -> 立即 None
/// - `name` 不在 skills 列表 -> None（让 caller 走原本 slash 分派 / Unknown 流程）
/// - 命中：异步读 SKILL.md，剥 frontmatter，返回拼好的 prompt（含 args 透传）
///
/// args 拼接策略简单：`{ARGS}` 占位符替换；没占位符则在 body 末尾追加。
/// 这是 first cut，刻意不实现 `$1` 等完整变量代换 -- 用户场景里现在不卡这个。
pub async fn try_expand_skill_command(
    line: &str,
    skills: &[SkillEntry],
) -> Option<Result<String, std::io::Error>> {
    let trimmed = line.trim();
    // Support both /skill and !skill prefix conventions
    let stripped = trimmed
        .strip_prefix('/')
        .or_else(|| trimmed.strip_prefix('!'))?;
    let (cmd, args) = match stripped.split_once(char::is_whitespace) {
        Some((c, a)) => (c, a.trim()),
        None => (stripped, ""),
    };
    let skill = skills.iter().find(|s| s.name == cmd)?;
    Some(format_skill_invocation(skill, args).await)
}

/// **P6 **: expand skill variables in `body`. Equivalent to
/// `expand_skill_vars_named(body, args, &[])` — no named-argument
/// substitution, only positional/legacy forms. Kept as the simple entry
/// point for the many call sites (and tests) that don't have a skill's
/// `arguments` frontmatter list on hand.
///
/// Substitution rules (applied in order):
/// 1. `{ARGS}` -> full `args` string (legacy agix convention)
/// 2. `$ARGUMENTS` -> full `args` string (alternative legacy convention)
/// 3. `$1`..`$9` -> corresponding positional arg (whitespace-separated); missing
///    positions become empty string
/// 4. If none of 1-3 substituted **anything** and `args` is non-empty, append
///    "\n\nUser arguments: {args}" at the end (legacy fallback).
pub fn expand_skill_vars(body: &str, args: &str) -> String {
    expand_skill_vars_named(body, args, &[])
}

/// Same as `expand_skill_vars`, plus named-argument substitution: for each
/// name in `arg_names` (the skill's `arguments:` frontmatter list, in
/// order), `$name` in the body is replaced with the positional argument at
/// that index (whitespace-separated, same splitting as `$1`..`$9`). A name
/// with no corresponding argument expands to an empty string, unlike an
/// *indexed* placeholder with no argument, which is left unchanged — that
/// asymmetry is intentional, not a bug.
pub fn expand_skill_vars_named(body: &str, args: &str, arg_names: &[String]) -> String {
    let positions: Vec<&str> = args.split_whitespace().collect();
    let mut out = body.to_string();
    let mut substituted = false;

    // Named args: $name -> positional value by declared order.
    for (i, name) in arg_names.iter().enumerate() {
        let placeholder = format!("${name}");
        if out.contains(&placeholder) {
            let value = positions.get(i).copied().unwrap_or("");
            out = out.replace(&placeholder, value);
            substituted = true;
        }
    }

    // Legacy: {ARGS} placeholder
    if out.contains("{ARGS}") {
        out = out.replace("{ARGS}", args);
        substituted = true;
    }
    // Legacy: $ARGUMENTS
    if out.contains("$ARGUMENTS") {
        out = out.replace("$ARGUMENTS", args);
        substituted = true;
    }
    // T2.3: $@ -- all arguments
    if out.contains("$@") {
        out = out.replace("$@", args);
        substituted = true;
    }
    // Positional args $1..$9 (reverse to avoid $10 partial match)
    for i in (1..=9).rev() {
        let placeholder = format!("${i}");
        if out.contains(&placeholder) {
            let value = positions.get(i - 1).copied().unwrap_or("");
            out = out.replace(&placeholder, value);
            substituted = true;
        }
    }

    // T2.3: ${AGIX_SKILL_DIR} and ${AGIX_SESSION_ID}
    if out.contains("${AGIX_SKILL_DIR}") {
        let skill_dir = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| ".".to_string());
        out = out.replace("${AGIX_SKILL_DIR}", &skill_dir);
        substituted = true;
    }
    if out.contains("${AGIX_SESSION_ID}") {
        let sid = std::env::var("AGIX_SESSION_ID").unwrap_or_else(|_| "unknown".to_string());
        out = out.replace("${AGIX_SESSION_ID}", &sid);
        substituted = true;
    }

    if !substituted && !args.is_empty() {
        format!("{out}\n\nUser arguments: {args}")
    } else {
        out
    }
}

pub async fn format_skill_invocation(
    skill: &SkillEntry,
    args: &str,
) -> Result<String, std::io::Error> {
    let body = skill.read_body().await?;
    // **P6 **: variable expansion supports:
    //   - `{ARGS}` -- full args string (legacy; we keep it for backwards compat)
    //   - `$ARGUMENTS` -- same (alternative convention)
    //   - `$1`..`$9` -- positional args split on whitespace
    // If none of the above are present and args is non-empty, args is appended
    // verbatim at the end (legacy behavior).
    let body_with_args =
        expand_skill_vars_named(&body, args, skill.arguments.as_deref().unwrap_or(&[]));
    let mut s = String::with_capacity(body_with_args.len() + 256);
    s.push_str(&format!(
        "Apply the skill `{}` ({}). Follow the playbook below.\n\n",
        skill.name, skill.description
    ));
    if let Some(w) = &skill.when_to_use {
        s.push_str(&format!("When to use: {w}\n\n"));
    }
    s.push_str("---\n\n");
    s.push_str(&body_with_args);
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Roots under a tempdir — the same shape a real instance gets, minus any
    /// dependence on who is running the test.
    fn test_paths(home: &Path) -> crate::paths::ConfigPaths {
        crate::paths::ConfigPaths::new(home.join(".agix"), home.join("project/.agix"), "code")
    }

    #[tokio::test]
    async fn collect_skills_loads_user_and_project() {
        let home = TempDir::new().unwrap();
        let cwd = TempDir::new().unwrap();
        // scene-specific skill
        let u_dir = home.path().join(".agix/scenes/code/skills/u-skill");
        tokio::fs::create_dir_all(&u_dir).await.unwrap();
        tokio::fs::write(
            u_dir.join("SKILL.md"),
            "---\ndescription: from user\n---\nbody",
        )
        .await
        .unwrap();
        // project skill — lives under the new authoritative `.agents/skills/`
        let p_dir = cwd.path().join(".agents/skills/p-skill");
        tokio::fs::create_dir_all(&p_dir).await.unwrap();
        tokio::fs::write(
            p_dir.join("SKILL.md"),
            "---\ndescription: from project\n---\nbody",
        )
        .await
        .unwrap();
        let skills = collect_skills(&test_paths(home.path()), cwd.path()).await;
        assert_eq!(skills.len(), 2);
        // user (scene) 在前
        assert_eq!(skills[0].name, "u-skill");
        assert_eq!(skills[0].source, SkillSource::User);
        assert_eq!(skills[1].name, "p-skill");
        assert_eq!(skills[1].source, SkillSource::Project);
    }

    #[tokio::test]
    async fn collect_skills_scene_overrides_global_default_by_name() {
        let home = TempDir::new().unwrap();
        let cwd = TempDir::new().unwrap();
        let global_dir = home.path().join(".agix/skills/shared");
        tokio::fs::create_dir_all(&global_dir).await.unwrap();
        tokio::fs::write(
            global_dir.join("SKILL.md"),
            "---\ndescription: global version\n---\nbody",
        )
        .await
        .unwrap();
        let scene_dir = home.path().join(".agix/scenes/code/skills/shared");
        tokio::fs::create_dir_all(&scene_dir).await.unwrap();
        tokio::fs::write(
            scene_dir.join("SKILL.md"),
            "---\ndescription: scene override\n---\nbody",
        )
        .await
        .unwrap();

        let skills = collect_skills(&test_paths(home.path()), cwd.path()).await;
        let shared: Vec<_> = skills.iter().filter(|s| s.name == "shared").collect();
        // Both entries are present in the raw list (same-name-both-kept
        // convention, see `SkillSource` docs) but the scene one comes first,
        // so callers that pick "first match wins" for slash dispatch get the
        // scene-specific version.
        assert_eq!(shared.len(), 2);
        assert_eq!(shared[0].description, "scene override");
    }

    #[tokio::test]
    async fn collect_skills_skips_dir_without_skill_md() {
        let home = TempDir::new().unwrap();
        let cwd = TempDir::new().unwrap();
        // 目录存在但没有 SKILL.md
        let d = home.path().join(".agix/scenes/code/skills/empty");
        tokio::fs::create_dir_all(&d).await.unwrap();
        let skills = collect_skills(&test_paths(home.path()), cwd.path()).await;
        assert!(skills.is_empty());
    }

    #[tokio::test]
    async fn collect_skills_ignores_old_agix_code_skills_path() {
        // Pre-release, no back-compat: `.agix/code/skills/` is not scanned at
        // all (only `.agents/skills/` is the project-level source of truth).
        let home = TempDir::new().unwrap();
        let cwd = TempDir::new().unwrap();
        let old_dir = cwd.path().join(".agix/code/skills/old-skill");
        tokio::fs::create_dir_all(&old_dir).await.unwrap();
        tokio::fs::write(
            old_dir.join("SKILL.md"),
            "---\ndescription: old path\n---\nbody",
        )
        .await
        .unwrap();

        let skills = collect_skills(&test_paths(home.path()), cwd.path()).await;
        assert!(
            skills.is_empty(),
            "`.agix/code/skills/` must not be scanned"
        );
    }

    #[tokio::test]
    async fn skill_read_body_strips_frontmatter() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("SKILL.md");
        tokio::fs::write(
            &p,
            "---\nname: foo\ndescription: bar\n---\nThis is the body.\nLine 2.\n",
        )
        .await
        .unwrap();
        let skill = SkillEntry {
            name: "foo".into(),
            description: "bar".into(),
            when_to_use: None,
            source: SkillSource::User,
            path: p,
            ..Default::default()
        };
        let body = skill.read_body().await.unwrap();
        assert!(body.contains("This is the body."));
        assert!(body.contains("Line 2."));
        assert!(!body.contains("---"));
        assert!(!body.contains("description"));
    }

    #[test]
    fn resolve_bundled_files_joins_relative_to_skill_dir() {
        let skill = SkillEntry {
            name: "foo".into(),
            path: PathBuf::from("/skills/foo/SKILL.md"),
            files: Some(vec!["notes.md".into(), "ref/extra.md".into()]),
            ..Default::default()
        };
        let resolved = skill.resolve_bundled_files();
        assert_eq!(
            resolved,
            vec![
                PathBuf::from("/skills/foo/notes.md"),
                PathBuf::from("/skills/foo/ref/extra.md"),
            ]
        );
    }

    #[test]
    fn resolve_bundled_files_empty_when_files_is_none() {
        let skill = SkillEntry {
            name: "foo".into(),
            path: PathBuf::from("/skills/foo/SKILL.md"),
            files: None,
            ..Default::default()
        };
        assert!(skill.resolve_bundled_files().is_empty());
    }

    #[test]
    fn resolve_bundled_files_empty_when_files_is_empty_vec() {
        let skill = SkillEntry {
            name: "foo".into(),
            path: PathBuf::from("/skills/foo/SKILL.md"),
            files: Some(vec![]),
            ..Default::default()
        };
        assert!(skill.resolve_bundled_files().is_empty());
    }

    #[test]
    fn resolve_bundled_files_does_not_reject_traversal_paths() {
        // Purely mechanical joining — traversal rejection is the caller's
        // job (SkillTool, via permissions::path_safety), not this method's.
        let skill = SkillEntry {
            name: "foo".into(),
            path: PathBuf::from("/skills/foo/SKILL.md"),
            files: Some(vec!["../../outside.md".into()]),
            ..Default::default()
        };
        assert_eq!(
            skill.resolve_bundled_files(),
            vec![PathBuf::from("/skills/foo/../../outside.md")]
        );
    }

    #[tokio::test]
    async fn try_expand_returns_none_for_unknown_skill() {
        let result = try_expand_skill_command("/ghost", &[]).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn try_expand_returns_none_for_non_slash() {
        let result = try_expand_skill_command("plain text", &[]).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn try_expand_skill_inlines_body_with_invocation_header() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("SKILL.md");
        tokio::fs::write(
            &p,
            "---\nname: summarize\ndescription: Summarize a PR\nwhen_to_use: when asked\n---\nRead the diff. Summarize in 3 bullets.",
        )
        .await
        .unwrap();
        let skills = vec![SkillEntry {
            name: "summarize".into(),
            description: "Summarize a PR".into(),
            when_to_use: Some("when asked".into()),
            source: SkillSource::User,
            path: p,
            ..Default::default()
        }];
        let result = try_expand_skill_command("/summarize", &skills)
            .await
            .expect("skill matched")
            .unwrap();
        assert!(result.contains("Apply the skill `summarize`"));
        assert!(result.contains("Summarize a PR"));
        assert!(result.contains("when asked"));
        assert!(result.contains("Read the diff. Summarize in 3 bullets."));
        assert!(
            !result.contains("---\nname:"),
            "frontmatter must be stripped"
        );
    }

    #[tokio::test]
    async fn try_expand_appends_args_when_no_placeholder() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("SKILL.md");
        tokio::fs::write(&p, "---\ndescription: test\n---\nbody without placeholder")
            .await
            .unwrap();
        let skills = vec![SkillEntry {
            name: "x".into(),
            description: "test".into(),
            when_to_use: None,
            source: SkillSource::User,
            path: p,
            ..Default::default()
        }];
        let result = try_expand_skill_command("/x focus on auth", &skills)
            .await
            .unwrap()
            .unwrap();
        assert!(result.contains("body without placeholder"));
        assert!(result.contains("User arguments: focus on auth"));
    }

    #[test]
    fn expand_dollar_arguments_replaces_full() {
        let body = "Review $ARGUMENTS now.";
        assert_eq!(
            expand_skill_vars(body, "feature/auth"),
            "Review feature/auth now."
        );
    }

    #[test]
    fn expand_dollar_positional_args() {
        let body = "Compare $1 to $2 ignoring $3.";
        assert_eq!(
            expand_skill_vars(body, "main develop trailing"),
            "Compare main to develop ignoring trailing."
        );
    }

    #[test]
    fn expand_missing_positional_becomes_empty() {
        let body = "first=$1 second=$2";
        assert_eq!(expand_skill_vars(body, "only"), "first=only second=");
    }

    #[test]
    fn expand_named_args_substitute_by_declared_order() {
        let body = "Migrate $component from $from to $to.";
        let names = vec![
            "component".to_string(),
            "from".to_string(),
            "to".to_string(),
        ];
        let r = expand_skill_vars_named(body, "SearchBar React Vue", &names);
        assert_eq!(r, "Migrate SearchBar from React to Vue.");
    }

    #[test]
    fn expand_named_arg_with_no_matching_positional_becomes_empty() {
        let body = "value=$only";
        let names = vec!["only".to_string()];
        let r = expand_skill_vars_named(body, "", &names);
        assert_eq!(r, "value=");
    }

    #[test]
    fn expand_skill_vars_with_empty_names_matches_unnamed_variant() {
        let body = "Run with {ARGS}.";
        assert_eq!(
            expand_skill_vars_named(body, "x y z", &[]),
            expand_skill_vars(body, "x y z")
        );
    }

    #[test]
    fn expand_curly_brace_args_still_works() {
        let body = "Run with {ARGS}.";
        assert_eq!(expand_skill_vars(body, "x y z"), "Run with x y z.");
    }

    #[test]
    fn expand_no_placeholder_falls_back_to_appendix() {
        let body = "Generic skill body.";
        let r = expand_skill_vars(body, "extra context");
        assert!(r.contains("Generic skill body."));
        assert!(r.contains("User arguments: extra context"));
    }

    #[test]
    fn expand_no_placeholder_empty_args_returns_body_unchanged() {
        assert_eq!(expand_skill_vars("body", ""), "body");
    }

    #[test]
    fn expand_dollar_arguments_overrides_appendix() {
        let body = "Argument: $ARGUMENTS";
        let r = expand_skill_vars(body, "go");
        assert_eq!(r, "Argument: go");
        assert!(!r.contains("User arguments:"));
    }

    #[tokio::test]
    async fn try_expand_substitutes_args_placeholder() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("SKILL.md");
        tokio::fs::write(&p, "---\ndescription: test\n---\nReview {ARGS} and report.")
            .await
            .unwrap();
        let skills = vec![SkillEntry {
            name: "x".into(),
            description: "test".into(),
            when_to_use: None,
            source: SkillSource::User,
            path: p,
            ..Default::default()
        }];
        let result = try_expand_skill_command("/x PR #42", &skills)
            .await
            .unwrap()
            .unwrap();
        assert!(result.contains("Review PR #42 and report."));
        assert!(!result.contains("{ARGS}"));
        assert!(!result.contains("User arguments:"));
    }
}
