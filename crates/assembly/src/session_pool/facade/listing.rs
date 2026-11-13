//! 会话查询：详情、列表、冷会话信息、场景与配置层说明、已服务项目的记录。

use crate::session_pool::*;

impl SessionPool {
    /// A session's recorded `session_kind`/`parent_session_id`/`resumable`,
    /// read off its `Meta` line (and terminal marker, for `resumable`).
    /// Sessions with no `Meta` at all (files written before the daemon wrote
    /// one — see `create()`) default to `(Primary, None, true)`. `resumable` is `false` only for a sidechain that has a
    /// `LogEntry::SessionEnd` marker —
    /// primary sessions and still-running/externally-cut-off sidechains are
    /// always `true`. One `load_entries` scan covers both, since listing
    /// every session would otherwise pay for the same disk read twice.
    pub(in crate::session_pool) async fn session_facts_and_resumable(
        &self,
        session_id: &str,
    ) -> (history::store::SessionFacts, bool) {
        let facts = self.directory.facts_of(session_id).await;
        // Only a sidechain can be non-resumable, and only the whole log can
        // say whether it ended — so only a sidechain pays for reading it.
        if !matches!(facts.session_kind, history::entry::SessionKind::Sidechain) {
            return (facts, true);
        }
        let ended = self
            .directory
            .load_entries(session_id)
            .await
            .map(|entries| {
                entries
                    .iter()
                    .any(|env| matches!(env.entry, history::entry::LogEntry::SessionEnd { .. }))
            })
            .unwrap_or(false);
        (facts, !ended)
    }

    /// `scene.describe {scene, project_root, include_secrets}` — what this
    /// scene is and what a session in it would run with.
    ///
    /// Reports what the daemon can actually answer: the scene's capability
    /// bits, its tool surface, and the settings a session created here would
    /// resolve to. It deliberately does **not** report which tier each field
    /// came from — settings are merged tier by tier and the provenance is not
    /// retained, so a `sources` map would have to be reconstructed by
    /// re-reading and re-merging every tier. That is a feature, not a field.
    pub async fn describe_scene(
        &self,
        scene_id: &str,
        project_root: Option<&Path>,
        include_secrets: bool,
    ) -> Result<api::SceneDescription, PoolError> {
        let Some(scene) = self.scenes.registry().resolve(scene_id) else {
            return Err(PoolError::SceneNotFound {
                scene: scene_id.to_string(),
            });
        };
        let settings = self.settings_for_project(project_root, scene_id).await;
        let mut settings_json = effective_settings_json(&settings);
        if !include_secrets {
            redact_settings_secrets(&mut settings_json);
        }

        Ok(api::SceneDescription {
            scene: scene.id.clone(),
            name: scene.name.clone(),
            description: scene.description.clone(),
            project_root: project_root.map(|p| p.display().to_string()),
            active: self.scenes.is_active(scene_id),
            capabilities: api::SceneCapabilities {
                requires_project: scene.requires_project,
                supports_team: scene.supports_team,
            },
            tools: api::SceneTools {
                allowed: scene
                    .tools
                    .allow
                    .as_ref()
                    .map(|allow| allow.iter().map(ToString::to_string).collect()),
                disallowed: scene.tools.deny.iter().map(ToString::to_string).collect(),
                deferred: scene.tools.deferred.clone(),
            },
            settings: settings_json,
        })
    }

