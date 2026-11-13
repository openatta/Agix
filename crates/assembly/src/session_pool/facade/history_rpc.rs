//! 历史相关 RPC：读取历史、导出会话、分叉会话，以及条目的视图转换。

use crate::session_pool::*;

impl SessionPool {
    /// `session.history` — a bounded page of a session's projected
    /// transcript.
    ///
    /// Reads the on-disk log, not the live `Agent`'s in-memory message list:
    /// the store is the only view that exists for an *inactive* session
    /// (the whole point — an IDE that restarted has nothing in memory), and
    /// for an active one it's the same content up to the last completed step
    /// (the transcript is written at every step boundary). A step currently in
    /// flight therefore isn't visible here until it completes; `active` in
    /// the response tells a caller when that caveat applies.
    ///
    /// Paging is `offset`/`limit` over **projected messages** (what
    /// `history::transcript::project_messages` produces — compaction
    /// boundaries already applied, metadata/sidechain entries already
    /// dropped), not raw log entries, so the indices line up with what the
    /// client renders. `total` always reports the full count regardless of
    /// the page, so a client wanting the tail of a long session can ask for
    /// `limit: 0` first (cheap, no message payload) and then request
    /// `offset: total - n`.
    pub async fn session_history(
        &self,
        session_id: &str,
        offset: usize,
        limit: usize,
    ) -> Result<api::HistoryPage, PoolError> {
        let entries = self.directory.load_entries(session_id).await?;
        let messages = history::transcript::project_messages_sourced(
            &entries,
            self.directory.require_store()?.projection(),
        );
        let total = messages.len();
        let start = offset.min(total);
        let end = start.saturating_add(limit).min(total);
        let page = messages[start..end]
            .iter()
            .enumerate()
            .map(|(i, m)| history_item(start + i, m))
            .collect();
        let active = self.live.contains(session_id).await;

        Ok(api::HistoryPage {
            session_id: session_id.to_string(),
            messages: page,
            total,
            offset: start,
            limit,
            has_more: end < total,
            entry_count: entries.len(),
            active,
        })
    }

