//! Repositories a command leaves nested in the project.
//!
//! The sandbox keeps the project's own `.git` from a command, but not a
//! repository the command creates in a subdirectory and adds to the index as a
//! gitlink: the user's own `git status` in the project, run later and outside
//! any sandbox, enters it and runs what its configuration names. A command
//! confined any looser than the strict profile is checked for one once it has
//! run (`Obligations::scan_nested_repos`).
//!
//! An undeclared gitlink whose git directory lies in the working tree and
//! whose configuration names a program is dangerous; what to do about it is
//! the caller's ([`quarantine`] is the remedy). An undeclared one without such
//! configuration, and a `.git` found anywhere else in the tree, are only
//! reported. Each is reported once per session, and a repository someone kept
//! is reported again only once its configuration changes.

use base::tool::ToolContext;
use std::collections::BTreeSet;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::Duration;

const MAX_DEPTH: usize = 8;
const MAX_ENTRIES: usize = 50_000;
const GIT_DEADLINE: Duration = Duration::from_secs(10);

/// A nested repository whose configuration names a program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dangerous {
    /// Its git directory: `<worktree>/.git`.
    pub git_dir: PathBuf,
    /// The configuration keys that name a program, as `section.key`.
    pub keys: Vec<String>,
    digest: u64,
}

impl Dangerous {
    /// Kept as it is: not reported again until its configuration changes.
    pub fn keep(&self, ctx: &ToolContext) {
        ctx.session
            .ledger()
            .nested_repos
            .insert(self.git_dir.clone(), self.digest);
    }
}

/// What one look through the project found.
#[derive(Debug, Default)]
pub struct Scan {
    pub dangerous: Vec<Dangerous>,
    /// One line for each nested repository only worth mentioning.
    pub notes: Vec<String>,
}

/// Look for repositories nested in the project `ctx` works in.
pub async fn scan(ctx: &ToolContext) -> Scan {
    let mut scan = Scan::default();
    let top = top_level(ctx).await;
    let root = top.clone().unwrap_or_else(|| ctx.cwd.clone());
    let gitlinks = match &top {
        Some(top) => gitlinks(ctx, top).await,
        None => Vec::new(),
    };
    let declared = match &top {
        Some(top) => declared_submodules(ctx, top).await,
        None => BTreeSet::new(),
    };

    let mut known: BTreeSet<PathBuf> = BTreeSet::new();
    for link in gitlinks {
        let Some(top) = &top else { break };
        let git_dir = top.join(&link).join(".git");
        known.insert(git_dir.clone());
        if declared.contains(&link) || !is_dir(ctx, &git_dir).await {
            continue;
        }
        let config = ctx
            .exec
            .filesystem
            .read_to_string(&git_dir.join("config"))
            .await
            .unwrap_or_default();
        let digest = digest(&config);
        let keys = dangerous_keys(&config);
        if keys.is_empty() {
            if first_report(ctx, &git_dir, digest) {
                scan.notes.push(format!(
                    "`{}` is a repository nested in the project and added to its index; its \
                     configuration names no program.",
                    shown(&root, &git_dir)
                ));
            }
            continue;
        }
        let kept = ctx.session.ledger().nested_repos.get(&git_dir) == Some(&digest);
        if !kept {
            scan.dangerous.push(Dangerous {
                git_dir,
                keys,
                digest,
            });
        }
    }

    for git_dir in nested_git_entries(ctx, &root).await {
        if known.contains(&git_dir) || !first_report(ctx, &git_dir, 0) {
            continue;
        }
        scan.notes.push(format!(
            "`{}` is a repository nested in the project.",
            shown(&root, &git_dir)
        ));
    }
    scan
}

/// Move a nested repository's git directory aside, so no git enters it, under
/// a name it can be moved back from: on the filesystem `ctx`'s commands run
/// against, by the engine, outside any sandbox.
pub async fn quarantine(
    ctx: &ToolContext,
    git_dir: &Path,
) -> Result<PathBuf, base::interface::exec::ExecError> {
    let stamp = time::OffsetDateTime::now_utc();
    let name = format!(
        ".git.agix-quarantine-{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        stamp.year(),
        u8::from(stamp.month()),
        stamp.day(),
        stamp.hour(),
        stamp.minute(),
        stamp.second()
    );
    let to = git_dir.with_file_name(name);
    ctx.exec.filesystem.rename(git_dir, &to).await?;
    Ok(to)
}

