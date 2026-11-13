//! What a call names, as the policy's checks read it: the rule a call matches,
//! and every string the protected-path checks look at, each worked out once
//! per call from its [`Subject`].

use crate::path_rule::ResolvedPath;
use crate::rule::format_rule_string;
use crate::ruleset::{path_rule_group, shell_rule_group, RuleHit, RuleSet};
use base::interface::permission::Subject;
use base::paths::SensitivePaths;
use base::permission::PermissionRule;
use base::tool::ToolContext;

pub(super) struct Named {
    subject: Subject,
    resolved: Option<ResolvedPath>,
    /// The content, each resolved form of its path, and for a command line
    /// each word and each assigned value.
    pub(super) candidates: Vec<String>,
}

impl Named {
    pub(super) fn of(subject: &Subject, ctx: &ToolContext) -> Self {
        let resolved = path_rule_group(subject)
            .zip(subject.content())
            .map(|(_, c)| ResolvedPath::new(c, &ctx.cwd));
        let candidates = candidates(subject, resolved.as_ref());
        Self {
            subject: subject.clone(),
            resolved,
            candidates,
        }
    }

    pub(super) fn content(&self) -> Option<&str> {
        self.subject.content()
    }

    pub(super) fn hit(&self, rules: &RuleSet, tool_name: &str, ctx: &ToolContext) -> RuleHit {
        if let (Some(group), Some(path)) = (path_rule_group(&self.subject), &self.resolved) {
            return rules.evaluate_path(tool_name, group, path, &ctx.cwd);
        }
        match &self.subject {
            Subject::Shell(line) => {
                rules.evaluate_shell(tool_name, shell_rule_group(&self.subject), line)
            }
            _ => rules.evaluate(tool_name, self.content()),
        }
    }

    /// The first string of the call that names a credential location, each
    /// taken as a path from the session's cwd, as is the value of a
    /// `--flag=value`.
    pub(super) fn credential(&self, ctx: &ToolContext, also: &SensitivePaths) -> Option<&str> {
        self.content()?;
        self.candidates
            .iter()
            .find(|candidate| {
                std::iter::once(candidate.as_str())
                    .chain(candidate.split_once('=').map(|(_, value)| value))
                    .filter(|word| !word.is_empty())
                    .any(|word| {
                        ResolvedPath::new(word, &ctx.cwd)
                            .all()
                            .any(|p| ctx.sandbox.paths.is_credential(p) || also.is_credential(p))
                    })
            })
            .map(String::as_str)
    }

    pub(super) fn bypass_immune(&self) -> Option<&str> {
        self.candidates
            .iter()
            .find(|s| is_path_bypass_immune(s))
            .map(String::as_str)
    }

    pub(super) fn names_configuration(&self) -> bool {
        self.candidates
            .iter()
            .any(|s| SensitivePaths::names_configuration(s))
    }
}

fn candidates(subject: &Subject, resolved: Option<&ResolvedPath>) -> Vec<String> {
    let content = subject.content();
    // The protected-path checks compare path components, which quotes
    // inside a word (`~/".ssh"/id_rsa`) would otherwise hide.
    let shell_line = match subject {
        Subject::Shell(line) => Some(line.as_str()),
        _ => None,
    };
    let unquoted = shell_line
        .into_iter()
        .flat_map(crate::shell::segments)
        .flat_map(|segment| crate::shell::lex(&segment.source))
        .map(|w| w.text);
    // A segment that is only an assignment (`OLDPWD=~/.ssh;`) never reaches
    // `segments`, but a later command can read through its value.
    let assignment_values = shell_line
        .into_iter()
        .flat_map(crate::shell::assignments)
        .map(|(_, value)| value);
    content
        .map(str::to_string)
        .into_iter()
        .chain(
            resolved
                .into_iter()
                .flat_map(|r| r.all().map(|p| p.to_string_lossy().into_owned())),
        )
        .chain(unquoted)
        .chain(assignment_values)
        .collect()
}

/// A system file, or a destructive command line, that every mode consulting
/// rules refuses. A substring match is what is wanted here.
pub(super) fn is_path_bypass_immune(content: &str) -> bool {
    let lowered = content.to_lowercase();
    if lowered.contains('/') {
        for sys_path in &["/etc/passwd", "/etc/shadow", "/etc/ssh/"] {
            if lowered.contains(sys_path) {
                return true;
            }
        }
    }
    // On a word boundary, so `echo norm -rf /` in a description is not one.
    for destructive in &["rm -rf /", "mkfs.", "dd if="] {
        if let Some(idx) = lowered.find(destructive) {
            let at_start = idx == 0;
            let after_space = idx > 0 && lowered.as_bytes().get(idx - 1) == Some(&b' ');
            if at_start || after_space {
                return true;
            }
        }
    }
    false
}

pub(super) fn unclear_message(tool_name: &str, rule: &PermissionRule) -> String {
    format!(
        "{tool_name}: which program a command here runs cannot be told from its text, so it \
         cannot be checked against {}",
        format_rule_string(rule)
    )
}

/// The rule that asks about a call and the question it asks, from a hit in
/// the managed layer's rules.
pub(super) fn managed_ask_of(tool_name: &str, hit: RuleHit) -> Option<(PermissionRule, String)> {
    match hit {
        RuleHit::Ask(rule) => {
            let message = format!(
                "{tool_name} matches managed rule {}, which requires confirmation",
                format_rule_string(&rule)
            );
            Some((rule, message))
        }
        RuleHit::Unclear(rule) => {
            let message = unclear_message(tool_name, &rule);
            Some((rule, message))
        }
        RuleHit::Allow(_) | RuleHit::Deny(_) | RuleHit::None => None,
    }
}

#[cfg(test)]
mod bypass_immune_tests {
    use super::is_path_bypass_immune;

    /// `.github` / `.gitignore` / `.gitlab-ci.yml` only share a prefix with
    /// `.git`, which would keep the agent from editing CI configuration.
    #[test]
    fn dot_git_prefix_matches_are_not_immune() {
        assert!(!is_path_bypass_immune("/repo/.github/workflows/ci.yml"));
        assert!(!is_path_bypass_immune("/repo/.gitignore"));
        assert!(!is_path_bypass_immune("/repo/.gitmodules"));
        assert!(!is_path_bypass_immune("/repo/.gitlab-ci.yml"));
        assert!(!is_path_bypass_immune("repo/.github/dependabot.yml"));
    }

    /// Configuration is not a credential: a person may approve it.
    #[test]
    fn config_dirs_are_not_denied() {
        for p in [
            "repo/.git/config",
            "/Users/me/.agix/settings.json",
            "/a/b/.claude/settings.local.json",
        ] {
            assert!(!is_path_bypass_immune(p), "{p}");
        }
    }

    #[test]
    fn ordinary_paths_are_not_immune() {
        assert!(!is_path_bypass_immune("/repo/src/main.rs"));
        assert!(!is_path_bypass_immune("crates/tools/src/bash/mod.rs"));
        assert!(!is_path_bypass_immune(".gitignore"));
    }

    #[test]
    fn system_paths_and_destructive_patterns_still_immune() {
        assert!(is_path_bypass_immune("/etc/passwd"));
        assert!(is_path_bypass_immune("/etc/shadow"));
        assert!(is_path_bypass_immune("/etc/ssh/sshd_config"));
        assert!(is_path_bypass_immune("rm -rf /"));
        assert!(is_path_bypass_immune("sudo rm -rf /"));
        assert!(is_path_bypass_immune("dd if=/dev/zero of=/dev/disk0"));
        assert!(!is_path_bypass_immune("echo norm -rf / please"));
    }
}
