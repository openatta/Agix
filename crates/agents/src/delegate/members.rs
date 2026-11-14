//! Persistent team members (the `Agent` tool's `team_name` + `name`): kept
//! across messages, keyed by team and name, each message a task of the
//! parent session queued in the member's `MemberInbox`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use base::context::{TaskLease, TaskType};
use base::interface::delegate::{
    AskRoute, DelegateEnd, DelegateError, Delegated, LiveMember, StartedAgent, TeamSeat,
};
use tokio_util::sync::CancellationToken;

use super::{Delegator, SlotRule};
use crate::agent_tool::{
    bg_task_id, conclude, mark_sidechain_terminal, open_inbox, run_one_turn, task_notification,
    Delegate, SpawnTelemetry, Spec, SubagentPayload,
};

/// A live member. Its agent and channels live inside its loop; `cancel` is
/// enough to stop it.
pub(crate) struct PersistentMember {
    /// Tells this member from one spawned later under the same key; a name
    /// reused after a stop gets a new id.
    pub(crate) id: String,
    pub(crate) session_id: String,
    pub(crate) inbox: MemberInbox,
    pub(crate) cancel: CancellationToken,
    /// Epoch seconds since the member last went idle, or `i64::MAX` while it
    /// runs a message: a running member is never reclaimed for idling.
    pub(crate) idle_since: Arc<AtomicI64>,
    /// Strictly increasing per idle transition (`u64::MAX` while running), so
    /// "oldest idle" never ties the way second-resolution `idle_since` can.
    pub(crate) idle_seq: Arc<AtomicU64>,
}

pub(crate) struct PersistentMessage {
    pub(crate) prompt: String,
    pub(crate) lease: TaskLease,
}

/// A member's queue. A message is its task's lease: queuing one registers
/// the task, and a message that never runs — refused at the door, or left
/// in the queue when the member stops — ends its task as stopped when it is
/// dropped.
#[derive(Clone)]
pub(crate) struct MemberInbox(tokio::sync::mpsc::UnboundedSender<PersistentMessage>);

impl MemberInbox {
    fn open() -> (
        Self,
        tokio::sync::mpsc::UnboundedReceiver<PersistentMessage>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (Self(tx), rx)
    }

    /// The task id of `prompt` queued for the member stopping on
    /// `member_cancel`, or `None` when its loop has already ended.
    fn deliver(
        &self,
        delegator: &Delegator,
        seat: &TeamSeat,
        member_cancel: &CancellationToken,
        prompt: String,
    ) -> Option<String> {
        let lease = TaskLease::new(
            delegator.env.parent.state.clone(),
            bg_task_id(),
            TaskType::Teammate,
            member_cancel.child_token(),
            serde_json::json!({"team": seat.team, "member": seat.member}),
        )
        .notifying(|task| task_notification(task, &DelegateEnd::Cancelled));
        let task_id = lease.task().task_id.clone();
        match self.0.send(PersistentMessage { prompt, lease }) {
            Ok(()) => Some(task_id),
            Err(refused) => {
                refused
                    .0
                    .lease
                    .conclude(base::context::RunningStatus::Cancelled, None);
                None
            }
        }
    }
}

pub(crate) struct PersistentMembers {
    /// Shared with `TeamCreate`/`TeamList`/`TeamDelete`; `None` when team
    /// coordination is off for this session.
    pub(crate) registry: Option<Arc<team::registry::TeamRegistry>>,
    pub(crate) live: Arc<RwLock<HashMap<(String, String), PersistentMember>>>,
    /// Source of `PersistentMember::idle_seq`: one counter for every member
    /// of the session, so "oldest idle" has a single ordering.
    pub(crate) idle_seq_counter: Arc<AtomicU64>,
    /// Serializes find-or-create: two concurrent calls for one new key would
    /// otherwise both spawn, and the second insert would orphan the first
    /// member's loop.
    pub(crate) spawn_lock: Arc<tokio::sync::Mutex<()>>,
}

impl PersistentMembers {
    pub(crate) fn new(registry: Option<Arc<team::registry::TeamRegistry>>) -> Self {
        Self {
            registry,
            live: Default::default(),
            idle_seq_counter: Default::default(),
            spawn_lock: Default::default(),
        }
    }
}

