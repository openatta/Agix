//! `Limits`: how many delegates a session tree runs at once, how deep they
//! go, and the leave each one is given to start.

use std::sync::Arc;

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

/// The sessions of the agents running for `"type": "agent"` hooks. Their own
/// events start no agent hooks: a guard that reads a file would otherwise ask
/// another guard about it, and that one another, until the first times out.
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

/// Leave to start one sub-agent: the depth it runs at and, when it counts
/// against the session's cap, the slot it holds until it ends.
pub(crate) struct Admission {
    pub(crate) depth: u32,
    pub(crate) _slot: Option<tokio::sync::OwnedSemaphorePermit>,
}

pub(crate) struct Limits {
    /// Delegation depth of the agent that owns these limits, not of the
    /// children it spawns — those get `depth + 1`.
    pub(crate) depth: u32,
    pub(crate) max_depth: u32,
    /// Sub-agents running anywhere under the root session; shared by every
    /// delegate's limits (see `AgentsModule::for_delegates`).
    pub(crate) subagent_slots: SubagentSlots,
    pub(crate) hook_agents: HookAgents,
    pub(crate) max_persistent_members: usize,
    pub(crate) member_idle_timeout_secs: u64,
}

impl Limits {
    pub(crate) fn new(
        config: &base::context::EngineConfig,
        depth: u32,
        subagent_slots: SubagentSlots,
    ) -> Self {
        Self {
            depth,
            max_depth: config.max_agent_depth,
            subagent_slots,
            hook_agents: HookAgents::default(),
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
    pub(crate) fn spawn_guard(&self) -> Result<u32, String> {
        let max = self.max_depth;
        if self.depth >= max {
            return Err(format!(
                "Delegation depth limit reached ({max}). This agent is already {} \
                 level(s) deep and cannot spawn another sub-agent. Complete the \
                 task directly instead of delegating it.",
                self.depth
            ));
        }
        Ok(self.depth + 1)
    }

    /// Refuses rather than waits when the session is at its cap: an agent
    /// holding a slot while it waits for one for its own child could
    /// deadlock a tree whose every slot is held that way.
    pub(crate) fn admit(&self) -> Result<Admission, String> {
        let depth = self.spawn_guard()?;
        let slot = self
            .subagent_slots
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| {
                format!(
                    "{} sub-agents are already running in this session, the most it allows \
                     (execution.max_concurrent_subagents). Wait for one to finish (TaskWait), \
                     or do this part yourself.",
                    self.subagent_slots.cap
                )
            })?;
        Ok(Admission {
            depth,
            _slot: Some(slot),
        })
    }

    /// [`admit`](Self::admit) for a program's fan-out: at the cap it waits
    /// for a slot instead of refusing. Only a root session runs programs and
    /// a root session holds no slot, so what it waits on is never itself.
    pub(crate) async fn admit_waiting(
        &self,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<Admission, String> {
        let depth = self.spawn_guard()?;
        let slot = tokio::select! {
            slot = self.subagent_slots.slots.clone().acquire_owned() => {
                slot.map_err(|_| "the session's sub-agent slots are closed".to_string())?
            }
            _ = cancel.cancelled() => {
                return Err("stopped while waiting for a sub-agent slot".into());
            }
        };
        Ok(Admission {
            depth,
            _slot: Some(slot),
        })
    }

    /// Leave that takes no slot: for `TeamCreate` members, bounded by
    /// `team_stage_concurrency` instead, and for hook agents, which are the
    /// session's configuration rather than the model's to start.
    pub(crate) fn admit_unslotted(&self) -> Result<Admission, String> {
        Ok(Admission {
            depth: self.spawn_guard()?,
            _slot: None,
        })
    }
}
