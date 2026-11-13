//! `Policy` — the engine's permission: every tool call is decided here, in
//! one order, and the allows a session is handed are kept here as rules.
//!
//! The order has two halves. Judging (`judge`) weighs the call's facts, the
//! rules and the mode and comes to a permit, a refusal or a question. Answering
//! (`answer`) settles a question without a person where it can — a sandbox
//! that holds an unattended command, a hook's approve — and refuses it where
//! nobody can be asked. A question still open is the caller's to put.
//!
//! A person's "allow every time" is kept apart from the rules (`exact`): it
//! allows the call it was given for, by the same text, and nothing its text
//! would match as a pattern.
//!
//! A host's own policy is one more tier of rules (`host`): asked about every
//! call, its refusal stands, its permit is an allow rule and no more.
//!
//! The managed layer's rules are among `rules` like any other tier's, and held
//! again in `ManagedLayer`: that copy is what a `bypassPermissions` session
//! still answers to, and what marks a question only a person may answer.

mod answer;
mod delegated;
mod host;
mod judge;
mod subject;
#[cfg(test)]
mod tests;

use crate::ruleset::RuleSet;
use async_trait::async_trait;
use base::interface::permission::{
    Answerable, Attendance, Basis, Grant, GrantError, GrantMatch, GrantScope, PermissionGrants,
    PermissionOutcome, PermissionPolicy, PermissionRequest, Reason, Stage, ToolFacts,
};
use base::permission::{PermissionMode, PermissionRule, RuleBehavior, RuleSource};
use base::tool::SandboxSettings;
use delegated::Delegated;
use host::HostSays;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

pub struct Policy {
    rules: RwLock<RuleSet>,
    /// Allows matched by their content's exact text.
    exact: RwLock<Vec<PermissionRule>>,
    managed: ManagedLayer,
    /// What the context of a call made without one is given, and where its
    /// credential locations are named.
    sandbox: SandboxSettings,
    /// Where a [`GrantScope::Project`] grant is written. `None` — a session
    /// with no project — keeps it for the session only: the state root's
    /// `settings.local.json` is the global tier, and a rule written there would
    /// allow the call everywhere.
    local_settings_file: Option<PathBuf>,
    host: Option<Arc<dyn PermissionPolicy>>,
    /// Set when nothing can put a question to a person: an attended call's
    /// question is refused for this reason instead.
    no_one_to_ask: Option<&'static str>,
}

/// What the deployment's managed settings (`base::managed_settings`) hold
/// every call to, whatever the session's mode.
#[derive(Clone, Default)]
pub struct ManagedLayer {
    rules: RuleSet,
    bypass_disabled: bool,
    unreadable: Option<String>,
}

/// A rule in the managed layer that does not parse makes the layer unreadable
/// rather than dropping the rule.
impl From<&Option<base::managed_settings::ManagedSettings>> for ManagedLayer {
    fn from(managed: &Option<base::managed_settings::ManagedSettings>) -> Self {
        let Some(managed) = managed else {
            return Self::default();
        };
        let mut unreadable = managed.error.clone();
        let mut rules = RuleSet::empty();
        for rule in &managed.permission_rules {
            match crate::rule::parse_rule_string(
                &rule.tool,
                RuleSource::PolicySettings,
                rule.action,
            ) {
                Ok(parsed) => rules.add(parsed),
                Err(e) => {
                    unreadable.get_or_insert_with(|| {
                        format!(
                            "{}: permission rule `{}` does not parse: {e}",
                            managed.origin, rule.tool
                        )
                    });
                }
            }
        }
        Self {
            rules,
            bypass_disabled: managed.disable_bypass_permissions_mode,
            unreadable,
        }
    }
}

impl ManagedLayer {
    /// Why the layer cannot be honoured, when it cannot.
    pub fn unreadable(&self) -> Option<&str> {
        self.unreadable.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn of_rules(rules: Vec<PermissionRule>) -> Self {
        Self {
            rules: RuleSet::new(rules),
            ..Self::default()
        }
    }
}

/// What judging a call came to, before anyone is asked.
pub(super) enum Verdict {
    Permit(Basis),
    Deny(Reason),
    Ask {
        reason: Reason,
        answerable: Answerable,
    },
}

impl Verdict {
    fn deny(stage: Stage, message: impl Into<String>) -> Self {
        Self::Deny(Reason::new(stage, message))
    }
}

impl Policy {
    pub fn new(rules: RuleSet) -> Self {
        Self {
            rules: RwLock::new(rules),
            exact: RwLock::default(),
            managed: ManagedLayer::default(),
            sandbox: SandboxSettings::default(),
            local_settings_file: None,
            host: None,
            no_one_to_ask: None,
        }
    }