pub(crate) fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl Delegator {
    /// Queue `prompt` for the member at `seat`, spawning it first when it is
    /// not alive. The reply is its message's task: the member outlives the
    /// call and cannot hand its text straight back.
    ///
    /// A new member's permission mode is the call's, else the one its team
    /// already settled on, else `plan`; the team keeps whichever was settled
    /// first. A live member keeps the mode it was built with.
    pub(super) async fn message_member(
        &self,
        seat: TeamSeat,
        spec: Spec,
        prompt: String,
    ) -> Result<Delegated, DelegateError> {
        let me = self.me()?;
        let _spawning = self.members.spawn_lock.lock().await;
        let key = (seat.team.clone(), seat.member.clone());
        // Never the member this call is about to look for: it would be
        // reclaimed a moment before the lookup and replaced by a fresh one.
        self.reclaim_idle_members(Some(&key));

        let live = &self.members.live;
        let existing = live
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .map(|m| (m.inbox.clone(), m.cancel.clone(), m.session_id.clone()));
        if let Some((inbox, cancel, session_id)) = &existing {
            if let Some(task_id) = inbox.deliver(self, &seat, cancel, prompt.clone()) {
                return Ok(Delegated::Started(StartedAgent::new(
                    task_id,
                    session_id.clone(),
                )));
            }
            // Its loop has ended since; spawn afresh under the same key.
            live.write().unwrap_or_else(|e| e.into_inner()).remove(&key);
        }
        let alive = live.read().unwrap_or_else(|e| e.into_inner()).len();
        if alive >= self.limits.max_persistent_members {
            return Err(DelegateError::Limit(format!(
                "team member limit reached: {alive} persistent members are busy, the most \
                 this session keeps (execution.team_max_persistent_members). Message one of \
                 them, wait for one to go idle, or stop one with TaskStop or TeamDelete."
            )));
        }

        let registry = self.members.registry.clone();
        let mode = spec
            .permission_mode
            .or_else(|| {
                registry
                    .as_ref()
                    .and_then(|r| r.team_permission_mode(&seat.team))
            })
            .unwrap_or(base::settings::PermissionMode::Plan);
        if let Some(r) = &registry {
            r.set_team_permission_mode_if_absent(&seat.team, mode);
        }
        let spec = Spec {
            permission_mode: Some(mode),
            seat: Some(seat.clone()),
            ..spec
        };
        self.spawn_member(me, key, seat, spec, prompt).await
    }

    /// A new member at `key`, its loop started on `prompt` under its type's
    /// prompt; later messages reach it as sent.
    async fn spawn_member(
        &self,
        me: Arc<Delegator>,
        key: (String, String),
        seat: TeamSeat,
        spec: Spec,
        prompt: String,
    ) -> Result<Delegated, DelegateError> {
        let registry = self.members.registry.clone();
        let live = &self.members.live;
        let depth = self.limits.spawn_guard()?;
        let delegate = self
            .build_delegate(
                &spec,
                depth,
                Vec::new(),
                self.relay(&spec.session_id, None, AskRoute::Host, Vec::new()),
                true,
            )
            .await
            .map_err(|e| match e {
                base::error::ToolError::Execution(e) => DelegateError::Build(e.to_string()),
                other => DelegateError::Build(other.to_string()),
            })?;
        self.fire_subagent_hook(
            hooks::HookEvent::SubagentStart,
            &delegate.payload,
            &self.env.parent.ended,
        )
        .await;
        let spawned = SpawnTelemetry::start(
            self,
            None,
            &spec.session_id,
            spec.agent_type.as_deref(),
            &delegate.agent.settings().values.model.model_name,
            true,
            None,
        );

        let prompt = self.opening_prompt(delegate.def.as_ref(), prompt);
        let id = format!("member-{}", bg_task_id());
        tracing::debug!(team = %seat.team, member = %seat.member, %id, "spawning new persistent team member");
        let (inbox, messages) = MemberInbox::open();
        let cancel = self.env.parent.ended.child_token();
        let idle_since = Arc::new(AtomicI64::new(i64::MAX));
        let idle_seq = Arc::new(AtomicU64::new(u64::MAX));
        live.write().unwrap_or_else(|e| e.into_inner()).insert(
            key,
            PersistentMember {
                id: id.clone(),
                session_id: spec.session_id.clone(),
                inbox: inbox.clone(),
                cancel: cancel.clone(),
                idle_since: idle_since.clone(),
                idle_seq: idle_seq.clone(),
            },
        );
        let task_id = inbox
            .deliver(self, &seat, &cancel, prompt)
            .ok_or_else(|| DelegateError::Build("the new member's inbox closed".into()))?;
        tokio::spawn(member_loop(MemberLoop {
            id,
            live: live.clone(),
            delegate,
            messages,
            cancel,
            registry,
            seat,
            idle_since,
            idle_seq,
            idle_seq_counter: self.members.idle_seq_counter.clone(),
            slots: self.limits.slots.clone(),
            hooks: me.env.hooks.clone(),
            session_ended: me.env.parent.ended.clone(),
            spawned,
        }));
        Ok(Delegated::Started(StartedAgent::new(
            task_id,
            spec.session_id,
        )))
    }

    /// Stop a member, idle or mid-turn, and forget it, so a later message
    /// under the same key spawns a fresh one. Nothing when there is none.
    pub(crate) fn stop_persistent_member(&self, seat: &TeamSeat) {
        let key = (seat.team.clone(), seat.member.clone());
        let member = self
            .members
            .live
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&key);
        if let Some(m) = member {
            tracing::debug!(team = %seat.team, member = %seat.member, id = %m.id, "stopping persistent team member");
            m.cancel.cancel();
        }
    }

    pub(crate) fn live_persistent_members(&self) -> Vec<LiveMember> {
        let mut members: Vec<LiveMember> = self
            .members
            .live
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|(_, m)| !m.cancel.is_cancelled())
            .map(|((team, member), m)| LiveMember {
                id: m.id.clone(),
                seat: TeamSeat {
                    team: team.clone(),
                    member: member.clone(),
                },
            })
            .collect();
        members.sort_by(|a, b| a.id.cmp(&b.id));
        members
    }

    /// Stops (a) every member idle longer than `team_member_idle_timeout_secs`
    /// and (b) the oldest idle ones while there are more than
    /// `team_max_persistent_members`, counting the member `protect` is about
    /// to add. A running member is never stopped; `protect` itself never is.
    /// Run before a member is spawned, not on a timer.
    pub(crate) fn reclaim_idle_members(&self, protect: Option<&(String, String)>) {
        let max_total = self.limits.max_persistent_members;
        let idle_timeout = self.limits.member_idle_timeout_secs;
        let now = now_secs();
        let victims: Vec<(String, String)> = {
            let members = self.members.live.read().unwrap_or_else(|e| e.into_inner());
            let mut idle: Vec<((String, String), i64, u64)> = members
                .iter()
                .filter(|(k, _)| protect != Some(*k))
                .filter_map(|(k, m)| {
                    let since = m.idle_since.load(Ordering::SeqCst);
                    let seq = m.idle_seq.load(Ordering::SeqCst);
                    (since != i64::MAX).then_some((k.clone(), since, seq))
                })
                .collect();
            idle.sort_by_key(|(_, _, seq)| *seq);
            let mut victims: Vec<(String, String)> = idle
                .iter()
                .filter(|(_, since, _)| now.saturating_sub(*since) as u64 >= idle_timeout)
                .map(|(k, _, _)| k.clone())
                .collect();
            let joining = protect.is_some_and(|k| !members.contains_key(k));
            let keep = if joining { max_total - 1 } else { max_total };
            let mut remaining = members.len();
            for (k, _, _) in &idle {
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
        for (team, member) in victims {
            self.stop_persistent_member(&TeamSeat { team, member });
        }
    }
}

struct MemberLoop {
    id: String,
    live: Arc<RwLock<HashMap<(String, String), PersistentMember>>>,
    delegate: Delegate,
    messages: tokio::sync::mpsc::UnboundedReceiver<PersistentMessage>,
    cancel: CancellationToken,
    registry: Option<Arc<team::registry::TeamRegistry>>,
    seat: TeamSeat,
    idle_since: Arc<AtomicI64>,
    idle_seq: Arc<AtomicU64>,
    idle_seq_counter: Arc<AtomicU64>,
    /// The spawning session's: a member runs each message on a slot, taken
    /// as that session may take one, and holds none while idle.
    slots: SlotRule,
    hooks: std::sync::Weak<hooks::HookRunner>,
    /// What its `SubagentStop` stops with: `cancel` has fired by then.
    session_ended: CancellationToken,
    spawned: SpawnTelemetry,
}

impl MemberLoop {
    /// Recorded for `TeamList` while this is the member under its key. Once
    /// stopped it only updates its own entry, if the team still has one: a
    /// turn that outlives the stop must not write over a member that took its
    /// place, nor bring back a team deleted under it.
    fn lifecycle(&self, state: team::coordinator::TeammateLifecycle, at: Option<u64>) {
        let Some(r) = &self.registry else { return };
        let (team, member) = (&self.seat.team, &self.seat.member);
        let live = self.live.read().unwrap_or_else(|e| e.into_inner());
        match live.get(&(team.clone(), member.clone())) {
            Some(m) if m.id == self.id => r.update_member_lifecycle(team, member, state, at),
            Some(_) => {}
            None => r.update_recorded_member_lifecycle(team, member, state, at),
        }
    }
}

/// One member's life: its messages one turn at a time, on the same agent —
/// that reuse is the persistence. Ends when the member is stopped or its
/// inbox closes; whatever is still queued then ends stopped.
async fn member_loop(mut m: MemberLoop) {
    use team::coordinator::TeammateLifecycle;
    let mut last = DelegateEnd::Completed;
    loop {
        let message = tokio::select! {
            biased;
            _ = m.cancel.cancelled() => break,
            message = m.messages.recv() => message,
        };
        let Some(PersistentMessage { prompt, lease }) = message else {
            break;
        };
        m.idle_since.store(i64::MAX, Ordering::SeqCst);
        m.idle_seq.store(u64::MAX, Ordering::SeqCst);
        m.lifecycle(TeammateLifecycle::Active, None);

        let task = lease.task().clone();
        let (text, end) = match m.slots.take(&task.cancel).await {
            Ok(slot) => {
                let d = &mut m.delegate;
                open_inbox(Some(&task), &d.agent);
                // No `SubagentTag`: a member outlives any one parent turn, so
                // there is no turn to attribute its events to; it shows
                // through `TeamProgress` and `TeamList` instead.
                let ran = run_one_turn(
                    &mut d.agent,
                    &mut d.event_rx,
                    &d.input_tx,
                    prompt,
                    task.cancel.clone(),
                    None,
                    Some(&task),
                )
                .await;
                drop(slot);
                ran
            }
            Err(_) if task.cancel.is_cancelled() => (String::new(), DelegateEnd::Cancelled),
            Err(e) => (String::new(), DelegateEnd::Failed(e.to_string())),
        };
        conclude(lease, &text, &end, true);
        last = end;

        let now = now_secs();
        m.idle_since.store(now, Ordering::SeqCst);
        m.idle_seq.store(
            m.idle_seq_counter.fetch_add(1, Ordering::SeqCst),
            Ordering::SeqCst,
        );
        m.lifecycle(TeammateLifecycle::Idle, Some(now as u64));
    }
    m.messages.close();
    while let Ok(queued) = m.messages.try_recv() {
        drop(queued);
    }
    m.lifecycle(TeammateLifecycle::Shutdown, None);
    let failed = matches!(last, DelegateEnd::Failed(_));
    m.spawned.finish(failed);
    let payload: SubagentPayload = m.delegate.payload.clone();
    if let Some(runner) = m.hooks.upgrade() {
        payload
            .clone()
            .stopped("shutdown")
            .fire(&runner, hooks::HookEvent::SubagentStop, &m.session_ended)
            .await;
    }
    let ended = if failed { last } else { DelegateEnd::Completed };
    mark_sidechain_terminal(
        m.delegate.history_store.as_ref(),
        &payload.session_id,
        &ended,
    )
    .await;
}
