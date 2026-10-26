//! Session persistence: the [`HistoryStore`] contract and the two
//! implementations that ship with the engine — one writing JSONL under a
//! project directory, one holding everything in memory.
//!
//! A `JsonlHistoryStore` serves **many projects**: each session's transcript
//! goes to the project it was created under (`Meta.project_root`), and the
//! cwd the store was built with is only the default for a session that never
//! said. Many sessions share one store, and writes within a session are
//! serialized by a mutex so two appends cannot interleave into a partial
//! line.

use crate::blob::{BlobRef, BlobStore, PasteStore};
use crate::entry::{EnvelopedEntry, LogEntry, SessionKind};
use crate::error::HistoryError;
use crate::path::{
    canonicalize_cwd, project_dir, project_dir_for, session_file, session_metadata_file,
    HistoryRoots,
};
use crate::project::SessionMetadata;
use crate::query::{SessionQuery, SessionScope};
use crate::transcript::{
    messages_match_query, preview_messages, project_messages_with, DefaultProjection,
    TranscriptProjection,
};
use async_trait::async_trait;
use base::message::{ContentBlock, ToolResultContent};
use base::session::SessionId;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub session_id: SessionId,
    pub last_modified: String,
    pub entry_count: usize,
    pub message_count: usize,
    pub preview: String,
    pub canonical_cwd: Option<String>,
    pub title: Option<String>,
    pub total_input_tokens: Option<u64>,
    pub total_output_tokens: Option<u64>,
    pub compact_count: u64,
}

/// What a session is: the facts fixed when it was created.
///
/// Distinct from [`SessionSummary`], which is about the conversation and
/// changes with every turn. These come from the log's `Meta` entry and never
/// change, which is why a listing can carry them and why nothing needs to
/// keep a second copy in step.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionFacts {
    /// Scene the session was created under. `None` on pre-v2 logs.
    pub scene: Option<String>,
    /// Absolute project root, or `None` for a no-project session — and also
    /// `None` on pre-v2 logs, which did not record one. The two cases are not
    /// distinguished here because nothing acts differently on them.
    pub project_root: Option<String>,
    pub session_kind: SessionKind,
    pub parent_session_id: Option<String>,
}

impl SessionFacts {
    /// The facts a `Meta` entry carries, or `None` for any other entry.
    pub fn of(entry: &LogEntry) -> Option<Self> {
        match entry {
            LogEntry::Meta {
                scene,
                project_root,
                session_kind,
                parent_session_id,
                ..
            } => Some(Self {
                scene: scene.clone(),
                project_root: project_root.clone(),
                session_kind: *session_kind,
                parent_session_id: parent_session_id.clone(),
            }),
            _ => None,
        }
    }
}

/// What has been recorded about a session rather than in it.
///
/// Counted as a by-product of appends that already happened — the alternative
/// is reading a whole transcript per session to answer "how much did this
/// cost", per listing, forever.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionStats {
    /// The name somebody chose, if anybody did.
    pub title: Option<String>,
    /// How many times this session's context has been compacted.
    pub compact_count: u64,
}

/// Where a session's log lives between runs.
///
/// The engine writes an append-only sequence of [`LogEntry`] per session and
/// reads it back to resume, fork or list. Nothing above this trait knows how
/// or where that happens: a backend may keep it in files, in a database, or
/// in memory, and the two shipped implementations do two of those.
///
/// # What an implementation must guarantee
///
/// * **Append-only order.** [`load`](Self::load) returns entries in the order
///   [`append`](Self::append) received them. Resume, fork and replay all read
///   position as meaning, so a store that reorders is not a slower store, it
///   is a wrong one.
/// * **A missing session is [`HistoryError::SessionNotFound`]**, not an empty
///   log. "Never existed" and "exists and is empty" are different answers and
///   callers act on the difference.
/// * **Durability is the backend's promise, not the caller's.** `append`
///   returning `Ok` means the entry survives whatever that backend survives.
#[async_trait]
pub trait HistoryStore: Send + Sync {
    async fn append(&self, session: SessionId, entry: LogEntry) -> Result<(), HistoryError>;

    /// [`append`](Self::append), recording what the writer knows about the
    /// line in its envelope. A backend that cannot keep the tags may drop
    /// them; the line itself must still be written.
    async fn append_tagged(
        &self,
        session: SessionId,
        entry: LogEntry,
        tags: crate::entry::EntryTags,
    ) -> Result<(), HistoryError> {
        let _ = tags;
        self.append(session, entry).await
    }

    async fn load(&self, session: SessionId) -> Result<Vec<EnvelopedEntry>, HistoryError>;
    async fn list_sessions(&self) -> Result<Vec<SessionId>, HistoryError>;

    /// Forget a session entirely — its log and anything the backend keeps
    /// alongside it. Deleting a session that is not there is not an error.
    async fn delete(&self, session: SessionId) -> Result<(), HistoryError>;

    /// The session's log as messages the model can be given directly.
    ///
    /// Projection rules, which are the engine's and not a backend's:
    /// - User / Assistant map across one to one;
    /// - a run of ToolResult entries collapses into a single `Message::User`,
    ///   because the API wants one user message carrying every `tool_result`
    ///   block of a round;
    /// - Meta / System / Compact / UsageSnapshot never reach the model.
    async fn load_messages(
        &self,
        session: SessionId,
    ) -> Result<Vec<base::message::Message>, HistoryError> {
        let entries = self.load(session).await?;
        Ok(project_messages_with(&entries, self.projection()))
    }

    /// Where this store's kept time and kept identifiers come from.
    ///
    /// On the store for the same reason the projection is: an entry's id and
    /// timestamp are written here and nowhere else, so this is the only place
    /// that has to be told.
    fn environment(&self) -> &dyn base::interface::environment::Environment {
        &base::interface::environment::SystemEnvironment
    }

    /// The rules this store's logs are read under.
    ///
    /// On the store rather than the engine because a log and the rules for
    /// reading it travel together: forking, resuming and searching a
    /// transcript must all see the same conversation, and they reach it
    /// through here.
    fn projection(&self) -> &dyn TranscriptProjection {
        &DefaultProjection
    }

    /// What a session is, as opposed to what was said in it.
    ///
    /// Scene, project and lineage are decided when a session is created and
    /// never change, and they are all written to the log's `Meta` entry — the
    /// first line of the file. A backend that can read that line alone
    /// answers this without materializing a transcript, which is what makes
    /// it usable from a listing: overriding this one method is enough to make
    /// both the parent-child walk and `session.list`'s per-entry facts cheap.
    ///
    /// Every field is optional in the same sense: a pre-v2 log has no
    /// `scene`/`project_root`, and the honest answer is `None` rather than a
    /// guess. Callers that need a scene for a *resume* infer it from the
    /// request instead (§3.4 of the RPC protocol).
    async fn session_facts(&self, session: SessionId) -> Result<SessionFacts, HistoryError> {
        let entries = match self.load(session).await {
            Ok(e) => e,
            Err(HistoryError::SessionNotFound(_)) => return Ok(SessionFacts::default()),
            Err(e) => return Err(e),
        };
        Ok(entries
            .iter()
            .find_map(|env| SessionFacts::of(&env.entry))
            .unwrap_or_default())
    }

    /// Give this session the name its owner calls it, or clear it (`None`).
    ///
    /// Not an entry in the log, and deliberately so. A name is not something
    /// that happened in the conversation — it is chosen afterwards, changed
    /// afterwards, and belongs to whoever is looking at the session rather
    /// than to the session's own history. It also has to be readable without
    /// replaying anything, which is why it lives beside the log instead of
    /// in it.
    ///
    /// The default refuses: a backend that cannot keep a name should say so
    /// rather than accept one and forget it, which is the failure a host
    /// discovers only when the name is gone.
    async fn set_session_title(
        &self,
        _session: SessionId,
        _title: Option<String>,
    ) -> Result<(), HistoryError> {
        Err(HistoryError::Path(
            "this history store cannot name sessions".into(),
        ))
    }

    /// What has been recorded *about* one session, as opposed to in it.
    ///
    /// The name somebody gave it and the running totals, which is everything
    /// that cannot be answered from the log's first line and should not cost
    /// a read of the whole log. A backend that keeps none of this answers
    /// with the empty record, and every field of it is honestly "nothing
    /// counted" rather than zero.
    async fn session_stats(&self, _session: SessionId) -> Result<SessionStats, HistoryError> {
        Ok(SessionStats::default())
    }

    /// Which session spawned this one, if any.
    ///
    /// Reads through [`session_facts`](Self::session_facts) so a backend has
    /// one place to make head-of-file lookups cheap rather than two that can
    /// disagree about the same line.
    async fn session_parent(&self, session: SessionId) -> Result<Option<String>, HistoryError> {
        Ok(self.session_facts(session).await?.parent_session_id)
    }

    /// Every session this one spawned.
    ///
    /// The default asks each known session who its parent is, which is one
    /// [`session_parent`](Self::session_parent) call per session and no better
    /// than linear. A backend that indexes the parent should override this
    /// outright; one that can merely read a session's head cheaply gets most
    /// of the win from overriding `session_parent` instead.
    async fn child_sessions(
        &self,
        parent_session_id: &str,
    ) -> Result<Vec<SessionId>, HistoryError> {
        let all = self.list_sessions().await?;
        let mut out = Vec::new();
        for sid in all {
            if self.session_parent(sid).await?.as_deref() == Some(parent_session_id) {
                out.push(sid);
            }
        }
        Ok(out)
    }

    /// Sessions matching a [`SessionQuery`], newest first.
    ///
    /// This is the whole of "which sessions are there" — recency listing and
    /// text search are the same question with and without a needle, so a
    /// backend that can answer one cheaply can answer both.
    ///
    /// # What an implementation must guarantee
    ///
    /// * **Newest first**, and a total order: ties on modification time break
    ///   on the session id, descending. Two backends holding the same sessions
    ///   answer in the same order or one of them is wrong.
    /// * **At most `limit`**, and `limit == 0` is the empty answer rather than
    ///   an unbounded one.
    /// * **A session whose transcript contains the needle is in the answer**
    ///   if the limit allows, matched case-insensitively over the rendered
    ///   text; so is one whose id contains it. A backend may match *more* than
    ///   that — an index over titles or working directories is why this is a
    ///   contract and not a function — but never less.
    ///
    /// The default reads every session to answer, which is exactly what a
    /// backend with an index should not do: overriding this one method is the
    /// whole of taking search over, and nothing above the trait changes.
    async fn find_sessions(
        &self,
        query: &SessionQuery,
    ) -> Result<Vec<SessionSummary>, HistoryError> {
        if query.limit == 0 {
            return Ok(Vec::new());
        }
        let needle = query.needle();
        let mut rows: Vec<(Option<time::OffsetDateTime>, SessionSummary)> = Vec::new();
        for session_id in self.list_sessions().await? {
            let entries = match self.load(session_id).await {
                Ok(entries) => entries,
                Err(HistoryError::SessionNotFound(_)) => continue,
                Err(e) => return Err(e),
            };
            let messages = project_messages_with(&entries, self.projection());
            if let Some(needle) = needle {
                if !session_id.to_string().contains(needle)
                    && !messages_match_query(&messages, needle)
                {
                    continue;
                }
            }
            let modified = entries.iter().map(|env| env.ts).max();
            rows.push((
                modified,
                SessionSummary {
                    session_id,
                    last_modified: modified.map(format_ts).unwrap_or_else(unknown_time),
                    entry_count: entries.len(),
                    message_count: messages.len(),
                    preview: preview_messages(&messages, 140),
                    canonical_cwd: None,
                    title: None,
                    total_input_tokens: None,
                    total_output_tokens: None,
                    compact_count: 0,
                },
            ));
        }
        rows.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| b.1.session_id.to_string().cmp(&a.1.session_id.to_string()))
        });
        rows.truncate(query.limit);
        Ok(rows.into_iter().map(|(_, summary)| summary).collect())
    }
}

/// Writes to `<projects_root>/<sanitize(project root)>/<session>.jsonl`.
pub struct JsonlHistoryStore {
    projects_root: PathBuf,
    /// Sidecar root — see [`HistoryRoots`]; carried rather than re-derived so
    /// the two halves of a session's state cannot land in different trees.
    sessions_root: PathBuf,
    /// The project a session with nothing recorded about it belongs to: the
    /// cwd this store was built with. Sessions created since transcripts were
    /// partitioned all record their own (see [`JsonlHistoryStore::home_of`]),
    /// so this answers for the ones written before that — and for a caller
    /// that asks about the "current" project without naming one.
    canonical_cwd: PathBuf,
    /// Which project each session belongs to, for the life of the process —
    /// `None` meaning a session with no project at all. The durable copy is
    /// the session's own sidecar; this is the cache in front of it, and the
    /// reason an append doesn't read a file to find out where to write.
    homes: Mutex<HashMap<SessionId, Option<PathBuf>>>,
    /// 序列化 append 调用，避免并发 partial-line 写。
    /// 不是 RwLock —— append 是写操作，读不互让有意义。
    append_lock: Arc<Mutex<()>>,
    /// Where content too big to keep inline goes. Without one, everything
    /// stays in the JSONL.
    blobs: Option<Arc<dyn BlobStore>>,
    /// How this store's logs become messages. `None` is the engine's rules.
    projection: Option<Arc<dyn TranscriptProjection>>,
    /// Where entry ids and timestamps come from. `None` is the machine's.
    environment: Option<Arc<dyn base::interface::environment::Environment>>,
}

