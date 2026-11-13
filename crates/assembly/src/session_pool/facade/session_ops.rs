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

        let store = self.require_history_store()?;
        let sid = base::session::SessionId::parse(session_id)
            .map_err(|_| PoolError::InvalidRequest(format!("invalid session_id: {session_id}")))?;
        // Naming something that does not exist would create a sidecar for a
        // session nothing else knows about — a row in every future listing
        // that nobody can open.
        let live = self.sessions.lock().await.contains_key(session_id);
        if !live
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

        {
            let mut sessions = self.sessions.lock().await;
            if let Some(live) = sessions.get_mut(session_id) {
                live.name = name.clone();
                live.named_by_user = name.is_some();
            }
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
        let store = self.require_history_store()?;
        let sid = base::session::SessionId::parse(session_id)
            .map_err(|_| PoolError::InvalidRequest(format!("invalid session_id: {session_id}")))?;
        // Appending to a session that does not exist would create a log for it.
        self.load_entries(session_id).await?;
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

        let entries = self.load_entries(session_id).await?;
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
        let store = self.require_history_store()?;
        let entries = self.load_entries(session_id).await?;
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
    /// The engine has been able to do this all along (`EngineCommand::
    /// UpdateModel`); what it had no way to hear was a client asking. The
    /// transcript is not touched: this is a change to what answers the next
    /// message, not a rewrite of the conversation that led to it.
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
        let (input_tx, project, scene) = {
            let mut sessions = self.sessions.lock().await;
            let Some(live) = sessions.get_mut(session_id) else {
                return Err(PoolError::session_not_found(session_id));
            };
            if let Some(busy) = live.turn.busy(session_id) {
                return Err(PoolError::SessionBusy(busy));
            }
            live.hub.touch();
            (
                live.input_tx.clone(),
                live.project.clone(),
                live.scene.id.clone(),
            )
        };

        let current_provider = self
            .lock_specs()
            .get(session_id)
            .and_then(SessionSpec::provider);
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
            self.with_session_lock(session_id, async {
                let held = self.take_idle_slot(session_id).await?;
                self.rebuild_on_changed_spec(session_id, held, change).await
            })
            .await
            .map_err(|e| e.into_error(session_id))?;
        } else {
            let spec = self.change_spec(session_id, change).1;
            self.persist_spec(session_id, &spec).await;
        }
        if let (false, Some(m)) = (switching_provider, model) {
            if input_tx
                .send(InputMessage::System {
                    kind: EngineCommand::UpdateModel,
                    content: m.to_string(),
                })
                .is_err()
            {
                return Err(PoolError::session_gone(
                    session_id,
                    format!("session is no longer running: {session_id}"),
                ));
            }
        }

        let spec = self
            .lock_specs()
            .get(session_id)
            .cloned()
            .unwrap_or_default();
        Ok(api::ModelChoice {
            session_id: session_id.to_string(),
            model: spec.model(),
            provider: spec.provider(),
            applies: api::Applies::NextTurn,
        })
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
        {
            let sessions = self.sessions.lock().await;
            if let Some(busy) = sessions
                .get(session_id)
                .and_then(|l| l.turn.busy(session_id))
            {
                return Err(PoolError::SessionBusy(busy));
            }
        }
        if !self.session_exists(session_id).await {
            return Err(PoolError::session_not_found(session_id));
        }
        if !self.lock_specs().contains_key(session_id) {
            let recorded = self.recorded_spec(session_id).await;
            self.lock_specs().insert(session_id.to_string(), recorded);
        }
        let change = |spec: &mut SessionSpec| {
            let mut prompt = spec.prompt().unwrap_or_default();
            change(&mut prompt);
            spec.prompt = Some(prompt);
        };
        let prompt = self
            .with_session_lock(session_id, async {
                let spec = match self.take_idle_slot(session_id).await {
                    Ok(held) => {
                        self.rebuild_on_changed_spec(session_id, held, change)
                            .await?
                    }
                    Err(Rebuild::NotLive) => {
                        let spec = self.change_spec(session_id, change).1;
                        self.persist_spec(session_id, &spec).await;
                        self.release_spec(session_id);
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
