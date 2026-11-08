//! Pid-reuse-safe process liveness.
//!
//! Shared by `daemon::discovery` (the daemon singleton lock) and
//! `team::lock` (the team-directory lock) — both need the same "is the
//! recorded holder of this pid still the process that wrote it, or did the
//! pid get recycled by something unrelated" check, so it lives here instead
//! of being duplicated.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// A process, identified precisely enough to answer "is this still the same
/// process I saw before, or did its pid get recycled?" — a bare pid can't
/// answer that on its own.
///
/// **Why this matters**: pid-only liveness (`kill(pid, 0) == 0`) has a
/// latent failure mode on long-running systems — if the holder crashes and
/// some unrelated new process happens to be assigned the same pid before
/// the next check, pid-only liveness reports "still alive" and the lock is
/// refused forever (until someone manually deletes the lock file). Not a
/// theoretical risk: pid space wraps around on any system that's been
/// running long enough. Comparing the recorded start time against the
/// current holder of that pid closes the gap — a reused pid almost never
/// has the exact same start time as whatever held it before.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub started_at: i64,
}

impl ProcessIdentity {
    pub fn current() -> Self {
        let pid = std::process::id();
        Self {
            pid,
            started_at: process_start_time(pid).unwrap_or(0),
        }
    }

    /// Start times within this many seconds of each other count as "the
    /// same process" — `process_start_time` samples `ps -o etime=`
    /// (whole-second resolution), so querying the very same still-running
    /// process twice a moment apart can legitimately drift by ~1s as the
    /// wall clock ticks over between the two `now - elapsed` computations.
    /// A real pid reuse lands nowhere near this close by comparison.
    const START_TIME_TOLERANCE_SECS: i64 = 2;

