//! The answering half of the policy's order (S15–S18): a question settled
//! without a person where it can be, refused where nobody can be asked, and
//! a permit given the confinement its basis calls for.
//!
//! The sandbox's say (S15) is weighed only for the question the mode asks,
//! after rules and configuration have had theirs, which is where an unattended
//! command's allowance always sat. A hook's approve (S16) comes before the
//! refusal for want of a person (S17): it is an answer given in advance.

use super::judge::Call;
use super::{Policy, Verdict};
use crate::ruleset::RuleHit;
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
        ctx: Option<&ToolContext>,
        hook: &HookAnswer,
        attendance: Option<Attendance>,
    ) -> PermissionOutcome {
        let confinement = ctx.map(|c| c.confinement).unwrap_or_default();
        let (reason, answerable) = match verdict {
            Verdict::Permit(basis) => return permit(basis, facts, attendance),
            Verdict::Deny(reason) => return PermissionOutcome::Deny { reason },
            Verdict::Ask { reason, answerable } => (reason, answerable),
        };
        if reason.stage == Stage::ModeDefault
            && attendance == Some(Attendance::Unattended)
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
            Some(Attendance::Unattended) => PermissionOutcome::Deny {
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
            Some(Attendance::NoOneToAsk(why)) => PermissionOutcome::Deny {
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

    /// A deny-rules-only policy: its rules' denial, or nothing in the way.
    pub(super) fn only_denials(&self, call: &Call<'_>) -> PermissionOutcome {
        let hit = call.named.hit(&self.rules_now(), call.name, call.ctx);
        denial_of(call.name, hit)
    }

    pub(super) fn only_denials_by_name(&self, tool_name: &str) -> PermissionOutcome {
        denial_of(tool_name, self.rules_now().evaluate(tool_name, None))
    }
}

fn denial_of(tool_name: &str, hit: RuleHit) -> PermissionOutcome {
    let (rule, message) = match hit {
        RuleHit::Deny(rule) => {
            let message = format!("denied by rule: {}", crate::rule::format_rule_string(&rule));
            (rule, message)
        }
        RuleHit::Unclear(rule) => {
            let message = super::subject::unclear_message(tool_name, &rule);
            (rule, message)
        }
        _ => {
            return PermissionOutcome::Permit {
                basis: Basis::Bypass,
                profile: ExecProfile::Session,
                obligations: Obligations::default(),
            }
        }
    };
    PermissionOutcome::Deny {
        reason: Reason::new(Stage::DelegateRule, message).with_rule(rule),
    }
}

/// The confinement a permitted call runs under follows from why it was let
/// through: on a command's own word it is held tighter than one a person (or
/// a rule, a hook, bypass) let through, and nobody attending tightens it too.
fn permit(basis: Basis, facts: &ToolFacts, attendance: Option<Attendance>) -> PermissionOutcome {
    let unattended = attendance == Some(Attendance::Unattended);
    let profile = match (&basis, facts.runs) {
        (Basis::Contained | Basis::AcceptEdits, Some(_)) => ExecProfile::Strict,
        (Basis::UnattendedConfined, _) => ExecProfile::Unattended,
        _ if unattended => ExecProfile::Unattended,
        _ => ExecProfile::Session,
    };
    let obligations =
        Obligations::scanning_nested_repos(facts.runs.is_some() && profile != ExecProfile::Strict);
    PermissionOutcome::Permit {
        basis,
        profile,
        obligations,
    }
}
