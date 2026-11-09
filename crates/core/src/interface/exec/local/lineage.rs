//! Whether a process is one this process started.
//!
//! The daemon serves whoever reaches its socket, and the commands it runs must
//! not be among them: a command that could call the daemon would be its own
//! host. A process counts as started here when this process is among its
//! ancestors, or when it or an ancestor is in a process group this process
//! started ([`super::orphans::listed`]) — which is where a command's
//! background child stays when its parent exits and it is handed to init. One
//! that also leaves the group (`setsid`) is not recognised; the OS sandbox,
//! which follows a process wherever it is reparented, is what refuses that one.

/// How far up the tree to look before giving up.
const MAX_DEPTH: usize = 256;

/// Whether `pid` is a process this process started, directly or not. This
/// process itself is not.
pub fn started_here(pid: u32) -> bool {
    let me = std::process::id();
    if pid == me {
        return false;
    }
    let groups = super::orphans::listed();
    let mut current = pid;
    for _ in 0..MAX_DEPTH {
        let Some(Parentage { parent, group }) = parentage(current) else {
            return false;
        };
        if parent == me || groups.contains(&group) {
            return true;
        }
        if parent <= 1 {
            return false;
        }
        current = parent;
    }
    false
}

struct Parentage {
    parent: u32,
    group: i32,
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
fn parentage(pid: u32) -> Option<Parentage> {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: `info` is `size` bytes, which is what the call may write.
    let written = unsafe {
        libc::proc_pidinfo(
            libc::c_int::try_from(pid).ok()?,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if written != size {
        return None;
    }
    // SAFETY: the call filled all `size` bytes.
    let info = unsafe { info.assume_init() };
    Some(Parentage {
        parent: info.pbi_ppid,
        group: i32::try_from(info.pbi_pgid).ok()?,
    })
}

#[cfg(target_os = "linux")]
fn parentage(pid: u32) -> Option<Parentage> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name is in parentheses and may itself contain them.
    let mut fields = stat.get(stat.rfind(')')? + 1..)?.split_whitespace();
    let _state = fields.next()?;
    Some(Parentage {
        parent: fields.next()?.parse().ok()?,
        group: fields.next()?.parse().ok()?,
    })
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn parentage(_pid: u32) -> Option<Parentage> {
    None
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    fn kill(pid: u32) {
        let _ = Command::new("kill").arg(pid.to_string()).status();
    }

    #[test]
    fn a_child_and_its_child_were_started_here() {
        let mut child = Command::new("sh")
            .args(["-c", "sleep 30 & echo $!; wait"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let grandchild: u32 = line.trim().parse().unwrap();

        assert!(started_here(child.id()));
        assert!(started_here(grandchild));
        kill(grandchild);
        let _ = child.wait();
    }

    #[test]
    #[allow(unsafe_code)]
    fn this_process_and_those_above_it_were_not() {
        assert!(!started_here(std::process::id()));
        // SAFETY: getppid has no preconditions.
        let parent = unsafe { libc::getppid() } as u32;
        assert!(!started_here(parent));
        assert!(!started_here(1));
    }

    /// A background process whose parent exited belongs to init, but it is
    /// still in the group of the command that started it.
    #[test]
    fn an_orphan_in_a_group_this_process_started_was() {
        let out = Command::new("sh")
            .args(["-c", "sleep 30 > /dev/null 2>&1 & echo $!"])
            .process_group(0)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let leader = i32::try_from(out.id()).unwrap();
        let output = out.wait_with_output().unwrap();
        let orphan: u32 = String::from_utf8(output.stdout)
            .unwrap()
            .trim()
            .parse()
            .unwrap();

        let unlisted = started_here(orphan);
        let listed = {
            let _listed = super::super::orphans::list(leader);
            started_here(orphan)
        };
        kill(orphan);
        assert!(
            !unlisted,
            "an orphan was traced to this process by its parents"
        );
        assert!(listed);
    }
}
