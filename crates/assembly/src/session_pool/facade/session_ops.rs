//! 对已有会话的元数据操作：重命名、追加与列出扩展条目、换模型、换提示词。

use crate::session_pool::*;

impl SessionPool {
    /// `session.rename` — what the person looking at this session calls it.
    ///
    /// `None` clears the name, putting the session back under whatever names
    /// it automatically. The name is written where the session's own facts
    /// live, so every host sharing `~/.agix` sees it — a name each host kept
    /// in an overlay of its own is a name that differs depending on which
    /// window you have open.
    ///
    /// Works on a session that is not in memory: naming is a fact about the
    /// session, not an operation on a running engine, so it does not resume
    /// anything to do it.
    pub async fn rename_session(
        &self,
        session_id: &str,
        name: Option<&str>,
    ) -> Result<api::Renamed, PoolError> {
        self.refuse_scene_mismatch(session_id).await?;
        let name = match name {
            None => None,
            Some(raw) => {
                let trimmed = raw.trim();
                if trimmed.is_empty() {
                    return Err(PoolError::InvalidRequest(
                        "name must not be empty; pass null to clear it".into(),
                    ));
                }
                if trimmed.chars().count() > Self::MAX_SESSION_NAME {
                    return Err(PoolError::InvalidRequest(format!(
                        "name must be at most {} characters",
                        Self::MAX_SESSION_NAME
                    )));
                }
                // A host renders this in a list. A newline or a control
                // character in it is not a name, it is a rendering bug
                // somebody else has to find.
                if trimmed.chars().any(char::is_control) {
                    return Err(PoolError::InvalidRequest(
                        "name must not contain control characters".into(),
                    ));
                }
                Some(trimmed.to_string())
            }
        };

        let store = self.directory.require_store()?;
        let sid = base::session::SessionId::parse(session_id)
            .map_err(|_| PoolError::InvalidRequest(format!("invalid session_id: {session_id}")))?;
        // Naming something that does not exist would create a sidecar for a
        // session nothing else knows about — a row in every future listing
        // that nobody can open.
        let live = self.live.core_of(session_id).await;
        if live.is_none()
            && matches!(
                store.load(sid).await,
                Err(history::error::HistoryError::SessionNotFound(_))
            )
        {
            return Err(PoolError::session_not_found(session_id));
        }

        store
            .set_session_title(sid, name.clone())
            .await
            .map_err(|e| PoolError::Internal(format!("could not rename: {e}")))?;

        if let Some(core) = self.live.core_of(session_id).await {
            let mut naming = core.naming();
            naming.name = name.clone();
            naming.named_by_user = name.is_some();
        }

        Ok(api::Renamed {
            session_id: session_id.to_string(),
            name,
        })
    }

    const MAX_ENTRY_NS: usize = 128;

    pub async fn append_entry(
        &self,
        session_id: &str,
        ns: &str,
        event: &str,
        payload: serde_json::Value,
    ) -> Result<api::EntryView, PoolError> {
        self.refuse_scene_mismatch(session_id).await?;
        if ns.is_empty() || ns.len() > Self::MAX_ENTRY_NS || ns.chars().any(char::is_control) {
            return Err(PoolError::InvalidRequest(format!(
                "ns must be 1–{} characters with no control characters",
                Self::MAX_ENTRY_NS
            )));
        }
        if ns.starts_with(ENGINE_NS_PREFIX) {
            return Err(PoolError::InvalidRequest(format!(
                "the `{ENGINE_NS_PREFIX}*` namespaces are the engine's own"
            )));
        }
        if event.is_empty() || event.chars().any(char::is_control) {
            return Err(PoolError::InvalidRequest(
                "event must be a non-empty name".into(),
            ));
        }
        let store = self.directory.require_store()?;
        let sid = base::session::SessionId::parse(session_id)
            .map_err(|_| PoolError::InvalidRequest(format!("invalid session_id: {session_id}")))?;
        // Appending to a session that does not exist would create a log for it.
        self.directory.load_entries(session_id).await?;
        let entry = history::entry::LogEntry::Extension {
            ns: ns.to_string(),
            event: event.to_string(),
            payload: payload.clone(),
        };
        store
            .append(sid, entry)
            .await
            .map_err(|e| PoolError::Internal(format!("could not append: {e}")))?;
        if let Err(e) = store.sync(sid).await {
            warn!(%session_id, error = %e, "an appended entry was not synced");
        }

        let entries = self.directory.load_entries(session_id).await?;
        let lengths = history::transcript::projected_lengths_with(&entries, store.projection());
        let written = entries
            .iter()
            .enumerate()
            .rev()
            .find_map(|(i, env)| match &env.entry {
                history::entry::LogEntry::Extension {
                    ns: n,
                    event: e,
                    payload: p,
                } if n == ns && e == event && *p == payload => Some(entry_view(i, env, &lengths)),
                _ => None,
            })
            .flatten()
            .ok_or_else(|| {
                PoolError::Internal("the entry was not found after writing it".to_string())
            })?;

        self.publish_between_turns(
            session_id,
            SessionEvent::ExtensionEntry(serde_json::to_value(&written).unwrap_or_default()),
        )
        .await;
        Ok(written)
    }

