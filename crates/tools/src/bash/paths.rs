//! Whether a command line stays where the session may go without asking: its path arguments inside the directories the session may read, and the git it runs in the repository the session is in.

use base::tool::ToolContext;
use permissions::invocation::{self, Invocation};
use permissions::path_rule::ResolvedPath;
use permissions::shell::{self, Word};
use std::path::{Path, PathBuf};

/// How many directories a line's `cd`s may leave it possibly in before it is
/// asked about rather than followed.
const MAX_PLACES: usize = 64;
/// How many paths one glob may name before it is asked about rather than
/// checked path by path.
const MAX_GLOB_MATCHES: usize = 512;

struct Operand {
    text: String,
    /// The target of a redirection: a path whatever it looks like.
    target: bool,
    /// Holds an unquoted `*`, `?` or `[`: the shell replaces it with what it
    /// matches.
    glob: bool,
}

/// Whether every path-looking argument of every command in `line` resolves
/// inside the session's cwd or one of its additional directories.
///
/// Only a literal can be resolved here: a word the shell still expands — a
/// `$VAR`, a `~` (whatever `HOME` the line set), a substitution — may name
/// anything, and so may a relative path after a `cd` whose target is not a
/// literal directory inside. Relative paths after a literal `cd` are checked
/// against every directory the line may be in by then. A glob is checked as
/// what it matches on disk now, so `.*/x` reaching `../x` and `link*/x`
/// following a symlink out are seen for what they are.
pub(super) fn path_args_stay_within(line: &str, ctx: &ToolContext) -> bool {
    if ctx.session.read_anywhere {
        return true;
    }
    let roots = crate::security::readable_roots(ctx);
    let inside = |arg: &str, base: &Path| {
        ResolvedPath::new(arg, base).all().all(|p| {
            roots.iter().any(|root| {
                p.starts_with(root) || permissions::path_rule::is_within_root_resolved(p, root)
            })
        })
    };
    let reaches_only_inside = |operand: &Operand, base: &Path| {
        let base = if operand.text.starts_with('/') {
            ctx.cwd.as_path()
        } else {
            base
        };
        if !operand.glob {
            return inside(&operand.text, base);
        }
        match glob_on_disk(&operand.text, base) {
            None => false,
            Some(found) if found.is_empty() => inside(&operand.text, base),
            Some(found) => found.iter().all(|p| inside(&p.to_string_lossy(), base)),
        }
    };
    let mut bases = vec![ctx.cwd.clone()];
    // Set once the line moves somewhere it does not name (`$OLDPWD`, the dir
    // stack): any relative read after that could land anywhere, so it is asked
    // about — but a line that only ends on such a move reads nothing more.
    let mut base_unknown = false;
    for segment in shell::simple_commands(line) {
        let words = shell::lex(&segment);
        let Some((command, args)) = words.split_first() else {
            continue;
        };
        let mut redirect_target = false;
        let mut operands = Vec::new();
        for word in args {
            if word.redirect {
                redirect_target = true;
                continue;
            }
            if word.substitutes || word.tilde {
                return false;
            }
            let target = std::mem::take(&mut redirect_target);
            let (text, flag_value) = match word.text.split_once('=') {
                Some((flag, value)) if !target && flag.starts_with('-') => (value, true),
                _ => (word.text.as_str(), false),
            };
            if flag_value && text.starts_with('~') {
                return false;
            }
            if !target && !flag_value && text.starts_with('-') && text != "-" {
                continue;
            }
            operands.push(Operand {
                text: text.to_string(),
                target,
                glob: word.globs,
            });
        }
        match directory_change(command, args) {
            Some(Some(target)) => {
                if !bases.iter().all(|base| inside(&target, base)) {
                    return false;
                }
                let entered: Vec<PathBuf> = bases
                    .iter()
                    .map(|base| ResolvedPath::new(&target, base).primary)
                    .collect();
                bases.extend(entered);
                if bases.len() > MAX_PLACES {
                    return false;
                }
                // Only an absolute target says where the shell is again: a
                // relative one is relative to wherever it was.
                if Path::new(&target).is_absolute() {
                    base_unknown = false;
                }
                continue;
            }
            Some(None) => {
                base_unknown = true;
                continue;
            }
            None => {}
        }
        // After a move to an unnamed directory, any operand here — even a bare
        // filename, which would otherwise be assumed to sit in a known cwd —
        // could be read from it. (Only non-flag words reach `operands`.)
        if base_unknown {
            if !operands.is_empty() {
                return false;
            }
            continue;
        }
        let stays = operands
            .iter()
            .filter(|o| o.target || o.glob || looks_like_path(&o.text))
            .all(|o| bases.iter().all(|base| reaches_only_inside(o, base)));
        if !stays {
            return false;
        }
    }
    true
}