    pub fn empty() -> Self {
        Self::new(RuleSet::empty())
    }

    /// Every permission-rule tier of `settings` with its own `RuleSource`
    /// (`crate::rule::rules_from`), then `extra_rules` — session-scoped rules
    /// a caller adds on top — with every call held to the managed layer.
    ///
    /// Every embedder that builds its own permission should come through
    /// here; `runtime::agent::Builder` does when it is given none.
    pub fn from_settings(
        settings: &base::settings::LoadedSettings,
        extra_rules: impl IntoIterator<Item = PermissionRule>,
    ) -> Self {
        let mut rules = crate::rule::rules_from(settings);
        rules.extend(extra_rules);
        let mut policy = Self::new(RuleSet::new(rules));
        policy.exact = RwLock::new(crate::rule::exact_allows_from(settings));
        policy
            .with_managed(ManagedLayer::from(&settings.managed))
            .with_sandbox(SandboxSettings {
                allow_read: settings.values.sandbox.allow_read.clone(),
                deny_read: settings.values.sandbox.deny_read.clone(),
                allowed_domains: settings.values.sandbox.allowed_domains.clone(),
                network_mode: settings.values.sandbox.network_mode,
                deny_write: settings.values.sandbox.deny_write.clone(),
                allow_write: settings.values.sandbox.allow_write.clone(),
                paths: std::sync::Arc::new(base::paths::SensitivePaths::for_paths(
                    &base::paths::ConfigPaths::from_settings(&settings.paths),
                )),
                require_enforcement: settings.values.sandbox.require_enforcement,
                unattended_requires_network_policy: settings
                    .values
                    .sandbox
                    .unattended_requires_network_policy,
            })
            .with_local_settings_file(settings.paths.local_settings_file())
    }

    pub fn with_managed(mut self, managed: ManagedLayer) -> Self {
        self.managed = managed;
        self
    }

    pub fn with_sandbox(mut self, sandbox: SandboxSettings) -> Self {
        self.sandbox = sandbox;
        self
    }

    pub fn with_local_settings_file(mut self, file: Option<PathBuf>) -> Self {
        self.local_settings_file = file;
        self
    }

    pub fn with_host(mut self, host: Option<Arc<dyn PermissionPolicy>>) -> Self {
        self.host = host;
        self
    }

    pub fn with_no_one_to_ask(mut self, why: &'static str) -> Self {
        self.no_one_to_ask = Some(why);
        self
    }

    /// Who can be asked about the call: as the request says, unless nothing
    /// reaches a person at all. A session nobody attends stays unattended.
    fn attendance_for(&self, req: &PermissionRequest<'_>) -> Attendance {
        match (req.attendance, self.no_one_to_ask) {
            (Attendance::Attended, Some(why)) => Attendance::NoOneToAsk(why),
            (attendance, _) => attendance,
        }
    }

    pub fn rules(&self) -> Vec<PermissionRule> {
        self.rules_now().rules().to_vec()
    }

    pub fn add_rules(&self, additional: Vec<PermissionRule>) {
        self.rules
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend(additional);
    }

    /// Drop every rule from `source` — see `RuleSet::remove_by_source`.
    pub fn remove_rules_by_source(&self, source: RuleSource) {
        self.rules
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove_by_source(source);
        self.exact
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|r| r.source != source);
    }

    /// The exact allow for a call to `tool_name` whose content is `content`.
    fn exact_allow(&self, tool_name: &str, content: Option<&str>) -> Option<PermissionRule> {
        self.exact
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .find(|r| {
                r.tool_name == tool_name
                    && match (r.rule_content.as_deref(), content) {
                        (None, _) => true,
                        (Some(allowed), Some(content)) => allowed.trim() == content.trim(),
                        (Some(_), None) => false,
                    }
            })
            .cloned()
    }

    fn rules_now(&self) -> std::sync::RwLockReadGuard<'_, RuleSet> {
        self.rules
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The mode the call is judged in: the session's live one, held over a
    /// delegate's as `PermissionMode::for_delegate` says, and never
    /// `bypassPermissions` while the managed layer turns it off.
    fn mode_for(&self, req: &PermissionRequest<'_>) -> PermissionMode {
        let own = req.session.permission_mode();
        let mode = match req.delegate {
            Some(delegate) => own.for_delegate(delegate.mode),
            None => own,
        };
        match mode {
            PermissionMode::BypassPermissions if self.managed.bypass_disabled => {
                PermissionMode::Default
            }
            mode => mode,
        }
    }

