//! Persistent team members on `AgentTool`: building, spawning or messaging, stopping
//! and reclaiming them.

use super::*;

impl AgentTool {
    /// Build the `Agent`/channels for a team member, without running any
    /// turn yet: a persistent member's caller keeps the built `Agent` alive
    /// across many turns. No `skills:` preload text — that only makes sense
    /// prepended to a *first* message, and a persistent member's first
    /// message goes through the same path as every later one. The
    /// `agent_type`'s `model`/`permission_mode`/`effort`/`max_turns`
    /// overrides still apply.
    /// `permission_mode` is the already-*resolved* grant this member runs
    /// under for its entire lifetime (resolution — explicit override vs.
    /// the team's established grant vs. the safe default — happens in
    /// `spawn_or_message_team_member`, once, not here), held to the parent's
    /// mode; see `Inner::team_member_permission` for its rules.
    pub(super) async fn build_team_member_agent(
        &self,
        tools: Arc<InMemoryToolRegistry>,
        cwd: std::path::PathBuf,
        subagent_type: Option<&str>,
        permission_mode: base::interface::settings::PermissionMode,
        seat: Option<base::interface::agent_spawner::TeamSeat>,
    ) -> Result<(Agent, EventReceiver, InputSender, SubagentPayload), base::error::ToolError> {
        let depth = self
            .inner
            .spawn_guard()
            .map_err(base::error::ToolError::Denied)?;
        let run = SubRun {
            permission_mode: Some(permission_mode),
            seat,
            ..SubRun::default()
        };
        let delegate = self
            .inner
            .build_delegate(DelegatePlan {
                tools,
                cwd,
                depth,
                subagent_type,
                grant: Grant::TeamMember,
                parent: None,
                run: &run,
                extracts_memories: true,
            })
            .await?;
        Ok((
            delegate.agent,
            delegate.event_rx,
            delegate.input_tx,
            delegate.payload,
        ))
    }