    /// `session.export` — the session, its sidechains and what they refer to,
    /// as one archive at `request.path` (see `crate::export`).
    ///
    /// Read-only: nothing is resumed or created, and a session in another
    /// scene exports like any other. The logs are cut at one moment, under
    /// this session's lock so a cascade delete cannot run through the middle
    /// of the cut; a turn running meanwhile carries on, and the archive holds
    /// what it had written by its last completed step. The recording, when
    /// asked for, is measured in the same moment.
    pub async fn export_session(
        &self,
        request: crate::export::ExportRequest,
    ) -> Result<api::ExportSummary, PoolError> {
        let store = self.directory.require_store()?.clone();
        let root = base::session::SessionId::parse(&request.session_id).map_err(|e| {
            PoolError::InvalidRequest(format!("invalid session_id `{}`: {e}", request.session_id))
        })?;
        let settings = self.bindings.pool_settings().await;
        let roots = history::path::HistoryRoots::under(&settings.paths.global_data_dir);
        crate::export::check_target(&request.path, &[roots.projects, roots.sessions.clone()])?;
        let include = request.include;
        let (lineage, cut_at, live, recordings) = self
            .live
            .with_session_lock(&request.session_id, async {
                let mut lineage = store.snapshot_lineage(root).await.map_err(|e| match e {
                    history::error::HistoryError::SessionNotFound(_) => {
                        PoolError::session_not_found(&request.session_id)
                    }
                    other => {
                        PoolError::Internal(format!("failed to read session history: {other}"))
                    }
                })?;
                let cut_at = store.environment().now();
                if !include.sidechains {
                    lineage.sessions.truncate(1);
                }
                let mut live: HashMap<String, (api::TurnState, Arc<base::context::SessionState>)> =
                    HashMap::new();
                for (sid, _) in &lineage.sessions {
                    let sid = sid.to_string();
                    let found = self
                        .live
                        .with_live(&sid, |live| (live.turn.state(), live.core.state().clone()))
                        .await;
                    if let Some(found) = found {
                        live.insert(sid, found);
                    }
                }
                let recordings = if include.recording {
                    self.measure_recordings(&settings, &request.session_id, &lineage)
                        .await
                } else {
                    Vec::new()
                };
                Ok::<_, PoolError>((lineage, cut_at, live, recordings))
            })
            .await?;

        let mut warnings = Vec::new();
        if include.recording && recordings.is_empty() {
            warnings.push("no recording was found for these sessions".to_string());
        }
        let mut sessions = Vec::with_capacity(lineage.sessions.len());
        for (sid, entries) in lineage.sessions {
            let id = sid.to_string();
            let (history, extension_entries) = export_views(&entries, store.projection())
                .map_err(|e| PoolError::Internal(format!("failed to serialize transcript: {e}")))?;
            let stats = store.session_stats(sid).await.unwrap_or_default();
            let metadata: Option<history::project::SessionMetadata> = tokio::fs::read_to_string(
                history::path::session_metadata_file(&roots.sessions, &sid),
            )
            .await
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok());
            let memory = tokio::fs::read_to_string(history::path::session_memory_file(
                &roots.sessions,
                &sid,
            ))
            .await
            .ok();
            let in_memory = live.get(&id).cloned();
            let turn_state = in_memory
                .as_ref()
                .map_or(api::TurnState::Idle, |(turn, _)| *turn);
            let runtime = in_memory.map(|(_, state)| {
                serde_json::json!({
                    "durable": false,
                    "permission_mode": state.permission_mode(),
                    "todos": state.todos(),
                    "goal": state.goal_view(),
                })
            });
            sessions.push(crate::export::SessionPart {
                session_id: sid,
                entries,
                history,
                extension_entries,
                title: stats.title,
                tags: metadata
                    .as_ref()
                    .map(|m| m.tags.clone())
                    .unwrap_or_default(),
                total_input_tokens: metadata.as_ref().and_then(|m| m.total_input_tokens),
                total_output_tokens: metadata.as_ref().and_then(|m| m.total_output_tokens),
                compact_count: stats.compact_count,
                memory,
                active: live.contains_key(&id),
                turn_state,
                runtime,
            });
        }
        let snapshot = crate::export::ExportSnapshot {
            root,
            cut_at,
            sessions,
            forks: lineage.forks,
            recordings,
            warnings,
        };
        tokio::task::spawn_blocking(move || crate::export::write(&request, snapshot))
            .await
            .map_err(|e| PoolError::Internal(format!("export did not finish: {e}")))?
            .map_err(PoolError::from)
    }

    /// The recordings these sessions write to, with how long each one's
    /// `calls.jsonl` is right now.
    ///
    /// A recording is named by its `recorder.name`, or else by the session
    /// whose calls it holds, under the root the session's recorder named or
    /// the daemon's own. `recorder` is never written to the log, so for a
    /// session this process did not build only the daemon's root and the
    /// session ids are known.
    pub(in crate::session_pool) async fn measure_recordings(
        &self,
        settings: &base::settings::LoadedSettings,
        root_session: &str,
        lineage: &history::store::LineageSnapshot,
    ) -> Vec<crate::export::RecordingPart> {
        let recorder = self
            .specs
            .get(root_session)
            .and_then(|spec| spec.options?.recorder);
        let mut dirs = vec![recordings_root(
            settings,
            recorder.as_ref().and_then(|r| r.dir.as_deref()),
        )];
        let default_root = recordings_root(settings, None);
        if !dirs.contains(&default_root) {
            dirs.push(default_root);
        }
        let ids: Vec<String> = lineage
            .sessions
            .iter()
            .map(|(sid, _)| sid.to_string())
            .collect();
        let mut names: Vec<(String, Vec<String>)> = Vec::new();
        if let Some(name) = recorder.as_ref().and_then(|r| r.name.clone()) {
            names.push((name, ids.clone()));
        }
        names.extend(ids.iter().map(|id| (id.clone(), vec![id.clone()])));

        let mut found: Vec<crate::export::RecordingPart> = Vec::new();
        for (name, sessions) in names {
            let one_component = matches!(
                Path::new(&name).components().collect::<Vec<_>>().as_slice(),
                [std::path::Component::Normal(_)]
            );
            if !one_component || found.iter().any(|r| r.name == name) {
                continue;
            }
            for root in &dirs {
                let dir = root.join(&name);
                if let Ok(meta) = tokio::fs::metadata(dir.join("calls.jsonl")).await {
                    found.push(crate::export::RecordingPart {
                        name: name.clone(),
                        dir,
                        calls_len: meta.len(),
                        sessions: sessions.clone(),
                    });
                    break;
                }
            }
        }
        found
    }

    /// `session.fork` — branch a session at a message boundary into a new
    /// session id whose history is a copy of everything up to that point.
    ///
    /// `at_message` counts **projected messages** (same coordinates
    /// `session.history` returns); `None` forks the whole transcript. The cut
    /// is resolved to the shortest *entry* prefix whose projection already
    /// holds that many messages (see `history::transcript::projected_lengths`
    /// — a `Compact` entry can shrink the running count, so this is a scan,
    /// not arithmetic on entry kinds).
    ///
    /// The fork is written to disk and is **not** made live in the pool:
    /// nothing about the parent is touched, no engine is spawned, and the
    /// new id behaves exactly like any other id with history — the next
    /// `session.resume`/`session.run_turn` carrying it resumes the copy.
    /// That's what makes the two independent: writes to the fork append to a
    /// different jsonl file.
    ///
    /// Two deliberate lossy points, both invisible to the projected view:
    /// sidechain entries (sub-agent side conversations) are dropped, because
    /// `HistoryStore::append` takes a `LogEntry` and can't carry the
    /// envelope's `is_sidechain`/`parent_id` flags through; and the source's
    /// `Meta` line is replaced with a fresh one recording
    /// `parent_session_id`, which is what makes the lineage queryable via
    /// `HistoryStore::child_sessions`.
    ///
    /// The fork records `session_kind: Primary`, and everything that walks
    /// the child list has to honour that. `parent_session_id` answers "where
    /// did this come from", which a sidechain and a fork answer the same way
    /// while meaning opposite things about who may delete whom.
    pub async fn fork_session(
        &self,
        session_id: &str,
        at_message: Option<usize>,
    ) -> Result<api::ForkReport, PoolError> {
        self.refuse_scene_mismatch(session_id).await?;
        let entries = self.directory.load_entries(session_id).await?;
        let store = self.directory.require_store()?;

        let lengths = history::transcript::projected_lengths_with(&entries, store.projection());
        let total_messages = lengths.last().copied().unwrap_or(0);
        let target = at_message.unwrap_or(total_messages).min(total_messages);
        // Shortest prefix already holding `target` messages. `target == 0`
        // keeps nothing (a fork of the bare session start), which is a legal
        // request: "give me this conversation's setup with none of its turns".
        // A fork of the whole conversation keeps what follows its last message
        // too: a host's entries written after the final turn.
        let cut = if target >= total_messages {
            entries.len()
        } else if target == 0 {
            0
        } else {
            lengths
                .iter()
                .position(|&len| len >= target)
                .map(|i| i + 1)
                .unwrap_or(entries.len())
        };

        let new_sid = base::session::SessionId::new();
        // Fork inherits the source session's own recorded scene and project
        // when known (pre-v2 sources have neither) rather than the pool's
        // defaults, since a source session may live under a different
        // project or scene.
        let (source_scene, source_project_root, source_schema_version) = entries
            .iter()
            .find_map(|env| match &env.entry {
                history::entry::LogEntry::Meta {
                    scene,
                    project_root,
                    schema_version,
                    ..
                } => Some((scene.clone(), project_root.clone(), *schema_version)),
                _ => None,
            })
            .unwrap_or((None, None, 1));
        let project_root_for_fork = if source_schema_version >= 2 {
            source_project_root.map(PathBuf::from)
        } else {
            Some(self.cwd().to_path_buf())
        };
        // A fork inherits the source session's scene, so its settings must be
        // resolved against that scene's tier — not the daemon's startup one.
        let fork_scene = match source_scene.clone() {
            Some(s) => s,
            None => self.bindings.startup_scene().await,
        };
        let settings = match &project_root_for_fork {
            Some(p) => self.settings_for_project(Some(p), &fork_scene).await,
            None => self.settings_for_project(None, &fork_scene).await,
        };
        let meta = history::entry::LogEntry::Meta {
            cwd: project_root_for_fork
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| settings.paths.global_data_dir.display().to_string()),
            started_at: store.environment().now(),
            model: settings.values.model.model_name.clone(),
            permission_mode: serde_json::to_value(settings.values.permission_mode)
                .ok()
                .and_then(|v| v.as_str().map(|s| s.to_string()))
                .unwrap_or_else(|| "default".to_string()),
            engine_version: env!("CARGO_PKG_VERSION").to_string(),
            agix_version: env!("CARGO_PKG_VERSION").to_string(),
            parent_session_id: Some(session_id.to_string()),
            scene: Some(source_scene.unwrap_or_else(|| self.scenes.default_id().to_string())),
            project_root: project_root_for_fork.map(|p| p.display().to_string()),
            session_kind: history::entry::SessionKind::Primary,
            schema_version: history::entry::CURRENT_META_SCHEMA_VERSION,
        };

        // Best-effort cleanup on a partial copy: a half-written fork that a
        // client then resumes would silently present a truncated
        // conversation as if it were complete. Deleting is safer than
        // leaving it, and the id was minted here so nothing else can be
        // referencing it yet.
        let mut copied = 0usize;
        if let Err(e) = store.append(new_sid, meta).await {
            let _ = store.delete(new_sid).await;
            return Err(PoolError::Internal(format!(
                "failed to write forked session metadata: {e}"
            )));
        }
        for env in entries.iter().take(cut) {
            if env.is_sidechain || matches!(env.entry, history::entry::LogEntry::Meta { .. }) {
                continue;
            }
            let tags = history::entry::EntryTags {
                turn_id: env.turn_id.clone(),
                origin: env.origin,
            };
            if let Err(e) = store.append_tagged(new_sid, env.entry.clone(), tags).await {
                let _ = store.delete(new_sid).await;
                return Err(PoolError::Internal(format!(
                    "failed to copy session history into the fork: {e}"
                )));
            }
            copied += 1;
        }

        let message_count =
            history::transcript::project_messages_with(&entries[..cut], store.projection()).len();
        info!(
            source = %session_id,
            fork = %new_sid,
            at_message = message_count,
            "session forked"
        );
        Ok(api::ForkReport {
            session_id: new_sid.to_string(),
            parent_session_id: session_id.to_string(),
            forked_at_message: message_count,
            message_count,
            entry_count: copied,
            source_message_count: total_messages,
            active: false,
        })
    }
}