/// `git_dir` relative to the project, for a person to read.
pub fn shown(root: &Path, git_dir: &Path) -> String {
    git_dir
        .strip_prefix(root)
        .unwrap_or(git_dir)
        .display()
        .to_string()
}

fn first_report(ctx: &ToolContext, git_dir: &Path, digest: u64) -> bool {
    let mut ledger = ctx.session.ledger();
    if ledger.nested_repos.contains_key(git_dir) {
        return false;
    }
    ledger.nested_repos.insert(git_dir.to_path_buf(), digest);
    true
}

fn digest(config: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    config.hash(&mut hasher);
    hasher.finish()
}

async fn is_dir(ctx: &ToolContext, path: &Path) -> bool {
    ctx.exec
        .filesystem
        .metadata(path)
        .await
        .is_ok_and(|m| m.is_dir && !m.is_symlink)
}

/// The working tree the session's directory is in.
async fn top_level(ctx: &ToolContext) -> Option<PathBuf> {
    for dir in ctx.cwd.ancestors() {
        if ctx
            .exec
            .filesystem
            .metadata(&dir.join(".git"))
            .await
            .is_ok()
        {
            return Some(dir.to_path_buf());
        }
    }
    None
}

/// The index's gitlinks, relative to `top`: what the engine's own git, in the
/// session's sandbox, lists with mode 160000.
async fn gitlinks(ctx: &ToolContext, top: &Path) -> Vec<PathBuf> {
    let listed = crate::bash::engine_exec(ctx)
        .git(
            top,
            base::engine_exec::GitAccess::ReadOnly,
            &["ls-files", "-s", "-z"],
            GIT_DEADLINE,
        )
        .await;
    let captured = match listed {
        Ok(captured) if captured.status.success => captured,
        Ok(captured) => {
            tracing::debug!(stderr = %captured.stderr_lossy(), "nested repositories: ls-files failed");
            return Vec::new();
        }
        Err(e) => {
            tracing::debug!(error = %e, "nested repositories: the engine's git did not run");
            return Vec::new();
        }
    };
    gitlinks_in(&captured.stdout_lossy())
}

fn gitlinks_in(ls_files: &str) -> Vec<PathBuf> {
    ls_files
        .split('\0')
        .filter_map(|entry| {
            let (meta, path) = entry.split_once('\t')?;
            meta.starts_with("160000 ").then(|| PathBuf::from(path))
        })
        .collect()
}

/// The paths `.gitmodules` declares, relative to `top`.
async fn declared_submodules(ctx: &ToolContext, top: &Path) -> BTreeSet<PathBuf> {
    let text = ctx
        .exec
        .filesystem
        .read_to_string(&top.join(".gitmodules"))
        .await
        .unwrap_or_default();
    config_entries(&text)
        .filter(|(section, key, _)| section.starts_with("submodule.") && key == "path")
        .filter_map(|(_, _, value)| value.map(|v| PathBuf::from(v.trim_matches('"'))))
        .collect()
}

/// Every `.git` directory below `root` but its own, depth and entries
/// bounded; a repository's git directory is not entered. A `.git` file links
/// to a git directory elsewhere: a worktree's, a declared submodule's.
async fn nested_git_entries(ctx: &ToolContext, root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut seen = 0usize;
    let mut level = vec![root.to_path_buf()];
    for _ in 0..MAX_DEPTH {
        let mut next = Vec::new();
        for dir in level {
            let Ok(entries) = ctx.exec.filesystem.read_dir(&dir).await else {
                continue;
            };
            for entry in entries {
                seen += 1;
                if seen > MAX_ENTRIES {
                    return found;
                }
                let Some(name) = entry.path.file_name() else {
                    continue;
                };
                if name == ".git" {
                    if entry.is_dir && dir != root {
                        found.push(entry.path);
                    }
                } else if entry.is_dir && !not_walked(name) {
                    next.push(entry.path);
                }
            }
        }
        level = next;
    }
    found
}

/// Dependency, build and cache directories, and git directories moved aside
/// by [`quarantine`]: large, and a repository in one is still caught by its
/// gitlink if it is indexed.
fn not_walked(name: &std::ffi::OsStr) -> bool {
    const DIRS: &[&str] = &[
        "node_modules",
        "bower_components",
        "target",
        ".venv",
        "__pycache__",
        ".tox",
        ".gradle",
        ".next",
    ];
    let Some(name) = name.to_str() else {
        return false;
    };
    DIRS.contains(&name) || name.starts_with(".git.agix-quarantine-")
}