/// Whether every `git` the line runs finds the repository the session's own
/// directory is in — a `.git` there or above it, not a bare repository or a
/// nested `.git` the project ships. That repository's configuration
/// (`core.fsmonitor`, `diff.external`, `core.sshCommand`…) names programs git
/// starts on its own; the session's is the user's, one shipped in the project
/// is whoever wrote the project's.
pub(crate) fn git_stays_in_the_sessions_repository(line: &str, ctx: &ToolContext) -> bool {
    let segments = shell::segments(line);
    let runs_git = |segment: &shell::Segment| {
        invocation::invocations(&segment.source).iter().any(|i| {
            matches!(i, Invocation::Runs(argv)
                if argv.first().is_some_and(|p| invocation::program_name(p) == "git"))
        })
    };
    if !segments.iter().any(runs_git) {
        return true;
    }
    let own = repository_of(&ctx.cwd);
    if matches!(own, Some(Repository::Bare(_))) {
        return false;
    }
    let mut bases = vec![ctx.cwd.clone()];
    let mut unknown = false;
    for segment in &segments {
        if runs_git(segment) && (unknown || bases.iter().any(|b| repository_of(b) != own)) {
            return false;
        }
        let words = shell::lex(&segment.text);
        let Some((command, args)) = words.split_first() else {
            continue;
        };
        match directory_change(command, args) {
            Some(Some(target)) => {
                let entered: Vec<PathBuf> = bases
                    .iter()
                    .map(|base| ResolvedPath::new(&target, base).primary)
                    .collect();
                bases.extend(entered);
                if bases.len() > MAX_PLACES {
                    return false;
                }
                if Path::new(&target).is_absolute() {
                    unknown = false;
                }
            }
            Some(None) => unknown = true,
            None => {}
        }
    }
    true
}