/// One extension entry as `session.entries` and the `extension_entry` frame
/// show it. `at` counts the messages before it, in `session.history`'s
/// coordinates.
pub(in crate::session_pool) fn entry_view(
    index: usize,
    env: &history::entry::EnvelopedEntry,
    lengths: &[usize],
) -> Option<api::EntryView> {
    if env.is_sidechain {
        return None;
    }
    let history::entry::LogEntry::Extension { ns, event, payload } = &env.entry else {
        return None;
    };
    let at = index
        .checked_sub(1)
        .map_or(0, |i| lengths.get(i).copied().unwrap_or(0));
    let ts = env
        .ts
        .format(&time::format_description::well_known::Rfc3339)
        .ok()?;
    Some(api::EntryView {
        id: env.id.to_string(),
        ts,
        ns: ns.clone(),
        event: event.clone(),
        payload: payload.clone(),
        at,
    })
}

/// A whole transcript as `session.history` pages it and the rows
/// `session.entries` lists, for an export to carry beside the raw log.
pub(in crate::session_pool) fn export_views(
    entries: &[history::entry::EnvelopedEntry],
    projection: &dyn history::transcript::TranscriptProjection,
) -> Result<(Vec<serde_json::Value>, Vec<serde_json::Value>), serde_json::Error> {
    let history = history::transcript::project_messages_sourced(entries, projection)
        .iter()
        .enumerate()
        .map(|(seq, m)| serde_json::to_value(history_item(seq, m)))
        .collect::<Result<Vec<_>, _>>()?;
    let lengths = history::transcript::projected_lengths_with(entries, projection);
    let rows = entries
        .iter()
        .enumerate()
        .filter(|(_, env)| {
            matches!(&env.entry, history::entry::LogEntry::Extension { ns, .. }
                if !ns.starts_with(ENGINE_NS_PREFIX))
        })
        .filter_map(|(i, env)| entry_view(i, env, &lengths))
        .map(serde_json::to_value)
        .collect::<Result<Vec<_>, _>>()?;
    Ok((history, rows))
}

