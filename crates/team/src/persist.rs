//! On-disk team formats: `team.json` (write-once declaration), `state.json` (atomic
//! rewrite on every lifecycle change), `events.jsonl` (append-only, the
//! authoritative record a future index rebuild replays).
//!
//! `team.json`/`state.json` are split for exactly the reason the design doc
//! gives: a lifecycle change (a member finishing a stage) is frequent and
//! must not force rewriting the immutable declaration (stage list, prompts)
//! every time.
//!
//! Not yet the *only* place a team's state lives — `registry::TeamRegistry`
//! remains the live, per-session, in-memory source of truth read by
//! `TeamList`/`TeamDelete` today (project-level promotion, §6.6, is
//! deferred). What's here is the write path that lands these three files
//! next to it, so a future `TeamStore` has correctly-shaped data to read
//! without a migration step.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub const CURRENT_TEAM_SCHEMA_VERSION: u32 = 1;
pub const CURRENT_STATE_SCHEMA_VERSION: u32 = 1;

// ── team.json — write once ──────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamMode {
    Batch,
    Persistent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemberDeclaration {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    pub prompt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageDeclaration {
    pub index: usize,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aggregate: Option<String>,
    pub members: Vec<MemberDeclaration>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamDeclaration {
    #[serde(default = "default_team_schema_version")]
    pub schema_version: u32,
    pub team_id: String,
    pub name: String,
    pub scene: String,
    pub project_root: String,
    pub owner_session_id: String,
    pub created_at: String,
    pub mode: TeamMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    pub stages: Vec<StageDeclaration>,
}

fn default_team_schema_version() -> u32 {
    CURRENT_TEAM_SCHEMA_VERSION
}

/// Write `team.json`. Creates `team_dir` if needed. Called once, at team
/// creation — a second call for the same team would silently overwrite a
/// declaration that's supposed to be immutable, so callers must not call
/// this more than once per `team_id`. Runs the blocking filesystem work on
/// `spawn_blocking` so callers don't need to wrap every call site
/// themselves.
pub async fn write_team_declaration(team_dir: &Path, decl: &TeamDeclaration) -> io::Result<()> {
    let team_dir = team_dir.to_path_buf();
    let decl = decl.clone();
    run_blocking(move || write_team_declaration_sync(&team_dir, &decl)).await
}

fn write_team_declaration_sync(team_dir: &Path, decl: &TeamDeclaration) -> io::Result<()> {
    fs::create_dir_all(team_dir)?;
    let body = serde_json::to_string_pretty(decl).map_err(json_to_io)?;
    write_atomic(team_dir, "team.json", &body)
}

pub fn read_team_declaration(team_dir: &Path) -> io::Result<Option<TeamDeclaration>> {
    read_json_if_exists(&team_dir.join("team.json"))
}

// ── state.json — atomic rewrite on every lifecycle change ──────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
    /// Daemon restarted while this team was `Running` with no matching live
    /// `AgentRef` — set by the (deferred) index-rebuild path, not written
    /// from here.
    Interrupted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemberState {
    pub label: String,
    pub lifecycle: crate::coordinator::TeammateLifecycle,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_since: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamStateFile {
    #[serde(default = "default_state_schema_version")]
    pub schema_version: u32,
    pub updated_at: String,
    pub current_stage: usize,
    pub status: TeamStatus,
    pub members: Vec<MemberState>,
}

fn default_state_schema_version() -> u32 {
    CURRENT_STATE_SCHEMA_VERSION
}

/// Atomically rewrite `state.json` (temp file + rename) so a reader never
/// observes a half-written file. Runs on `spawn_blocking` — see
/// [`write_team_declaration`]'s doc comment for why.
pub async fn write_team_state_atomic(team_dir: &Path, state: &TeamStateFile) -> io::Result<()> {
    let team_dir = team_dir.to_path_buf();
    let state = state.clone();
    run_blocking(move || write_team_state_atomic_sync(&team_dir, &state)).await
}

fn write_team_state_atomic_sync(team_dir: &Path, state: &TeamStateFile) -> io::Result<()> {
    fs::create_dir_all(team_dir)?;
    let body = serde_json::to_string_pretty(state).map_err(json_to_io)?;
    write_atomic(team_dir, "state.json", &body)
}

pub fn read_team_state(team_dir: &Path) -> io::Result<Option<TeamStateFile>> {
    read_json_if_exists(&team_dir.join("state.json"))
}

// ── events.jsonl — append-only ──────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TeamEvent {
    TeamCreated {
        team_id: String,
    },
    StageStarted {
        index: usize,
        name: String,
        members: Vec<String>,
    },
    MemberSpawned {
        label: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
    },
    MemberCompleted {
        label: String,
        is_error: bool,
    },
    StageCompleted {
        index: usize,
        failed: Vec<String>,
    },
    TeamCompleted {
        status: TeamStatus,
    },
    /// Written in place of the entries it drops when `append_team_event`'s
    /// retention check truncates the file — an auditor reading
    /// `events.jsonl` from the top sees *why* the record doesn't go back
    /// further, instead of silently losing history.
    Truncated {
        dropped: usize,
        reason: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub ts: String,
    #[serde(flatten)]
    pub event: TeamEvent,
}

#[derive(Debug, Clone, Copy)]
pub struct EventsRetention {
    pub max_entries: usize,
}

impl Default for EventsRetention {
    /// `settings.execution.team_retention.events_max_entries`'s default —
    /// see the design doc §6.4.
    fn default() -> Self {
        Self { max_entries: 5000 }
    }
}

/// Append one event to `events.jsonl`, then apply retention. The design
/// doc's §6.4 calls for checking every *N* writes rather than every write
/// (to avoid a full-file line count on the hot path); this checks every
/// write instead — simpler and still correct, at the cost of doing that
/// line count more often than strictly necessary. Once a truncation fires,
/// [`maybe_truncate_events`] cuts well below `max_entries` rather than
/// exactly down to it, so this per-write line count is cheap (short file)
/// far more often than it is expensive (full read + rewrite) — see that
/// function's doc comment. Runs on `spawn_blocking` — see
/// [`write_team_declaration`]'s doc comment for why.
pub async fn append_team_event(
    team_dir: &Path,
    event: TeamEvent,
    retention: EventsRetention,
) -> io::Result<()> {
    let team_dir = team_dir.to_path_buf();
    run_blocking(move || append_team_event_sync(&team_dir, event, retention)).await
}

fn append_team_event_sync(
    team_dir: &Path,
    event: TeamEvent,
    retention: EventsRetention,
) -> io::Result<()> {
    fs::create_dir_all(team_dir)?;
    let path = team_dir.join("events.jsonl");
    let envelope = EventEnvelope {
        ts: iso_now(),
        event,
    };
    let mut line = serde_json::to_string(&envelope).map_err(json_to_io)?;
    line.push('\n');
    {
        use std::io::Write;
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        f.write_all(line.as_bytes())?;
    }
    maybe_truncate_events(&path, retention)
}

/// When the file exceeds `max_entries`, cut down to `TRUNCATE_TARGET_RATIO`
/// of `max_entries` instead of exactly `max_entries` — landing exactly on
/// the limit would make the very next append cross it again, turning every
/// single append after the first truncation into another full
/// read+rewrite. Cutting further below buys headroom: a batch of appends
/// has to accumulate again before the next full rewrite is needed.
const TRUNCATE_TARGET_RATIO: usize = 80; // percent

fn maybe_truncate_events(path: &Path, retention: EventsRetention) -> io::Result<()> {
    let content = fs::read_to_string(path)?;
    let lines: Vec<&str> = content.lines().collect();
    if lines.len() <= retention.max_entries {
        return Ok(());
    }
    let target = (retention.max_entries * TRUNCATE_TARGET_RATIO / 100).max(1);
    let dropped = lines.len() - target;
    let kept = &lines[dropped..];
    let marker = EventEnvelope {
        ts: iso_now(),
        event: TeamEvent::Truncated {
            dropped,
            reason: "max_entries".to_string(),
        },
    };
    let mut new_content = serde_json::to_string(&marker).map_err(json_to_io)?;
    new_content.push('\n');
    for l in kept {
        new_content.push_str(l);
        new_content.push('\n');
    }
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let filename = path
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or("events.jsonl");
    write_atomic(dir, filename, &new_content)
}

/// Reads and parses every line of `events.jsonl`. A line that fails to
/// parse (e.g. a half-written line left by a process killed mid-`write_all`)
/// is skipped — logged, not fatal — so one corrupt line doesn't take down a
/// team's entire event history.
pub fn read_events(team_dir: &Path) -> io::Result<Vec<EventEnvelope>> {
    let path = team_dir.join("events.jsonl");
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = fs::read_to_string(&path)?;
    let mut out = Vec::new();
    for (i, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str(line) {
            Ok(envelope) => out.push(envelope),
            Err(e) => {
                tracing::warn!(
                    team_dir = %team_dir.display(),
                    line_number = i + 1,
                    error = %e,
                    "skipping unparseable line in events.jsonl"
                );
            }
        }
    }
    Ok(out)
}

// ── shared helpers ───────────────────────────────────────────────────────

/// Write `body` to `dir/filename` via temp-file-then-rename so a concurrent
/// reader never observes a partially-written file.
fn write_atomic(dir: &Path, filename: &str, body: &str) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let path = dir.join(filename);
    let tmp = dir.join(format!("{filename}.tmp-{}", std::process::id()));
    fs::write(&tmp, body)?;
    fs::rename(&tmp, &path)
}

/// Run blocking filesystem work off the async executor. `spawn_blocking`
/// only fails if the closure panics (`JoinError`) — surfaced as an `Other`
/// io error rather than propagating the panic, since a poisoned write
/// shouldn't take the caller's task down with it.
async fn run_blocking<F>(f: F) -> io::Result<()>
where
    F: FnOnce() -> io::Result<()> + Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(result) => result,
        Err(join_err) => Err(io::Error::other(format!(
            "blocking team-persist task panicked: {join_err}"
        ))),
    }
}