    /// Four-way liveness/reuse check against a *recorded* identity (e.g.
    /// one read back from a lock file):
    /// - `Alive` — `pid` is running and its start time still matches
    ///   (within [`START_TIME_TOLERANCE_SECS`]); the recorded holder is
    ///   genuinely still there.
    /// - `Reused` — `pid` is running but its start time no longer matches
    ///   the recorded one — a different process now holds this pid.
    /// - `Dead` — `pid` isn't running at all.
    /// - `Unknown` — liveness or start time couldn't be determined at all
    ///   (unsupported platform, `ps` unavailable, or the recorded value
    ///   itself was never captured — `started_at == 0`). Distinct from
    ///   `Reused` on purpose: "can't tell" and "confirmed a different
    ///   process" call for different caller behavior (see
    ///   [`decide_stale_lock`]) — collapsing them would mean an
    ///   unverifiable lock gets silently reclaimed out from under a holder
    ///   that may well still be alive.
    pub fn check(recorded: &ProcessIdentity) -> ProcessLiveness {
        match pid_alive(recorded.pid) {
            Some(false) => return ProcessLiveness::Dead,
            None => return ProcessLiveness::Unknown,
            Some(true) => {}
        }
        if recorded.started_at == 0 {
            return ProcessLiveness::Unknown;
        }
        match process_start_time(recorded.pid) {
            Some(current_start)
                if (current_start - recorded.started_at).abs()
                    <= Self::START_TIME_TOLERANCE_SECS =>
            {
                ProcessLiveness::Alive
            }
            Some(_) => ProcessLiveness::Reused,
            None => ProcessLiveness::Unknown,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessLiveness {
    Alive,
    Reused,
    Dead,
    Unknown,
}

/// What a caller weighing whether to reclaim a stale lock/discovery file
/// should do, given a [`ProcessLiveness`] — shared by `daemon::discovery`
/// and `team::lock` so their accept/reclaim decisions and log messages
/// can't drift apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaleLockDecision {
    /// The recorded holder might still be there — confirmed alive, or
    /// unconfirmed and therefore assumed alive out of caution. Don't touch
    /// the existing file.
    KeepExisting,
    /// The recorded holder is confirmed gone (dead pid, or the pid now
    /// belongs to someone else) — safe to overwrite.
    Reclaim,
}

/// Runs [`ProcessIdentity::check`] against `recorded` and logs the decision
/// via `tracing`, so call sites just branch on the result instead of each
/// re-matching all four [`ProcessLiveness`] variants and re-writing the same
/// log lines. `subject` names what's being checked (e.g. `"daemon.lock"`)
/// and `path` is where it lives on disk — both go into the log for
/// post-mortem debugging.
pub fn decide_stale_lock(
    recorded: &ProcessIdentity,
    subject: &str,
    path: &Path,
) -> StaleLockDecision {
    match ProcessIdentity::check(recorded) {
        ProcessLiveness::Alive => StaleLockDecision::KeepExisting,
        ProcessLiveness::Unknown => {
            tracing::warn!(
                pid = recorded.pid,
                path = %path.display(),
                "could not confirm whether {subject}'s recorded pid is still alive \
                 (no reliable liveness probe on this platform/environment, or its \
                 start time was never captured); conservatively treating it as still held"
            );
            StaleLockDecision::KeepExisting
        }
        ProcessLiveness::Reused => {
            tracing::warn!(
                stale_pid = recorded.pid,
                path = %path.display(),
                "{subject}'s pid was reused by an unrelated process (start time mismatch); overwriting"
            );
            StaleLockDecision::Reclaim
        }
        ProcessLiveness::Dead => {
            tracing::info!(
                stale_pid = recorded.pid,
                path = %path.display(),
                "stale {subject} from dead pid; overwriting"
            );
            StaleLockDecision::Reclaim
        }
    }
}

/// Best-effort liveness probe — `kill -0 pid` semantics on Unix.
/// `Some(true)`/`Some(false)` are a confirmed answer; `None` means the
/// platform has no probe wired up here (see the `cfg(not(unix))` arm) —
/// callers must not treat that as "alive" (see [`ProcessLiveness::Unknown`]).
#[cfg(unix)]
#[allow(unsafe_code)]
fn pid_alive(pid: u32) -> Option<bool> {
    let r = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if r == 0 {
        return Some(true);
    }
    let err = std::io::Error::last_os_error();
    Some(err.raw_os_error() != Some(libc::ESRCH))
}

#[cfg(not(unix))]
fn pid_alive(_pid: u32) -> Option<bool> {
    None
}

/// `pid`'s process start time as Unix epoch seconds (rounded to the nearest
/// second — see [`ProcessIdentity::check`]'s tolerance for why sub-second
/// precision isn't needed), or `None` if it can't be determined (process
/// gone by the time we looked, `ps` unavailable, or a read/parse failure —
/// all treated the same by callers: "couldn't verify, can't use this for
/// the reuse check").
///
/// Deliberately shells out to `ps -o etime=` rather than hand-rolling the
/// `sysctl(KERN_PROC_PID)`/`kinfo_proc` FFI: `libc` doesn't expose
/// `kinfo_proc`'s layout for macOS (only for the free/net/openbsd targets),
/// so getting that struct's field offsets right would mean hand-maintaining
/// an unsafe `#[repr(C)]` mirror of an Apple-internal struct with no
/// compiler to check it against — a correctness risk `ps`'s stable,
/// documented output doesn't carry.
///
/// `etime` (not GNU/Linux procps's `etimes` extension, which macOS's `ps`
/// rejects outright — `ps: etimes: keyword not found`) prints elapsed time
/// as `[[dd-]hh:]mm:ss`, a purely numeric, locale- and timezone-independent
/// format supported by both, so one parser (`parse_elapsed`) covers both
/// rather than two platform-specific implementations.
///
/// Unix-only: there's no `ps -o etime=` equivalent on other platforms.
/// Gated with `cfg` rather than left to fail at runtime so a non-Unix build
/// never shells out to a command that can't exist there — see the
/// `cfg(not(unix))` arm just below.
#[cfg(unix)]
pub fn process_start_time(pid: u32) -> Option<i64> {
    let output = std::process::Command::new("ps")
        .args(["-o", "etime=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let elapsed_secs = parse_elapsed(String::from_utf8(output.stdout).ok()?.trim())?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs() as i64;
    plausible(now - elapsed_secs)
}

/// A start time that could not be one is not a reading.
///
/// The value here is derived — `now` minus whatever `ps` said — so a `ps` this
/// code does not understand produces a number rather than an error. One Linux
/// host answered `441056556-16:45:20`, four hundred million days, and the
/// subtraction dutifully placed the process's start a million years before the
/// epoch. Taken at face value that is not a harmless oddity: every recorded
/// identity then fails the reuse check, so instance files are deleted out from
/// under live daemons and locks are reclaimed from processes still holding
/// them. "Could not verify" is a state this code already has, and it is the
/// honest one here — `None` becomes `ProcessLiveness::Unknown`, which callers
/// treat as "leave it alone".
///
/// The window is deliberately loose. It is not trying to validate a start
/// time; it is refusing an answer that is not one.
#[cfg(unix)]
fn plausible(start: i64) -> Option<i64> {
    /// 2000-01-01. No process this code meets started before it.
    const FLOOR: i64 = 946_684_800;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs() as i64;
    // A little slack above `now` for clock adjustment between the two reads.
    (FLOOR..=now + 60).contains(&start).then_some(start)
}

#[cfg(not(unix))]
pub fn process_start_time(_pid: u32) -> Option<i64> {
    None
}

/// Parses `ps -o etime=`'s `[[dd-]hh:]mm:ss` elapsed-time format into total
/// seconds.
#[cfg(unix)]
fn parse_elapsed(s: &str) -> Option<i64> {
    let (days, rest) = match s.split_once('-') {
        Some((d, r)) => (d.parse::<i64>().ok()?, r),
        None => (0, s),
    };
    let parts: Vec<&str> = rest.split(':').collect();
    let (hours, minutes, seconds) = match parts.as_slice() {
        [h, m, s] => (
            h.parse::<i64>().ok()?,
            m.parse::<i64>().ok()?,
            s.parse::<i64>().ok()?,
        ),
        [m, s] => (0, m.parse::<i64>().ok()?, s.parse::<i64>().ok()?),
        _ => return None,
    };
    Some(days * 86400 + hours * 3600 + minutes * 60 + seconds)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `ps` this code cannot read produces a number, not an error, and the
    /// number is worse than nothing: every identity built on it fails the
    /// reuse check, so live daemons lose their instance files and running
    /// processes lose their locks.
    #[test]
    fn a_start_time_that_could_not_be_one_is_not_a_reading() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        assert_eq!(
            plausible(now - 3600),
            Some(now - 3600),
            "an hour ago is fine"
        );
        assert_eq!(plausible(now), Some(now), "just now is fine");

        // What the Linux host that found this actually produced.
        assert_eq!(plausible(-38_107_284_710_668), None);
        assert_eq!(plausible(0), None, "the epoch is not a process start time");
        assert_eq!(plausible(now + 86_400), None, "nor is tomorrow");
    }

    /// A short-lived child process — gives tests a real, distinct, live pid
    /// (with its own real start time) instead of reusing the test's own,
    /// which would trip the "same pid" branch instead of the "another
    /// process" branch these tests mean to exercise.
    struct Child(std::process::Child);
    impl Child {
        fn spawn() -> Self {
            Self(
                std::process::Command::new("sleep")
                    .arg("5")
                    .spawn()
                    .expect("spawn a short-lived child for the test"),
            )
        }
        fn pid(&self) -> u32 {
            self.0.id()
        }
    }
    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn process_identity_check_reports_alive_for_the_current_process() {
        let identity = ProcessIdentity::current();
        assert_eq!(ProcessIdentity::check(&identity), ProcessLiveness::Alive);
    }

    #[test]
    fn process_identity_check_reports_dead_for_an_unused_pid() {
        let fake = ProcessIdentity {
            pid: 999_999_999,
            started_at: 1,
        };
        assert_eq!(ProcessIdentity::check(&fake), ProcessLiveness::Dead);
    }

    #[test]
    fn process_identity_check_reports_reused_when_start_time_mismatches() {
        let mut identity = ProcessIdentity::current();
        identity.started_at += 1_000_000; // clearly wrong, well outside tolerance
        assert_eq!(ProcessIdentity::check(&identity), ProcessLiveness::Reused);
    }

    /// `Alive` is the verdict for a process whose start time still matches —
    /// which presumes the host can read one at all. Where it cannot, `check`
    /// answers `Unknown` by design, and that is a different case with its own
    /// test rather than a weaker version of this one.
    #[test]
    fn process_identity_check_reports_alive_for_a_real_child_with_matching_start_time() {
        let child = Child::spawn();
        let Some(recorded_start) = process_start_time(child.pid()) else {
            eprintln!("skipping: this host cannot read process start times");
            return;
        };
        let recorded = ProcessIdentity {
            pid: child.pid(),
            started_at: recorded_start,
        };
        assert_eq!(ProcessIdentity::check(&recorded), ProcessLiveness::Alive);
    }

    /// And that different case: no readable start time is "cannot verify", not
    /// "a different process". A host whose `ps` this code cannot read must not
    /// have its locks reclaimed and its instance files deleted.
    #[test]
    fn an_unreadable_start_time_is_unknown_rather_than_reused() {
        let child = Child::spawn();
        let recorded = ProcessIdentity {
            pid: child.pid(),
            started_at: 0,
        };
        assert_eq!(ProcessIdentity::check(&recorded), ProcessLiveness::Unknown);
    }

    #[test]
    fn process_identity_check_reports_reused_for_a_real_pid_with_mismatched_start_time() {
        let child = Child::spawn();
        let recorded = ProcessIdentity {
            pid: child.pid(),
            started_at: 12345, // deliberately wrong — simulates reuse
        };
        assert_eq!(ProcessIdentity::check(&recorded), ProcessLiveness::Reused);
    }

    #[test]
    fn process_identity_check_reports_unknown_when_recorded_start_time_was_never_captured() {
        // `started_at == 0` is what `ProcessIdentity::current()` records when
        // `process_start_time` itself failed to probe — an unverifiable
        // record, not a confirmed mismatch, so this must not read as `Reused`.
        let mut identity = ProcessIdentity::current();
        identity.started_at = 0;
        assert_eq!(ProcessIdentity::check(&identity), ProcessLiveness::Unknown);
    }

    #[test]
    fn decide_stale_lock_keeps_existing_for_a_confirmed_alive_process() {
        let identity = ProcessIdentity::current();
        assert_eq!(
            decide_stale_lock(&identity, "test.lock", Path::new("/tmp/test.lock")),
            StaleLockDecision::KeepExisting
        );
    }

    #[test]
    fn decide_stale_lock_keeps_existing_when_liveness_is_unverifiable() {
        let mut identity = ProcessIdentity::current();
        identity.started_at = 0;
        assert_eq!(
            decide_stale_lock(&identity, "test.lock", Path::new("/tmp/test.lock")),
            StaleLockDecision::KeepExisting
        );
    }

    #[test]
    fn decide_stale_lock_reclaims_a_dead_pid() {
        let fake = ProcessIdentity {
            pid: 999_999_999,
            started_at: 1,
        };
        assert_eq!(
            decide_stale_lock(&fake, "test.lock", Path::new("/tmp/test.lock")),
            StaleLockDecision::Reclaim
        );
    }

    #[test]
    fn decide_stale_lock_reclaims_a_reused_pid() {
        let mut identity = ProcessIdentity::current();
        identity.started_at += 1_000_000; // clearly wrong, well outside tolerance
        assert_eq!(
            decide_stale_lock(&identity, "test.lock", Path::new("/tmp/test.lock")),
            StaleLockDecision::Reclaim
        );
    }

    #[test]
    #[cfg(unix)]
    fn parse_elapsed_handles_every_ps_etime_shape() {
        assert_eq!(parse_elapsed("00:05"), Some(5));
        assert_eq!(parse_elapsed("02:30"), Some(150));
        assert_eq!(parse_elapsed("01:02:03"), Some(3723));
        assert_eq!(parse_elapsed("3-01:02:03"), Some(3 * 86400 + 3723));
        assert_eq!(parse_elapsed("not-a-time"), None);
        assert_eq!(parse_elapsed(""), None);
    }
}
