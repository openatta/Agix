//! Permission rules from `settings.json`, asked through the real tools.
//!
//! A rule names a path or a command one way; a model can name the same thing
//! many others — a path relative to cwd, through `..`, through a symlink,
//! through a different tool that reads the same bytes; a command chained
//! after another, inside `$(...)`, behind a variable assignment. Every case
//! here goes in through `Permission::check` with the built-in tool the model
//! would actually call.

use base::interface::permission::{Permission, PermissionOutcome};
use base::permission::{PermissionMode, PermissionRule, RuleBehavior, RuleSource};
use permissions::gate::PermissionGate;
use permissions::rule_set_permission::RuleSetPermission;
use permissions::ruleset::RuleSet;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;

fn rule(behavior: RuleBehavior, tool: &str, content: impl Into<String>) -> PermissionRule {
    PermissionRule {
        source: RuleSource::UserSettings,
        behavior,
        tool_name: tool.into(),
        rule_content: Some(content.into()),
    }
}

fn permission(rules: Vec<PermissionRule>, mode: PermissionMode) -> RuleSetPermission {
    let registry = Arc::new(base::tool::InMemoryToolRegistry::new());
    tools::register_builtin_tools(&registry);
    RuleSetPermission::new(
        Arc::new(PermissionGate::new(RuleSet::new(rules))),
        registry,
        mode,
    )
}

async fn check(
    perm: &RuleSetPermission,
    tool: &str,
    input: Value,
    cwd: &Path,
) -> PermissionOutcome {
    perm.check(tool, &input, cwd, "s").await
}

fn is_deny(o: &PermissionOutcome) -> bool {
    matches!(o, PermissionOutcome::Deny { .. })
}

#[tokio::test]
async fn a_denied_file_inside_cwd_cannot_be_read_by_a_relative_or_dotted_name() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path();
    std::fs::create_dir(cwd.join("src")).unwrap();
    std::fs::write(cwd.join("secret.txt"), "x").unwrap();
    let perm = permission(
        vec![rule(
            RuleBehavior::Deny,
            "Read",
            cwd.join("secret.txt").to_string_lossy(),
        )],
        PermissionMode::Default,
    );
    for name in ["secret.txt", "./secret.txt", "src/../secret.txt"] {
        let o = check(&perm, "Read", json!({"file_path": name}), cwd).await;
        assert!(is_deny(&o), "{name}: {o:?}");
    }
    let o = check(&perm, "Read", json!({"file_path": "src/other.rs"}), cwd).await;
    assert!(matches!(o, PermissionOutcome::Permit), "{o:?}");
}

#[tokio::test]
async fn a_relative_rule_pattern_is_read_relative_to_cwd() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path();
    let perm = permission(
        vec![rule(RuleBehavior::Deny, "Read", "private/**")],
        PermissionMode::Default,
    );
    let abs = cwd.join("private/notes.md");
    let o = check(
        &perm,
        "Read",
        json!({"file_path": abs.to_string_lossy()}),
        cwd,
    )
    .await;
    assert!(is_deny(&o), "{o:?}");
}

#[tokio::test]
async fn a_read_rule_also_governs_grep_and_glob() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path();
    std::fs::create_dir(cwd.join("vault")).unwrap();
    let perm = permission(
        vec![rule(
            RuleBehavior::Deny,
            "Read",
            cwd.join("vault").to_string_lossy(),
        )],
        PermissionMode::Default,
    );
    let o = check(
        &perm,
        "Grep",
        json!({"pattern": "password", "path": "vault"}),
        cwd,
    )
    .await;
    assert!(is_deny(&o), "grep: {o:?}");
    let o = check(&perm, "Glob", json!({"pattern": "*", "path": "vault"}), cwd).await;
    assert!(is_deny(&o), "glob: {o:?}");
}

#[tokio::test]
async fn an_edit_rule_also_governs_write() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path();
    let perm = permission(
        vec![rule(
            RuleBehavior::Deny,
            "Edit",
            format!("{}/locked/**", cwd.display()),
        )],
        PermissionMode::AcceptEdits,
    );
    let o = check(
        &perm,
        "Write",
        json!({"file_path": "locked/a.txt", "content": "x"}),
        cwd,
    )
    .await;
    assert!(is_deny(&o), "{o:?}");
}

#[tokio::test]
async fn a_tilde_in_a_rule_means_the_home_directory() {
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let perm = permission(
        vec![rule(RuleBehavior::Deny, "Read", "~/attacore-test-creds/**")],
        PermissionMode::BypassPermissions,
    );
    let target = Path::new(&home).join("attacore-test-creds/token");
    let o = check(
        &perm,
        "Read",
        json!({"file_path": target.to_string_lossy()}),
        dir.path(),
    )
    .await;
    assert!(is_deny(&o), "{o:?}");
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlink_to_a_denied_file_is_denied_too() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = std::fs::canonicalize(dir.path()).unwrap();
    std::fs::write(cwd.join("secret.txt"), "x").unwrap();
    std::os::unix::fs::symlink(cwd.join("secret.txt"), cwd.join("innocent.txt")).unwrap();
    let perm = permission(
        vec![rule(
            RuleBehavior::Deny,
            "Read",
            cwd.join("secret.txt").to_string_lossy(),
        )],
        PermissionMode::Default,
    );
    let o = check(&perm, "Read", json!({"file_path": "innocent.txt"}), &cwd).await;
    assert!(is_deny(&o), "{o:?}");
}