impl JsonlHistoryStore {
    /// 两个根都由调用方给：transcript 落 `roots.projects`，sidecar 落
    /// `roots.sessions`。
    pub async fn with_roots(cwd: &Path, roots: HistoryRoots) -> Result<Self, HistoryError> {
        let canonical = canonicalize_cwd(cwd).await?;
        Ok(Self {
            projects_root: roots.projects,
            sessions_root: roots.sessions,
            canonical_cwd: canonical,
            homes: Mutex::new(HashMap::new()),
            append_lock: Arc::new(Mutex::new(())),
            blobs: None,
            projection: None,
            environment: None,
        })
    }

    /// Move large content and images out of the JSONL and into `blobs`.
    ///
    /// Written entries that qualify are replaced by a [`LogEntry::Blob`]
    /// naming this store; a load with the same store attached puts them back.
    /// Without a blob store nothing is externalized, and a log that has
    /// references in it still loads — with a gap where the content was.
    pub fn with_blob_store(mut self, blobs: Arc<dyn BlobStore>) -> Self {
        self.blobs = Some(blobs);
        self
    }

    /// Read this store's logs under someone else's rules.
    ///
    /// The rules must be reconstructible from the log alone — see
    /// [`crate::transcript::model_visible_content_is_reconstructible`]. A
    /// projection that reads anything else produces a session that cannot be
    /// reopened.
    pub fn with_projection(mut self, projection: Arc<dyn TranscriptProjection>) -> Self {
        self.projection = Some(projection);
        self
    }

    /// Take entry ids and timestamps from somewhere other than the machine.
    pub fn with_environment(
        mut self,
        environment: Arc<dyn base::interface::environment::Environment>,
    ) -> Self {
        self.environment = Some(environment);
        self
    }

    /// [`with_blob_store`](Self::with_blob_store) with the shipped default.
    pub fn with_paste_store(self, paste_store: PasteStore) -> Self {
        self.with_blob_store(Arc::new(paste_store))
    }

    pub fn project_dir_path(&self) -> PathBuf {
        project_dir(&self.projects_root, &self.canonical_cwd)
    }

    /// Where a session that never recorded a project of its own is written —
    /// not where an arbitrary session is. Use [`Self::session_path_of`] for
    /// that; this one cannot ask the disk, and so cannot answer.
    pub fn session_file_path(&self, session: &SessionId) -> PathBuf {
        session_file(&self.project_dir_path(), session)
    }

    /// The project this session belongs to: `Some(root)`, or `None` for a
    /// session created with no project at all.
    ///
    /// Three answers, in order. The in-memory map, filled in when the session
    /// wrote its `Meta` line and kept for as long as this process lives. The
    /// session's own sidecar, which is indexed by session id alone and is
    /// therefore reachable without already knowing the project — this is the
    /// tier that survives a restart, and why `Meta` writes the sidecar before
    /// it writes the log. And finally the cwd this store was built with,
    /// which is where every session lived back when a store served one
    /// project, and so is the right answer for transcripts written then.
    async fn home_of(&self, session: SessionId) -> Option<PathBuf> {
        if let Some(home) = self.homes.lock().await.get(&session) {
            return home.clone();
        }
        let resolved = match load_session_metadata(&self.sessions_root, session).await {
            // An empty `canonical_cwd` is how a no-project session records
            // itself; `SessionMetadata::new` is given an empty path for it.
            Some(meta) if meta.canonical_cwd.is_empty() => None,
            Some(meta) => Some(PathBuf::from(meta.canonical_cwd)),
            None => Some(self.canonical_cwd.clone()),
        };
        self.homes.lock().await.insert(session, resolved.clone());
        resolved
    }

    /// The transcript directory of the project rooted at `home`, resolving
    /// the root the same way a session's own `Meta` is resolved.
    async fn project_dir_of_root(&self, home: Option<&Path>) -> PathBuf {
        let canonical = match home {
            Some(root) => Some(
                canonicalize_cwd(root)
                    .await
                    .unwrap_or_else(|_| root.to_path_buf()),
            ),
            None => None,
        };
        project_dir_for(&self.projects_root, canonical.as_deref())
    }

    /// The directory holding this session's transcript.
    async fn session_dir_of(&self, session: SessionId) -> PathBuf {
        project_dir_for(&self.projects_root, self.home_of(session).await.as_deref())
    }

    /// The path of this session's transcript.
    async fn session_path_of(&self, session: SessionId) -> PathBuf {
        session_file(&self.session_dir_of(session).await, &session)
    }

    /// The project a session is about to be created under, read off the
    /// `Meta` line that creates it.
    ///
    /// A sidechain inherits its parent's project rather than landing in the
    /// no-project bucket: it carries no `project_root` of its own (the engine
    /// spawning it has no concept of one), but its transcript is anchored to
    /// the parent's — cascade delete and the parent-child walk both look for
    /// it there.
    async fn home_from_meta(&self, entry: &LogEntry) -> Option<Option<PathBuf>> {
        let LogEntry::Meta {
            project_root,
            parent_session_id,
            ..
        } = entry
        else {
            return None;
        };
        if let Some(root) = project_root {
            let root = Path::new(root);
            return Some(Some(
                canonicalize_cwd(root)
                    .await
                    .unwrap_or_else(|_| root.to_path_buf()),
            ));
        }
        match parent_session_id.as_deref() {
            Some(parent) => Some(match SessionId::parse(parent) {
                Ok(parent) => self.home_of(parent).await,
                // A parent id this store cannot resolve to a session: keep the
                // child where a store that served one project would have put
                // it, which is also where `child_sessions` looks for it.
                Err(_) => Some(self.canonical_cwd.clone()),
            }),
            None => Some(None),
        }
    }

    pub fn canonical_cwd(&self) -> &Path {
        &self.canonical_cwd
    }

    /// 列出当前 project 目录下最近 N 个 session 的 (id, last_modified) 元组。
    /// 按修改时间倒序（最新先）。给 `/resume` slash 列最近会话用。
    pub async fn list_recent_sessions(
        &self,
        max: usize,
    ) -> Result<Vec<(SessionId, String)>, HistoryError> {
        Ok(self
            .session_files_by_mtime(&self.project_dir_path(), max)
            .await?
            .into_iter()
            .map(|(id, _, mtime)| (id, format_mtime(mtime)))
            .collect())
    }

    /// The `max` most-recently-modified session summaries for the current
    /// project directory, newest first.
    pub async fn list_recent_session_summaries(
        &self,
        max: usize,
    ) -> Result<Vec<SessionSummary>, HistoryError> {
        self.find_sessions(&SessionQuery::recent(max)).await
    }

    /// Search the current project directory by session ID and message
    /// content. An empty query lists the most recent instead.
    pub async fn search_session_summaries(
        &self,
        query: &str,
        max: usize,
    ) -> Result<Vec<SessionSummary>, HistoryError> {
        self.find_sessions(&SessionQuery::matching(query, max))
            .await
    }

    /// Search **all** project directories under the history root. Used by
    /// `/resume @all <query>`.
    pub async fn search_all_project_session_summaries(
        &self,
        query: &str,
        max: usize,
    ) -> Result<Vec<SessionSummary>, HistoryError> {
        self.find_sessions(&SessionQuery::matching(query, max).within(SessionScope::AllProjects))
            .await
    }

    /// Search project directories that share the same git repository as the
    /// current working directory — a monorepo with per-subdirectory history.
    /// Used by `/resume @repo <query>`.
    pub async fn search_same_repo_session_summaries(
        &self,
        query: &str,
        max: usize,
    ) -> Result<Vec<SessionSummary>, HistoryError> {
        let root = repo_root_or_cwd(&self.canonical_cwd).await;
        self.find_sessions(&SessionQuery::matching(query, max).within(SessionScope::Under(root)))
            .await
    }

    async fn recent_summaries(
        &self,
        dir: &Path,
        max: usize,
        detail: crate::query::SummaryDetail,
    ) -> Result<Vec<SessionSummary>, HistoryError> {
        let files = self.session_files_by_mtime(dir, max).await?;
        let mut out = Vec::new();
        for (session_id, _path, mtime) in files {
            match detail {
                // The directory listing already answered the question. Opening
                // each transcript to fill in a preview nobody asked for turns
                // a listing into a full parse of every session in the project.
                //
                // The sidecar is a different matter and is read: it is one
                // small JSON document per session, and it holds the things
                // that exist nowhere else — the name a person gave this
                // session, and the counters. Leaving it out is why a renamed
                // session used to come back unnamed.
                crate::query::SummaryDetail::IdsOnly => {
                    let metadata = load_session_metadata(&self.sessions_root, session_id).await;
                    out.push(self.summary_from_parts(session_id, mtime, 0, &[], metadata))
                }
                crate::query::SummaryDetail::Full => {
                    if let Some(summary) = self.session_summary(session_id, mtime).await? {
                        out.push(summary);
                    }
                }
            }
        }
        Ok(out)
    }

    async fn search_one_project(
        &self,
        dir: &Path,
        needle: &str,
        max: usize,
    ) -> Result<Vec<SessionSummary>, HistoryError> {
        let files = self.session_files_by_mtime(dir, usize::MAX).await?;
        let mut out = Vec::new();
        for (session_id, _path, mtime) in files {
            let entries = match self.load(session_id).await {
                Ok(entries) => entries,
                Err(HistoryError::SessionNotFound(_)) => continue,
                Err(e) => return Err(e),
            };
            let messages = project_messages_with(&entries, self.projection());
            if session_id.to_string().contains(needle) || messages_match_query(&messages, needle) {
                let metadata = load_session_metadata(&self.sessions_root, session_id).await;
                out.push(self.summary_from_parts(
                    session_id,
                    mtime,
                    entries.len(),
                    &messages,
                    metadata,
                ));
            }
            if out.len() >= max {
                break;
            }
        }
        Ok(out)
    }

    async fn search_project_dirs(
        &self,
        query: &str,
        max: usize,
        filter: ProjectDirFilter,
    ) -> Result<Vec<SessionSummary>, HistoryError> {
        if max == 0 {
            return Ok(Vec::new());
        }
        let query = query.trim().to_lowercase();
        let mut project_dirs = match tokio::fs::read_dir(&self.projects_root).await {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(HistoryError::Io(e)),
        };
        let mut candidates = Vec::new();
        while let Some(entry) = project_dirs.next_entry().await? {
            let Ok(ft) = entry.file_type().await else {
                continue;
            };
            if !ft.is_dir() {
                continue;
            }
            let mut files = collect_session_files(entry.path()).await?;
            candidates.append(&mut files);
        }
        candidates.sort_by_key(|p| std::cmp::Reverse(p.2));

        let mut out = Vec::new();
        for (session_id, path, mtime) in candidates {
            let entries = load_entries_from_path(&path, session_id).await?;
            let metadata = load_session_metadata(&self.sessions_root, session_id).await;
            if !filter.matches(metadata.as_ref()) {
                continue;
            }
            let messages = project_messages_with(&entries, self.projection());
            let haystack = format!(
                "{}\n{}\n{}\n{}",
                session_id,
                metadata
                    .as_ref()
                    .and_then(|m| m.title.as_deref())
                    .unwrap_or_default(),
                metadata
                    .as_ref()
                    .map(|m| m.canonical_cwd.as_str())
                    .unwrap_or_default(),
                crate::transcript::render_search_text(&messages)
            )
            .to_lowercase();
            if query.is_empty() || haystack.contains(&query) {
                out.push(self.summary_from_parts(
                    session_id,
                    mtime,
                    entries.len(),
                    &messages,
                    metadata,
                ));
            }
            if out.len() >= max {
                break;
            }
        }
        Ok(out)
    }

    /// `None` for a session whose log cannot be read back. A half-written or
    /// hand-edited transcript is a property of that one session; letting it
    /// fail the call would hide every other session in the directory behind
    /// it.
    async fn session_summary(
        &self,
        session_id: SessionId,
        mtime: std::time::SystemTime,
    ) -> Result<Option<SessionSummary>, HistoryError> {
        let entries = match self.load(session_id).await {
            Ok(entries) => entries,
            Err(HistoryError::SessionNotFound(_)) | Err(HistoryError::Parse { .. }) => {
                return Ok(None)
            }
            Err(e) => return Err(e),
        };
        let messages = project_messages_with(&entries, self.projection());
        let metadata = load_session_metadata(&self.sessions_root, session_id).await;
        Ok(Some(self.summary_from_parts(
            session_id,
            mtime,
            entries.len(),
            &messages,
            metadata,
        )))
    }