    /// `config.get {scene, project_root, tier}` — one settings tier as it
    /// sits on disk, or `effective` for the merged result.
    ///
    /// A tier that has no file reads as `null`, which is the honest answer:
    /// "nothing at this layer" and "an empty object here" are different
    /// states and a client showing the config should be able to tell them
    /// apart.
    pub async fn config_tier(
        &self,
        scene_id: &str,
        project_root: Option<&Path>,
        tier: api::ConfigTierName,
        include_secrets: bool,
    ) -> Result<api::ConfigTier, PoolError> {
        use api::ConfigTierName as Tier;
        if self.scenes.registry().resolve(scene_id).is_none() {
            return Err(PoolError::SceneNotFound {
                scene: scene_id.to_string(),
            });
        }

        let paths = self.bindings.paths().as_ref();
        let path = match tier {
            Tier::Global => Some(paths.global_root().join("settings.json")),
            Tier::Scene => Some(
                paths
                    .global_root()
                    .join("scenes")
                    .join(scene_id)
                    .join("settings.json"),
            ),
            Tier::Project => project_root.map(|p| p.join(".agix").join("settings.json")),
            Tier::Effective => None,
        };

        let mut managed = serde_json::Value::Null;
        let mut value = match tier {
            Tier::Effective => {
                let settings = self.settings_for_project(project_root, scene_id).await;
                managed = serde_json::to_value(&settings.managed).unwrap_or_default();
                effective_settings_json(&settings)
            }
            _ => match path {
                Some(p) => tokio::fs::read_to_string(&p)
                    .await
                    .ok()
                    .and_then(|raw| serde_json::from_str(&raw).ok())
                    .unwrap_or(serde_json::Value::Null),
                // `project` with no project: there is no such tier to read.
                None => serde_json::Value::Null,
            },
        };
        if !include_secrets {
            redact_settings_secrets(&mut value);
        }

        Ok(api::ConfigTier {
            scene: scene_id.to_string(),
            project_root: project_root.map(|p| p.display().to_string()),
            tier,
            settings: value,
            managed,
        })
    }

    /// One session's summary plus what only the live pool knows.
    ///
    /// Built from `list_all` rather than a second, parallel assembly: a
    /// `session.get` that disagreed with the `session.list` entry for the
    /// same session would be worse than not having it.
    pub async fn session_detail(&self, session_id: &str) -> Result<api::SessionDetail, PoolError> {
        let listed = self
            .list_all(true, None)
            .await
            .into_iter()
            .find(|s| s.session_id == session_id);
        let info = match listed {
            Some(info) => info,
            None => self.cold_session_info(session_id).await?,
        };

        // The entry's own `scene`, not a second lookup: `session.get`
        // disagreeing with the `session.list` entry for the same session is
        // the failure this whole assembly is arranged to prevent.
        let scene_active = match &info.scene {
            Some(id) => self.scenes.is_active(id),
            None => false,
        };
        let current_turn_id = self.current_turn_of(session_id).await;
        let (context, health) = self
            .live
            .with_live(session_id, |live| {
                (live.core.context(), live.engine.health.clone())
            })
            .await
            .map(|(context, health)| {
                (
                    context,
                    (!health.is_empty()).then(|| health.report().to_json()),
                )
            })
            .unwrap_or_default();
        Ok(api::SessionDetail {
            info,
            scene_active,
            turn_state: if current_turn_id.is_some() {
                api::TurnState::Running
            } else {
                api::TurnState::Idle
            },
            current_turn_id,
            context,
            health,
            scripts: self.script_report(session_id).await,
        })
    }

    /// What this session's scripts have done, for `session.get`.
    ///
    /// `None` when the session bound none — a key that is absent says "no
    /// scripts here", which is a different answer from one that is present and
    /// empty, and the difference is the first thing somebody debugging a
    /// script needs.
    ///
    /// The counts answer "is it running at all"; `recent` answers "and why
    /// not". A failed call carries its reason, because "the script threw",
    /// "it ran out of time" and "it answered in a shape this point cannot use"
    /// send a reader to three different places.
    pub(in crate::session_pool) async fn script_report(
        &self,
        session_id: &str,
    ) -> Option<api::ScriptReport> {
        use api::ScriptCallOutcome;
        use base::interface::script::ScriptOutcome;

        let ledger = self
            .live
            .with_live(session_id, |live| live.engine.scripts.clone())
            .await??;
        let records = ledger.records();

        let (mut applied, mut no_change, mut failed, mut refused) = (0, 0, 0, 0);
        for r in &records {
            match r.outcome {
                ScriptOutcome::Applied => applied += 1,
                ScriptOutcome::NoChange { .. } => no_change += 1,
                ScriptOutcome::Failed { .. } => failed += 1,
                ScriptOutcome::Refused { .. } => refused += 1,
            }
        }

        /// Enough to see a pattern, few enough to read.
        const RECENT: usize = 20;
        let recent = records
            .iter()
            .rev()
            .take(RECENT)
            .map(|r| {
                let (outcome, detail) = match &r.outcome {
                    ScriptOutcome::Applied => (ScriptCallOutcome::Applied, None),
                    ScriptOutcome::NoChange { detail } => {
                        (ScriptCallOutcome::NoChange, detail.clone())
                    }
                    ScriptOutcome::Failed { error } => {
                        (ScriptCallOutcome::Failed, Some(error.to_string()))
                    }
                    ScriptOutcome::Refused { detail } => {
                        (ScriptCallOutcome::Refused, Some(detail.clone()))
                    }
                };
                api::ScriptCall {
                    point: r.point.clone(),
                    script: r.script.clone(),
                    entry: r.entry.clone(),
                    turn: r.turn,
                    outcome,
                    detail,
                }
            })
            .collect();

        Some(api::ScriptReport {
            calls: records.len(),
            applied,
            no_change,
            failed,
            refused,
            dropped: ledger.dropped(),
            recent,
        })
    }