fn read_json_if_exists<T: serde::de::DeserializeOwned>(path: &Path) -> io::Result<Option<T>> {
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(path)?;
    serde_json::from_slice(&bytes).map(Some).map_err(json_to_io)
}

fn json_to_io(e: serde_json::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e)
}

pub(crate) fn iso_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    time::OffsetDateTime::from_unix_timestamp(secs as i64)
        .ok()
        .and_then(|t| {
            t.format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn declaration() -> TeamDeclaration {
        TeamDeclaration {
            schema_version: CURRENT_TEAM_SCHEMA_VERSION,
            team_id: "team-refactor-1".into(),
            name: "refactor".into(),
            scene: "code".into(),
            project_root: "/repo".into(),
            owner_session_id: "S1".into(),
            created_at: iso_now(),
            mode: TeamMode::Batch,
            permission_mode: Some("plan".into()),
            stages: vec![StageDeclaration {
                index: 0,
                name: "explore".into(),
                aggregate: Some("concat".into()),
                members: vec![MemberDeclaration {
                    label: "a".into(),
                    agent_type: Some("Explore".into()),
                    prompt: "look around".into(),
                }],
            }],
        }
    }

    #[tokio::test]
    async fn team_declaration_round_trips_through_disk() {
        let dir = TempDir::new().unwrap();
        let team_dir = dir.path().join("team-refactor-1");
        write_team_declaration(&team_dir, &declaration())
            .await
            .unwrap();
        let read = read_team_declaration(&team_dir).unwrap().unwrap();
        assert_eq!(read.team_id, "team-refactor-1");
        assert_eq!(read.stages.len(), 1);
        assert_eq!(read.stages[0].members[0].label, "a");
    }

    #[test]
    fn missing_team_declaration_reads_as_none() {
        let dir = TempDir::new().unwrap();
        assert!(read_team_declaration(dir.path()).unwrap().is_none());
    }

    /// Exercises the normal (non-crash) path of the tmp+rename write: no
    /// leftover `.tmp-*` file survives a successful write, and the final
    /// `team.json` reads back correctly. Actually interrupting the process
    /// mid-rename to prove atomicity isn't practical to assert in a unit
    /// test — `fs::rename` within the same directory being atomic is an OS
    /// guarantee, not something this test can independently verify; this
    /// test instead documents and checks the write's *observable* contract.
    #[tokio::test]
    async fn team_declaration_write_is_atomic_tmp_then_rename() {
        let dir = TempDir::new().unwrap();
        let team_dir = dir.path().join("team-refactor-1");
        write_team_declaration(&team_dir, &declaration())
            .await
            .unwrap();

        let leftover_tmp = fs::read_dir(&team_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().contains(".tmp-"));
        assert!(
            !leftover_tmp,
            "a successful atomic write must not leave a temp file behind"
        );
        let read = read_team_declaration(&team_dir).unwrap().unwrap();
        assert_eq!(read.team_id, "team-refactor-1");
    }

    #[tokio::test]
    async fn team_state_atomic_write_round_trips_and_overwrites() {
        let dir = TempDir::new().unwrap();
        let team_dir = dir.path().join("team-refactor-1");
        let state = TeamStateFile {
            schema_version: CURRENT_STATE_SCHEMA_VERSION,
            updated_at: iso_now(),
            current_stage: 0,
            status: TeamStatus::Running,
            members: vec![MemberState {
                label: "a".into(),
                lifecycle: crate::coordinator::TeammateLifecycle::Active,
                agent_id: Some("ag_1".into()),
                session_id: Some("S1c1".into()),
                idle_since: None,
                last_error: None,
            }],
        };
        write_team_state_atomic(&team_dir, &state).await.unwrap();
        let read = read_team_state(&team_dir).unwrap().unwrap();
        assert_eq!(read.status, TeamStatus::Running);
        assert_eq!(
            read.members[0].lifecycle,
            crate::coordinator::TeammateLifecycle::Active
        );

        let mut updated = state;
        updated.status = TeamStatus::Completed;
        updated.members[0].lifecycle = crate::coordinator::TeammateLifecycle::Completed;
        write_team_state_atomic(&team_dir, &updated).await.unwrap();
        let read_again = read_team_state(&team_dir).unwrap().unwrap();
        assert_eq!(read_again.status, TeamStatus::Completed);
    }

    #[test]
    fn missing_team_state_reads_as_none() {
        let dir = TempDir::new().unwrap();
        assert!(read_team_state(dir.path()).unwrap().is_none());
    }

    #[tokio::test]
    async fn events_append_in_order_and_round_trip() {
        let dir = TempDir::new().unwrap();
        let team_dir = dir.path().join("team-refactor-1");
        let retention = EventsRetention::default();
        append_team_event(
            &team_dir,
            TeamEvent::TeamCreated {
                team_id: "team-refactor-1".into(),
            },
            retention,
        )
        .await
        .unwrap();
        append_team_event(
            &team_dir,
            TeamEvent::MemberSpawned {
                label: "a".into(),
                agent_id: Some("ag_1".into()),
                session_id: Some("S1c1".into()),
            },
            retention,
        )
        .await
        .unwrap();

        let events = read_events(&team_dir).unwrap();
        assert_eq!(events.len(), 2);
        match &events[0].event {
            TeamEvent::TeamCreated { team_id } => assert_eq!(team_id, "team-refactor-1"),
            other => panic!("expected TeamCreated, got {other:?}"),
        }
        match &events[1].event {
            TeamEvent::MemberSpawned { label, .. } => assert_eq!(label, "a"),
            other => panic!("expected MemberSpawned, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn events_truncate_once_max_entries_is_exceeded_and_leave_a_marker() {
        let dir = TempDir::new().unwrap();
        let team_dir = dir.path().join("team-refactor-1");
        let retention = EventsRetention { max_entries: 3 };
        for i in 0..5 {
            append_team_event(
                &team_dir,
                TeamEvent::MemberCompleted {
                    label: format!("m{i}"),
                    is_error: false,
                },
                retention,
            )
            .await
            .unwrap();
        }

        let events = read_events(&team_dir).unwrap();
        // Truncation targets 80% of max_entries (2, for max_entries=3), so
        // only the marker + the 2 most recent entries survive.
        assert_eq!(events.len(), 3);
        match &events[0].event {
            TeamEvent::Truncated { dropped, reason } => {
                assert_eq!(*dropped, 2);
                assert_eq!(reason, "max_entries");
            }
            other => panic!("expected Truncated marker first, got {other:?}"),
        }
        // The most recent entries are the ones kept.
        match &events[2].event {
            TeamEvent::MemberCompleted { label, .. } => assert_eq!(label, "m4"),
            other => panic!("expected MemberCompleted(m4) last, got {other:?}"),
        }
    }

    /// §6.4's cost model: truncating down to 80% of `max_entries` (rather
    /// than exactly `max_entries`) must leave enough headroom that the very
    /// next append doesn't immediately trigger another full read+rewrite.
    #[tokio::test]
    async fn truncation_leaves_headroom_so_the_next_append_does_not_immediately_retrigger() {
        let dir = TempDir::new().unwrap();
        let team_dir = dir.path().join("team-refactor-1");
        let retention = EventsRetention { max_entries: 10 };
        for i in 0..11 {
            append_team_event(
                &team_dir,
                TeamEvent::MemberCompleted {
                    label: format!("m{i}"),
                    is_error: false,
                },
                retention,
            )
            .await
            .unwrap();
        }

        let events = read_events(&team_dir).unwrap();
        let truncations = events
            .iter()
            .filter(|e| matches!(e.event, TeamEvent::Truncated { .. }))
            .count();
        assert_eq!(
            truncations, 1,
            "expected exactly one truncation right after crossing max_entries"
        );

        // One more append lands right at (not over) max_entries again —
        // it must not force a second truncation immediately.
        append_team_event(
            &team_dir,
            TeamEvent::MemberCompleted {
                label: "m11".into(),
                is_error: false,
            },
            retention,
        )
        .await
        .unwrap();

        let events_after = read_events(&team_dir).unwrap();
        let truncations_after = events_after
            .iter()
            .filter(|e| matches!(e.event, TeamEvent::Truncated { .. }))
            .count();
        assert_eq!(
            truncations_after, 1,
            "the append right after a truncation must not immediately retrigger a full rewrite"
        );
    }

    #[test]
    fn missing_events_file_reads_as_empty() {
        let dir = TempDir::new().unwrap();
        assert!(read_events(dir.path()).unwrap().is_empty());
    }

    /// A half-written / corrupted line (e.g. left by a process killed
    /// mid-`write_all`) must not take down the whole read — the events
    /// before and after it should still come back.
    #[test]
    fn read_events_skips_a_corrupted_line_and_keeps_the_rest() {
        let dir = TempDir::new().unwrap();
        let team_dir = dir.path().join("team-refactor-1");
        fs::create_dir_all(&team_dir).unwrap();
        let good1 = serde_json::to_string(&EventEnvelope {
            ts: iso_now(),
            event: TeamEvent::TeamCreated {
                team_id: "t1".into(),
            },
        })
        .unwrap();
        let good2 = serde_json::to_string(&EventEnvelope {
            ts: iso_now(),
            event: TeamEvent::MemberCompleted {
                label: "a".into(),
                is_error: false,
            },
        })
        .unwrap();
        let content = format!("{good1}\n{{not valid json\n{good2}\n");
        fs::write(team_dir.join("events.jsonl"), content).unwrap();

        let events = read_events(&team_dir).unwrap();
        assert_eq!(
            events.len(),
            2,
            "the corrupted middle line should be skipped, not abort the whole read"
        );
        match &events[0].event {
            TeamEvent::TeamCreated { team_id } => assert_eq!(team_id, "t1"),
            other => panic!("expected TeamCreated, got {other:?}"),
        }
        match &events[1].event {
            TeamEvent::MemberCompleted { label, .. } => assert_eq!(label, "a"),
            other => panic!("expected MemberCompleted, got {other:?}"),
        }
    }
}
