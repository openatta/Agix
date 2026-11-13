//! A host's own `PermissionPolicy`, weighed as one more tier of rules (S8):
//! its refusal stands in every mode, `bypassPermissions` included; its
//! question is a rule's ask; its permit is an allow rule, which a deny, an
//! ask, a refusal of the mode or the policy's own checks still override.

use super::Verdict;
use base::interface::permission::{
    Answerable, PermissionOutcome, PermissionPolicy, PermissionRequest, Reason, Stage,
};
use base::permission::{PermissionRule, RuleBehavior, RuleSource};
use std::sync::Arc;

pub(super) enum HostSays {
    Nothing,
    Allow(PermissionRule),
    Ask {
        reason: Reason,
        answerable: Answerable,
    },
    Deny(Reason),
}

impl HostSays {
    pub(super) async fn of(
        host: Option<&Arc<dyn PermissionPolicy>>,
        req: &PermissionRequest<'_>,
    ) -> Self {
        let Some(host) = host else {
            return Self::Nothing;
        };
        let rule = |behavior| PermissionRule {
            source: RuleSource::Host,
            behavior,
            tool_name: req.tool_name.to_string(),
            rule_content: None,
        };
        match host.decide(req).await {
            PermissionOutcome::Permit { .. } => Self::Allow(rule(RuleBehavior::Allow)),
            PermissionOutcome::Deny { reason } => Self::Deny(
                Reason::new(
                    Stage::Rule,
                    format!("denied by the host: {}", reason.message),
                )
                .with_rule(rule(RuleBehavior::Deny)),
            ),
            PermissionOutcome::Prompt {
                message,
                answerable,
                ..
            } => Self::Ask {
                reason: Reason::new(Stage::Rule, message).with_rule(rule(RuleBehavior::Ask)),
                answerable,
            },
        }
    }

    pub(super) fn denial(&self) -> Option<Verdict> {
        match self {
            Self::Deny(reason) => Some(Verdict::Deny(reason.clone())),
            _ => None,
        }
    }
}
