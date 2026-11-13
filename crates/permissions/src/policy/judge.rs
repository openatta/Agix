//! The judging half of the policy's order (S2–S14): what a call's facts, the
//! rules and the mode make of it before anyone is asked anything.
//!
//! Three things here are deliberately late. A tool's word that its call is
//! contained (S12) is honoured only after deny rules, the bypass-immune paths
//! and the mode's refusals (S8–S11) have had their say, or none of them would
//! reach `Write` or a read-only command. A write to configuration skips that
//! fast path but not a user's allow rule (S13). And the managed layer's ask is
//! reported as its own even when another rule asks too, so that a hook cannot
//! answer it.

use super::delegated::Delegated;
use super::subject::{managed_ask_of, unclear_message, Named};
use super::{Policy, Verdict};
use crate::rule::format_rule_string;
use crate::ruleset::RuleHit;
use base::interface::permission::{
    Answerable, Basis, HookAnswer, ModeStance, Reach, Reason, Stage, ToolFacts,
};
use base::permission::{PermissionMode, PermissionRule};
use base::tool::ToolContext;

pub(super) struct Call<'a> {
    pub(super) name: &'a str,
    pub(super) facts: &'a ToolFacts,
    pub(super) ctx: &'a ToolContext,
    pub(super) mode: PermissionMode,
    pub(super) hook: &'a HookAnswer,
    pub(super) named: Named,
    pub(super) delegated: Delegated,
}

struct Asked {
    rule: PermissionRule,
    message: String,
    managed: bool,
}

impl Asked {
    fn stage(&self) -> Stage {
        if self.managed {
            Stage::ManagedRule
        } else {
            Stage::Rule
        }
    }
}

impl Policy {
    pub(super) fn judge(&self, call: &Call<'_>) -> Verdict {
        if let Some(path) = call.named.credential(call.ctx, &self.sandbox.paths) {
            return Verdict::deny(
                Stage::Credentials,
                format!(
                    "'{path}' holds credentials; no tool reads or writes it, in any permission mode"
                ),
            );
        }
        if let HookAnswer::Block { reason, .. } = call.hook {
            return Verdict::deny(Stage::Hook, format!("Blocked by PreToolUse hook: {reason}"));
        }
        if let Some(denied) = call.delegated.denial(call) {
            return Verdict::Deny(denied);
        }
        if call.mode == PermissionMode::BypassPermissions {
            return self.under_bypass(call);
        }
        if let Reach::Refused(why) = &call.facts.reach {
            return Verdict::deny(Stage::ToolRefusal, why.clone());
        }

        let (rule_allow, rule_ask) = match self.rule_hit(call) {
            Err(denied) => return denied,
            Ok(hits) => hits,
        };
        if let Some(content) = call.named.bypass_immune() {
            return Verdict::deny(
                Stage::BypassImmune,
                format!(
                    "access to protected path '{content}' is blocked regardless of \
                     permission mode — this path is bypass-immune"
                ),
            );
        }
        if let Some(refused) = mode_refusal(call, rule_allow.is_some()) {
            return refused;
        }
        if let Some(asked) = rule_ask {
            return match call.mode {
                PermissionMode::DontAsk => Verdict::Deny(
                    Reason::new(asked.stage(), format!("dontAsk mode: {}", asked.message))
                        .with_rule(asked.rule),
                ),
                _ => Verdict::Ask {
                    answerable: if asked.managed {
                        Answerable::PersonOnly
                    } else {
                        Answerable::HookOrPerson
                    },
                    reason: Reason::new(asked.stage(), asked.message).with_rule(asked.rule),
                },
            };
        }

        let writes_configuration = !call.facts.traits.read_only && call.named.names_configuration();
        let background = call.facts.runs.is_some_and(|r| r.background);
        if contained(&call.facts.reach) && !background && !writes_configuration {
            return Verdict::Permit(Basis::Contained);
        }
        if let Some(rule) = rule_allow {
            return Verdict::Permit(Basis::Rule(rule));
        }
        by_mode(call, writes_configuration)
    }

    /// `bypassPermissions` answers to the managed layer alone.
    fn under_bypass(&self, call: &Call<'_>) -> Verdict {
        let hit = if self.managed.rules.is_empty() {
            RuleHit::None
        } else {
            call.named.hit(&self.managed.rules, call.name, call.ctx)
        };
        if let RuleHit::Deny(rule) = hit {
            return Verdict::Deny(
                Reason::new(
                    Stage::ManagedRule,
                    format!("denied by managed rule: {}", format_rule_string(&rule)),
                )
                .with_rule(rule),
            );
        }
        match managed_ask_of(call.name, hit) {
            Some((rule, message)) => Verdict::Ask {
                reason: Reason::new(Stage::ManagedRule, message).with_rule(rule),
                answerable: Answerable::PersonOnly,
            },
            None => Verdict::Permit(Basis::Bypass),
        }
    }

