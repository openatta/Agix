//! The answering half of the policy's order (S15–S18): a question settled
//! without a person where it can be, refused where nobody can be asked, and
//! a permit given the confinement its basis calls for.
//!
//! The sandbox's say (S15) is weighed only for the question the mode asks,
//! after rules and configuration have had theirs, which is where an unattended
//! command's allowance always sat. A hook's approve (S16) comes before the
//! refusal for want of a person (S17): it is an answer given in advance.

use super::{Policy, Verdict};
use base::interface::permission::{
    Answerable, Attendance, Basis, ExecProfile, HookAnswer, Obligations, PermissionOutcome, Reach,
    Reason, Stage, ToolFacts,
};
use base::tool::ToolContext;

impl Policy {
    pub(super) fn conclude(
        &self,
        verdict: Verdict,
        facts: &ToolFacts,
        ctx: &ToolContext,
        hook: &HookAnswer,
        attendance: Attendance,
    ) -> PermissionOutcome {
        let confinement = ctx.confinement;
        let (reason, answerable) = match verdict {
            Verdict::Permit(basis) => return permit(basis, facts, attendance),
            Verdict::Deny(reason) => return PermissionOutcome::Deny { reason },
            Verdict::Ask { reason, answerable } => (reason, answerable),
        };
        if reason.stage == Stage::ModeDefault
            && attendance == Attendance::Unattended
            && facts.runs.is_some()
            && matches!(facts.reach, Reach::Unknown | Reach::Inside)
            && confinement.holds_unattended()
        {
            return permit(Basis::UnattendedConfined, facts, attendance);
        }
        if let (HookAnswer::Approve { hook }, Answerable::HookOrPerson) = (hook, answerable) {
            return permit(Basis::HookApproved(hook.clone()), facts, attendance);
        }
        match attendance {
            Attendance::Unattended => PermissionOutcome::Deny {
                reason: Reason::new(
                    Stage::Attendance,
                    format!(
                        "{} — and nobody is attending this session to approve it. Work within \
                         what is allowed; if the task needs this, report it: the instance's \
                         settings grant it (`sandbox.additional_directories`, \
                         `sandbox.read_directories`, or a `permission_rules` allow rule).",
                        reason.message
                    ),
                ),
            },
            Attendance::NoOneToAsk(why) => PermissionOutcome::Deny {
                reason: Reason::new(Stage::Attendance, format!("{} — {why}", reason.message)),
            },
            _ => PermissionOutcome::Prompt {
                prompt_id: uuid::Uuid::new_v4().to_string(),
                message: reason.message.clone(),
                paths: Vec::new(),
                reason,
                answerable,
            },
        }
    }
}

fn permit(basis: Basis, facts: &ToolFacts, attendance: Attendance) -> PermissionOutcome {
    let profile = ExecProfile::for_permit(&basis, facts.runs, attendance);
    let obligations = Obligations::after(facts.runs, profile);
    PermissionOutcome::Permit {
        basis,
        profile,
        obligations,
    }
}