/// Where `command args` moves the shell: `None` when it does not move it,
/// `Some(None)` when it moves it somewhere the line does not name — `cd`
/// alone (home), `cd -` (`$OLDPWD`), `popd` / `pushd +N` (the stack), a target
/// the shell still expands — else the literal target.
fn directory_change(command: &Word, args: &[Word]) -> Option<Option<String>> {
    match command.text.as_str() {
        "popd" => Some(None),
        "cd" | "pushd" => {
            let mut redirect_target = false;
            let target = args.iter().find(|w| {
                if w.redirect {
                    redirect_target = true;
                    return false;
                }
                if std::mem::take(&mut redirect_target) {
                    return false;
                }
                !(w.text.starts_with('-') && w.text.len() > 1)
            });
            Some(target.and_then(|w| {
                (w.is_literal() && w.text != "-" && !w.text.starts_with('+'))
                    .then(|| w.text.clone())
            }))
        }
        _ => None,
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Repository {
    /// A `.git` — directory, or file pointing elsewhere — in this directory.
    DotGit(PathBuf),
    /// A directory that is itself a repository's git directory.
    Bare(PathBuf),
}

/// The repository git finds from `dir`, looking where git looks: a `.git`
/// in the directory, else the directory itself as a git directory, else the
/// same one level up.
fn repository_of(dir: &Path) -> Option<Repository> {
    let start = dir
        .ancestors()
        .find_map(|d| std::fs::canonicalize(d).ok())?;
    start.ancestors().find_map(|d| {
        let dot_git = d.join(".git");
        if dot_git.symlink_metadata().is_ok() {
            Some(Repository::DotGit(dot_git))
        } else if is_git_directory(d) {
            Some(Repository::Bare(d.to_path_buf()))
        } else {
            None
        }
    })
}

fn is_git_directory(dir: &Path) -> bool {
    dir.join("HEAD").is_file()
        && (dir.join("commondir").is_file()
            || (dir.join("objects").is_dir() && dir.join("refs").is_dir()))
}

fn looks_like_path(arg: &str) -> bool {
    arg.starts_with(['/', '~']) || arg.contains('/') || arg == ".."
}

/// The paths `pattern` names under `base` as the shell would expand it now:
/// empty when it names none (the shell then passes it on as written), `None`
/// when it names more than [`MAX_GLOB_MATCHES`] or is too costly to match.
/// `.` and `..` are matched like any other name, as the `bash` that runs the
/// command (3.2 on macOS) matches them.
fn glob_on_disk(pattern: &str, base: &Path) -> Option<Vec<PathBuf>> {
    let (mut found, rest) = match pattern.strip_prefix('/') {
        Some(rest) => (vec![PathBuf::from("/")], rest),
        None => (vec![base.to_path_buf()], pattern),
    };
    for component in rest.split('/').filter(|c| !c.is_empty()) {
        if !component.contains(['*', '?', '[']) {
            found.iter_mut().for_each(|p| p.push(component));
            continue;
        }
        if component.matches('*').count() > 8 {
            return None;
        }
        let pattern: Vec<char> = component.chars().collect();
        let mut next = Vec::new();
        for dir in &found {
            let listed = std::fs::read_dir(dir)
                .into_iter()
                .flatten()
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned());
            for name in [".".to_string(), "..".to_string()]
                .into_iter()
                .chain(listed)
            {
                if name.starts_with('.') && !component.starts_with('.') {
                    continue;
                }
                let name_chars: Vec<char> = name.chars().collect();
                if glob_matches(&pattern, &name_chars) {
                    next.push(dir.join(&name));
                    if next.len() > MAX_GLOB_MATCHES {
                        return None;
                    }
                }
            }
        }
        found = next;
    }
    Some(found)
}

fn glob_matches(pattern: &[char], name: &[char]) -> bool {
    match pattern.split_first() {
        None => name.is_empty(),
        Some(('*', rest)) => (0..=name.len()).any(|k| glob_matches(rest, &name[k..])),
        Some(('?', rest)) => name
            .split_first()
            .is_some_and(|(_, tail)| glob_matches(rest, tail)),
        Some(('[', rest)) => {
            let Some((&c, tail)) = name.split_first() else {
                return false;
            };
            match bracket_matches(rest, c) {
                Some((hit, after)) => hit && glob_matches(after, tail),
                None => c == '[' && glob_matches(rest, tail),
            }
        }
        Some((&p, rest)) => name
            .split_first()
            .is_some_and(|(&c, tail)| c == p && glob_matches(rest, tail)),
    }
}

/// Whether the bracket expression starting just past a `[` admits `c`, and
/// the pattern after its `]`; `None` when it has no `]` (the `[` is then a
/// literal). A character class (`[:punct:]`) is taken to admit anything.
fn bracket_matches(rest: &[char], c: char) -> Option<(bool, &[char])> {
    let (negated, body) = match rest.first() {
        Some('!' | '^') => (true, &rest[1..]),
        _ => (false, rest),
    };
    // A `]` first in the expression is a member, not its end; nor is the one
    // closing a `[:class:]`.
    let mut close = 0;
    let mut has_class = false;
    loop {
        match body.get(close)? {
            ']' if close > 0 => break,
            '[' if body.get(close + 1) == Some(&':') => {
                has_class = true;
                close += 2 + body[close + 2..].windows(2).position(|w| w == [':', ']'])? + 2;
            }
            _ => close += 1,
        }
    }
    let set = &body[..close];
    if has_class {
        return Some((true, &body[close + 1..]));
    }
    let mut hit = false;
    let mut i = 0;
    while i < set.len() {
        if i + 2 < set.len() && set[i + 1] == '-' {
            hit |= set[i] <= c && c <= set[i + 2];
            i += 3;
        } else {
            hit |= set[i] == c;
            i += 1;
        }
    }
    Some((hit != negated, &body[close + 1..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(pattern: &str, name: &str) -> bool {
        glob_matches(
            &pattern.chars().collect::<Vec<_>>(),
            &name.chars().collect::<Vec<_>>(),
        )
    }

    #[test]
    fn a_glob_matches_names_the_way_the_shell_does() {
        assert!(matches("*.rs", "a.rs"));
        assert!(!matches("*.rs", "a.rsx"));
        assert!(matches(".*", ".."));
        assert!(matches(".?", ".."));
        assert!(matches(".[.]", ".."));
        assert!(matches(".[[:punct:]]", ".."));
        assert!(!matches(".*rc", ".."));
        assert!(matches("[a-c]x", "bx"));
        assert!(!matches("[!a-c]x", "bx"));
        assert!(matches("[]]", "]"));
        assert!(matches("a[", "a["));
    }

    #[test]
    fn a_glob_on_disk_reaches_the_parent_through_a_dot_pattern() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), "").unwrap();
        let found = glob_on_disk(".*", dir.path()).unwrap();
        assert!(found.contains(&dir.path().join("..")), "{found:?}");
        assert_eq!(
            glob_on_disk("*.md", dir.path()).unwrap(),
            [dir.path().join("a.md")]
        );
        assert!(glob_on_disk("*.none", dir.path()).unwrap().is_empty());
    }
}