    /// The rules' allow and ask for the call, or its denial.
    fn rule_hit(
        &self,
        call: &Call<'_>,
    ) -> Result<(Option<PermissionRule>, Option<Asked>), Verdict> {
        let hit = call.named.hit(&self.rules_now(), call.name, call.ctx);
        let asked = |rule: PermissionRule, message: String| Asked {
            managed: self.managed.rules.rules().contains(&rule),
            rule,
            message,
        };
        let (allow, ask) = match hit {
            RuleHit::Deny(rule) => {
                let stage = if self.managed.rules.rules().contains(&rule) {
                    Stage::ManagedRule
                } else {
                    Stage::Rule
                };
                return Err(Verdict::Deny(
                    Reason::new(
                        stage,
                        format!("denied by rule: {}", format_rule_string(&rule)),
                    )
                    .with_rule(rule),
                ));
            }
            RuleHit::Allow(rule) => (Some(rule), None),
            RuleHit::Ask(rule) => {
                let message = format!(
                    "{} matches {}, which requires confirmation",
                    call.name,
                    format_rule_string(&rule)
                );
                (None, Some(asked(rule, message)))
            }
            RuleHit::Unclear(rule) => {
                let message = unclear_message(call.name, &rule);
                (None, Some(asked(rule, message)))
            }
            RuleHit::None => (None, None),
        };
        // Reported as the managed layer's whenever it is among the askers,
        // even when a more specific rule of another layer asks too.
        let ask = ask.map(|ask| {
            let managed = if self.managed.rules.is_empty() {
                None
            } else {
                managed_ask_of(
                    call.name,
                    call.named.hit(&self.managed.rules, call.name, call.ctx),
                )
            };
            match managed {
                Some((rule, message)) => Asked {
                    rule,
                    message,
                    managed: true,
                },
                None => ask,
            }
        });
        Ok((allow, ask))
    }
}

fn contained(reach: &Reach) -> bool {
    matches!(
        reach,
        Reach::SessionOnly | Reach::Delegates | Reach::Operator | Reach::Inside
    )
}

/// `Plan` refuses what is not read-only, `DontAsk` what no rule allows; a
/// contained coordination call is spared both.
fn mode_refusal(call: &Call<'_>, allowed_by_rule: bool) -> Option<Verdict> {
    let facts = call.facts;
    let spared = contained(&facts.reach) && facts.stance == ModeStance::Coordination;
    if spared {
        return None;
    }
    if call.mode == PermissionMode::Plan
        && !facts.traits.read_only
        && facts.stance != ModeStance::LeavesPlan
    {
        return Some(Verdict::deny(
            Stage::PlanMode,
            "plan mode forbids non-readonly tools",
        ));
    }
    if call.mode == PermissionMode::DontAsk && !allowed_by_rule {
        return Some(Verdict::deny(
            Stage::DontAsk,
            "dontAsk mode and no rule matched",
        ));
    }
    None
}

fn by_mode(call: &Call<'_>, writes_configuration: bool) -> Verdict {
    let facts = call.facts;
    let stage = if writes_configuration {
        Stage::Configuration
    } else {
        Stage::ModeDefault
    };
    let question = || {
        facts
            .question
            .clone()
            .unwrap_or_else(|| format!("Allow {}?", call.name))
    };
    let ask = |message: String| Verdict::Ask {
        reason: Reason::new(stage, message),
        answerable: Answerable::HookOrPerson,
    };
    match call.mode {
        PermissionMode::AcceptEdits
            if facts.traits.read_only
                && matches!(
                    facts.reach,
                    Reach::SessionOnly | Reach::Inside | Reach::Host { listed: true }
                ) =>
        {
            Verdict::Permit(Basis::AcceptEdits)
        }
        // A `DontAsk` call got here only with an allow rule, which let it
        // through above; refusing keeps it closed if that order changes.
        PermissionMode::DontAsk => {
            Verdict::deny(Stage::DontAsk, "dontAsk mode and no rule matched")
        }
        PermissionMode::Bubble => {
            ask("Bubble: forwarding permission request to parent agent".into())
        }
        PermissionMode::AcceptEdits
        | PermissionMode::Default
        | PermissionMode::Plan
        | PermissionMode::BypassPermissions => ask(question()),
    }
}