    async fn write_project_rule(
        &self,
        path: &std::path::Path,
        grant: &Grant,
    ) -> Result<(), GrantError> {
        let not_persisted = |reason: String| GrantError::NotPersisted {
            path: path.to_path_buf(),
            reason,
        };
        let Some(rule) =
            crate::settings_patch::build_rule_string(&grant.tool_name, grant.content.as_deref())
        else {
            return Err(not_persisted(
                "the call has no content a rule could be scoped to".into(),
            ));
        };
        let target = path.to_path_buf();
        let matching = grant.matching;
        match tokio::task::spawn_blocking(move || match matching {
            GrantMatch::Exact => crate::settings_patch::append_exact_allow(&target, &rule),
            _ => crate::settings_patch::append_permission_rule(
                &target,
                crate::settings_patch::AppendTarget::Allow,
                &rule,
            ),
        })
        .await
        {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => Err(not_persisted(e.to_string())),
            Err(e) => Err(not_persisted(e.to_string())),
        }
    }

    fn allow(&self, source: RuleSource, grant: &Grant) {
        let rule = PermissionRule {
            source,
            behavior: RuleBehavior::Allow,
            tool_name: grant.tool_name.clone(),
            rule_content: grant.content.clone(),
        };
        match grant.matching {
            GrantMatch::Exact => self
                .exact
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(rule),
            _ => self.add_rules(vec![rule]),
        }
    }
}

#[async_trait]
impl PermissionPolicy for Policy {
    /// Judged against the session's own state, or against the delegate's in
    /// the stricter of the two sessions' modes, under this policy's rules.
    async fn decide(&self, req: &PermissionRequest<'_>) -> PermissionOutcome {
        if let Some(why) = self.managed.unreadable() {
            return PermissionOutcome::deny(
                Stage::ManagedUnreadable,
                format!(
                    "the managed settings cannot be honoured, so every tool call is refused \
                     until they are fixed: {why}"
                ),
            );
        }
        let hook = req.hook;
        let attendance = self.attendance_for(req);
        // Not a permit, even though dispatch's own "Tool not found" would
        // follow: a caller that asks before looking the tool up must not be
        // able to run it unchecked by naming it wrongly.
        let delegated = Delegated::of(req);
        if req.tool.is_none() {
            if let Some(denied) = delegated.denial_by_name(req.tool_name) {
                return PermissionOutcome::Deny { reason: denied };
            }
            if let HostSays::Deny(reason) = HostSays::of(self.host.as_ref(), req).await {
                return PermissionOutcome::Deny { reason };
            }
            let unknown = ToolFacts::unknown_tool();
            let verdict = Verdict::Ask {
                reason: Reason::new(
                    Stage::UnknownTool,
                    format!(
                        "`{}` is not a tool this session has. It is either an unknown tool, or \
                         one that reached the permission check without being looked up — \
                         approve only if you expected it.",
                        req.tool_name
                    ),
                ),
                answerable: Answerable::HookOrPerson,
            };
            return self.conclude(verdict, &unknown, req.ctx, hook, attendance);
        }

        let mode = self.mode_for(req);
        let (ctx, facts) = (req.ctx, req.facts);
        let host = HostSays::of(self.host.as_ref(), &req.with_attendance(attendance)).await;
        let call = judge::Call {
            name: req.tool_name,
            facts,
            ctx,
            mode,
            hook,
            named: subject::Named::of(&facts.subject, ctx),
            delegated,
            host,
        };
        let verdict = call.delegated.granting(self.judge(&call), &call);
        self.conclude(verdict, facts, ctx, hook, attendance)
    }
}

#[async_trait]
impl PermissionGrants for Policy {
    async fn grant(&self, grant: Grant) -> Result<(), GrantError> {
        if grant.scope == GrantScope::Turn {
            self.allow(RuleSource::Command, &grant);
            return Ok(());
        }
        // `Session`, not `Command`: `clear(Turn)` drops every `Command` rule
        // when a skill's turn ends, and a person's answer must outlive that.
        self.allow(RuleSource::Session, &grant);
        match (grant.scope, &self.local_settings_file) {
            (GrantScope::Project, Some(path)) => self.write_project_rule(path, &grant).await,
            _ => Ok(()),
        }
    }

    fn clear(&self, scope: GrantScope) {
        self.remove_rules_by_source(match scope {
            GrantScope::Turn => RuleSource::Command,
            _ => RuleSource::Session,
        });
    }
}
