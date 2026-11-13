//! `Limits`: how many delegates a session tree runs at once, how deep they
//! go, and the leave each one is given to start.

use std::sync::Arc;

use base::interface::delegate::DelegateError;
use tokio::sync::OwnedSemaphorePermit;

pub const DEFAULT_MAX_CONCURRENT_SUBAGENTS: usize = 6;

/// The cap on sub-agents running at once in one session tree.
#[derive(Clone, Debug)]
pub struct SubagentSlots {
    pub(crate) slots: Arc<tokio::sync::Semaphore>,
    pub(crate) cap: usize,
}

impl SubagentSlots {
    pub fn new(cap: usize) -> Self {
        let cap = cap.max(1);
        Self {
            slots: Arc::new(tokio::sync::Semaphore::new(cap)),
            cap,
        }
    }

    pub fn running(&self) -> usize {
        self.cap - self.slots.available_permits()
    }
}

/// The sessions of the agents running for `"type": "agent"` hooks, and of
/// whatever those delegate in turn; one set for the whole session tree. Their
/// own events start no agent hooks: a guard that reads a file would otherwise
/// ask another guard about it, and that one another, until the first times
/// out.
#[derive(Clone, Default)]
pub(crate) struct HookAgents(Arc<std::sync::Mutex<std::collections::HashSet<String>>>);

impl HookAgents {
    pub(crate) fn includes(&self, session_id: &str) -> bool {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(session_id)
    }

    pub(crate) fn enter(&self, session_id: String) -> HookAgentRunning {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(session_id.clone());
        HookAgentRunning {
            agents: self.clone(),
            session_id,
        }
    }
}

pub(crate) struct HookAgentRunning {
    agents: HookAgents,
    session_id: String,
}

impl Drop for HookAgentRunning {
    fn drop(&mut self) {
        self.agents
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.session_id);
    }
}

/// How a session asks for a slot for its delegates. Whether the session
/// holds one itself is decided once, when its limits are built, from where it
/// sits: a delegate runs on a slot, a root session does not. A holder that
/// waited for a slot could wait on one only it can give back, so a holder is
/// refused at the cap; a session holding none may wait.
#[derive(Clone)]
pub(crate) struct SlotRule {
    pool: SubagentSlots,
    holds_slot: bool,
}

impl SlotRule {
    fn new(pool: SubagentSlots, depth: u32) -> Self {
        Self {
            pool,
            holds_slot: depth > 0,
        }
    }

    /// A slot now, or refused at the cap.
    pub(crate) fn try_take(&self) -> Result<OwnedSemaphorePermit, DelegateError> {
        self.pool.slots.clone().try_acquire_owned().map_err(|_| {
            DelegateError::Limit(format!(
                "{} sub-agents are already running in this session, the most it allows \
                 (execution.max_concurrent_subagents). Wait for one to finish (TaskWait), \
                 or do this part yourself.",
                self.pool.cap
            ))
        })
    }

    /// A slot, waited for until `stop` when this session holds none;
    /// [`try_take`](Self::try_take) when it holds one.
    pub(crate) async fn take(
        &self,
        stop: &tokio_util::sync::CancellationToken,
    ) -> Result<OwnedSemaphorePermit, DelegateError> {
        if self.holds_slot {
            return self.try_take();
        }
        tokio::select! {
            slot = self.pool.slots.clone().acquire_owned() => slot.map_err(|_| {
                DelegateError::Limit("the session's sub-agent slots are closed".into())
            }),
            _ = stop.cancelled() => Err(DelegateError::Limit(
                "stopped while waiting for a sub-agent slot".into(),
            )),
        }
    }
}

/// Leave to start one sub-agent: the depth it runs at and, when it counts
/// against the session's cap, the slot it holds until it ends. Only
/// [`Limits`] gives one out.
pub(crate) struct Admission {
    depth: u32,
    _slot: Option<OwnedSemaphorePermit>,
}

impl Admission {
    pub(crate) fn depth(&self) -> u32 {
        self.depth
    }
}

pub(crate) struct Limits {
    /// Delegation depth of the agent that owns these limits, not of the
    /// children it spawns — those get `depth + 1`.
    pub(crate) depth: u32,
    pub(crate) max_depth: u32,
    /// Sub-agents running anywhere under the root session; the pool is
    /// shared by every delegate's limits (see `AgentsModule::for_delegates`).
    pub(crate) slots: SlotRule,
    /// Shared the same way.
    pub(crate) hook_agents: HookAgents,
    pub(crate) max_persistent_members: usize,
    pub(crate) member_idle_timeout_secs: u64,
}

impl Limits {
    pub(crate) fn new(
        config: &base::context::EngineConfig,
        depth: u32,
        subagent_slots: SubagentSlots,
        hook_agents: HookAgents,
    ) -> Self {
        Self {
            depth,
            max_depth: config.max_agent_depth,
            slots: SlotRule::new(subagent_slots, depth),
            hook_agents,
            max_persistent_members: config.team_max_persistent_members.max(1),
            member_idle_timeout_secs: config.team_member_idle_timeout_secs,
        }
    }

    /// Depth to build a child at, or the message to hand the model when the
    /// chain has run out of room.
    ///
    /// Every spawn path goes through here, because a delegate's own agents
    /// module registers an `Agent` of its own below the limit: tool
    /// filtering cannot bound them. Refusing with a tool error lets the model
    /// do the work itself instead of recursing until the process dies.
    pub(crate) fn spawn_guard(&self) -> Result<u32, DelegateError> {
        let max = self.max_depth;
        if self.depth >= max {
            return Err(DelegateError::Depth(format!(
                "Delegation depth limit reached ({max}). This agent is already {} \
                 level(s) deep and cannot spawn another sub-agent. Complete the \
                 task directly instead of delegating it.",
                self.depth
            )));
        }
        Ok(self.depth + 1)
    }

    /// Leave with a slot, refused at the cap.
    pub(crate) fn admit_now(&self) -> Result<Admission, DelegateError> {
        let depth = self.spawn_guard()?;
        Ok(Admission {
            depth,
            _slot: Some(self.slots.try_take()?),
        })
    }

    /// Leave with a slot, by [`SlotRule::take`]: waited for, until `stop`,
    /// only when this session holds no slot of its own.
    pub(crate) async fn admit(
        &self,
        stop: &tokio_util::sync::CancellationToken,
    ) -> Result<Admission, DelegateError> {
        let depth = self.spawn_guard()?;
        Ok(Admission {
            depth,
            _slot: Some(self.slots.take(stop).await?),
        })
    }

    /// Leave that takes no slot, for hook agents: they are the session's
    /// configuration rather than the model's to start.
    pub(crate) fn admit_unslotted(&self) -> Result<Admission, DelegateError> {
        Ok(Admission {
            depth: self.spawn_guard()?,
            _slot: None,
        })
    }
}