    /// Persistent-member counterpart to `run_sub`/`launch_bg`: routes
    /// `prompt` to an existing, still-alive `(team_name, member_name)`
    /// member if one exists, or builds a fresh one under that key. Either
    /// way returns a task id — the reply to *this* message is retrieved via
    /// `TaskOutput`/`TaskStop` the same way any other background task's is,
    /// because unlike `run_sub`'s one-shot callers, the member itself
    /// outlives this one message and can't just hand its text straight back
    /// from this call.
    /// `permission_mode`: `Some` sets (or, for the team's first member,
    /// establishes) this team's permission grant — see
    /// `AgentInput::permission_mode`'s doc comment for "one team, one
    /// authorization". Only consulted when actually spawning a *new*
    /// member; a message to an already-alive member reuses whatever grant
    /// it was built with, it can't be changed mid-lifetime.
    ///
    /// Holds `Inner::team_spawn_lock` for its entire body — see that
    /// field's doc comment for why the find-or-create decision needs to be
    /// atomic across concurrent calls.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn spawn_or_message_team_member(
        &self,
        team_name: String,
        member_name: String,
        prompt: String,
        subagent_type: Option<String>,
        cwd: std::path::PathBuf,
        session: Arc<base::context::SessionState>,
        permission_mode: Option<base::interface::settings::PermissionMode>,
    ) -> Result<String, base::error::ToolError> {
        let _spawn_guard = self.inner.team_spawn_lock.lock().await;
        let key = (team_name.clone(), member_name.clone());

        // Never evict the exact member this call is about to look for —
        // otherwise a message to an existing, currently-idle member could
        // have that member reclaimed a moment before the lookup below finds
        // it, silently spawning a fresh replacement (losing its
        // conversation) instead of reaching the one the caller meant.
        self.reclaim_idle_team_members(Some(&key)).await;

        let tid = bg_task_id();
        let register = |member_cancel: &tokio_util::sync::CancellationToken| {
            session.register_task(
                tid.clone(),
                base::context::TaskType::Teammate,
                member_cancel.child_token(),
                serde_json::json!({"team": team_name, "member": member_name}),
            )
        };

        let existing = self
            .inner
            .team_members
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .map(|m| (m.message_tx.clone(), m.cancel.clone()));
        let existing_entry_is_stale = existing.is_some();

        if let Some((tx, member_cancel)) = existing.filter(|(tx, _)| !tx.is_closed()) {
            let task = register(&member_cancel);
            if tx
                .send(PersistentMessage {
                    prompt: prompt.clone(),
                    task,
                })
                .is_ok()
            {
                return Ok(tid);
            }
        }
        if existing_entry_is_stale {
            // The loop already exited (crashed, or stopped concurrently by
            // TeamDelete/reclaim right after the read above) — its map
            // entry is stale. Drop it and fall through to spawn fresh under
            // the same key, rather than surfacing a confusing send failure.
            self.inner
                .team_members
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&key);
        }
        let max_total = self.inner.config.team_max_persistent_members.max(1);
        let alive = self
            .inner
            .team_members
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .len();
        if alive >= max_total {
            return Err(base::error::ToolError::Denied(format!(
                "team member limit reached: {alive} persistent members are busy, the most \
                 this session keeps (execution.team_max_persistent_members). Message one of \
                 them, wait for one to go idle, or stop one with TaskStop or TeamDelete."
            )));
        }

        // Resolution order: explicit override on this call > this team's
        // already-established grant (set by whichever earlier call spawned
        // its first member) > the safe zero-config default. Persisted back
        // via `set_team_permission_mode_if_absent` so later spawns under
        // the same `team_name` (even without repeating `permission_mode`)
        // inherit whatever was decided here — "one team, one authorization".
        let registry = self.inner.team_registry.get();
        let resolved_permission_mode = permission_mode
            .or_else(|| {
                registry
                    .as_ref()
                    .and_then(|r| r.team_permission_mode(&team_name))
            })
            .unwrap_or(base::interface::settings::PermissionMode::Plan);
        if let Some(r) = &registry {
            r.set_team_permission_mode_if_absent(&team_name, resolved_permission_mode);
        }

        let tools = self.delegate_tools(subagent_type.as_deref(), &[], &[]);
        let (agent, event_rx, input_tx, payload) = self
            .build_team_member_agent(
                tools,
                cwd,
                subagent_type.as_deref(),
                resolved_permission_mode,
                Some(base::interface::agent_spawner::TeamSeat {
                    team: team_name.clone(),
                    member: member_name.clone(),
                }),
            )
            .await?;
        self.inner
            .fire_subagent_hook(hooks::HookEvent::SubagentStart, &payload)
            .await;
        let spawned = SpawnTelemetry::start(
            &self.inner,
            None,
            &payload.session_id,
            subagent_type.as_deref(),
            &agent.settings().model.model_name,
            true,
        );

        let member_id = format!("member-{}", bg_task_id());
        tracing::debug!(
            team = %team_name,
            member = %member_name,
            id = %member_id,
            "spawning new persistent team member"
        );
        let (message_tx, message_rx) = tokio::sync::mpsc::unbounded_channel::<PersistentMessage>();
        let member_cancel = self.inner.session_ended().child_token();
        let task = register(&member_cancel);
        let idle_since = Arc::new(std::sync::atomic::AtomicI64::new(i64::MAX));
        let idle_seq = Arc::new(std::sync::atomic::AtomicU64::new(u64::MAX));

        self.inner
            .team_members
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                key,
                PersistentMember {
                    id: member_id,
                    message_tx: message_tx.clone(),
                    cancel: member_cancel.clone(),
                    idle_since: idle_since.clone(),
                    idle_seq: idle_seq.clone(),
                },
            );

        let _ = message_tx.send(PersistentMessage { prompt, task });

        tokio::spawn(run_persistent_member_loop(
            agent,
            event_rx,
            input_tx,
            message_rx,
            member_cancel,
            registry,
            team_name,
            member_name,
            idle_since,
            idle_seq,
            self.inner.idle_seq_counter.clone(),
            session,
            self.inner
                .hooks()
                .map(|runner| (Arc::downgrade(&runner), payload)),
            spawned,
        ));

        Ok(tid)
    }

    /// Stop a persistent team member and drop its handle — cancels its loop
    /// (interrupting it whether idle or mid-turn) and removes it from
    /// `team_members` so a later `spawn_or_message_team_member` call under
    /// the same key builds a fresh one instead of trying to reach the
    /// stopped one. No-op if the key isn't present.
    pub(crate) async fn stop_team_member(&self, team_name: &str, member_name: &str) {
        let key = (team_name.to_string(), member_name.to_string());
        let member = self
            .inner
            .team_members
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&key);
        if let Some(m) = member {
            tracing::debug!(
                team = %team_name,
                member = %member_name,
                id = %m.id,
                "stopping persistent team member"
            );
            m.cancel.cancel();
        }
    }

    /// Reclaim policy: (a) any single member idle longer than
    /// `team_member_idle_timeout_secs`, and (b) total pool size over
    /// `team_max_persistent_members`, counting the member `protect` is about
    /// to add when it is not there yet (oldest-idle evicted first, just
    /// enough to get back under the cap) — either condition alone triggers
    /// a sweep; they're independent, not combined. Never touches a
    /// currently-`Active` member (its `idle_since` sentinel makes it
    /// ineligible for either rule).
    ///
    /// Called opportunistically before spawning a new member, not on a
    /// separate ticking timer — a session that never spawns another member
    /// after the pool fills up won't self-trim until it does. An accepted
    /// simplification.
    ///
    /// `protect`, when given, excludes that exact `(team_name, member_name)`
    /// key from eviction candidates entirely — used by
    /// `spawn_or_message_team_member` so a sweep run just before it looks up
    /// an existing member can never evict the very member the caller is
    /// about to message (it still counts toward the total for rule (a),
    /// it's just never itself a victim).
    pub(super) async fn reclaim_idle_team_members(&self, protect: Option<&(String, String)>) {
        let max_total = self.inner.config.team_max_persistent_members.max(1);
        let idle_timeout = self.inner.config.team_member_idle_timeout_secs;
        let now = now_secs();

        let victims: Vec<(String, String)> = {
            let members = self
                .inner
                .team_members
                .read()
                .unwrap_or_else(|e| e.into_inner());
            // `since` (wall clock, 1s resolution) drives the timeout rule
            // (b); `seq` (a strictly-increasing counter, never ties) drives
            // the "oldest idle first" ordering for the total-count rule
            // (a) — see `PersistentMember::idle_seq`'s doc comment for why
            // the timestamp alone isn't reliable enough to sort by.
            let mut idle_entries: Vec<((String, String), i64, u64)> = members
                .iter()
                .filter(|(k, _)| protect != Some(*k))
                .filter_map(|(k, m)| {
                    let since = m.idle_since.load(std::sync::atomic::Ordering::SeqCst);
                    let seq = m.idle_seq.load(std::sync::atomic::Ordering::SeqCst);
                    (since != i64::MAX).then_some((k.clone(), since, seq))
                })
                .collect();
            idle_entries.sort_by_key(|(_, _, seq)| *seq);

            let mut victims: Vec<(String, String)> = idle_entries
                .iter()
                .filter(|(_, since, _)| now.saturating_sub(*since) as u64 >= idle_timeout)
                .map(|(k, _, _)| k.clone())
                .collect();

            // A member about to be added needs a place of its own.
            let joining = protect.is_some_and(|k| !members.contains_key(k));
            let keep = if joining { max_total - 1 } else { max_total };
            let mut remaining = members.len();
            for (k, _, _) in &idle_entries {
                if remaining <= keep {
                    break;
                }
                remaining -= 1;
                if !victims.contains(k) {
                    victims.push(k.clone());
                }
            }
            victims
        };

        for (team_name, member_name) in victims {
            self.stop_team_member(&team_name, &member_name).await;
        }
    }
}