    fn summary_from_parts(
        &self,
        session_id: SessionId,
        mtime: std::time::SystemTime,
        entry_count: usize,
        messages: &[base::message::Message],
        metadata: Option<SessionMetadata>,
    ) -> SessionSummary {
        SessionSummary {
            session_id,
            last_modified: format_mtime(mtime),
            entry_count,
            message_count: messages.len(),
            preview: metadata
                .as_ref()
                .and_then(|m| m.latest_summary.clone())
                .unwrap_or_else(|| preview_messages(messages, 140)),
            canonical_cwd: metadata.as_ref().map(|m| m.canonical_cwd.clone()),
            title: metadata.as_ref().and_then(|m| m.title.clone()),
            total_input_tokens: metadata.as_ref().and_then(|m| m.total_input_tokens),
            total_output_tokens: metadata.as_ref().and_then(|m| m.total_output_tokens),
            compact_count: metadata.as_ref().map(|m| m.compact_count).unwrap_or(0),
        }
    }

    async fn session_files_by_mtime(
        &self,
        dir: &Path,
        max: usize,
    ) -> Result<Vec<(SessionId, PathBuf, std::time::SystemTime)>, HistoryError> {
        if max == 0 {
            return Ok(Vec::new());
        }
        let mut entries = match tokio::fs::read_dir(&dir).await {
            Ok(e) => e,
            Err(_) => return Ok(Vec::new()),
        };
        let mut found: Vec<(SessionId, PathBuf, std::time::SystemTime)> = Vec::new();
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            // 文件名 `<id>.jsonl`
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(id) = SessionId::parse(stem) else {
                continue;
            };
            let mtime = match entry.metadata().await {
                Ok(m) => m.modified().unwrap_or(std::time::UNIX_EPOCH),
                Err(_) => std::time::UNIX_EPOCH,
            };
            found.push((id, path, mtime));
        }
        // 倒序按 mtime
        found.sort_by_key(|p| std::cmp::Reverse(p.2));
        found.truncate(max);
        Ok(found)
    }
}

fn format_mtime(t: std::time::SystemTime) -> String {
    format_ts(time::OffsetDateTime::from(t))
}

fn format_ts(dt: time::OffsetDateTime) -> String {
    dt.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| unknown_time())
}

fn unknown_time() -> String {
    "unknown".into()
}

enum ProjectDirFilter {
    All,
    UnderPath(PathBuf),
}

impl ProjectDirFilter {
    fn matches(&self, metadata: Option<&SessionMetadata>) -> bool {
        match self {
            Self::All => true,
            Self::UnderPath(root) => metadata
                .map(|m| Path::new(&m.canonical_cwd).starts_with(root))
                .unwrap_or(false),
        }
    }
}

/// Every `<session>.jsonl` in one project directory, by id. A directory that
/// isn't there holds no sessions — not an error, just a project nothing has
/// been written for yet.
async fn session_ids_in(dir: &Path) -> Result<Vec<SessionId>, HistoryError> {
    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(HistoryError::Io(e)),
    };
    let mut out = Vec::new();
    while let Some(ent) = entries.next_entry().await? {
        let name = ent.file_name();
        let Some(s) = name.to_str() else { continue };
        let Some(stem) = s.strip_suffix(".jsonl") else {
            continue;
        };
        if let Ok(id) = SessionId::parse(stem) {
            out.push(id);
        }
    }
    Ok(out)
}

async fn repo_root_or_cwd(cwd: &Path) -> PathBuf {
    let output = tokio::process::Command::new("git")
        .arg("rev-parse")
        .arg("--show-toplevel")
        .current_dir(cwd)
        .output()
        .await;
    match output {
        Ok(out) if out.status.success() => {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if s.is_empty() {
                cwd.to_path_buf()
            } else {
                PathBuf::from(s)
            }
        }
        _ => cwd.to_path_buf(),
    }
}

async fn collect_session_files(
    dir: PathBuf,
) -> Result<Vec<(SessionId, PathBuf, std::time::SystemTime)>, HistoryError> {
    let mut entries = match tokio::fs::read_dir(&dir).await {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(HistoryError::Io(e)),
    };
    let mut out = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Ok(session_id) = SessionId::parse(stem) else {
            continue;
        };
        let mtime = entry
            .metadata()
            .await
            .ok()
            .and_then(|m| m.modified().ok())
            .unwrap_or(std::time::UNIX_EPOCH);
        out.push((session_id, path, mtime));
    }
    Ok(out)
}

async fn load_entries_from_path(
    path: &Path,
    session: SessionId,
) -> Result<Vec<EnvelopedEntry>, HistoryError> {
    let content = match tokio::fs::read_to_string(path).await {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(HistoryError::SessionNotFound(session.to_string()));
        }
        Err(e) => return Err(HistoryError::Io(e)),
    };
    let mut entries = Vec::new();
    for (i, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let env = serde_json::from_str::<EnvelopedEntry>(line)
            .map_err(|error| HistoryError::Parse { line: i + 1, error })?;
        entries.push(env);
    }
    Ok(entries)
}

async fn load_session_metadata(
    sessions_root: &Path,
    session: SessionId,
) -> Option<SessionMetadata> {
    let path = session_metadata_file(sessions_root, &session);
    let content = tokio::fs::read_to_string(path).await.ok()?;
    serde_json::from_str(&content).ok()
}

#[async_trait]
impl HistoryStore for JsonlHistoryStore {
    fn projection(&self) -> &dyn TranscriptProjection {
        match &self.projection {
            Some(p) => &**p,
            None => &DefaultProjection,
        }
    }

    fn environment(&self) -> &dyn base::interface::environment::Environment {
        match &self.environment {
            Some(e) => &**e,
            None => &base::interface::environment::SystemEnvironment,
        }
    }

    async fn append(&self, session: SessionId, entry: LogEntry) -> Result<(), HistoryError> {
        self.append_tagged(session, entry, crate::entry::EntryTags::default())
            .await
    }

    async fn append_tagged(
        &self,
        session: SessionId,
        entry: LogEntry,
        tags: crate::entry::EntryTags,
    ) -> Result<(), HistoryError> {
        let _guard = self.append_lock.lock().await;

        // A `Meta` line is where a session says which project it belongs to,
        // so it is also where that gets recorded — in the map for this
        // process and in the sidecar for every later one. The sidecar is
        // written *before* the log line, which is the opposite order from the
        // counters below and for the opposite reason: the counters are a
        // convenience computed from a log that already exists, while this is
        // the index that says where the log is. A crash between the two
        // leaves an empty index entry, which is recoverable; the other order
        // leaves a transcript nothing can find.
        let home = match self.home_from_meta(&entry).await {
            Some(home) => {
                let dir = project_dir_for(&self.projects_root, home.as_deref());
                crate::project::ensure_session_sidecar_in(
                    &self.sessions_root,
                    home.as_deref().unwrap_or_else(|| Path::new("")),
                    &dir,
                    session,
                )
                .await?;
                self.homes.lock().await.insert(session, home.clone());
                home
            }
            None => self.home_of(session).await,
        };
        let project_history_dir = project_dir_for(&self.projects_root, home.as_deref());
        let path = session_file(&project_history_dir, &session);

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Read before the entry can be externalized: an assistant message big
        // enough to move out to a blob would take its usage with it, and the
        // totals would quietly stop counting anything over the threshold.
        let counters = crate::project::CounterDelta::of(&entry);

        let entry = match &self.blobs {
            Some(blobs) => externalize_if_bulky(entry, blobs.as_ref())?,
            None => entry,
        };

        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path)?;

        let enveloped = EnvelopedEntry::new_in(self.environment(), session, entry).with_tags(tags);
        let line = serde_json::to_string(&enveloped)?;

        // 一次 write_all 把 line + '\n' 一起出，减少 partial line 风险。
        use std::io::Write;
        let mut buf = line.into_bytes();
        buf.push(b'\n');
        f.write_all(&buf)?;
        f.flush()?;

        // After the log, and never in front of it. The transcript is the
        // record; these are a convenience computed from it, so a sidecar that
        // cannot be written costs a count and not the entry — still inside
        // `append_lock`, which is what keeps two appends from reading the
        // same total and writing back the same sum.
        if let Err(e) = crate::project::add_counters_in(
            &self.sessions_root,
            home.as_deref().unwrap_or_else(|| Path::new("")),
            &project_history_dir,
            session,
            counters,
        )
        .await
        {
            tracing::warn!(error = %e, %session, "could not update this session's counters");
        }
        Ok(())
    }

    async fn load(&self, session: SessionId) -> Result<Vec<EnvelopedEntry>, HistoryError> {
        let path = self.session_path_of(session).await;
        let content = match tokio::fs::read_to_string(&path).await {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(HistoryError::SessionNotFound(session.to_string()));
            }
            Err(e) => return Err(HistoryError::Io(e)),
        };

        let mut entries = Vec::new();
        for (i, line) in content.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let mut env = serde_json::from_str::<EnvelopedEntry>(line)
                .map_err(|error| HistoryError::Parse { line: i + 1, error })?;

            if let Some(blobs) = &self.blobs {
                hydrate(&mut env, blobs.as_ref());
            }

            entries.push(env);
        }
        Ok(entries)
    }

    async fn delete(&self, session: SessionId) -> Result<(), HistoryError> {
        let jsonl_path = self.session_path_of(session).await;
        match tokio::fs::remove_file(&jsonl_path).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(HistoryError::Io(e)),
        }
        let meta_path = session_metadata_file(&self.sessions_root, &session);
        let _ = tokio::fs::remove_file(meta_path).await;
        self.homes.lock().await.remove(&session);
        Ok(())
    }

    async fn set_session_title(
        &self,
        session: SessionId,
        title: Option<String>,
    ) -> Result<(), HistoryError> {
        let home = self.home_of(session).await;
        crate::project::set_session_title_in(
            &self.sessions_root,
            home.as_deref().unwrap_or_else(|| Path::new("")),
            &project_dir_for(&self.projects_root, home.as_deref()),
            session,
            title,
        )
        .await
    }

    async fn session_stats(&self, session: SessionId) -> Result<SessionStats, HistoryError> {
        crate::project::session_stats_in(&self.sessions_root, session).await
    }

    /// Reads only as far as the `Meta` entry, which the engine writes first.
    /// The default would parse — and hydrate every paste of — an entire
    /// transcript to reach the fields on its first line.
    async fn session_facts(&self, session: SessionId) -> Result<SessionFacts, HistoryError> {
        let path = self.session_path_of(session).await;
        let content = match tokio::fs::read_to_string(&path).await {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(SessionFacts::default())
            }
            Err(e) => return Err(HistoryError::Io(e)),
        };
        for (i, line) in content.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let env = serde_json::from_str::<EnvelopedEntry>(line)
                .map_err(|error| HistoryError::Parse { line: i + 1, error })?;
            if let Some(facts) = SessionFacts::of(&env.entry) {
                return Ok(facts);
            }
        }
        Ok(SessionFacts::default())
    }

    /// The sessions of the project this store was built for — not every
    /// session it can reach, which is what `find_sessions` is for.
    async fn list_sessions(&self) -> Result<Vec<SessionId>, HistoryError> {
        session_ids_in(&self.project_dir_path()).await
    }

    /// Looks in the parent's project rather than this store's own, which is
    /// the same directory the child's transcript went to (`home_from_meta`
    /// gives a sidechain its parent's project). The default walks
    /// `list_sessions`, and would find nothing for a parent belonging to any
    /// project but this store's — taking the cascade delete in
    /// `session.close` down with it.
    async fn child_sessions(
        &self,
        parent_session_id: &str,
    ) -> Result<Vec<SessionId>, HistoryError> {
        let dir = match SessionId::parse(parent_session_id) {
            Ok(parent) => self.session_dir_of(parent).await,
            Err(_) => self.project_dir_path(),
        };
        let mut out = Vec::new();
        for sid in session_ids_in(&dir).await? {
            if self.session_facts(sid).await?.parent_session_id.as_deref()
                == Some(parent_session_id)
            {
                out.push(sid);
            }
        }
        Ok(out)
    }

    /// Orders by the file's modification time and narrows by directory before
    /// reading anything, neither of which the default can do.
    ///
    /// A recency query still opens each transcript to build its preview,
    /// unless the caller asked for [`SummaryDetail::IdsOnly`] — which is what
    /// a listing wants, and what keeps `session.list` a directory read rather
    /// than a parse of every session in the project.
    ///
    /// [`SummaryDetail::IdsOnly`]: crate::query::SummaryDetail::IdsOnly
    async fn find_sessions(
        &self,
        query: &SessionQuery,
    ) -> Result<Vec<SessionSummary>, HistoryError> {
        match (&query.scope, query.needle()) {
            (SessionScope::CurrentProject, None) => {
                self.recent_summaries(&self.project_dir_path(), query.limit, query.detail)
                    .await
            }
            (SessionScope::CurrentProject, Some(needle)) => {
                self.search_one_project(&self.project_dir_path(), needle, query.limit)
                    .await
            }
            (SessionScope::Project(home), needle) => {
                // Resolved the same way the project a session records is, or
                // a caller naming the project it just created a session in
                // would miss it over a symlink (`/tmp` against
                // `/private/tmp`, every macOS temp directory).
                let dir = self.project_dir_of_root(home.as_deref()).await;
                match needle {
                    None => self.recent_summaries(&dir, query.limit, query.detail).await,
                    Some(needle) => self.search_one_project(&dir, needle, query.limit).await,
                }
            }
            (SessionScope::AllProjects, _) => {
                self.search_project_dirs(&query.text, query.limit, ProjectDirFilter::All)
                    .await
            }
            (SessionScope::Under(root), _) => {
                self.search_project_dirs(
                    &query.text,
                    query.limit,
                    ProjectDirFilter::UnderPath(root.clone()),
                )
                .await
            }
        }
    }
}