    /// 列出所有 session（活跃的 + 历史的），合并去重。
    ///
    /// The historical half is a `HistoryStore::find_sessions` answer rather
    /// than a directory walk of the pool's own, so a store that keeps an
    /// index over its sessions answers this RPC from the index. Only the ids
    /// and their order are taken from it: the fields a summary could fill in
    /// (`preview`, `message_count`, timestamps) are what `session.list` has
    /// always reported empty for an inactive session, and filling them here
    /// would change the RPC.
    ///
    /// `include_children == false` (the default) hides sidechain sessions —
    /// they're sub-agent/team-member transcripts, not something a user
    /// browsing their conversation list should see. `parent_session_id`,
    /// when given, ignores `include_children` entirely and instead returns
    /// every session recording that one as its parent (delegating to
    /// `HistoryStore::child_sessions`, the same path `agent.list`'s
    /// post-restart fallback uses).
    ///
    /// Each child reports its own recorded `session_kind`. Both a sidechain
    /// and a fork name a parent, and answering `sidechain` for all of them
    /// makes a fork look like something the parent's `session.close` is about
    /// to delete.
    pub async fn list_all(
        &self,
        include_children: bool,
        parent_session_id: Option<&str>,
    ) -> Vec<SessionInfo> {
        if let Some(parent) = parent_session_id {
            let Some(store) = self.directory.store() else {
                return Vec::new();
            };
            let Ok(children) = store.child_sessions(parent).await else {
                return Vec::new();
            };
            let active_ids: std::collections::HashSet<String> =
                self.live.ids().await.into_iter().collect();
            let mut out = Vec::with_capacity(children.len());
            for sid in children {
                let sid_str = sid.to_string();
                let active = active_ids.contains(&sid_str);
                let (facts, resumable) = self.session_facts_and_resumable(&sid_str).await;
                let stats = store.session_stats(sid).await.unwrap_or_default();
                out.push(SessionInfo {
                    session_id: sid_str,
                    name: stats.title,
                    preview: None,
                    message_count: 0,
                    created_at: String::new(),
                    last_active: String::new(),
                    status: if active {
                        SessionStatus::Active
                    } else {
                        SessionStatus::Inactive
                    },
                    session_kind: facts.session_kind,
                    scene: facts.scene,
                    project_root: facts.project_root,
                    compact_count: stats.compact_count,
                    parent_session_id: Some(parent.to_string()),
                    resumable,
                });
            }
            return out;
        }

        // Snapshot active sessions and release the lock before the disk
        // reads below (`session_facts_and_resumable` per session, plus
        // `store.find_sessions()`) — holding the session table across that
        // much I/O would block every other pool operation for the duration.
        let active_snapshot: Vec<(String, Option<String>, Instant, Instant)> = self
            .live
            .map_all(|live| {
                (
                    live.core.id().to_string(),
                    live.core.naming().name.clone(),
                    live.core.created_at(),
                    live.core.hub().last_active(),
                )
            })
            .await;

        let mut out: Vec<SessionInfo> = Vec::new();
        let mut seen = std::collections::HashSet::new();

        for (sid, name, created_at, last_active) in active_snapshot {
            let (facts, resumable) = self.session_facts_and_resumable(&sid).await;
            // From the same place a cold session's do: the store counts as it
            // writes, so a live session and one on disk are answered alike
            // instead of from two accountings that can disagree.
            let stats = self.directory.stats_of(&sid).await;
            seen.insert(sid.clone());
            if !include_children
                && matches!(facts.session_kind, history::entry::SessionKind::Sidechain)
            {
                continue;
            }
            out.push(SessionInfo {
                session_id: sid,
                name,
                preview: None,
                message_count: 0,
                created_at: format_instant(created_at),
                last_active: format_instant(last_active),
                status: SessionStatus::Active,
                session_kind: facts.session_kind,
                scene: facts.scene,
                project_root: facts.project_root,
                compact_count: stats.compact_count,
                parent_session_id: facts.parent_session_id,
                resumable,
            });
        }

        // 从 HistoryStore 查历史（inactive sessions）
        if let Some(store) = self.directory.store() {
            let mut summaries = Vec::new();
            for home in self.directory.served_projects(self.cwd()).await {
                let query = history::query::SessionQuery::recent(usize::MAX)
                    .ids_only()
                    .within(history::query::SessionScope::Project(home));
                if let Ok(found) = store.find_sessions(&query).await {
                    summaries.extend(found);
                }
            }
            {
                for summary in summaries {
                    let sid_str = summary.session_id.to_string();
                    if !seen.insert(sid_str.clone()) {
                        continue;
                    }
                    // The name somebody gave it. A cold session has no live
                    // entry to carry one, which is why `session.rename`
                    // writes it beside the log rather than into the pool.
                    let name = summary.title.clone();
                    let (facts, resumable) = self.session_facts_and_resumable(&sid_str).await;
                    if !include_children
                        && matches!(facts.session_kind, history::entry::SessionKind::Sidechain)
                    {
                        continue;
                    }
                    out.push(SessionInfo {
                        session_id: sid_str,
                        name,
                        preview: None,
                        message_count: 0,
                        created_at: String::new(),
                        last_active: String::new(),
                        status: SessionStatus::Inactive,
                        session_kind: facts.session_kind,
                        scene: facts.scene,
                        project_root: facts.project_root,
                        compact_count: summary.compact_count,
                        parent_session_id: facts.parent_session_id,
                        resumable,
                    });
                }
            }
        }

        out
    }

