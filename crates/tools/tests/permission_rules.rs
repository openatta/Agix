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
    let dir = tempfile::tempdir().unwrap();
    let home = permissions::path_rule::resolve_pattern("~", dir.path());
    let perm = permission(
        vec![rule(RuleBehavior::Deny, "Read", "~/agix-test-creds/**")],
        PermissionMode::Default,
    );
    let target = Path::new(&home).join("agix-test-creds/token");
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

#[cfg(unix)]
#[tokio::test]
async fn a_deny_on_where_a_write_really_lands_reaches_a_dotdot_after_a_symlink() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let (proj, outside) = (root.join("proj"), root.join("outside"));
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::create_dir_all(outside.join("sub")).unwrap();
    std::os::unix::fs::symlink(outside.join("sub"), proj.join("link")).unwrap();
    let perm = permission(
        vec![rule(
            RuleBehavior::Deny,
            "Write",
            format!("{}/**", outside.display()),
        )],
        PermissionMode::Default,
    );
    let o = check(
        &perm,
        "Write",
        json!({"file_path": "link/../x", "content": "x"}),
        &proj,
    )
    .await;
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
        PermissionMode::Default,
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

/// What a user who wants "anything but deleting" writes.
fn anything_but_rm() -> RuleSetPermission {
    permission(
        vec![
            PermissionRule {
                rule_content: None,
                ..rule(RuleBehavior::Allow, "Bash", "")
            },
            rule(RuleBehavior::Deny, "Bash", "rm:*"),
        ],
        PermissionMode::AcceptEdits,
    )
}

#[tokio::test]
async fn a_deny_reaches_the_program_however_its_name_is_spelled() {
    let dir = tempfile::tempdir().unwrap();
    let perm = anything_but_rm();
    for line in [
        "\"rm\" -rf build",
        "\\rm -rf build",
        "r''m -rf build",
        "{rm,-rf,build}",
        "/bin/rm -rf build",
        "./node_modules/.bin/rm x",
        "command -p rm -rf build",
        "env -i rm -rf build",
        "exec rm -rf build",
        "builtin rm x",
        "nice -n 5 rm -rf build",
        "time rm -rf build",
        "sudo -u root rm -rf build",
        "timeout 5 rm -rf build",
        "bash -c 'rm -rf build'",
        "eval rm -rf build",
        "find . -name build -exec rm -rf {} +",
        "ls | xargs rm",
        ">/dev/null rm -rf build",
        "busybox rm -rf build",
        "flock /tmp/lock rm -rf build",
        "ionice -c 3 rm -rf build",
        "chrt -o 0 rm -rf build",
        "taskset -c 0 rm -rf build",
        "su -c 'rm -rf build' root",
        "runuser -u me -- rm -rf build",
        "caffeinate -i rm -rf build",
        "pkexec rm -rf build",
    ] {
        let o = check(&perm, "Bash", bash(line), dir.path()).await;
        assert!(is_deny(&o), "{line}: {o:?}");
    }
    for line in ["echo rm", "git rm --cached x", "command -v rm", "ls rm"] {
        let o = check(&perm, "Bash", bash(line), dir.path()).await;
        assert!(matches!(o, PermissionOutcome::Permit), "{line}: {o:?}");
    }
}

#[tokio::test]
async fn a_program_that_cannot_be_read_off_the_line_is_asked_about_under_a_deny() {
    let dir = tempfile::tempdir().unwrap();
    for line in [
        "$CMD -rf build",
        "\"$(echo rm)\" -rf build",
        "$'\\x72\\x6d' -rf build",
        "/bin/r? -rf build",
        "sudo --some-new-flag rm -rf build",
        "case x in *) rm -rf build;; esac",
        "function f { rm -rf build; }; f",
        "echo 'rm -rf build' | sh",
        "bash <<< 'rm -rf build'",
        "flock -c 'rm -rf build' /tmp/lock",
    ] {
        let o = check(&anything_but_rm(), "Bash", bash(line), dir.path()).await;
        assert!(
            matches!(o, PermissionOutcome::Prompt { .. }),
            "{line}: {o:?}"
        );
        let refused = check(
            &permission(
                vec![
                    rule(RuleBehavior::Allow, "Bash", "*"),
                    rule(RuleBehavior::Deny, "Bash", "rm:*"),
                ],
                PermissionMode::DontAsk,
            ),
            "Bash",
            bash(line),
            dir.path(),
        )
        .await;
        assert!(is_deny(&refused), "dontAsk {line}: {refused:?}");
    }
    let unruled = permission(
        vec![PermissionRule {
            rule_content: None,
            ..rule(RuleBehavior::Allow, "Bash", "")
        }],
        PermissionMode::AcceptEdits,
    );
    let o = check(&unruled, "Bash", bash("$CMD -rf build"), dir.path()).await;
    assert!(
        matches!(o, PermissionOutcome::Permit),
        "with no deny to slip past, nothing changes: {o:?}"
    );
}