/// Keeps every session's log in memory and forgets it when dropped.
///
/// The second implementation, and the one that makes [`HistoryStore`] a
/// contract rather than one type's interface. Useful in its own right — a
/// host embedding the engine for a throwaway conversation has no reason to
/// touch the disk — and useful as the thing a new backend is checked against:
/// it does nothing but honor the contract, so a test that passes here and
/// fails elsewhere is testing the backend, not the engine.
#[derive(Default)]
pub struct InMemoryHistoryStore {
    sessions: std::sync::Mutex<std::collections::HashMap<SessionId, Vec<EnvelopedEntry>>>,
    titles: std::sync::Mutex<std::collections::HashMap<SessionId, String>>,
}

impl InMemoryHistoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl HistoryStore for InMemoryHistoryStore {
    async fn append(&self, session: SessionId, entry: LogEntry) -> Result<(), HistoryError> {
        self.append_tagged(session, entry, crate::entry::EntryTags::default())
            .await
    }

    async fn append_tagged(
        &self,
        session: SessionId,
        entry: LogEntry,
        tags: crate::entry::EntryTags,
    ) -> Result<(), HistoryError> {
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(session)
            .or_default()
            .push(EnvelopedEntry::new_in(self.environment(), session, entry).with_tags(tags));
        Ok(())
    }

    async fn load(&self, session: SessionId) -> Result<Vec<EnvelopedEntry>, HistoryError> {
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&session)
            .cloned()
            // Absent is not empty: a caller resuming a session it believes in
            // needs to hear that it is not here.
            .ok_or_else(|| HistoryError::SessionNotFound(session.to_string()))
    }

    async fn list_sessions(&self) -> Result<Vec<SessionId>, HistoryError> {
        Ok(self
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .copied()
            .collect())
    }

    async fn delete(&self, session: SessionId) -> Result<(), HistoryError> {
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&session);
        self.titles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&session);
        Ok(())
    }

    async fn set_session_title(
        &self,
        session: SessionId,
        title: Option<String>,
    ) -> Result<(), HistoryError> {
        let mut titles = self.titles.lock().unwrap_or_else(|e| e.into_inner());
        match title {
            Some(t) => titles.insert(session, t),
            None => titles.remove(&session),
        };
        Ok(())
    }

    async fn session_stats(&self, session: SessionId) -> Result<SessionStats, HistoryError> {
        Ok(SessionStats {
            title: self
                .titles
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&session)
                .cloned(),
            ..SessionStats::default()
        })
    }
}

// ---------------------------------------------------------------------------
// Externalizing bulky content
// ---------------------------------------------------------------------------

/// Serialized content above this is worth a round trip to the blob store.
const BULKY_CONTENT_BYTES: usize = 1024;

/// Replace `entry` with a reference if it is one of the bulky kinds.
///
/// The whole entry is what gets stored, tag and auxiliary fields included, so
/// the blob is self-describing and hydration is a plain deserialize rather
/// than a reconstruction that has to agree with this function.
fn externalize_if_bulky(entry: LogEntry, blobs: &dyn BlobStore) -> Result<LogEntry, HistoryError> {
    let Some(describes) = externalizable_kind(&entry) else {
        return Ok(entry);
    };
    let json = serde_json::to_string(&entry)?;
    let id = blobs.put(json.as_bytes()).map_err(HistoryError::Io)?;
    Ok(LogEntry::Blob {
        blob: BlobRef {
            store: blobs.name().to_string(),
            id,
            describes: describes.to_string(),
            bytes: json.len() as u64,
        },
    })
}

/// The `kind` tag of an entry worth moving out, or `None` to leave it inline.
///
/// Images leave at any size. Their bytes are never read as text, never
/// matched by a search and never skimmed by a person, so the threshold that
/// exists to keep short messages readable does not apply to them.
fn externalizable_kind(entry: &LogEntry) -> Option<&'static str> {
    let (kind, blocks): (_, &[ContentBlock]) = match entry {
        LogEntry::User { content } => ("user", content),
        LogEntry::Assistant { content, .. } => ("assistant", content),
        LogEntry::ToolResult { content, .. } => {
            let carries_image = match content {
                ToolResultContent::Text(_) => false,
                ToolResultContent::Blocks(blocks) => blocks_carry_image(blocks),
            };
            return (carries_image || serialized_len(content) > BULKY_CONTENT_BYTES)
                .then_some("tool_result");
        }
        _ => return None,
    };
    (blocks_carry_image(blocks) || serialized_len(blocks) > BULKY_CONTENT_BYTES).then_some(kind)
}

fn blocks_carry_image(blocks: &[ContentBlock]) -> bool {
    blocks.iter().any(|block| match block {
        ContentBlock::Image { .. } => true,
        ContentBlock::ToolResult {
            content: ToolResultContent::Blocks(nested),
            ..
        } => blocks_carry_image(nested),
        _ => false,
    })
}

/// The content's JSON size — the dominant term in the serialized line.
fn serialized_len<T: serde::Serialize + ?Sized>(value: &T) -> usize {
    serde_json::to_string(value).map(|s| s.len()).unwrap_or(0)
}