/// The keys of a git configuration that name a program git runs, in the
/// order they appear.
pub fn dangerous_keys(config: &str) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    for (section, key, value) in config_entries(config) {
        let name = format!("{section}.{key}");
        let runs_a_program = match (section.as_str(), key.as_str()) {
            ("core", "fsmonitor") => value.is_some_and(|v| !is_boolean(v)),
            ("core", "hookspath" | "sshcommand" | "pager" | "editor")
            | ("diff", "external")
            | ("sequence", "editor")
            | ("credential", "helper")
            | ("include", "path")
            | ("gpg", "program")
            | ("uploadpack", "packobjectshook") => true,
            (s, "clean" | "smudge" | "process") if s.starts_with("filter.") => true,
            (s, "helper") if s.starts_with("credential.") => true,
            (s, "program") if s.starts_with("gpg.") => true,
            (s, _) if s.starts_with("includeif.") => true,
            _ => false,
        };
        if runs_a_program && !keys.contains(&name) {
            keys.push(name);
        }
    }
    keys
}

fn is_boolean(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "true" | "false" | "yes" | "no" | "on" | "off" | "1" | "0" | ""
    )
}

/// `(section, key, value)` for each variable of a git configuration file:
/// the section and key lowercased, a subsection kept as written after a dot.
fn config_entries(text: &str) -> impl Iterator<Item = (String, String, Option<&str>)> {
    let mut section = String::new();
    text.lines().filter_map(move |line| {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            return None;
        }
        if let Some(header) = line.strip_prefix('[') {
            let (header, rest) = header.split_once(']').unwrap_or((header, ""));
            section = match header.split_once(char::is_whitespace) {
                Some((name, sub)) => format!(
                    "{}.{}",
                    name.to_ascii_lowercase(),
                    sub.trim().trim_matches('"')
                ),
                None => header.to_ascii_lowercase(),
            };
            let rest = rest.trim();
            if rest.is_empty() {
                return None;
            }
            return entry(&section, rest);
        }
        entry(&section, line)
    })
}

fn entry<'a>(section: &str, line: &'a str) -> Option<(String, String, Option<&'a str>)> {
    if section.is_empty() {
        return None;
    }
    let (key, value) = match line.split_once('=') {
        Some((key, value)) => (key.trim(), Some(value.trim())),
        None => (line, None),
    };
    Some((section.to_string(), key.to_ascii_lowercase(), value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_keys_that_name_a_program_are_found_and_nothing_else() {
        let config = "\
[core]
\trepositoryformatversion = 0
\tfsmonitor = \"touch /tmp/pwned; false\"
\tbare = false
[core]
\tfsmonitor = true
[filter \"lfs\"]
\tsmudge = git-lfs smudge %f
\trequired = true
[Diff]
\tExternal = ./diff.sh
[includeIf \"gitdir:~/\"]
\tpath = ~/.x
[credential \"https://example.com\"]
\thelper = store
[user]
\tname = T
";
        assert_eq!(
            dangerous_keys(config),
            [
                "core.fsmonitor",
                "filter.lfs.smudge",
                "diff.external",
                "includeif.gitdir:~/.path",
                "credential.https://example.com.helper",
            ]
        );
        assert!(dangerous_keys("[core]\n\tfsmonitor = false\n\tbare = false\n").is_empty());
    }

    #[test]
    fn gitlinks_are_the_entries_of_mode_160000() {
        let listed = "100644 aaaa 0\ta.txt\x00160000 bbbb 0\tsub\x00160000 cccc 0\tdeep/er\x00";
        assert_eq!(
            gitlinks_in(listed),
            [PathBuf::from("sub"), PathBuf::from("deep/er")]
        );
    }

    #[test]
    fn declared_submodules_are_read_from_gitmodules() {
        let text = "[submodule \"lib\"]\n\tpath = vendor/lib\n\turl = https://x\n";
        let paths: Vec<_> = config_entries(text)
            .filter(|(s, k, _)| s.starts_with("submodule.") && k == "path")
            .filter_map(|(_, _, v)| v.map(str::to_string))
            .collect();
        assert_eq!(paths, ["vendor/lib"]);
    }
}
