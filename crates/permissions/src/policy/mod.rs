//! `Policy` — the engine's permission: every tool call is decided here, in
//! one order, and the allows a session is handed are kept here as rules.
//!
//! The order has two halves. Judging (`judge`) weighs the call's facts, the
//! rules and the mode and comes to a permit, a refusal or a question. Answering
//! (`answer`) settles a question without a person where it can — a sandbox
//! that holds an unattended command, a hook's approve — and refuses it where
//! nobody can be asked. A question still open is the caller's to put.
//!
//! The managed layer's rules are among `rules` like any other tier's, and held
//! again in `ManagedLayer`: that copy is what a `bypassPermissions` session
//! still answers to, and what marks a question only a person may answer.

mod answer;
mod delegated;
mod judge;
mod subject;
#[cfg(test)]
mod tests;

use crate::ruleset::RuleSet;
use async_trait::async_trait;
use base::interface::permission::{
    Answerable, Basis, Grant, GrantError, GrantScope, HookAnswer, PermissionGrants,
    PermissionOutcome, PermissionPolicy, PermissionRequest, Reason, Stage, ToolFacts,
};
use base::permission::{PermissionMode, PermissionRule, RuleBehavior, RuleSource};
use base::tool::{SandboxSettings, ToolContext};
use delegated::Delegated;
use std::path::PathBuf;
use std::sync::RwLock;

pub struct Policy {
    rules: RwLock<RuleSet>,
    managed: ManagedLayer,
    /// What the context of a call made without one is given, and where its
    /// credential locations are named.
    sandbox: SandboxSettings,
    /// Where a [`GrantScope::Project`] grant is written. `None` — a session
    /// with no project — keeps it for the session only: the state root's
    /// `settings.local.json` is the global tier, and a rule written there would
    /// allow the call everywhere.
    local_settings_file: Option<PathBuf>,
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
            let behavior = match rule.action {
                base::settings::PermissionAction::Allow => RuleBehavior::Allow,
                base::settings::PermissionAction::Deny => RuleBehavior::Deny,
                base::settings::PermissionAction::Ask => RuleBehavior::Ask,
            };
            match crate::rule::parse_rule_string(&rule.tool, RuleSource::PolicySettings, behavior) {
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
            managed: ManagedLayer::default(),
            sandbox: SandboxSettings::default(),
            local_settings_file: None,
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
    /// here: `runtime::agent::Builder` takes an injected `PermissionSet` and
    /// reads no rules from `Settings`.
    pub fn from_settings(
        settings: &base::settings::LoadedSettings,
        extra_rules: impl IntoIterator<Item = PermissionRule>,
    ) -> Self {
        let mut rules = crate::rule::rules_from(settings);
        rules.extend(extra_rules);
        Self::new(RuleSet::new(rules))
            .with_managed(ManagedLayer::from(&settings.managed))
            .with_sandbox(SandboxSettings {
                allow_read: settings.sandbox.allow_read.clone(),
                deny_read: settings.sandbox.deny_read.clone(),
                allowed_domains: settings.sandbox.allowed_domains.clone(),
                network_mode: settings.sandbox.network_mode,
                deny_write: settings.sandbox.deny_write.clone(),
                allow_write: settings.sandbox.allow_write.clone(),
                paths: std::sync::Arc::new(base::paths::SensitivePaths::for_paths(
                    &base::paths::ConfigPaths::from_settings(&settings.paths),
                )),
                require_enforcement: settings.sandbox.require_enforcement,
                unattended_requires_network_policy: settings
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

    /// The context of a call whose request brought none: the judged
    /// session's, with this policy's sandbox settings. A session that never
    /// had providers bound is judged in-process, and no sandbox is known to
    /// hold its commands.
    fn context_for(&self, req: &PermissionRequest<'_>, mode: PermissionMode) -> ToolContext {
        let judged = req.judged();
        let exec = judged
            .exec()
            .unwrap_or_else(base::interface::exec::ExecProviders::in_process);
        let mut ctx = ToolContext::from_engine_ctx_with(
            req.cwd.to_path_buf(),
            tokio_util::sync::CancellationToken::new(),
            exec,
        );
        ctx.session_id = req.session_id.to_string();
        ctx.sandbox = self.sandbox.clone();
        ctx.session = judged.clone();
        ctx.permission_mode = mode;
        let env = ctx.session.env();
        ctx.additional_writable_dirs = env.additional_writable_dirs.clone();
        ctx.dangerously_disable_sandbox = env.sandbox_disabled;
        if let Some(attendance) = req.attendance {
            ctx.attendance = attendance;
        }
        ctx
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
        match tokio::task::spawn_blocking(move || {
            crate::settings_patch::append_permission_rule(
                &target,
                crate::settings_patch::AppendTarget::Allow,
                &rule,
            )
        })
        .await
        {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => Err(not_persisted(e.to_string())),
            Err(e) => Err(not_persisted(e.to_string())),
        }
    }

    fn allow(&self, source: RuleSource, tool_name: &str, rule_content: Option<String>) {
        self.add_rules(vec![PermissionRule {
            source,
            behavior: RuleBehavior::Allow,
            tool_name: tool_name.to_string(),
            rule_content,
        }]);
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
        let hook = req.hook.unwrap_or(&HookAnswer::None);
        // Not a permit, even though dispatch's own "Tool not found" would
        // follow: a caller that asks before looking the tool up must not be
        // able to run it unchecked by naming it wrongly.
        let delegated = Delegated::of(req);
        let Some(tool) = req.tool else {
            if let Some(denied) = delegated.denial_by_name(req.tool_name) {
                return PermissionOutcome::Deny { reason: denied };
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
            return self.conclude(verdict, &unknown, None, hook, req.attendance);
        };

        let mode = self.mode_for(req);
        let own_ctx;
        let ctx = match req.ctx {
            Some(ctx) => ctx,
            None => {
                own_ctx = self.context_for(req, mode);
                &own_ctx
            }
        };
        let own_facts;
        let facts = match req.facts {
            Some(facts) => facts,
            None => {
                own_facts = ToolFacts::bridged(tool.as_ref(), req.input, ctx).await;
                &own_facts
            }
        };
        let call = judge::Call {
            name: req.tool_name,
            facts,
            ctx,
            mode,
            hook,
            named: subject::Named::of(&facts.subject, ctx),
            delegated,
        };
        let verdict = call.delegated.granting(self.judge(&call), &call);
        self.conclude(verdict, facts, Some(ctx), hook, req.attendance)
    }
}

#[async_trait]
impl PermissionGrants for Policy {
    async fn grant(&self, grant: Grant) -> Result<(), GrantError> {
        if grant.scope == GrantScope::Turn {
            self.allow(RuleSource::Command, &grant.tool_name, grant.content);
            return Ok(());
        }
        // `Session`, not `Command`: `clear(Turn)` drops every `Command` rule
        // when a skill's turn ends, and a person's answer must outlive that.
        self.allow(
            RuleSource::Session,
            &grant.tool_name,
            grant
                .content
                .as_deref()
                .map(|content| crate::ruleset::literal_rule_content(&grant.tool_name, content)),
        );
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
