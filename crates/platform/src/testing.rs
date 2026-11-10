//! Test support for the crates that start processes: running one test again
//! as the session leader of a pseudo-terminal of its own, so that it can check
//! that what it starts cannot reach that terminal. Through `/dev/tty` a child
//! could push input into it (`TIOCSTI`) for the shell the daemon was started
//! from to run, outside any sandbox. Built only with the `testing` feature.

use std::ffi::OsString;

const ON_A_TERMINAL: &str = "AGIX_TEST_ON_A_TERMINAL";

/// A `sh -c` script that prints `has-a-terminal` when it can open `/dev/tty`
/// and `has-no-terminal` when it cannot. `true`, not `:`: a redirection that
/// fails on a special builtin makes a POSIX `sh` such as dash exit.
pub const PROBE: &str =
    "{ true <>/dev/tty; } 2>/dev/null && echo has-a-terminal || echo has-no-terminal";

/// Runs `body` with a controlling terminal. Called from the test function
/// `test` of module `module` (its `module_path!()`), it runs the test binary
/// again, that one test alone, with `env` added, as the session leader of a
/// new pseudo-terminal, and asserts that the run passed; there, it calls
/// `body`.
pub fn on_a_terminal_of_its_own(
    module: &str,
    test: &str,
    env: &[(&str, OsString)],
    body: impl FnOnce(),
) {
    if std::env::var_os(ON_A_TERMINAL).is_some() {
        assert!(
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/tty")
                .is_ok(),
            "the test did not get a terminal of its own"
        );
        return body();
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

    let filter = match module.split_once("::") {
        Some((_, inner)) => format!("{inner}::{test}"),
        None => test.to_string(),
    };
    let exe = std::env::current_exe().unwrap_or_else(|e| panic!("current_exe: {e}"));
    let mut again = std::process::Command::new(exe);
    again
        .args([
            filter.as_str(),
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(ON_A_TERMINAL, "1")
        .envs(env.iter().map(|(k, v)| (k, v)))
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
    let out = again.output();
    // SAFETY: both fds came from `openpty` above and are closed once.
    unsafe {
        libc::close(slave);
        libc::close(master);
    }
    let out = out.unwrap_or_else(|e| panic!("running {filter} again: {e}"));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && stdout.contains("1 passed"),
        "{filter}: {}\n{stdout}\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
}
