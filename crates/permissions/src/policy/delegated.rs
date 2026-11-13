//! What a delegation adds to the policy's own rules for its delegate's calls
//! (`PermissionRequest::delegate`): its `deny` rules, which refuse the call in
//! any mode, `bypassPermissions` included — a delegate can be held to less
//! than its parent, never let through more.

use super::judge::Call;
use crate::ruleset::{RuleHit, RuleSet};
use base::interface::permission::{PermissionRequest, Reason, Stage};
use base::permission::RuleBehavior;

#[derive(Default)]
pub(super) struct Delegated {
    denies: RuleSet,
}

impl Delegated {
    pub(super) fn of(req: &PermissionRequest<'_>) -> Self {
        let mut denies = RuleSet::empty();
        if let Some(delegate) = req.delegate {
            denies.extend(
                delegate
                    .rules
                    .iter()
                    .filter(|r| r.behavior == RuleBehavior::Deny)
                    .cloned(),
            );
        }
        Self { denies }
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
