//! What a panicking daemon leaves behind: a crash record, and none of its
//! commands.
//!
//! Built with `panic = "abort"`, a panic ends the process on the spot — no
//! `Drop` runs, so the process groups its commands lead keep running with no
//! one to stop them. The hook installed here kills those groups first, then
//! writes a record of the panic and hands over to the previous hook. Built
//! with `unwind`, a panic may be one a task boundary catches while the daemon
//! carries on, so the groups are left to their owners and only the record is
//! written.

use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::orphans;

/// Installs the hook. Records go to `<dir>/<unix-seconds>-<pid>.txt`.
pub fn install(dir: PathBuf, program: &'static str, version: &'static str) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let killed = if cfg!(panic = "abort") {
            orphans::kill_all()
        } else {
            Vec::new()
        };
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| info.payload().downcast_ref::<String>().map(String::as_str))
            .unwrap_or("<non-string panic payload>");
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_default();
        let record = Record {
            program,
            version,
            message,
            location: &location,
            killed: &killed,
        };
        let _ = record.write(&dir);
        previous(info);
    }));
}

struct Record<'a> {
    program: &'a str,
    version: &'a str,
    message: &'a str,
    location: &'a str,
    killed: &'a [i32],
}

impl Record<'_> {
    fn write(&self, dir: &std::path::Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let pid = std::process::id();
        let mut f = std::fs::File::create(dir.join(format!("{secs}-{pid}.txt")))?;
        let thread = std::thread::current();
        writeln!(f, "{} {}", self.program, self.version)?;
        writeln!(f, "pid: {pid}")?;
        writeln!(f, "time: {secs}")?;
        writeln!(f, "thread: {}", thread.name().unwrap_or("<unnamed>"))?;
        writeln!(
            f,
            "panic strategy: {}",
            if cfg!(panic = "abort") {
                "abort"
            } else {
                "unwind"
            }
        )?;
        writeln!(f, "at: {}", self.location)?;
        writeln!(f, "message: {}", self.message)?;
        writeln!(f, "killed process groups: {:?}", self.killed)?;
        writeln!(f)?;
        writeln!(f, "{}", std::backtrace::Backtrace::force_capture())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_names_the_panic_and_where_it_happened() {
        let dir = tempfile::tempdir().unwrap();
        Record {
            program: "agixd",
            version: "0.0.0",
            message: "index out of bounds",
            location: "src/x.rs:1:2",
            killed: &[42],
        }
        .write(dir.path())
        .unwrap();
        let entry = std::fs::read_dir(dir.path())
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        let text = std::fs::read_to_string(entry.path()).unwrap();
        assert!(text.starts_with("agixd 0.0.0\n"), "{text}");
        assert!(text.contains("message: index out of bounds"), "{text}");
        assert!(text.contains("at: src/x.rs:1:2"), "{text}");
        assert!(text.contains("killed process groups: [42]"), "{text}");
    }
}