    /// A session that is not in memory and not under any project `session.list`
    /// enumerates — one whose project no session in this process has touched
    /// yet. `session.resume` finds those by id, so `session.get` must too.
    pub(in crate::session_pool) async fn cold_session_info(
        &self,
        session_id: &str,
    ) -> Result<SessionInfo, PoolError> {
        let not_found = || PoolError::session_not_found(session_id);
        let store = self.directory.store().ok_or_else(not_found)?;
        let sid = base::session::SessionId::parse(session_id).map_err(|_| not_found())?;
        match store.load(sid).await {
            Ok(entries) if !entries.is_empty() => {}
            _ => return Err(not_found()),
        }
        let (facts, resumable) = self.session_facts_and_resumable(session_id).await;
        let stats = self.directory.stats_of(session_id).await;
        Ok(SessionInfo {
            session_id: session_id.to_string(),
            name: stats.title,
            preview: None,
            message_count: 0,
            created_at: String::new(),
            last_active: String::new(),
            status: SessionStatus::Inactive,
            session_kind: facts.session_kind,
            scene: facts.scene,
            project_root: facts.project_root,
            compact_count: stats.compact_count,
            parent_session_id: facts.parent_session_id,
            resumable,
        })
    }

    /// 获取活跃 session 数量。
    pub async fn active_count(&self) -> usize {
        self.live.len().await
    }

    /// Whether `session_id` names a session this pool has — in memory, or a
    /// transcript it can resume.
    pub async fn session_exists(&self, session_id: &str) -> bool {
        if self.live.contains(session_id).await {
            return true;
        }
        self.directory.load_entries(session_id).await.is_ok()
    }
}

/// What `scene.describe` and `config.get {tier: "effective"}` report as the
/// merged settings: the values, where the scene's data lives, and the rules
/// the shared settings files contribute.
fn effective_settings_json(settings: &LoadedSettings) -> serde_json::Value {
    let mut value =
        serde_json::to_value(&settings.values).unwrap_or_else(|_| serde_json::json!({}));
    let rules: Vec<_> = settings
        .rules
        .iter()
        .filter(|r| !r.machine_local)
        .map(|r| &r.rule)
        .collect();
    if let Some(object) = value.as_object_mut() {
        object.insert(
            "paths".into(),
            serde_json::to_value(&settings.paths).unwrap_or_default(),
        );
        if !rules.is_empty() {
            object.insert(
                "permission_rules".into(),
                serde_json::to_value(rules).unwrap_or_default(),
            );
        }
    }
    value
}
