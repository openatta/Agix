//! One-time relocation of session state left behind by the older layouts.
//!
//! Two things moved:
//!
//! - transcripts, from `<data>/sessions/<sanitized-cwd>/` to
//!   `<data>/projects/<sanitized-cwd>/` — they are partitioned by project, and
//!   sharing `sessions/` with the session-id-keyed sidecars meant that one
//!   directory held two different naming schemes;
//! - sidecars, from `<data>/code/sessions/<session-id>/` to
//!   `<data>/sessions/<session-id>/` — the `code` segment was a product name
//!   written into the path, and it put the history crate's idea of the data
//!   root one level below everyone else's.
//!
//! Skipping this would not corrupt anything, it would just orphan every
//! existing session: the transcripts stay on disk and nothing looks for them
//! there again.
//!
//! Deliberately conservative. Entries are **moved, never merged or deleted**,
//! and a destination that already exists is left alone — a half-finished run,
//! or a user who moved something by hand, must not lose data to a second
//! attempt. Everything it does is logged.

use std::path::Path;

/// What a migration pass did. Every count is directories, not files.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MigrationReport {
    /// Project transcript directories moved into `projects/`.
    pub transcripts_moved: usize,
    /// Session sidecar directories moved out of the old `code/` root.
    pub sidecars_moved: usize,
    /// Entries left in place because the destination already existed.
    pub skipped_existing: usize,
    /// Entries left in place because the move itself failed.
    pub failed: usize,
}

impl MigrationReport {
    pub fn did_nothing(&self) -> bool {
        *self == Self::default()
    }
}

/// A directory name is a session id if it parses as one.
///
/// This is what separates the two kinds of entry sharing the old `sessions/`
/// root: sidecar directories are named by session id, transcript directories
/// by sanitized cwd. Sanitizing replaces every non-alphanumeric byte with `-`,
/// and an absolute path always starts with a separator, so a transcript
/// directory begins with `-` and cannot parse as a base58 id.
fn is_session_id(name: &str) -> bool {
    base::session::SessionId::parse(name).is_ok()
}

/// Move `from` to `to`, reporting which bucket the outcome belongs in.
fn move_dir(
    from: &Path,
    to: &Path,
    report: &mut MigrationReport,
    moved: impl Fn(&mut MigrationReport),
) {
    if to.exists() {
        tracing::debug!(from = %from.display(), to = %to.display(), "migration: destination exists, leaving the source alone");
        report.skipped_existing += 1;
        return;
    }
    if let Some(parent) = to.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            tracing::warn!(to = %to.display(), error = %e, "migration: cannot create destination parent");
            report.failed += 1;
            return;
        }
    }
    match std::fs::rename(from, to) {
        Ok(()) => {
            tracing::info!(from = %from.display(), to = %to.display(), "migration: moved");
            moved(report);
        }
        Err(e) => {
            tracing::warn!(from = %from.display(), to = %to.display(), error = %e, "migration: move failed, leaving the source in place");
            report.failed += 1;
        }
    }
}

/// Run both relocations against `data_root` (`~/.agix` unless overridden).
///
/// Idempotent: a second call over an already-migrated tree finds nothing to do
/// and reports `did_nothing()`. Safe to call unconditionally at startup.
pub fn migrate_layout(data_root: &Path) -> MigrationReport {
    let mut report = MigrationReport::default();
    let sessions = data_root.join("sessions");
    let projects = data_root.join("projects");
    let legacy_sidecars = data_root.join("code").join("sessions");

    // 1. Transcripts out of `sessions/`. Only entries that are *not* session
    //    ids — the sidecars sharing this root stay exactly where they are.
    if let Ok(entries) = std::fs::read_dir(&sessions) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if !entry.path().is_dir() || is_session_id(name) {
                continue;
            }
            move_dir(&entry.path(), &projects.join(name), &mut report, |r| {
                r.transcripts_moved += 1
            });
        }
    }

    // 2. Sidecars out of the old `code/sessions/` root.
    if let Ok(entries) = std::fs::read_dir(&legacy_sidecars) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if !entry.path().is_dir() {
                continue;
            }
            move_dir(&entry.path(), &sessions.join(name), &mut report, |r| {
                r.sidecars_moved += 1
            });
        }
    }

    // The emptied `code/` husk is left behind on purpose: removing a directory
    // this never wrote is not this function's call to make, and it may still
    // hold state some other component put there.
    if !report.did_nothing() {
        tracing::info!(
            transcripts = report.transcripts_moved,
            sidecars = report.sidecars_moved,
            skipped = report.skipped_existing,
            failed = report.failed,
            "session layout migration complete"
        );
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(p: &Path) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, b"x").unwrap();
    }

    fn a_session_id() -> String {
        base::session::SessionId::new().to_string()
    }

    #[test]
    fn separates_transcripts_from_sidecars_sharing_the_old_sessions_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let sid = a_session_id();

        // A transcript directory (sanitized cwd) and a sidecar directory
        // (session id), side by side — the layout this exists to untangle.
        touch(&root.join("sessions/-Users-me-work/abc.jsonl"));
        touch(&root.join("sessions").join(&sid).join("session_memory.md"));

        let r = migrate_layout(root);

        assert_eq!(r.transcripts_moved, 1);
        assert_eq!(r.sidecars_moved, 0, "already in the right place");
        assert!(root.join("projects/-Users-me-work/abc.jsonl").exists());
        assert!(!root.join("sessions/-Users-me-work").exists());
        assert!(
            root.join("sessions")
                .join(&sid)
                .join("session_memory.md")
                .exists(),
            "a sidecar keyed by session id must not be treated as a project"
        );
    }

    #[test]
    fn lifts_sidecars_out_of_the_legacy_code_segment() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let sid = a_session_id();
        touch(&root.join("code/sessions").join(&sid).join("metadata.json"));

        let r = migrate_layout(root);

        assert_eq!(r.sidecars_moved, 1);
        assert!(root
            .join("sessions")
            .join(&sid)
            .join("metadata.json")
            .exists());
    }

    #[test]
    fn is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(&root.join("sessions/-Users-me-work/abc.jsonl"));
        touch(
            &root
                .join("code/sessions")
                .join(a_session_id())
                .join("metadata.json"),
        );

        let first = migrate_layout(root);
        assert!(!first.did_nothing());

        let second = migrate_layout(root);
        assert!(
            second.did_nothing(),
            "second pass must find nothing: {second:?}"
        );
    }

    #[test]
    fn never_overwrites_an_existing_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(&root.join("sessions/-Users-me-work/old.jsonl"));
        touch(&root.join("projects/-Users-me-work/new.jsonl"));

        let r = migrate_layout(root);

        assert_eq!(r.transcripts_moved, 0);
        assert_eq!(r.skipped_existing, 1);
        assert!(
            root.join("sessions/-Users-me-work/old.jsonl").exists(),
            "the source must survive a skipped move"
        );
        assert!(root.join("projects/-Users-me-work/new.jsonl").exists());
    }

    #[test]
    fn a_clean_tree_is_a_no_op() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(migrate_layout(tmp.path()).did_nothing());
    }
}