    pub async fn list_entries(
        &self,
        session_id: &str,
        namespaces: Option<&[String]>,
    ) -> Result<api::EntryList, PoolError> {
        let store = self.directory.require_store()?;
        let entries = self.directory.load_entries(session_id).await?;
        let lengths = history::transcript::projected_lengths_with(&entries, store.projection());
        let listed: Vec<api::EntryView> = entries
            .iter()
            .enumerate()
            .filter(|(_, env)| match &env.entry {
                history::entry::LogEntry::Extension { ns, .. } => {
                    !ns.starts_with(ENGINE_NS_PREFIX)
                        && namespaces.is_none_or(|wanted| wanted.iter().any(|w| w == ns))
                }
                _ => false,
            })
            .filter_map(|(i, env)| entry_view(i, env, &lengths))
            .collect();
        Ok(api::EntryList { entries: listed })
    }

    /// `session.setModel` — what the next API call this session makes should
    /// ask for.
    ///
    /// The choice is written into the session's spec and onto its state,
    /// which every engine the session runs on reads as a turn starts — so it
    /// holds whichever engine runs the next turn, a rebuild in between
    /// included. The transcript is not touched: this is a change to what
    /// answers the next message, not a rewrite of the conversation that led to
    /// it.
    ///
    /// Refused while a turn is running, rather than queued behind it. The
    /// engine's main loop is inside `process_turn` for the duration and is
    /// not reading its input channel, so a command accepted here would take
    /// effect at some unannounced later moment — the caller would have been
    /// told the model changed and watched the old one answer.
    ///
    /// With `provider`, the conversation moves to that provider's client. An
    /// engine is built around its client, so the session is rebuilt on the
    /// new spec; it is idle, so nothing is cut off. A model is checked
    /// against the provider it will be sent to and refused when that
    /// provider's `models` list does not have it — sending a name to an
    /// endpoint that does not serve it fails only later, and less legibly.
    pub async fn set_session_model(
        &self,
        session_id: &str,
        model: Option<&str>,
        provider: Option<&str>,
    ) -> Result<api::ModelChoice, PoolError> {
        self.refuse_scene_mismatch(session_id).await?;
        self.wait_out_finishing_turn(session_id).await;
        let (project, scene) = self
            .live
            .with_live(session_id, |live| {
                if let Some(busy) = live.turn.busy(session_id) {
                    return Err(PoolError::SessionBusy(busy));
                }
                live.core.hub().touch();
                Ok((
                    live.core.project().map(Path::to_path_buf),
                    live.core.scene().id.clone(),
                ))
            })
            .await
            .ok_or_else(|| PoolError::session_not_found(session_id))??;

        let current_provider = self.specs.get(session_id).and_then(|spec| spec.provider());
        let binding = self.binding_for(project.as_deref(), &scene).await;
        binding
            .check_model_choice(provider.or(current_provider.as_deref()), model)
            .map_err(PoolError::InvalidRequest)?;

        let switching_provider = provider.is_some_and(|p| current_provider.as_deref() != Some(p));
        let change = |spec: &mut SessionSpec| {
            if let Some(p) = provider {
                spec.provider = Some(p.to_string());
                // A model chosen for the old provider means nothing to the new
                // one; without a new choice the new provider's default applies.
                if switching_provider {
                    spec.model = None;
                }
            }
            if let Some(m) = model {
                spec.model = Some(m.to_string());
            }
        };

        // A provider is a different client, which an engine is built around
        // rather than handed mid-life: rebuild the session on the new spec.
        // The turn slot is taken before the change is made, so a turn that
        // started since the check above refuses both rather than being cut
        // off or finding the change applied by some later rebuild.
        if switching_provider {
            self.live
                .with_session_lock(session_id, async {
                    let held = self.hold_for_rebuild(session_id).await?;
                    self.rebuild_on_changed_spec(session_id, held, change).await
                })
                .await
                .map_err(|e| e.into_error(session_id))?;
        } else {
            let spec = self.update_spec(session_id, change).await;
            self.specs.persist(session_id, &spec).await;
        }

        let spec = self.specs.get(session_id).unwrap_or_default();
        Ok(api::ModelChoice {
            session_id: session_id.to_string(),
            model: spec.model(),
            provider: spec.provider(),
            applies: api::Applies::NextTurn,
        })
    }