/// Put the original entry back, if this entry is a reference and `blobs` has
/// what it points at.
///
/// Every other case leaves the entry as it is, deliberately and without an
/// error: a reference into a store nobody mounted, content that was cleaned
/// up, or a blob that no longer deserializes are all reasons for a session to
/// load with a gap and none of them are reasons for it not to load.
fn hydrate(env: &mut EnvelopedEntry, blobs: &dyn BlobStore) {
    let id = match &env.entry {
        LogEntry::Blob { blob } if blob.store == blobs.name() => &blob.id,
        LogEntry::PasteRef { paste_id } => paste_id,
        _ => return,
    };
    let fetched = match blobs.get(id) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!(blob = %id, error = %e, "blob store unreadable; leaving the reference in place");
            return;
        }
    };
    match serde_json::from_slice::<LogEntry>(&fetched) {
        Ok(hydrated) => env.entry = hydrated,
        Err(e) => {
            tracing::warn!(blob = %id, error = %e, "blob does not deserialize; leaving the reference in place");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::{self, SessionKind};
    use base::message::{ContentBlock, StopReason, ToolResultContent};
    use base::permission::PermissionMode;
    use serde_json::json;
    use tempfile::TempDir;
    use time::OffsetDateTime;

    async fn make_store() -> (JsonlHistoryStore, TempDir, TempDir) {
        let cwd_tmp = TempDir::new().unwrap();
        let projects_tmp = TempDir::new().unwrap();
        let store =
            JsonlHistoryStore::with_roots(cwd_tmp.path(), HistoryRoots::under(projects_tmp.path()))
                .await
                .unwrap();
        (store, cwd_tmp, projects_tmp)
    }

    #[tokio::test]
    async fn append_creates_file_under_sanitized_dir() {
        let (store, _cwd, _proj) = make_store().await;
        let s = SessionId::new();
        store
            .append(
                s,
                LogEntry::User {
                    content: vec![ContentBlock::Text {
                        text: "hi".into(),
                        cache_control: None,
                    }],
                },
            )
            .await
            .unwrap();

        let path = store.session_file_path(&s);
        assert!(path.exists());
        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(content.ends_with('\n'));
        assert_eq!(content.lines().count(), 1);
    }

    #[tokio::test]
    async fn append_then_load_roundtrip() {
        let (store, _cwd, _proj) = make_store().await;
        let s = SessionId::new();

        // meta + 3 messages
        store
            .append(
                s,
                LogEntry::Meta {
                    cwd: "/tmp/test".into(),
                    started_at: OffsetDateTime::now_utc(),
                    model: "claude-sonnet-4-6".into(),
                    permission_mode: format!("{:?}", PermissionMode::Default),
                    engine_version: "0.0.1".into(),
                    agix_version: "0.0.1".into(),
                    parent_session_id: None,
                    scene: Some("coding".into()),
                    project_root: None,
                    session_kind: SessionKind::Primary,
                    schema_version: entry::CURRENT_META_SCHEMA_VERSION,
                },
            )
            .await
            .unwrap();

        for txt in &["one", "two", "three"] {
            store
                .append(
                    s,
                    LogEntry::User {
                        content: vec![ContentBlock::Text {
                            text: (*txt).into(),
                            cache_control: None,
                        }],
                    },
                )
                .await
                .unwrap();
        }

        let loaded = store.load(s).await.unwrap();
        assert_eq!(loaded.len(), 4);
        assert!(matches!(loaded[0].entry, LogEntry::Meta { .. }));
        for (env, expected) in loaded.iter().skip(1).zip(["one", "two", "three"].iter()) {
            match &env.entry {
                LogEntry::User { content } => match &content[0] {
                    ContentBlock::Text { text, .. } => assert_eq!(text, expected),
                    _ => panic!(),
                },
                _ => panic!(),
            }
        }
    }

    #[tokio::test]
    async fn load_unknown_session_errors_with_session_not_found() {
        let (store, _cwd, _proj) = make_store().await;
        let s = SessionId::new();
        let err = store.load(s).await.unwrap_err();
        assert!(matches!(err, HistoryError::SessionNotFound(_)));
    }

    #[tokio::test]
    async fn list_sessions_returns_existing_files() {
        let (store, _cwd, _proj) = make_store().await;
        let a = SessionId::new();
        let b = SessionId::new();
        for s in [a, b] {
            store
                .append(
                    s,
                    LogEntry::User {
                        content: vec![ContentBlock::Text {
                            text: "x".into(),
                            cache_control: None,
                        }],
                    },
                )
                .await
                .unwrap();
        }
        let mut listed = store.list_sessions().await.unwrap();
        listed.sort_by_key(|s| s.to_string());
        let mut expected = vec![a, b];
        expected.sort_by_key(|s| s.to_string());
        assert_eq!(listed, expected);
    }

    #[tokio::test]
    async fn recent_session_summaries_include_counts_and_preview() {
        let (store, _cwd, _proj) = make_store().await;
        let s = SessionId::new();
        store
            .append(
                s,
                LogEntry::User {
                    content: vec![ContentBlock::Text {
                        text: "first request".into(),
                        cache_control: None,
                    }],
                },
            )
            .await
            .unwrap();
        store
            .append(
                s,
                LogEntry::Assistant {
                    content: vec![ContentBlock::Text {
                        text: "useful answer".into(),
                        cache_control: None,
                    }],
                    stop_reason: None,
                    usage: None,
                    model: None,
                },
            )
            .await
            .unwrap();

        let summaries = store.list_recent_session_summaries(5).await.unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].session_id, s);
        assert_eq!(summaries[0].entry_count, 2);
        assert_eq!(summaries[0].message_count, 2);
        assert!(summaries[0].preview.contains("useful answer"));
    }

    /// A listing asks which sessions there are. Building a preview for each
    /// costs a full transcript read, and `session.list` throws every preview
    /// away — so a few hundred sessions turned a directory read into a parse
    /// of all of them.
    #[tokio::test]
    async fn an_ids_only_query_does_not_open_the_transcripts() {
        let (store, _cwd, _proj) = make_store().await;
        let s = SessionId::new();
        for text in ["a question", "a useful answer"] {
            store
                .append(
                    s,
                    LogEntry::User {
                        content: vec![ContentBlock::Text {
                            text: text.into(),
                            cache_control: None,
                        }],
                    },
                )
                .await
                .unwrap();
        }

        let full = store
            .find_sessions(&crate::query::SessionQuery::recent(5))
            .await
            .unwrap();
        assert_eq!(full.len(), 1);
        assert_eq!(full[0].message_count, 2);
        assert!(full[0].preview.contains("useful answer"));

        let ids = store
            .find_sessions(&crate::query::SessionQuery::recent(5).ids_only())
            .await
            .unwrap();
        assert_eq!(ids.len(), 1, "the same sessions, in the same order");
        assert_eq!(ids[0].session_id, s);
        assert_eq!(
            (ids[0].message_count, ids[0].entry_count),
            (0, 0),
            "a count that was never read is reported as unread, not guessed"
        );
        assert!(
            ids[0].preview.is_empty(),
            "a preview here would mean the transcript was opened after all"
        );
    }

    #[tokio::test]
    async fn a_transcript_that_will_not_parse_hides_only_itself() {
        let (store, _cwd, _proj) = make_store().await;
        let good = SessionId::new();
        store
            .append(
                good,
                LogEntry::User {
                    content: vec![ContentBlock::Text {
                        text: "readable".into(),
                        cache_control: None,
                    }],
                },
            )
            .await
            .unwrap();

        let corrupt = SessionId::new();
        tokio::fs::write(store.session_file_path(&corrupt), "{not json at all\n")
            .await
            .unwrap();

        let summaries = store
            .find_sessions(&SessionQuery::recent(10))
            .await
            .expect("one unreadable log must not fail the listing");
        assert_eq!(
            summaries.iter().map(|s| s.session_id).collect::<Vec<_>>(),
            vec![good]
        );
    }

    #[tokio::test]
    async fn search_session_summaries_matches_transcript_text_and_id() {
        let (store, _cwd, _proj) = make_store().await;
        let matching = SessionId::new();
        let other = SessionId::new();
        store
            .append(
                matching,
                LogEntry::User {
                    content: vec![ContentBlock::Text {
                        text: "needle topic".into(),
                        cache_control: None,
                    }],
                },
            )
            .await
            .unwrap();
        store
            .append(
                other,
                LogEntry::User {
                    content: vec![ContentBlock::Text {
                        text: "different topic".into(),
                        cache_control: None,
                    }],
                },
            )
            .await
            .unwrap();

        let by_text = store.search_session_summaries("needle", 10).await.unwrap();
        assert_eq!(by_text.len(), 1);
        assert_eq!(by_text[0].session_id, matching);

        let id_prefix = &other.to_string()[..8];
        let by_id = store.search_session_summaries(id_prefix, 10).await.unwrap();
        assert_eq!(by_id.len(), 1);
        assert_eq!(by_id[0].session_id, other);
    }

    #[tokio::test]
    async fn search_all_project_session_summaries_scans_other_project_dirs() {
        let cwd_a = TempDir::new().unwrap();
        let cwd_b = TempDir::new().unwrap();
        let projects = TempDir::new().unwrap();
        let store_a =
            JsonlHistoryStore::with_roots(cwd_a.path(), HistoryRoots::under(projects.path()))
                .await
                .unwrap();
        let store_b =
            JsonlHistoryStore::with_roots(cwd_b.path(), HistoryRoots::under(projects.path()))
                .await
                .unwrap();
        let session_b = SessionId::new();
        store_b
            .append(
                session_b,
                LogEntry::User {
                    content: vec![ContentBlock::Text {
                        text: "cross project needle".into(),
                        cache_control: None,
                    }],
                },
            )
            .await
            .unwrap();

        let found = store_a
            .search_all_project_session_summaries("needle", 10)
            .await
            .unwrap();

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].session_id, session_b);
    }

    #[tokio::test]
    async fn list_sessions_empty_dir() {
        let (store, _cwd, _proj) = make_store().await;
        let listed = store.list_sessions().await.unwrap();
        assert!(listed.is_empty());
    }

    #[tokio::test]
    async fn parse_error_carries_line_number() {
        let (store, _cwd, proj) = make_store().await;
        let s = SessionId::new();
        let dir = store.project_dir_path();
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let path = store.session_file_path(&s);

        // 第 1 行合法；第 2 行坏；第 3 行合法
        let id1 = base::id::Id::new();
        let id3 = base::id::Id::new();
        let line1 = serde_json::to_string(&json!({
            "v": 1, "id": id1, "ts": "2026-05-04T00:00:00Z",
            "session_id": s, "kind": "user", "content": []
        }))
        .unwrap();
        let line2 = "{ this is broken json";
        let line3 = serde_json::to_string(&json!({
            "v": 1, "id": id3, "ts": "2026-05-04T00:00:01Z",
            "session_id": s, "kind": "user", "content": []
        }))
        .unwrap();

        let blob = format!("{line1}\n{line2}\n{line3}\n");
        tokio::fs::write(&path, blob).await.unwrap();

        let err = store.load(s).await.unwrap_err();
        match err {
            HistoryError::Parse { line, .. } => assert_eq!(line, 2),
            other => panic!("expected Parse, got {other:?}"),
        }
        // store 仍可被 list（项目目录还在）
        let listed = store.list_sessions().await.unwrap();
        assert_eq!(listed, vec![s]);
        drop(proj);
    }

    #[tokio::test]
    async fn concurrent_appends_do_not_interleave_lines() {
        let (store, _cwd, _proj) = make_store().await;
        let s = SessionId::new();
        let store = Arc::new(store);

        let mut tasks = Vec::new();
        for i in 0..30u32 {
            let st = store.clone();
            tasks.push(tokio::spawn(async move {
                st.append(
                    s,
                    LogEntry::User {
                        content: vec![ContentBlock::Text {
                            text: format!("msg-{i}"),
                            cache_control: None,
                        }],
                    },
                )
                .await
                .unwrap();
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }

        let loaded = store.load(s).await.unwrap();
        assert_eq!(loaded.len(), 30);
        // 每行都能解析；顺序不保证（concurrent）但不应该出现 partial line 错
    }

    #[tokio::test]
    async fn delete_removes_session_file() {
        let (store, _cwd, _proj) = make_store().await;
        let s = SessionId::new();
        store
            .append(
                s,
                LogEntry::User {
                    content: vec![ContentBlock::Text {
                        text: "will be deleted".into(),
                        cache_control: None,
                    }],
                },
            )
            .await
            .unwrap();
        let path = store.session_file_path(&s);
        assert!(path.exists());
        store.delete(s).await.unwrap();
        assert!(!path.exists());
        // 再次删除不报错
        store.delete(s).await.unwrap();
    }

    #[tokio::test]
    async fn tool_result_roundtrip() {
        let (store, _cwd, _proj) = make_store().await;
        let s = SessionId::new();
        store
            .append(
                s,
                LogEntry::ToolResult {
                    tool_use_id: "toolu_01".into(),
                    content: ToolResultContent::Text("stdout".into()),
                    is_error: false,
                },
            )
            .await
            .unwrap();
        let loaded = store.load(s).await.unwrap();
        assert_eq!(loaded.len(), 1);
        match &loaded[0].entry {
            LogEntry::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                assert_eq!(tool_use_id, "toolu_01");
                assert_eq!(content, &ToolResultContent::Text("stdout".into()));
                assert!(!is_error);
            }
            _ => panic!(),
        }
    }

    #[tokio::test]
    async fn search_all_projects_finds_match() {
        let cwd_a = TempDir::new().unwrap();
        let cwd_b = TempDir::new().unwrap();
        let projects = TempDir::new().unwrap();
        let store_a =
            JsonlHistoryStore::with_roots(cwd_a.path(), HistoryRoots::under(projects.path()))
                .await
                .unwrap();
        let store_b =
            JsonlHistoryStore::with_roots(cwd_b.path(), HistoryRoots::under(projects.path()))
                .await
                .unwrap();

        let session_a = SessionId::new();
        store_a
            .append(
                session_a,
                LogEntry::User {
                    content: vec![ContentBlock::Text {
                        text: "project alpha unrelated".into(),
                        cache_control: None,
                    }],
                },
            )
            .await
            .unwrap();

        let session_b = SessionId::new();
        store_b
            .append(
                session_b,
                LogEntry::User {
                    content: vec![ContentBlock::Text {
                        text: "project beta with needle".into(),
                        cache_control: None,
                    }],
                },
            )
            .await
            .unwrap();

        let found = store_a
            .search_all_project_session_summaries("needle", 10)
            .await
            .unwrap();

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].session_id, session_b);
    }

    #[tokio::test]
    async fn search_empty_query_returns_all() {
        let (store, _cwd, _proj) = make_store().await;
        let a = SessionId::new();
        let b = SessionId::new();
        for (s, txt) in [(&a, "first session"), (&b, "second session")] {
            store
                .append(
                    *s,
                    LogEntry::User {
                        content: vec![ContentBlock::Text {
                            text: (*txt).into(),
                            cache_control: None,
                        }],
                    },
                )
                .await
                .unwrap();
        }

        let results = store.search_session_summaries("", 10).await.unwrap();

        assert_eq!(results.len(), 2);
        let mut ids: Vec<_> = results.iter().map(|s| s.session_id).collect();
        ids.sort_by_key(|id| id.to_string());
        let mut expected = vec![a, b];
        expected.sort_by_key(|id| id.to_string());
        assert_eq!(ids, expected);
    }

    #[tokio::test]
    async fn search_same_repo_filters_correctly() {
        let repo_dir = TempDir::new().unwrap();
        let outside_dir = TempDir::new().unwrap();
        let shared_projects = TempDir::new().unwrap();
        let config_home = TempDir::new().unwrap();

        // Init git in repo_dir
        let init_output = tokio::process::Command::new("git")
            .arg("init")
            .current_dir(repo_dir.path())
            .output()
            .await
            .unwrap();
        assert!(init_output.status.success());

        // Both stores share one global root, so a session written by either is
        // visible to the other — which is the thing this test filters on.
        let roots = HistoryRoots::under(config_home.path());
        let repo_store = JsonlHistoryStore::with_roots(repo_dir.path(), roots.clone())
            .await
            .unwrap();
        let outside_store = JsonlHistoryStore::with_roots(outside_dir.path(), roots.clone())
            .await
            .unwrap();

        // Create session in repo
        let session_in_repo = SessionId::new();
        repo_store
            .append(
                session_in_repo,
                LogEntry::User {
                    content: vec![ContentBlock::Text {
                        text: "inside repo work".into(),
                        cache_control: None,
                    }],
                },
            )
            .await
            .unwrap();

        // Create session outside repo
        let session_outside = SessionId::new();
        outside_store
            .append(
                session_outside,
                LogEntry::User {
                    content: vec![ContentBlock::Text {
                        text: "outside repo work".into(),
                        cache_control: None,
                    }],
                },
            )
            .await
            .unwrap();

        // Create metadata for in-repo session with canonicalized cwd
        let canonical_repo = tokio::fs::canonicalize(repo_dir.path()).await.unwrap();
        let sessions_root_dir = roots.sessions.clone();
        let in_repo_meta_dir = sessions_root_dir.join(session_in_repo.to_string());
        tokio::fs::create_dir_all(&in_repo_meta_dir).await.unwrap();
        let in_repo_meta = SessionMetadata::new(
            &canonical_repo,
            &shared_projects.path().join("in-repo"),
            session_in_repo,
        );
        tokio::fs::write(
            in_repo_meta_dir.join("metadata.json"),
            serde_json::to_string_pretty(&in_repo_meta).unwrap(),
        )
        .await
        .unwrap();

        // Create metadata for outside session with canonicalized cwd
        let canonical_outside = tokio::fs::canonicalize(outside_dir.path()).await.unwrap();
        let outside_meta_dir = sessions_root_dir.join(session_outside.to_string());
        tokio::fs::create_dir_all(&outside_meta_dir).await.unwrap();
        let outside_meta = SessionMetadata::new(
            &canonical_outside,
            &shared_projects.path().join("outside"),
            session_outside,
        );
        tokio::fs::write(
            outside_meta_dir.join("metadata.json"),
            serde_json::to_string_pretty(&outside_meta).unwrap(),
        )
        .await
        .unwrap();

        // Search from repo store with empty query to find all matching filter
        let found = repo_store
            .search_same_repo_session_summaries("", 10)
            .await
            .unwrap();

        // Only the in-repo session should be found
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].session_id, session_in_repo);
    }

    // -----------------------------------------------------------------------
    // Externalized content
    // -----------------------------------------------------------------------

    use crate::blob::{BlobStore, InMemoryBlobStore};
    use base::message::ImageSource;

    async fn make_store_with_blobs() -> (JsonlHistoryStore, TempDir, TempDir, TempDir) {
        let cwd_tmp = TempDir::new().unwrap();
        let projects_tmp = TempDir::new().unwrap();
        let paste_base = TempDir::new().unwrap();
        let store =
            JsonlHistoryStore::with_roots(cwd_tmp.path(), HistoryRoots::under(projects_tmp.path()))
                .await
                .unwrap()
                .with_paste_store(PasteStore::new(paste_base.path()));
        (store, cwd_tmp, projects_tmp, paste_base)
    }

    fn text_block(text: &str) -> ContentBlock {
        ContentBlock::Text {
            text: text.to_string(),
            cache_control: None,
        }
    }

    fn image_block() -> ContentBlock {
        ContentBlock::Image {
            source: ImageSource::Base64 {
                media_type: "image/png".into(),
                data: "iVBORw0KGgo=".into(),
            },
        }
    }

    #[tokio::test]
    async fn small_content_stays_in_the_log() {
        let (store, _cwd, _proj, _base) = make_store_with_blobs().await;
        let s = SessionId::new();
        store
            .append(
                s,
                LogEntry::User {
                    content: vec![text_block("short")],
                },
            )
            .await
            .unwrap();

        let loaded = store.load(s).await.unwrap();
        assert_eq!(loaded.len(), 1);
        assert!(
            matches!(loaded[0].entry, LogEntry::User { .. }),
            "expected User, got {:?}",
            loaded[0].entry
        );
    }

    #[tokio::test]
    async fn large_content_leaves_the_log_and_comes_back_on_load() {
        let (store, _cwd, _proj, _base) = make_store_with_blobs().await;
        let s = SessionId::new();
        let big_text = "X".repeat(1500);
        store
            .append(
                s,
                LogEntry::User {
                    content: vec![text_block(&big_text)],
                },
            )
            .await
            .unwrap();

        let loaded = store.load(s).await.unwrap();
        assert_eq!(loaded.len(), 1);
        match &loaded[0].entry {
            LogEntry::User { content } => match &content[0] {
                ContentBlock::Text { text, .. } => assert_eq!(text, &big_text),
                other => panic!("expected Text block, got {other:?}"),
            },
            other => panic!("expected User, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn the_jsonl_holds_a_reference_and_the_blob_holds_the_content() {
        let (store, _cwd, _proj, base) = make_store_with_blobs().await;
        let s = SessionId::new();
        let big_text = "Y".repeat(1500);
        store
            .append(
                s,
                LogEntry::User {
                    content: vec![text_block(&big_text)],
                },
            )
            .await
            .unwrap();

        let raw = tokio::fs::read_to_string(store.session_file_path(&s))
            .await
            .unwrap();
        assert!(raw.contains("\"kind\":\"blob\""), "raw jsonl: {raw}");
        assert!(
            !raw.contains(&big_text),
            "the body must not also be inline: {raw}"
        );

        let entries: Vec<EnvelopedEntry> = raw
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let LogEntry::Blob { blob } = &entries[0].entry else {
            panic!("expected a Blob reference, got {:?}", entries[0].entry);
        };
        assert_eq!(blob.store, "paste");
        assert_eq!(blob.describes, "user");
        assert!(blob.bytes > big_text.len() as u64);

        let stored = tokio::fs::read_to_string(base.path().join("pastes").join(&blob.id))
            .await
            .unwrap();
        assert!(stored.contains(&big_text));
    }

    #[tokio::test]
    async fn a_large_assistant_entry_keeps_every_field_across_the_round_trip() {
        let (store, _cwd, _proj, _base) = make_store_with_blobs().await;
        let s = SessionId::new();
        let big_text = "Z".repeat(1500);
        store
            .append(
                s,
                LogEntry::Assistant {
                    content: vec![text_block(&big_text)],
                    stop_reason: Some(StopReason::EndTurn),
                    usage: None,
                    model: Some("claude-sonnet-4-6".into()),
                },
            )
            .await
            .unwrap();

        let loaded = store.load(s).await.unwrap();
        match &loaded[0].entry {
            LogEntry::Assistant {
                content,
                stop_reason,
                model,
                ..
            } => {
                match &content[0] {
                    ContentBlock::Text { text, .. } => assert_eq!(text, &big_text),
                    other => panic!("expected Text block, got {other:?}"),
                }
                assert_eq!(*stop_reason, Some(StopReason::EndTurn));
                assert_eq!(model.as_deref(), Some("claude-sonnet-4-6"));
            }
            other => panic!("expected Assistant, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_big_tool_result_leaves_the_log_too() {
        let (store, _cwd, _proj, _base) = make_store_with_blobs().await;
        let s = SessionId::new();
        let output = "line\n".repeat(400);
        store
            .append(
                s,
                LogEntry::ToolResult {
                    tool_use_id: "toolu_01".into(),
                    content: ToolResultContent::Text(output.clone()),
                    is_error: false,
                },
            )
            .await
            .unwrap();

        let raw = tokio::fs::read_to_string(store.session_file_path(&s))
            .await
            .unwrap();
        assert!(
            raw.contains("\"describes\":\"tool_result\""),
            "raw jsonl: {raw}"
        );
        assert!(!raw.contains(&output));

        match &store.load(s).await.unwrap()[0].entry {
            LogEntry::ToolResult { content, .. } => {
                assert_eq!(content, &ToolResultContent::Text(output));
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    /// An image is bytes nobody reads as text and no search matches, so it
    /// leaves the log regardless of how small it is.
    #[tokio::test]
    async fn an_image_leaves_the_log_at_any_size() {
        let (store, _cwd, _proj, _base) = make_store_with_blobs().await;
        let s = SessionId::new();
        store
            .append(
                s,
                LogEntry::User {
                    content: vec![text_block("look"), image_block()],
                },
            )
            .await
            .unwrap();
        store
            .append(
                s,
                LogEntry::ToolResult {
                    tool_use_id: "toolu_02".into(),
                    content: ToolResultContent::Blocks(vec![image_block()]),
                    is_error: false,
                },
            )
            .await
            .unwrap();

        let raw = tokio::fs::read_to_string(store.session_file_path(&s))
            .await
            .unwrap();
        assert_eq!(
            raw.lines()
                .filter(|l| l.contains("\"kind\":\"blob\""))
                .count(),
            2,
            "both the message and the tool result carry an image: {raw}"
        );
        assert!(!raw.contains("iVBORw0KGgo="));

        let loaded = store.load(s).await.unwrap();
        assert!(matches!(loaded[0].entry, LogEntry::User { .. }));
        assert!(matches!(loaded[1].entry, LogEntry::ToolResult { .. }));
    }

    #[tokio::test]
    async fn load_messages_sees_hydrated_content() {
        let (store, _cwd, _proj, _base) = make_store_with_blobs().await;
        let s = SessionId::new();
        let big_text = "A".repeat(1500);
        store
            .append(
                s,
                LogEntry::User {
                    content: vec![text_block(&big_text)],
                },
            )
            .await
            .unwrap();

        let messages = store.load_messages(s).await.unwrap();
        assert_eq!(messages.len(), 1);
        match &messages[0] {
            base::message::Message::User { content } => match &content[0] {
                ContentBlock::Text { text, .. } => assert_eq!(text, &big_text),
                other => panic!("expected Text block, got {other:?}"),
            },
            other => panic!("expected User message, got {other:?}"),
        }
    }

    /// The acceptance: uninstall the blob backend and the old log still opens.
    #[tokio::test]
    async fn a_log_written_with_a_blob_backend_loads_without_one() {
        let cwd = TempDir::new().unwrap();
        let projects = TempDir::new().unwrap();
        let paste_base = TempDir::new().unwrap();
        let s = SessionId::new();

        let roots = || HistoryRoots::under(projects.path());
        let with_blobs = JsonlHistoryStore::with_roots(cwd.path(), roots())
            .await
            .unwrap()
            .with_paste_store(PasteStore::new(paste_base.path()));
        with_blobs
            .append(
                s,
                LogEntry::System {
                    subkind: crate::entry::SystemSubkind::Notice,
                    text: "before".into(),
                },
            )
            .await
            .unwrap();
        with_blobs
            .append(
                s,
                LogEntry::User {
                    content: vec![text_block(&"B".repeat(1500))],
                },
            )
            .await
            .unwrap();
        with_blobs
            .append(
                s,
                LogEntry::User {
                    content: vec![text_block("after")],
                },
            )
            .await
            .unwrap();

        for (label, store) in [
            (
                "nothing mounted",
                JsonlHistoryStore::with_roots(cwd.path(), roots())
                    .await
                    .unwrap(),
            ),
            (
                "a different store mounted",
                JsonlHistoryStore::with_roots(cwd.path(), roots())
                    .await
                    .unwrap()
                    .with_blob_store(Arc::new(InMemoryBlobStore::new())),
            ),
            (
                "the right store, emptied",
                JsonlHistoryStore::with_roots(cwd.path(), roots())
                    .await
                    .unwrap()
                    .with_paste_store(PasteStore::new(TempDir::new().unwrap().path())),
            ),
        ] {
            let loaded = store
                .load(s)
                .await
                .unwrap_or_else(|e| panic!("{label}: a session must still load: {e:?}"));
            assert_eq!(
                loaded.len(),
                3,
                "{label}: an unreachable entry is still an entry"
            );
            assert!(
                matches!(loaded[1].entry, LogEntry::Blob { .. }),
                "{label}: expected the reference left in place, got {:?}",
                loaded[1].entry
            );
            assert_eq!(
                store.load_messages(s).await.unwrap().len(),
                1,
                "{label}: an unresolved reference is not something the model sees"
            );
        }
    }

    /// A reference names its store, and a store that is not the one named
    /// does not get to answer for it — even when it happens to hold that id,
    /// which content addressing makes likely rather than exotic. Resolving a
    /// foreign id is how a backend with its own id space hands back the wrong
    /// content.
    #[tokio::test]
    async fn a_reference_is_only_resolved_by_the_store_it_names() {
        let cwd = TempDir::new().unwrap();
        let projects = TempDir::new().unwrap();
        let paste_base = TempDir::new().unwrap();
        let s = SessionId::new();
        let entry = LogEntry::User {
            content: vec![text_block(&"C".repeat(1500))],
        };

        let written =
            JsonlHistoryStore::with_roots(cwd.path(), HistoryRoots::under(projects.path()))
                .await
                .unwrap()
                .with_paste_store(PasteStore::new(paste_base.path()));
        written.append(s, entry.clone()).await.unwrap();

        let impostor = InMemoryBlobStore::new();
        impostor
            .put(serde_json::to_string(&entry).unwrap().as_bytes())
            .unwrap();
        let reader =
            JsonlHistoryStore::with_roots(cwd.path(), HistoryRoots::under(projects.path()))
                .await
                .unwrap()
                .with_blob_store(Arc::new(impostor));

        assert!(
            matches!(
                reader.load(s).await.unwrap()[0].entry,
                LogEntry::Blob { .. }
            ),
            "a store that was not named must leave the reference alone"
        );
    }

    /// The pre-`Blob` form is still read, and a missing paste no longer takes
    /// the session down with it.
    #[tokio::test]
    async fn an_old_paste_ref_line_still_hydrates_and_survives_its_paste_going_missing() {
        let cwd = TempDir::new().unwrap();
        let projects = TempDir::new().unwrap();
        let paste_base = TempDir::new().unwrap();
        let pastes = PasteStore::new(paste_base.path());
        let s = SessionId::new();

        let original = LogEntry::User {
            content: vec![text_block("what the paste held")],
        };
        let paste_id = pastes
            .store(&serde_json::to_string(&original).unwrap())
            .unwrap();
        let store = JsonlHistoryStore::with_roots(cwd.path(), HistoryRoots::under(projects.path()))
            .await
            .unwrap()
            .with_paste_store(pastes);
        store
            .append(
                s,
                LogEntry::PasteRef {
                    paste_id: paste_id.clone(),
                },
            )
            .await
            .unwrap();

        match &store.load(s).await.unwrap()[0].entry {
            LogEntry::User { content } => match &content[0] {
                ContentBlock::Text { text, .. } => assert_eq!(text, "what the paste held"),
                other => panic!("expected Text block, got {other:?}"),
            },
            other => panic!("expected the hydrated User entry, got {other:?}"),
        }

        std::fs::remove_file(paste_base.path().join("pastes").join(&paste_id)).unwrap();
        let loaded = store
            .load(s)
            .await
            .expect("a missing paste is a gap, not a broken session");
        assert!(matches!(loaded[0].entry, LogEntry::PasteRef { .. }));
    }
}

/// Watches entries go into the log.
///
/// Read-only, and read-only *in the types* rather than by agreement: the entry
/// arrives behind a shared reference and nothing is returned, so there is no
/// expression an observer can write that changes what was appended.
///
/// That is not caution, it is the append-only guarantee. Every part of the
/// engine that resumes, forks or replays a session reads position as meaning;
/// an observer that could rewrite an entry on its way past would make the log
/// a record of what something decided to record rather than of what happened.
///
/// Observers run *after* the append has succeeded, so the entry they are shown
/// is already durable. An observer that panics still takes its caller down —
/// it just cannot un-write anything first.
pub trait AppendObserver: Send + Sync {
    fn observed(&self, session: SessionId, entry: &LogEntry);
}

/// Any store, with observers on its appends.
///
/// A decorator rather than a method on the trait, so a backend does not have
/// to implement — or remember to call — anything to be observable, and so the
/// observation cannot be forgotten by whoever writes the next backend.
pub struct ObservedHistoryStore {
    inner: Arc<dyn HistoryStore>,
    observers: Vec<Arc<dyn AppendObserver>>,
}

impl ObservedHistoryStore {
    pub fn new(inner: Arc<dyn HistoryStore>, observers: Vec<Arc<dyn AppendObserver>>) -> Self {
        Self { inner, observers }
    }
}

#[async_trait]
impl HistoryStore for ObservedHistoryStore {
    async fn append(&self, session: SessionId, entry: LogEntry) -> Result<(), HistoryError> {
        // Written first. An observer sees what is on the record, not what was
        // proposed — a failed append is not an event.
        self.inner.append(session, entry.clone()).await?;
        for observer in &self.observers {
            observer.observed(session, &entry);
        }
        Ok(())
    }

    async fn append_tagged(
        &self,
        session: SessionId,
        entry: LogEntry,
        tags: crate::entry::EntryTags,
    ) -> Result<(), HistoryError> {
        self.inner
            .append_tagged(session, entry.clone(), tags)
            .await?;
        for observer in &self.observers {
            observer.observed(session, &entry);
        }
        Ok(())
    }

    async fn load(&self, session: SessionId) -> Result<Vec<EnvelopedEntry>, HistoryError> {
        self.inner.load(session).await
    }

    async fn list_sessions(&self) -> Result<Vec<SessionId>, HistoryError> {
        self.inner.list_sessions().await
    }

    async fn delete(&self, session: SessionId) -> Result<(), HistoryError> {
        self.inner.delete(session).await
    }

    async fn load_messages(
        &self,
        session: SessionId,
    ) -> Result<Vec<base::message::Message>, HistoryError> {
        self.inner.load_messages(session).await
    }

    async fn session_facts(&self, session: SessionId) -> Result<SessionFacts, HistoryError> {
        self.inner.session_facts(session).await
    }

    async fn set_session_title(
        &self,
        session: SessionId,
        title: Option<String>,
    ) -> Result<(), HistoryError> {
        self.inner.set_session_title(session, title).await
    }

    async fn session_stats(&self, session: SessionId) -> Result<SessionStats, HistoryError> {
        self.inner.session_stats(session).await
    }

    async fn session_parent(&self, session: SessionId) -> Result<Option<String>, HistoryError> {
        self.inner.session_parent(session).await
    }

    fn projection(&self) -> &dyn TranscriptProjection {
        self.inner.projection()
    }

    fn environment(&self) -> &dyn base::interface::environment::Environment {
        self.inner.environment()
    }

    async fn child_sessions(
        &self,
        parent_session_id: &str,
    ) -> Result<Vec<SessionId>, HistoryError> {
        self.inner.child_sessions(parent_session_id).await
    }

    async fn find_sessions(
        &self,
        query: &SessionQuery,
    ) -> Result<Vec<SessionSummary>, HistoryError> {
        self.inner.find_sessions(query).await
    }
}

/// The contract, exercised against every implementation.
///
/// One body per property, run once per backend. A behavior asserted here is a
/// promise [`HistoryStore`] makes, not a detail of how one backend keeps its
/// bytes — which is the distinction that lets a host swap the backend and
/// know what still holds.
#[cfg(test)]
mod contract_tests {
    use super::*;
    use crate::entry::SessionKind;
    use base::permission::PermissionMode;

    async fn jsonl() -> (
        Box<dyn HistoryStore>,
        Option<tempfile::TempDir>,
        tempfile::TempDir,
    ) {
        let cwd = tempfile::TempDir::new().unwrap();
        let projects = tempfile::TempDir::new().unwrap();
        let store = JsonlHistoryStore::with_roots(cwd.path(), HistoryRoots::under(projects.path()))
            .await
            .unwrap();
        (Box::new(store), Some(cwd), projects)
    }

    fn in_memory() -> Box<dyn HistoryStore> {
        Box::new(InMemoryHistoryStore::new())
    }

    fn user(text: &str) -> LogEntry {
        LogEntry::User {
            content: vec![base::message::ContentBlock::Text {
                text: text.to_string(),
                cache_control: None,
            }],
        }
    }

    fn meta(parent: Option<&str>) -> LogEntry {
        LogEntry::Meta {
            cwd: "/tmp".into(),
            started_at: time::OffsetDateTime::now_utc(),
            model: "test-model".into(),
            permission_mode: format!("{:?}", PermissionMode::Default),
            engine_version: "0.0.1".into(),
            agix_version: "0.0.1".into(),
            parent_session_id: parent.map(str::to_string),
            scene: None,
            project_root: None,
            session_kind: SessionKind::Primary,
            schema_version: crate::entry::CURRENT_META_SCHEMA_VERSION,
        }
    }

    fn meta_in(scene: &str, project_root: &str) -> LogEntry {
        match meta(None) {
            LogEntry::Meta {
                cwd,
                started_at,
                model,
                permission_mode,
                engine_version,
                agix_version,
                parent_session_id,
                session_kind,
                schema_version,
                ..
            } => LogEntry::Meta {
                cwd,
                started_at,
                model,
                permission_mode,
                engine_version,
                agix_version,
                parent_session_id,
                scene: Some(scene.to_string()),
                project_root: Some(project_root.to_string()),
                session_kind,
                schema_version,
            },
            other => other,
        }
    }

    /// Runs one property against both shipped backends, so a divergence is a
    /// failing test rather than a surprise at the call site.
    macro_rules! for_each_store {
        ($name:ident, |$store:ident| $body:block) => {
            #[tokio::test]
            async fn $name() {
                {
                    let (boxed, _cwd, _projects) = jsonl().await;
                    let $store: &dyn HistoryStore = boxed.as_ref();
                    $body
                }
                {
                    let boxed = in_memory();
                    let $store: &dyn HistoryStore = boxed.as_ref();
                    $body
                }
            }
        };
    }

    for_each_store!(entries_come_back_in_the_order_they_went_in, |store| {
        let s = SessionId::new();
        for i in 0..5 {
            store.append(s, user(&format!("m{i}"))).await.unwrap();
        }
        let loaded = store.load(s).await.unwrap();
        let texts: Vec<String> = loaded
            .iter()
            .filter_map(|e| match &e.entry {
                LogEntry::User { content } => content.iter().find_map(|b| match b {
                    base::message::ContentBlock::Text { text, .. } => Some(text.clone()),
                    _ => None,
                }),
                _ => None,
            })
            .collect();
        assert_eq!(texts, ["m0", "m1", "m2", "m3", "m4"]);
    });

    for_each_store!(
        a_session_that_was_never_written_is_not_an_empty_one,
        |store| {
            let err = store.load(SessionId::new()).await.unwrap_err();
            assert!(
                matches!(err, HistoryError::SessionNotFound(_)),
                "absent must be distinguishable from empty: {err:?}"
            );
        }
    );

    for_each_store!(listing_finds_every_written_session, |store| {
        let a = SessionId::new();
        let b = SessionId::new();
        store.append(a, user("hi")).await.unwrap();
        store.append(b, user("hi")).await.unwrap();
        let mut listed: Vec<String> = store
            .list_sessions()
            .await
            .unwrap()
            .iter()
            .map(SessionId::to_string)
            .collect();
        listed.sort();
        let mut want = vec![a.to_string(), b.to_string()];
        want.sort();
        assert_eq!(listed, want);
    });

    for_each_store!(deleting_forgets_the_session_and_is_idempotent, |store| {
        let s = SessionId::new();
        store.append(s, user("hi")).await.unwrap();
        store.delete(s).await.unwrap();
        assert!(matches!(
            store.load(s).await.unwrap_err(),
            HistoryError::SessionNotFound(_)
        ));
        store
            .delete(s)
            .await
            .expect("deleting what is not there is not an error");
    });

    for_each_store!(
        the_parent_link_is_readable_and_drives_the_child_walk,
        |store| {
            let parent = SessionId::new();
            let child = SessionId::new();
            let orphan = SessionId::new();
            store.append(parent, meta(None)).await.unwrap();
            store
                .append(child, meta(Some(&parent.to_string())))
                .await
                .unwrap();
            store.append(orphan, meta(None)).await.unwrap();

            assert_eq!(store.session_parent(parent).await.unwrap(), None);
            assert_eq!(
                store.session_parent(child).await.unwrap(),
                Some(parent.to_string())
            );
            assert_eq!(
                store.child_sessions(&parent.to_string()).await.unwrap(),
                vec![child]
            );
        }
    );

    for_each_store!(
        the_scene_and_project_a_session_was_created_under_are_readable,
        |store| {
            let s = SessionId::new();
            store.append(s, meta_in("chat", "/repo/a")).await.unwrap();
            store.append(s, user("hi")).await.unwrap();

            let facts = store.session_facts(s).await.unwrap();
            assert_eq!(facts.scene.as_deref(), Some("chat"));
            assert_eq!(facts.project_root.as_deref(), Some("/repo/a"));
            assert_eq!(facts.session_kind, SessionKind::Primary);
        }
    );

    // A pre-v2 log has no `scene`/`project_root` on its `Meta` line, and a
    // session that has not been written to has no `Meta` line at all. Both
    // answer `None` rather than an inferred value — inferring here is what
    // §3.4's scene check exists to refuse.
    for_each_store!(
        facts_a_log_does_not_record_are_absent_not_guessed,
        |store| {
            let written = SessionId::new();
            store.append(written, meta(None)).await.unwrap();
            let facts = store.session_facts(written).await.unwrap();
            assert_eq!(facts.scene, None);
            assert_eq!(facts.project_root, None);

            let never_written = SessionId::new();
            assert_eq!(
                store.session_facts(never_written).await.unwrap(),
                SessionFacts::default(),
                "a session with no log is not an error here, it is no facts"
            );
        }
    );

    for_each_store!(a_session_keeps_the_name_a_person_gave_it, |store| {
        let s = SessionId::new();
        store.append(s, meta(None)).await.unwrap();
        assert_eq!(store.session_stats(s).await.unwrap().title, None);

        store
            .set_session_title(s, Some("重构 daemon 装配".into()))
            .await
            .unwrap();
        assert_eq!(
            store.session_stats(s).await.unwrap().title.as_deref(),
            Some("重构 daemon 装配")
        );

        // Renaming twice is renaming, not accumulating.
        store
            .set_session_title(s, Some("second".into()))
            .await
            .unwrap();
        assert_eq!(
            store.session_stats(s).await.unwrap().title.as_deref(),
            Some("second")
        );

        store.set_session_title(s, None).await.unwrap();
        assert_eq!(
            store.session_stats(s).await.unwrap().title,
            None,
            "clearing the name puts the session back to whatever names it automatically"
        );
    });

    // A name is not an entry in the log — it is chosen after the fact and
    // changed after the fact, so naming a session must not disturb what was
    // said in it.
    for_each_store!(naming_a_session_does_not_touch_its_transcript, |store| {
        let s = SessionId::new();
        store.append(s, meta(None)).await.unwrap();
        store.append(s, user("hello")).await.unwrap();
        let before = store.load(s).await.unwrap().len();

        store
            .set_session_title(s, Some("named".into()))
            .await
            .unwrap();

        assert_eq!(store.load(s).await.unwrap().len(), before);
    });

    for_each_store!(an_extension_entry_comes_back_exactly_as_written, |store| {
        let s = SessionId::new();
        store.append(s, meta(None)).await.unwrap();
        store
            .append(
                s,
                LogEntry::Extension {
                    ns: "com.example.gone".into(),
                    event: "checkpoint".into(),
                    payload: serde_json::json!({"step": 3, "nested": {"a": [1, 2]}}),
                },
            )
            .await
            .unwrap();
        store.append(s, user("after")).await.unwrap();

        let loaded = store.load(s).await.unwrap();
        assert_eq!(loaded.len(), 3, "an unreadable entry is still an entry");
        let LogEntry::Extension { ns, event, payload } = &loaded[1].entry else {
            panic!(
                "expected the extension entry in position, got {:?}",
                loaded[1].entry
            );
        };
        assert_eq!(ns, "com.example.gone");
        assert_eq!(event, "checkpoint");
        assert_eq!(
            payload,
            &serde_json::json!({"step": 3, "nested": {"a": [1, 2]}})
        );

        assert_eq!(
            store.load_messages(s).await.unwrap().len(),
            1,
            "it must not have become something the model sees"
        );
    });

    for_each_store!(messages_are_projected_from_the_log, |store| {
        let s = SessionId::new();
        store.append(s, meta(None)).await.unwrap();
        store.append(s, user("hello")).await.unwrap();
        let messages = store.load_messages(s).await.unwrap();
        assert_eq!(messages.len(), 1, "Meta must not reach the model");
    });
}

#[cfg(test)]
mod append_observer_tests {
    use super::*;
    use base::message::ContentBlock;

    #[derive(Default)]
    struct Recording {
        seen: std::sync::Mutex<Vec<String>>,
    }

    impl AppendObserver for Recording {
        fn observed(&self, _session: SessionId, entry: &LogEntry) {
            let label = match entry {
                LogEntry::User { content } => content
                    .iter()
                    .find_map(|b| match b {
                        ContentBlock::Text { text, .. } => Some(text.clone()),
                        _ => None,
                    })
                    .unwrap_or_default(),
                other => format!("{other:?}"),
            };
            self.seen.lock().unwrap().push(label);
        }
    }

    fn user(text: &str) -> LogEntry {
        LogEntry::User {
            content: vec![ContentBlock::Text {
                text: text.to_string(),
                cache_control: None,
            }],
        }
    }

    /// The observer sees every append, in order — and the log is exactly what
    /// it would have been without one. Wrapping a store must be free of
    /// consequence to what it stores; that is the whole point of the seam
    /// being read-only.
    #[tokio::test]
    async fn observing_a_log_does_not_change_it() {
        let plain: Arc<dyn HistoryStore> = Arc::new(InMemoryHistoryStore::new());
        let watched_inner: Arc<dyn HistoryStore> = Arc::new(InMemoryHistoryStore::new());
        let observer = Arc::new(Recording::default());
        let watched = ObservedHistoryStore::new(watched_inner.clone(), vec![observer.clone()]);

        let a = SessionId::new();
        for text in ["one", "two", "three"] {
            plain.append(a, user(text)).await.unwrap();
            watched.append(a, user(text)).await.unwrap();
        }

        let plain_entries = plain.load(a).await.unwrap();
        let watched_entries = watched.load(a).await.unwrap();
        assert_eq!(plain_entries.len(), watched_entries.len());
        for (p, w) in plain_entries.iter().zip(&watched_entries) {
            assert_eq!(
                serde_json::to_value(&p.entry).unwrap(),
                serde_json::to_value(&w.entry).unwrap(),
                "an observed append must store the same bytes as an unobserved one"
            );
        }

        assert_eq!(
            observer.seen.lock().unwrap().as_slice(),
            ["one", "two", "three"],
            "the observer must see every append, in order"
        );
    }

    /// P3-8's acceptance: a store mounted with a different projection hands
    /// back a different conversation, and every reader of that store — resume,
    /// fork, search — sees the same one, because they all come through here.
    #[tokio::test]
    async fn a_store_projection_decides_what_the_model_reads_back() {
        let cwd = tempfile::tempdir().unwrap();
        let projects = tempfile::tempdir().unwrap();
        let s = SessionId::new();

        let entries = [
            LogEntry::User {
                content: vec![base::message::ContentBlock::Text {
                    text: "ship it".into(),
                    cache_control: None,
                }],
            },
            LogEntry::Extension {
                ns: "com.acme.deploy".into(),
                event: "finished".into(),
                payload: serde_json::json!({"version": "1.4.0"}),
            },
        ];

        let engine_rules =
            JsonlHistoryStore::with_roots(cwd.path(), HistoryRoots::under(projects.path()))
                .await
                .unwrap();
        for e in entries.iter().cloned() {
            engine_rules.append(s, e).await.unwrap();
        }
        assert_eq!(
            engine_rules.load_messages(s).await.unwrap().len(),
            1,
            "the engine's rules keep an extension's state out of the conversation"
        );

        let host_rules =
            JsonlHistoryStore::with_roots(cwd.path(), HistoryRoots::under(projects.path()))
                .await
                .unwrap()
                .with_projection(Arc::new(crate::transcript::ExtensionsAreVisible {
                    namespaces: vec!["com.acme.deploy".into()],
                }));
        let messages = host_rules.load_messages(s).await.unwrap();
        assert_eq!(messages.len(), 2);
        assert!(
            crate::transcript::render_search_text(&messages).contains("1.4.0"),
            "and the same log now carries the deploy into the conversation"
        );
    }

    /// A failed append is not an event. An observer that was told about one
    /// would be recording something that did not happen.
    #[tokio::test]
    async fn an_append_that_fails_is_not_observed() {
        struct AlwaysFails;

        #[async_trait]
        impl HistoryStore for AlwaysFails {
            async fn append(&self, _s: SessionId, _e: LogEntry) -> Result<(), HistoryError> {
                Err(HistoryError::Io(std::io::Error::other("disk is gone")))
            }
            async fn load(&self, s: SessionId) -> Result<Vec<EnvelopedEntry>, HistoryError> {
                Err(HistoryError::SessionNotFound(s.to_string()))
            }
            async fn list_sessions(&self) -> Result<Vec<SessionId>, HistoryError> {
                Ok(Vec::new())
            }
            async fn delete(&self, _s: SessionId) -> Result<(), HistoryError> {
                Ok(())
            }
        }

        let observer = Arc::new(Recording::default());
        let watched = ObservedHistoryStore::new(Arc::new(AlwaysFails), vec![observer.clone()]);
        assert!(watched
            .append(SessionId::new(), user("never"))
            .await
            .is_err());
        assert!(
            observer.seen.lock().unwrap().is_empty(),
            "an observer must not be told about a write that did not happen"
        );
    }

    /// Everything else passes through untouched — the decorator is about
    /// appends and must not become a place where reads acquire behavior.
    #[tokio::test]
    async fn every_other_operation_is_the_inner_store() {
        let inner: Arc<dyn HistoryStore> = Arc::new(InMemoryHistoryStore::new());
        let watched = ObservedHistoryStore::new(inner.clone(), vec![]);
        let s = SessionId::new();
        watched.append(s, user("hi")).await.unwrap();

        assert_eq!(watched.list_sessions().await.unwrap(), vec![s]);
        assert_eq!(watched.load_messages(s).await.unwrap().len(), 1);
        assert_eq!(watched.session_parent(s).await.unwrap(), None);
        watched.delete(s).await.unwrap();
        assert!(matches!(
            inner.load(s).await.unwrap_err(),
            HistoryError::SessionNotFound(_)
        ));
    }
}

/// The query contract, exercised against a scanning backend and an indexing
/// one.
///
/// The pair is the point: an index answers from something it built as entries
/// arrived, a scan answers by reading everything, and a caller must not be
/// able to tell which one it is talking to.
#[cfg(test)]
mod query_contract_tests {
    use super::*;
    use crate::transcript::render_search_text;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// What an index keeps per session so a search never opens a transcript.
    struct IndexRow {
        modified: Option<time::OffsetDateTime>,
        id: String,
        text_lower: String,
        summary: SessionSummary,
    }

    /// A backend that takes search over outright.
    ///
    /// Everything a query needs is derived once, on append, and the search
    /// path touches nothing else — which is what [`load_calls`] is counted to
    /// prove.
    struct IndexedHistoryStore {
        inner: Arc<InMemoryHistoryStore>,
        index: std::sync::Mutex<HashMap<SessionId, IndexRow>>,
        load_calls: AtomicUsize,
    }

    impl IndexedHistoryStore {
        fn new(inner: Arc<InMemoryHistoryStore>) -> Self {
            Self {
                inner,
                index: std::sync::Mutex::new(HashMap::new()),
                load_calls: AtomicUsize::new(0),
            }
        }

        fn load_calls(&self) -> usize {
            self.load_calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl HistoryStore for IndexedHistoryStore {
        async fn append(&self, session: SessionId, entry: LogEntry) -> Result<(), HistoryError> {
            self.inner.append(session, entry).await?;
            let entries = self.inner.load(session).await?;
            let messages = project_messages_with(&entries, self.projection());
            let row = IndexRow {
                modified: entries.iter().map(|env| env.ts).max(),
                id: session.to_string(),
                text_lower: render_search_text(&messages).to_lowercase(),
                summary: SessionSummary {
                    session_id: session,
                    last_modified: entries
                        .iter()
                        .map(|env| env.ts)
                        .max()
                        .map(format_ts)
                        .unwrap_or_else(unknown_time),
                    entry_count: entries.len(),
                    message_count: messages.len(),
                    preview: preview_messages(&messages, 140),
                    canonical_cwd: None,
                    title: None,
                    total_input_tokens: None,
                    total_output_tokens: None,
                    compact_count: 0,
                },
            };
            self.index
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(session, row);
            Ok(())
        }

        async fn load(&self, session: SessionId) -> Result<Vec<EnvelopedEntry>, HistoryError> {
            self.load_calls.fetch_add(1, Ordering::SeqCst);
            self.inner.load(session).await
        }

        async fn list_sessions(&self) -> Result<Vec<SessionId>, HistoryError> {
            self.inner.list_sessions().await
        }

        async fn delete(&self, session: SessionId) -> Result<(), HistoryError> {
            self.index
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&session);
            self.inner.delete(session).await
        }

        async fn find_sessions(
            &self,
            query: &SessionQuery,
        ) -> Result<Vec<SessionSummary>, HistoryError> {
            if query.limit == 0 {
                return Ok(Vec::new());
            }
            let lowered = query.needle().map(str::to_lowercase);
            let index = self.index.lock().unwrap_or_else(|e| e.into_inner());
            let mut hits: Vec<&IndexRow> = index
                .values()
                .filter(|row| match (query.needle(), &lowered) {
                    (Some(needle), Some(lowered)) => {
                        row.id.contains(needle) || row.text_lower.contains(lowered)
                    }
                    _ => true,
                })
                .collect();
            hits.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| b.id.cmp(&a.id)));
            hits.truncate(query.limit);
            Ok(hits.into_iter().map(|row| row.summary.clone()).collect())
        }
    }

    fn user(text: &str) -> LogEntry {
        LogEntry::User {
            content: vec![base::message::ContentBlock::Text {
                text: text.to_string(),
                cache_control: None,
            }],
        }
    }

    const CORPUS: [(&str, &str); 4] = [
        ("first", "planning the Postgres migration"),
        ("second", "renaming a struct"),
        ("third", "postgres again, and a deadlock"),
        ("fourth", "nothing to do with databases"),
    ];

    async fn fill(store: &dyn HistoryStore, ids: &[SessionId]) {
        for (id, (label, text)) in ids.iter().zip(CORPUS) {
            store.append(*id, user(label)).await.unwrap();
            store.append(*id, user(text)).await.unwrap();
        }
    }

    fn queries() -> Vec<SessionQuery> {
        vec![
            SessionQuery::recent(10),
            SessionQuery::recent(2),
            SessionQuery::matching("postgres", 10),
            SessionQuery::matching("  Postgres ", 10),
            SessionQuery::matching("deadlock", 10),
            SessionQuery::matching("nothing anyone wrote", 10),
            SessionQuery::matching("postgres", 1),
            SessionQuery::recent(0),
        ]
    }

    /// The acceptance: an index answers exactly what the scan would have, down
    /// to the order and the preview text, and answers it without reading a
    /// single transcript.
    #[tokio::test]
    async fn an_index_and_a_scan_are_indistinguishable_to_a_caller() {
        let shared = Arc::new(InMemoryHistoryStore::new());
        let indexed = IndexedHistoryStore::new(shared.clone());
        let ids: Vec<SessionId> = (0..CORPUS.len()).map(|_| SessionId::new()).collect();
        // Written once, through the indexing backend, so both sides see the
        // same envelopes — two appends of the same entry get two timestamps.
        fill(&indexed, &ids).await;

        let before = indexed.load_calls();
        for query in queries() {
            let scanned = shared.find_sessions(&query).await.unwrap();
            let from_index = indexed.find_sessions(&query).await.unwrap();
            assert_eq!(
                from_index, scanned,
                "the two backends disagree on {query:?}"
            );
        }
        assert_eq!(
            indexed.load_calls(),
            before,
            "an index that reads transcripts to answer a query is not an index"
        );
    }

    /// A ceiling the caller sets is a ceiling both backends keep.
    #[tokio::test]
    async fn a_limit_is_a_ceiling_and_zero_means_nothing() {
        let shared = Arc::new(InMemoryHistoryStore::new());
        let indexed = IndexedHistoryStore::new(shared.clone());
        let ids: Vec<SessionId> = (0..CORPUS.len()).map(|_| SessionId::new()).collect();
        fill(&indexed, &ids).await;

        for store in [&*shared as &dyn HistoryStore, &indexed as &dyn HistoryStore] {
            assert_eq!(
                store.find_sessions(&SessionQuery::recent(0)).await.unwrap(),
                vec![]
            );
            assert_eq!(
                store
                    .find_sessions(&SessionQuery::recent(2))
                    .await
                    .unwrap()
                    .len(),
                2
            );
            assert_eq!(
                store
                    .find_sessions(&SessionQuery::recent(99))
                    .await
                    .unwrap()
                    .len(),
                CORPUS.len()
            );
        }
    }

    /// Give each session file a distinct mtime, in the order they were
    /// written.
    ///
    /// The two backends order by different clocks — a file's mtime and an
    /// entry's timestamp — and only one of them has a resolution this test
    /// controls. On a filesystem whose mtime granularity is coarser than the
    /// loop that wrote these files (ext4 with small inodes rounds to the
    /// second), all four land on the same mtime, the file-backed store falls
    /// through to its id tie-break, and a *limited* query then answers with a
    /// different set rather than a different order. That is the tie the test
    /// below says it tolerates in ordering — but a set difference is not an
    /// ordering difference, and the assertion is about the set.
    fn stamp_distinct_mtimes(store: &JsonlHistoryStore, ids: &[SessionId]) {
        let base = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        for (i, id) in ids.iter().enumerate() {
            let path = store.session_file_path(id);
            let when = base + std::time::Duration::from_secs(10 * (i as u64 + 1));
            let file = std::fs::File::options()
                .write(true)
                .open(&path)
                .expect("the session file this test just wrote");
            file.set_times(std::fs::FileTimes::new().set_modified(when))
                .expect("the temp filesystem allows setting mtimes");
        }
    }

    /// The two shipped backends have different notions of "when was this last
    /// touched" — a file's mtime and an entry's timestamp — so their orders
    /// can differ by a tie. Which sessions match cannot.
    #[tokio::test]
    async fn the_shipped_backends_agree_on_which_sessions_match() {
        let cwd = tempfile::TempDir::new().unwrap();
        let projects = tempfile::TempDir::new().unwrap();
        let jsonl = JsonlHistoryStore::with_roots(cwd.path(), HistoryRoots::under(projects.path()))
            .await
            .unwrap();
        let memory = InMemoryHistoryStore::new();
        let ids: Vec<SessionId> = (0..CORPUS.len()).map(|_| SessionId::new()).collect();
        fill(&jsonl, &ids).await;
        fill(&memory, &ids).await;
        stamp_distinct_mtimes(&jsonl, &ids);

        for query in queries() {
            let matched = |summaries: Vec<SessionSummary>| {
                let mut ids: Vec<String> = summaries
                    .into_iter()
                    .map(|s| s.session_id.to_string())
                    .collect();
                ids.sort();
                ids
            };
            let from_files = matched(jsonl.find_sessions(&query).await.unwrap());
            let from_memory = matched(memory.find_sessions(&query).await.unwrap());
            assert_eq!(
                from_files, from_memory,
                "the shipped backends disagree on {query:?}"
            );
        }
    }
}