#[tokio::test]
async fn a_single_star_in_a_path_rule_stays_within_one_directory() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path();
    let perm = permission(
        vec![rule(
            RuleBehavior::Allow,
            "Read",
            format!("{}/docs/*", cwd.display()),
        )],
        PermissionMode::DontAsk,
    );
    let o = check(&perm, "Read", json!({"file_path": "docs/a.md"}), cwd).await;
    assert!(matches!(o, PermissionOutcome::Permit), "{o:?}");
    let o = check(&perm, "Read", json!({"file_path": "docs/sub/b.md"}), cwd).await;
    assert!(is_deny(&o), "{o:?}");
}

#[tokio::test]
async fn an_ask_rule_is_not_waved_through_by_the_tools_own_allow() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path();
    let rules = || {
        vec![rule(
            RuleBehavior::Ask,
            "Read",
            cwd.join("notes.md").to_string_lossy(),
        )]
    };
    let input = || json!({"file_path": "notes.md"});

    let o = check(
        &permission(rules(), PermissionMode::Default),
        "Read",
        input(),
        cwd,
    )
    .await;
    assert!(matches!(o, PermissionOutcome::Prompt { .. }), "{o:?}");
    let o = check(
        &permission(rules(), PermissionMode::DontAsk),
        "Read",
        input(),
        cwd,
    )
    .await;
    assert!(is_deny(&o), "{o:?}");
    let o = check(
        &permission(rules(), PermissionMode::BypassPermissions),
        "Read",
        input(),
        cwd,
    )
    .await;
    assert!(matches!(o, PermissionOutcome::Permit), "{o:?}");
}

fn bash(command: &str) -> Value {
    json!({ "command": command })
}

#[tokio::test]
async fn an_allow_for_one_command_does_not_cover_what_is_chained_after_it() {
    let dir = tempfile::tempdir().unwrap();
    let perm = permission(
        vec![rule(RuleBehavior::Allow, "Bash", "cd:*")],
        PermissionMode::DontAsk,
    );
    let o = check(&perm, "Bash", bash("cd sub"), dir.path()).await;
    assert!(matches!(o, PermissionOutcome::Permit), "{o:?}");
    for line in [
        "cd . && xforge approve",
        "cd .; xforge approve",
        "cd $(xforge approve)",
    ] {
        let o = check(&perm, "Bash", bash(line), dir.path()).await;
        assert!(is_deny(&o), "{line}: {o:?}");
    }
}

#[tokio::test]
async fn a_deny_for_one_command_reaches_it_wherever_it_sits_in_the_line() {
    let dir = tempfile::tempdir().unwrap();
    let perm = permission(
        vec![rule(RuleBehavior::Deny, "Bash", "rm:*")],
        PermissionMode::BypassPermissions,
    );
    for line in [
        "true; rm -rf build",
        "ls && rm -rf build",
        "echo \"$(rm -rf build)\"",
        "FOO=1 rm -rf build",
        "if true; then rm -rf build; fi",
        "echo 'a;b' | rm -rf build",
    ] {
        let o = check(&perm, "Bash", bash(line), dir.path()).await;
        assert!(is_deny(&o), "{line}: {o:?}");
    }
    let o = check(&perm, "Bash", bash("echo 'rm -rf build'"), dir.path()).await;
    assert!(matches!(o, PermissionOutcome::Permit), "{o:?}");
}

#[tokio::test]
async fn an_exact_allow_for_a_whole_line_still_permits_that_line() {
    let dir = tempfile::tempdir().unwrap();
    let perm = permission(
        vec![rule(
            RuleBehavior::Allow,
            "Bash",
            "npm run build && npm test",
        )],
        PermissionMode::DontAsk,
    );
    let o = check(&perm, "Bash", bash("npm run build && npm test"), dir.path()).await;
    assert!(matches!(o, PermissionOutcome::Permit), "{o:?}");
}

#[tokio::test]
async fn a_read_only_command_is_only_silent_inside_the_project() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path();
    let perm = permission(vec![], PermissionMode::Default);
    for line in [
        "cat README.md",
        "ls src/",
        "git log origin/main",
        "grep -r foo .",
    ] {
        let o = check(&perm, "Bash", bash(line), cwd).await;
        assert!(matches!(o, PermissionOutcome::Permit), "{line}: {o:?}");
    }
    for line in [
        "cat /etc/hosts",
        "ls ../",
        "cat ~/.bashrc",
        "grep -r token --include=x /var/log",
        "head --file=/etc/hosts",
        "cat ~root/.profile",
    ] {
        let o = check(&perm, "Bash", bash(line), cwd).await;
        assert!(
            matches!(o, PermissionOutcome::Prompt { .. }),
            "{line}: {o:?}"
        );
    }
}
