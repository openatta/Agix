//! What a delegation adds to the policy's own rules for its delegate's calls
//! (`PermissionRequest::delegate`).
//!
//! Its `deny` rules refuse the call in any mode, `bypassPermissions`
//! included. Its `allow` rules are the delegate's own grants — a forked
//! skill's `allowed-tools`, a skill the delegate ran itself — and answer only
//! the question the mode would put: a deny, a refusal of the mode, an ask
//! rule and anything only a person may answer stand. They never reach the
//! session relaying the call, so its own calls are asked about as before.

use super::judge::Call;
use super::Verdict;
use crate::ruleset::{RuleHit, RuleSet};
use base::interface::permission::{Answerable, Basis, PermissionRequest, Reason, Stage};
use base::permission::RuleBehavior;

#[derive(Default)]
pub(super) struct Delegated {
    denies: RuleSet,
    grants: RuleSet,
}

impl Delegated {
    pub(super) fn of(req: &PermissionRequest<'_>) -> Self {
        let mut out = Self::default();
        for rule in req.delegate.iter().flat_map(|d| d.rules) {
            match rule.behavior {
                RuleBehavior::Deny => out.denies.add(rule.clone()),
                RuleBehavior::Allow => out.grants.add(rule.clone()),
                RuleBehavior::Ask => {}
            }
        }
        out
    }

    pub(super) fn denial(&self, call: &Call<'_>) -> Option<Reason> {
        if self.denies.is_empty() {
            return None;
        }
        denial_of(call.name, call.named.hit(&self.denies, call.name, call.ctx))
    }

    pub(super) fn denial_by_name(&self, tool_name: &str) -> Option<Reason> {
        if self.denies.is_empty() {
            return None;
        }
        denial_of(tool_name, self.denies.evaluate(tool_name, None))
    }

    /// `verdict`, or a permit where it is the mode's question and a grant
    /// covers the call.
    pub(super) fn granting(&self, verdict: Verdict, call: &Call<'_>) -> Verdict {
        let modes_question = matches!(
            &verdict,
            Verdict::Ask {
                reason,
                answerable: Answerable::HookOrPerson,
            } if matches!(reason.stage, Stage::ModeDefault | Stage::Configuration)
        );
        if !modes_question || self.grants.is_empty() {
            return verdict;
        }
        match call.named.hit(&self.grants, call.name, call.ctx) {
            RuleHit::Allow(rule) => Verdict::Permit(Basis::Rule(rule)),
            _ => verdict,
        }
    }
}

/// A command whose program cannot be told from its text is refused too: with
/// only deny rules here, there is nobody whose ask it could become.
fn denial_of(tool_name: &str, hit: RuleHit) -> Option<Reason> {
    let (rule, message) = match hit {
        RuleHit::Deny(rule) => {
            let message = format!("denied by rule: {}", crate::rule::format_rule_string(&rule));
            (rule, message)
        }
        RuleHit::Unclear(rule) => {
            let message = super::subject::unclear_message(tool_name, &rule);
            (rule, message)
        }
        RuleHit::Allow(_) | RuleHit::Ask(_) | RuleHit::None => return None,
    };
    Some(Reason::new(Stage::DelegateRule, message).with_rule(rule))
}