#[tokio::test]
async fn a_quoted_protected_path_is_still_protected() {
    let dir = tempfile::tempdir().unwrap();
    let perm = permission(vec![], PermissionMode::AcceptEdits);
    for line in [
        "cat ~/\".ssh\"/id_rsa",
        "cat ~/.s''sh/id_rsa",
        "cat ~/'.aws'/credentials",
        "OLDPWD=~/.ssh; cd - && cat id_rsa",
        "K=~/.aws/credentials; cat $K",
    ] {
        let o = check(&perm, "Bash", bash(line), dir.path()).await;
        assert!(is_deny(&o), "{line}: {o:?}");
    }
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

#[tokio::test]
async fn a_read_through_anything_the_shell_still_expands_is_asked_about() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path();
    std::fs::create_dir(cwd.join("src")).unwrap();
    let perm = permission(vec![], PermissionMode::Default);
    for line in [
        "HOME=/etc; cat $HOME/hosts",
        "cat \"${D}\"/x",
        "cat $F",
        "echo $ANTHROPIC_API_KEY",
        "HOME=/etc cat ~/hosts",
        "D=/etc; cd $D && cat hosts",
        "cd && cat .bashrc",
        "cd ~ && cat .bashrc",
        "cd /etc && cat hosts",
        "cd .. && cat x",
        "pushd /etc && cat hosts",
        "cat </etc/hosts",
        "cd - && cat x",
        "popd && cat x",
        "CDPATH=/; cd etc && cat passwd",
        "PWD=/etc; cd . && cat hosts",
        "OLDPWD=/etc; cd - && cat hosts",
    ] {
        let o = check(&perm, "Bash", bash(line), cwd).await;
        assert!(
            matches!(o, PermissionOutcome::Prompt { .. }),
            "{line}: {o:?}"
        );
    }
    for line in [
        "cd src && ls",
        "cd src && cat a.rs",
        "grep -n 'foo$' src/a.rs",
        "echo '$HOME'",
    ] {
        let o = check(&perm, "Bash", bash(line), cwd).await;
        assert!(matches!(o, PermissionOutcome::Permit), "{line}: {o:?}");
    }
}

#[tokio::test]
async fn a_read_only_name_that_runs_a_program_is_asked_about_and_refused_in_plan_mode() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path();
    for line in [
        "fd . -x touch ran.txt",
        "rg --pre ./x.sh . x.txt",
        "git grep -O./x.sh foo",
        "cargo check",
        "PATH=.; ls",
        "sort -o out.txt in.txt",
        "git config core.pager cat",
    ] {
        let o = check(
            &permission(vec![], PermissionMode::Default),
            "Bash",
            bash(line),
            cwd,
        )
        .await;
        assert!(
            matches!(o, PermissionOutcome::Prompt { .. }),
            "{line}: {o:?}"
        );
        let o = check(
            &permission(vec![], PermissionMode::Plan),
            "Bash",
            bash(line),
            cwd,
        )
        .await;
        assert!(is_deny(&o), "plan {line}: {o:?}");
    }
}

/// A shell command that writes the engine's own configuration is not
/// read-only, so it goes to the normal confirmation flow rather than the
/// silent fast path — while reading it stays silent.
#[tokio::test]
async fn a_shell_command_writing_the_engines_configuration_is_asked_about() {
    let dir = tempfile::tempdir().unwrap();
    let perm = permission(vec![], PermissionMode::Default);
    let o = check(&perm, "Bash", bash("git config user.name x"), dir.path()).await;
    assert!(matches!(o, PermissionOutcome::Prompt { .. }), "{o:?}");
    let o = check(&perm, "Bash", bash("git config user.name"), dir.path()).await;
    assert!(matches!(o, PermissionOutcome::Permit), "read: {o:?}");
}