/// One `session.history` item: the projected message as it always was
/// (`role`, `content`, …) plus where it sits and where it came from.
pub(in crate::session_pool) fn history_item(
    seq: usize,
    sourced: &history::transcript::SourcedMessage<'_>,
) -> api::HistoryItem {
    use api::HistoryOrigin;
    use history::entry::{LogEntry, MessageOrigin};
    let env = sourced.entry;
    let id = match sourced.part {
        Some(part) => format!("{}#{part}", env.id),
        None => env.id.to_string(),
    };
    let origin = match (&sourced.message, sourced.part, &env.entry) {
        (base::message::Message::Assistant { .. }, _, _) => Some(HistoryOrigin::Assistant),
        (_, Some(_), _) => Some(HistoryOrigin::Compact),
        (_, None, LogEntry::ToolResult { .. }) => Some(HistoryOrigin::ToolResult),
        (_, None, LogEntry::Extension { .. }) => Some(HistoryOrigin::Extension),
        _ => env.origin.map(|origin| match origin {
            MessageOrigin::User => HistoryOrigin::User,
            MessageOrigin::Steer => HistoryOrigin::Steer,
            MessageOrigin::Command => HistoryOrigin::Command,
            MessageOrigin::Injected => HistoryOrigin::Injected,
            MessageOrigin::ToolResult => HistoryOrigin::ToolResult,
        }),
    };
    let usage = match (&env.entry, sourced.part) {
        (LogEntry::Assistant { usage: Some(u), .. }, None) => Some(api::HistoryUsage::from(u)),
        _ => None,
    };
    let ts = env
        .ts
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default();
    api::HistoryItem {
        message: sourced.message.clone(),
        seq,
        id,
        ts,
        turn_id: env.turn_id.clone(),
        origin,
        usage,
    }
}
