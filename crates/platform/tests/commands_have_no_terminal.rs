//! A command has no controlling terminal, even when the process running it
//! has one: through `/dev/tty` it could push input into that terminal
//! (`TIOCSTI`) for whatever reads it next — the shell the daemon was started
//! from — to run outside any sandbox.
//!
//! The test runs itself again as the session leader of a pseudo-terminal of
//! its own, and spawns the command from there.

#![cfg(unix)]
#![allow(clippy::unwrap_used)]

use base::interface::exec::{Process, ProcessSpec, Sandbox, SandboxPolicy};
use futures::StreamExt;
use platform::{LocalProcess, PlatformSandbox};
use tokio_util::sync::CancellationToken;

const ON_A_TERMINAL: &str = "AGIX_TEST_ON_A_TERMINAL";

const PROBE: &str = "{ : <>/dev/tty; } 2>/dev/null && echo has-a-terminal || echo has-no-terminal";

#[test]
#[allow(unsafe_code)]
fn a_command_cannot_reach_the_terminal_of_the_process_running_it() {
    if std::env::var_os(ON_A_TERMINAL).is_some() {
        return from_the_terminal();
    }
    let (mut master, mut slave) = (-1, -1);
    // SAFETY: two out-pointers to live ints; the optional arguments are null.
    let opened = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(opened, 0, "openpty: {}", std::io::Error::last_os_error());

    let test = format!(
        "{}::a_command_cannot_reach_the_terminal_of_the_process_running_it",
        module_path!().split_once("::").map_or("", |(_, m)| m)
    );
    let mut again = std::process::Command::new(std::env::current_exe().unwrap());
    again
        .args([test.as_str(), "--exact", "--nocapture", "--test-threads=1"])
        .env(ON_A_TERMINAL, "1")
        .stdin(std::process::Stdio::null());
    // SAFETY: `setsid` and `ioctl` are async-signal-safe; `slave` is a plain
    // fd number.
    unsafe {
        std::os::unix::process::CommandExt::pre_exec(&mut again, move || {
            if libc::setsid() == -1 || libc::ioctl(slave, libc::TIOCSCTTY as _, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let out = again.output().unwrap();
    // SAFETY: both fds came from `openpty` above and are closed once.
    unsafe {
        libc::close(slave);
        libc::close(master);
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && stdout.contains("1 passed"),
        "{}\n{stdout}\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn from_the_terminal() {
    assert!(
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .is_ok(),
        "the test did not get a terminal of its own"
    );
    let dir = tempfile::tempdir().unwrap();
    let bare = ProcessSpec::new("bash", dir.path()).args(["-c".to_string(), PROBE.to_string()]);
    let confined = PlatformSandbox
        .confine(bare.clone(), &SandboxPolicy::default())
        .spec;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    for spec in [bare, confined] {
        let program = spec.program.clone();
        let printed = runtime.block_on(stdout_of(spec));
        assert_eq!(printed.trim(), "has-no-terminal", "under {program}");
    }
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