fn monitor(command: &str) -> Value {
    json!({ "command": command, "description": "watch" })
}

#[tokio::test]
async fn a_bash_deny_also_holds_monitor() {
    let dir = tempfile::tempdir().unwrap();
    let perm = permission(
        vec![
            PermissionRule {
                rule_content: None,
                ..rule(RuleBehavior::Allow, "Monitor", "")
            },
            rule(RuleBehavior::Deny, "Bash", "rm:*"),
            rule(RuleBehavior::Ask, "Bash", "curl:*"),
        ],
        PermissionMode::Default,
    );
    for line in ["rm -rf build", "true && \\rm -rf build"] {
        let o = check(&perm, "Monitor", monitor(line), dir.path()).await;
        assert!(is_deny(&o), "{line}: {o:?}");
    }
    let o = check(&perm, "Monitor", monitor("curl -s x"), dir.path()).await;
    assert!(matches!(o, PermissionOutcome::Prompt { .. }), "{o:?}");
    let o = check(&perm, "Monitor", monitor("tail -f log"), dir.path()).await;
    assert!(matches!(o, PermissionOutcome::Permit), "{o:?}");
}

/// A watch script is only as read-only as the command it runs: plan mode
/// refuses one that writes or starts other programs, and acceptEdits does not
/// wave it through.
#[tokio::test]
async fn a_monitor_script_is_read_only_only_when_its_command_is() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path();
    let plan = permission(vec![], PermissionMode::Plan);
    let accept_edits = permission(vec![], PermissionMode::AcceptEdits);
    for line in [
        "python3 watch.py",
        "while true; do date >> log; sleep 5; done",
        "npm run dev",
    ] {
        let o = check(&plan, "Monitor", monitor(line), cwd).await;
        assert!(is_deny(&o), "plan {line}: {o:?}");
        let o = check(&accept_edits, "Monitor", monitor(line), cwd).await;
        assert!(
            matches!(o, PermissionOutcome::Prompt { .. }),
            "acceptEdits {line}: {o:?}"
        );
    }
    let o = check(&plan, "Monitor", monitor("tail -f log"), cwd).await;
    assert!(
        matches!(o, PermissionOutcome::Prompt { .. }),
        "plan, read-only: {o:?}"
    );
    let o = check(&accept_edits, "Monitor", monitor("tail -f log"), cwd).await;
    assert!(
        matches!(o, PermissionOutcome::Permit),
        "acceptEdits, read-only: {o:?}"
    );
}

/// Plan mode adds a refusal of writes to Default mode and nothing else: a
/// read outside the project is asked about there as it is in Default.
#[tokio::test]
async fn plan_mode_asks_before_reading_outside_the_project() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let cwd = dir.path();
    std::fs::write(cwd.join("a.txt"), "a").unwrap();
    std::fs::write(outside.path().join("b.txt"), "b").unwrap();
    let elsewhere = outside.path().join("b.txt").display().to_string();
    for mode in [PermissionMode::Plan, PermissionMode::Default] {
        let perm = permission(vec![], mode);
        let read = |path: String| json!({ "file_path": path });
        let o = check(
            &perm,
            "Read",
            read(cwd.join("a.txt").display().to_string()),
            cwd,
        )
        .await;
        assert!(matches!(o, PermissionOutcome::Permit), "{mode:?}: {o:?}");
        let o = check(&perm, "Read", read(elsewhere.clone()), cwd).await;
        assert!(
            matches!(o, PermissionOutcome::Prompt { .. }),
            "{mode:?}: {o:?}"
        );
        let o = check(&perm, "Bash", bash(&format!("cat {elsewhere}")), cwd).await;
        assert!(
            matches!(o, PermissionOutcome::Prompt { .. }),
            "{mode:?}: {o:?}"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn a_cd_through_a_symlink_out_of_the_project_is_asked_about() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let cwd = dir.path();
    std::os::unix::fs::symlink(outside.path(), cwd.join("elsewhere")).unwrap();
    let perm = permission(vec![], PermissionMode::Default);
    let o = check(&perm, "Bash", bash("cd elsewhere && cat x"), cwd).await;
    assert!(matches!(o, PermissionOutcome::Prompt { .. }), "{o:?}");
}