    /// Change `sid`'s spec, and put the model it now names onto the session's
    /// state when the session is in memory.
    pub(in crate::session_pool) async fn update_spec(
        &self,
        sid: &str,
        change: impl FnOnce(&mut SessionSpec),
    ) -> SessionSpec {
        let spec = self.specs.update(sid, change).1;
        self.project_model_choice(sid).await;
        spec
    }

    pub(in crate::session_pool) async fn project_model_choice(&self, sid: &str) {
        let model = self.specs.get(sid).and_then(|spec| spec.model());
        if let Some(core) = self.live.core_of(sid).await {
            core.state().set_model_choice(model);
        }
    }

    /// `session.setPrompt` — this session's own system-prompt append and
    /// override from its next turn on. A string sets a field, `None` in
    /// `change` leaves it. Refused while a turn runs, like `setModel`; an
    /// idle session is rebuilt on the new prompt at once, its history kept.
    pub async fn set_session_prompt(
        &self,
        session_id: &str,
        change: impl FnOnce(&mut runtime::agent::SessionPrompt),
    ) -> Result<api::PromptChoice, PoolError> {
        self.refuse_scene_mismatch(session_id).await?;
        self.wait_out_finishing_turn(session_id).await;
        if let Some(busy) = self
            .live
            .with_live(session_id, |l| l.turn.busy(session_id))
            .await
            .flatten()
        {
            return Err(PoolError::SessionBusy(busy));
        }
        if !self.session_exists(session_id).await {
            return Err(PoolError::session_not_found(session_id));
        }
        self.specs.load(session_id).await;
        let change = |spec: &mut SessionSpec| {
            let mut prompt = spec.prompt().unwrap_or_default();
            change(&mut prompt);
            spec.prompt = Some(prompt);
        };
        let prompt = self
            .live
            .with_session_lock(session_id, async {
                let spec = match self.hold_for_rebuild(session_id).await {
                    Ok(held) => {
                        self.rebuild_on_changed_spec(session_id, held, change)
                            .await?
                    }
                    Err(Rebuild::NotLive) => {
                        let spec = self.specs.update(session_id, change).1;
                        self.specs.persist(session_id, &spec).await;
                        self.specs.release(session_id);
                        spec
                    }
                    Err(busy) => return Err(busy),
                };
                Ok(spec.prompt().unwrap_or_default())
            })
            .await
            .map_err(|e| e.into_error(session_id))?;
        Ok(api::PromptChoice {
            session_id: session_id.to_string(),
            append: prompt.append,
            override_text: prompt.override_text,
            applies: api::Applies::NextTurn,
        })
    }
}
