//! 一个会话的闸口：权限、向宿主提的问题、hooks，以及回合开始时要重置的授权与计数。

use crate::agent::{PendingPermissions, PermissionDecision};
use crate::elicitation::PendingQuestions;
use base::interface::elicitation::Elicitation;
use base::interface::permission::PermissionSet;
use hooks::HookRunner;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

/// Permission, the questions put to the host, and hooks.
pub(crate) struct Gate {
    permission: PermissionSet,
    /// A `prompt_id` with no entry (timed out, stale or duplicate response)
    /// is ignored by whoever resolves it.
    pending_permissions: PendingPermissions,
    pending_questions: PendingQuestions,
    elicitation: Arc<dyn Elicitation>,
    /// Atomic and shared because the input demultiplexer resolves permission
    /// responses off the turn's thread, with no `&mut self`.
    permission_denial_count: Arc<AtomicU32>,
    hooks: Arc<HookRunner>,
    turn_starts: Arc<Vec<crate::plugin::TurnStart>>,
    /// Set before the turn begins and taken once its prompt has been screened;
    /// a turn a hook blocks leaves them for the next one.
    slash_skill_grants: Vec<(String, Option<String>)>,
}

/// What the input demultiplexer resolves responses against, off the turn.
pub(crate) struct DemuxHandles {
    pub(crate) pending: PendingPermissions,
    pub(crate) questions: PendingQuestions,
    pub(crate) denials: Arc<AtomicU32>,
}

impl Gate {
    pub(crate) fn new(
        permission: PermissionSet,
        elicitation: Arc<dyn Elicitation>,
        pending_permissions: PendingPermissions,
        pending_questions: PendingQuestions,
        hooks: Arc<HookRunner>,
        turn_starts: Vec<crate::plugin::TurnStart>,
    ) -> Self {
        Self {
            permission,
            elicitation,
            pending_permissions,
            pending_questions,
            permission_denial_count: Arc::new(AtomicU32::new(0)),
            hooks,
            turn_starts: Arc::new(turn_starts),
            slash_skill_grants: Vec::new(),
        }
    }

    pub(crate) fn permission(&self) -> &PermissionSet {
        &self.permission
    }

    #[cfg(test)]
    pub(crate) fn permission_mut(&mut self) -> &mut PermissionSet {
        &mut self.permission
    }

    pub(crate) fn hooks(&self) -> &Arc<HookRunner> {
        &self.hooks
    }

    pub(crate) fn elicitation(&self) -> &Arc<dyn Elicitation> {
        &self.elicitation
    }

    pub(crate) fn demux_handles(&self) -> DemuxHandles {
        DemuxHandles {
            pending: self.pending_permissions.clone(),
            questions: self.pending_questions.clone(),
            denials: self.permission_denial_count.clone(),
        }
    }

    pub(crate) fn resolve_permission(&self, prompt_id: String, decision: PermissionDecision) {
        super::resolve_permission_response(
            &self.pending_permissions,
            &self.permission_denial_count,
            prompt_id,
            decision,
        );
    }

    pub(crate) fn resolve_question(&self, prompt_id: &str, answer: crate::agent::QuestionAnswer) {
        crate::elicitation::resolve_question_response(&self.pending_questions, prompt_id, answer);
    }

    pub(crate) fn denials(&self) -> u32 {
        self.permission_denial_count.load(Ordering::Relaxed)
    }

    pub(crate) fn reset_denials(&self) {
        self.permission_denial_count.store(0, Ordering::Relaxed);
    }

    pub(crate) fn run_turn_starts(&self) {
        for turn_start in self.turn_starts.iter() {
            turn_start();
        }
    }

    pub(crate) fn set_slash_grants(&mut self, grants: Vec<(String, Option<String>)>) {
        self.slash_skill_grants = grants;
    }

    /// Drop the previous turn's grants and give this one the slash skill's.
    pub(crate) async fn begin_turn(&mut self) {
        use base::interface::permission::{Grant, GrantScope};
        let slash_skill_grants = std::mem::take(&mut self.slash_skill_grants);
        if let Some(grants) = &self.permission.grants {
            grants.clear(GrantScope::Turn);
            for (tool, pattern) in slash_skill_grants {
                let _ = grants
                    .grant(Grant::new(tool, pattern, GrantScope::Turn))
                    .await;
            }
        }
    }
}
