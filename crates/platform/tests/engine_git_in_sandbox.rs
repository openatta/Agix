//! The engine's own git, run in a session's sandbox, does not run what a
//! repository's config names outside it.

#![cfg(all(feature = "sandbox", target_os = "macos"))]

use base::engine_exec::{Confinement, EngineExec};
use base::frozen::FrozenContext;
use base::paths::ConfigPaths;
use base::project_trust::ProjectTrust;
use std::path::Path;
use tempfile::TempDir;

fn test_paths(root: &Path) -> ConfigPaths {
    ConfigPaths::new(root.join(".agix"), root.join(".agix"), "code")
}

fn run_git(cwd: &Path) -> impl Fn(&[&str]) + '_ {
    move |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap_or_else(|e| panic!("could not run `git {}`: {e}", args.join(" ")));
        assert!(
            out.status.success(),
            "`git {}` failed ({}): {}{}",
            args.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stderr),
            String::from_utf8_lossy(&out.stdout),
        );
    }
}

/// A repository a sandboxed command can leave behind on Linux, where a
/// project with no `.git` can be given one: a clean filter and an
/// fsmonitor, both a script that writes a marker. macOS refuses to create
/// that `.git` from inside the sandbox, so the test plants it directly.
/// The engine's git for a sandboxed session runs the filter inside the
/// sandbox, where the write is refused, and still reports the status.
#[tokio::test]
async fn a_filter_the_repository_names_runs_inside_the_sessions_sandbox() {
    use base::interface::exec::SandboxPolicy;
    use platform::PlatformSandbox;
    use std::os::unix::fs::PermissionsExt;
    let sandbox_exec_works = std::process::Command::new("sandbox-exec")
        .args(["-p", "(version 1)(allow default)", "true"])
        .status()
        .is_ok_and(|s| s.success());
    if !sandbox_exec_works {
        return;
    }
    let dirs: Vec<TempDir> = (0..3)
        .map(|_| tempfile::Builder::new().tempdir_in("/tmp").unwrap())
        .collect();
    let (project, elsewhere, state_root) = (dirs[0].path(), dirs[1].path(), dirs[2].path());
    let run = run_git(project);
    run(&["init", "-q"]);
    run(&["config", "user.email", "test@example.com"]);
    run(&["config", "user.name", "Test"]);
    std::fs::write(project.join("f.txt"), "a\n").unwrap();
    run(&["add", "f.txt"]);
    run(&["commit", "-qm", "first"]);

    let marker = project.join("filter-ran");
    let script = elsewhere.join("plant.sh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\ntouch '{}'\ncat\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let script = script.to_str().unwrap();
    run(&["config", "core.fsmonitor", script]);
    run(&["config", "filter.x.clean", script]);
    std::fs::write(project.join(".gitattributes"), "*.txt filter=x\n").unwrap();
    // Same size, new mtime: git can only tell whether it changed by
    // reading it through the filter.
    std::fs::File::options()
        .write(true)
        .open(project.join("f.txt"))
        .unwrap()
        .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(5))
        .unwrap();

    let sandboxed = EngineExec::new(
        base::interface::exec::ExecProviders {
            sandbox: std::sync::Arc::new(PlatformSandbox),
            ..platform::local_providers()
        },
        Confinement::Sandboxed {
            policy: SandboxPolicy {
                state_root: Some(state_root.to_path_buf()),
                ..Default::default()
            },
            require_enforcement: true,
            tmpdir: None,
        },
        ProjectTrust::Trusted,
    )
    .with_min_deadline(std::time::Duration::from_secs(60));
    let mut ctx = FrozenContext::collect(
        project.to_path_buf(),
        &test_paths(state_root),
        &base::interface::environment::SystemEnvironment,
        &sandboxed,
    )
    .await;
    ctx.refresh_git_status(&sandboxed).await;
    ctx.refresh_git(&sandboxed).await;
    assert!(ctx.is_git);
    assert!(
        ctx.git_status
            .as_deref()
            .unwrap_or_default()
            .contains(".gitattributes"),
        "{:?}",
        ctx.git_status
    );
    assert!(!marker.exists(), "the filter wrote outside the sandbox");

    // Unsandboxed, the same status does run it, so the check above is
    // looking at something that would happen.
    ctx.refresh_git_status(
        &EngineExec::new(
            platform::local_providers(),
            Confinement::Unconfined,
            ProjectTrust::Trusted,
        )
        .with_min_deadline(std::time::Duration::from_secs(60)),
    )
    .await;
    assert!(ctx.git_status.is_some());
    assert!(marker.exists());
}
