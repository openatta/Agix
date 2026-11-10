//! A command has no controlling terminal, even when the process running it
//! has one: through `/dev/tty` it could push input into that terminal
//! (`TIOCSTI`) for whatever reads it next — the shell the daemon was started
//! from — to run outside any sandbox. That holds for what the engine's own git
//! starts from a repository's config, too.
//!
//! Each test runs itself again as the session leader of a pseudo-terminal of
//! its own, and starts the command from there.

#![cfg(unix)]
#![allow(clippy::unwrap_used)]

use base::frozen::git::EngineGit;
use base::frozen::FrozenContext;
use base::interface::exec::{Process, ProcessSpec, Sandbox, SandboxPolicy};
use futures::StreamExt;
use platform::testing::{on_a_terminal_of_its_own, PROBE};
use platform::{LocalProcess, PlatformSandbox};
use std::path::Path;
use tokio_util::sync::CancellationToken;

fn current_thread() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn a_command_cannot_reach_the_terminal_of_the_process_running_it() {
    on_a_terminal_of_its_own(
        module_path!(),
        "a_command_cannot_reach_the_terminal_of_the_process_running_it",
        &[],
        || {
            let dir = tempfile::tempdir().unwrap();
            let bare =
                ProcessSpec::new("bash", dir.path()).args(["-c".to_string(), PROBE.to_string()]);
            let confined = PlatformSandbox
                .confine(bare.clone(), &SandboxPolicy::default())
                .spec;
            let runtime = current_thread();
            for spec in [bare, confined] {
                let program = spec.program.clone();
                let printed = runtime.block_on(stdout_of(spec));
                assert_eq!(printed.trim(), "has-no-terminal", "under {program}");
            }
        },
    );
}

/// The filter fails when it can open `/dev/tty`, and git, told the filter
/// is required, then fails the status. When it cannot, the status shows
/// the file changed, which also proves the filter ran.
#[test]
fn a_filter_the_engines_git_runs_cannot_reach_the_terminal() {
    on_a_terminal_of_its_own(
        module_path!(),
        "a_filter_the_engines_git_runs_cannot_reach_the_terminal",
        &[],
        || {
            let runtime = current_thread();
            let state = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
            let sandboxed = EngineGit::sandboxed(
                std::sync::Arc::new(PlatformSandbox),
                SandboxPolicy {
                    state_root: Some(state.path().to_path_buf()),
                    ..Default::default()
                },
                std::env::var_os("AGIX_REQUIRE_SANDBOX").is_some(),
                None,
            );
            for git in [EngineGit::Direct, sandboxed] {
                let project = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
                repository_with_a_terminal_probing_filter(project.path());
                let ctx = runtime.block_on(FrozenContext::collect(
                    project.path().to_path_buf(),
                    &base::paths::ConfigPaths::new(
                        state.path().join(".agix"),
                        state.path().join(".agix"),
                        "code",
                    ),
                    &base::interface::environment::SystemEnvironment,
                    &git,
                ));
                assert!(ctx.is_git, "{git:?}");
                assert!(
                    ctx.git_status
                        .as_deref()
                        .is_some_and(|s| s.contains("f.txt")),
                    "{git:?}: {:?}",
                    ctx.git_status
                );
            }
        },
    );
}

fn repository_with_a_terminal_probing_filter(project: &Path) {
    let run = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(project)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "`git {}`: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
    };
    run(&["init", "-q"]);
    run(&["config", "user.email", "test@example.com"]);
    run(&["config", "user.name", "Test"]);
    std::fs::write(project.join("f.txt"), "a\n").unwrap();
    run(&["add", "f.txt"]);
    run(&["commit", "-qm", "first"]);
    run(&[
        "config",
        "filter.x.clean",
        "cat >/dev/null; { true <>/dev/tty; } 2>/dev/null && exit 1; echo filtered",
    ]);
    run(&["config", "filter.x.required", "true"]);
    std::fs::write(project.join(".gitattributes"), "*.txt filter=x\n").unwrap();
    // Same size, new mtime: git can only tell whether it changed by reading
    // it through the filter.
    std::fs::File::options()
        .write(true)
        .open(project.join("f.txt"))
        .unwrap()
        .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(5))
        .unwrap();
}

async fn stdout_of(spec: ProcessSpec) -> String {
    let mut handle = LocalProcess::default()
        .spawn(spec, CancellationToken::new())
        .await
        .unwrap();
    let mut output = handle.output().unwrap();
    let mut stdout = Vec::new();
    while let Some(chunk) = output.next().await {
        let chunk = chunk.unwrap();
        if chunk.stream == base::interface::exec::OutputStream::Stdout {
            stdout.extend(chunk.bytes);
        }
    }
    handle.wait().await.unwrap();
    String::from_utf8_lossy(&stdout).into_owned()
}
