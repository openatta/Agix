//! A delegate's permission. `DelegateRelay` hands each of its calls to the
//! parent's policy, `ParentElicitation` puts the questions that leaves open to
//! the parent's host, and `ParentGrants` keeps the person's "always allow"
//! with the parent. The delegate's own runtime asks and waits, as for any
//! session's question.

use super::*;
use base::interface::elicitation::{ElicitKind, ElicitOutcome, ElicitRequest, Elicitation};
use base::interface::permission::{
    Attendance, Delegate, GrantError, GrantScope, PermissionGrants, PermissionOutcome,
    PermissionPolicy, PermissionRequest,
};

/// What a delegate is built with to ask about its calls.
pub(crate) struct Relay {
    pub(crate) permission: PermissionSet,
    pub(crate) elicitation: Arc<dyn Elicitation>,
}

/// A delegate's policy: its calls judged by the parent's policy, against the
/// session they run in, so that spawning a sub-agent gets round none of the
/// parent's rules.
pub(crate) struct DelegateRelay {
    pub(crate) parent: Arc<dyn PermissionPolicy>,
    /// The session `parent` belongs to.
    pub(crate) parent_session: Arc<base::context::SessionState>,
    pub(crate) asks: AskRoute,
    /// What the delegation adds on top of the parent's rules: its deny rules
    /// and the grants it was started with.
    pub(crate) rules: Vec<base::permission::PermissionRule>,
    /// What the delegate granted itself since its turn began.
    pub(crate) turn_grants: Arc<TurnGrants>,
}

const NO_ONE_TO_ASK: &str = "a sub-agent started from a program cannot ask for the user's \
                             approval — leave this step out of the program and run it directly";

#[async_trait]
impl PermissionPolicy for DelegateRelay {
    async fn decide(&self, req: &PermissionRequest<'_>) -> PermissionOutcome {
        let own = req.session.permission_mode();
        let (session, mode, inner) = match req.delegate {
            Some(delegate) => (
                delegate.session,
                own.for_delegate(delegate.mode),
                delegate.rules,
            ),
            None => (req.session, own, &[][..]),
        };
        let rules = [inner, &self.rules, &self.turn_grants.now()].concat();
        let mut relayed = req.for_delegate(Delegate::new(session, mode, &rules));
        relayed.session = &self.parent_session;
        if self.asks == AskRoute::Refuse && relayed.attendance != Attendance::Unattended {
            relayed.attendance = Attendance::NoOneToAsk(NO_ONE_TO_ASK);
        }
        self.parent.decide(&relayed).await
    }
}

/// A delegate's questions, put to the parent's host: registered in the
/// parent's pending table and emitted on the parent's channel, the pair the
/// parent's own questions use, so `session.respondToPrompt` answers them with
/// no plumbing of their own. The parent's input demultiplexer takes the answer
/// ahead of turn dispatch, so it lands even while the parent's turn is blocked
/// in the call that spawned the delegate. How the question ended goes out on
/// the parent's channel too, as the parent's own would.
pub(crate) struct ParentElicitation {
    pub(crate) delegate_id: String,
    pub(crate) events: runtime::agent::EventSender,
    pub(crate) pending: runtime::agent::PendingPermissions,
    /// Shown as waiting while a question of the delegate's is open.
    pub(crate) task: Option<Arc<base::context::RunningTask>>,
}

#[async_trait]
impl Elicitation for ParentElicitation {
    async fn ask(&self, request: ElicitRequest) -> ElicitOutcome {
        self.put(request, None)
            .await
            .unwrap_or_else(|| ElicitOutcome::declined("the question was withdrawn"))
    }

    async fn ask_within(
        &self,
        request: ElicitRequest,
        within: Option<std::time::Duration>,
    ) -> Option<ElicitOutcome> {
        self.put(request, within).await
    }

    fn can_clarify(&self) -> bool {
        false
    }
}

impl ParentElicitation {
    async fn put(
        &self,
        request: ElicitRequest,
        within: Option<std::time::Duration>,
    ) -> Option<ElicitOutcome> {
        let ElicitKind::Authorization {
            tool_name, paths, ..
        } = request.kind
        else {
            return Some(ElicitOutcome::declined(
                "a sub-agent can put only permission questions to the person, through its \
                 parent session",
            ));
        };
        let (tx, rx) = tokio::sync::oneshot::channel();
        let mut open = OpenQuestion::register(self, &request.id, tx);
        let prompt = base::event::AgentEvent::PermissionPrompt {
            prompt_id: request.id.clone(),
            tool_name,
            message: format!("[sub-agent {}] {}", self.delegate_id, request.message),
            paths,
            turn_id: String::new(),
            tool_use_id: None,
            task_id: self.task.as_ref().map(|task| task.task_id.clone()),
        };
        if self.events.send(prompt).is_err() {
            return Some(ElicitOutcome::declined(format!(
                "the parent session closed before it could answer. Original prompt: {}",
                request.message
            )));
        }
        Some(match open.open.wait(rx, within).await? {
            Ok(decision) => ElicitOutcome::answered(&decision),
            Err(_) => ElicitOutcome::declined("permission request closed without an answer"),
        })
    }
}

/// A question registered with the parent and its task shown as waiting, both
/// undone when the ask ends however it ends.
struct OpenQuestion<'a> {
    asker: &'a ParentElicitation,
    prompt_id: &'a str,
    open: runtime::elicitation::OpenPrompt<runtime::agent::PermissionDecision>,
}

impl<'a> OpenQuestion<'a> {
    fn register(
        asker: &'a ParentElicitation,
        prompt_id: &'a str,
        tx: tokio::sync::oneshot::Sender<runtime::agent::PermissionDecision>,
    ) -> Self {
        let open = runtime::elicitation::OpenPrompt::register(
            prompt_id,
            &asker.pending,
            tx,
            asker.events.clone(),
        );
        if let Some(task) = &asker.task {
            task.begin_waiting(prompt_id);
        }
        Self {
            asker,
            prompt_id,
            open,
        }
    }
}

impl Drop for OpenQuestion<'_> {
    fn drop(&mut self) {
        if let Some(task) = &self.asker.task {
            task.end_waiting(self.prompt_id);
        }
    }
}

/// A delegate's grants. An answer the person gave is kept by the parent,
/// where it lasts as long as it would have here. A turn's grant — a skill's
/// `allowed-tools` — stays with the delegate, for its own calls until its turn
/// ends. A delegate clears nothing of its parent's.
pub(crate) struct ParentGrants {
    pub(crate) parent: Arc<dyn PermissionGrants>,
    pub(crate) turn: Arc<TurnGrants>,
}

#[async_trait]
impl PermissionGrants for ParentGrants {
    async fn grant(&self, grant: base::interface::permission::Grant) -> Result<(), GrantError> {
        match grant.scope {
            GrantScope::Turn => {
                self.turn.add(grant_rule(grant));
                Ok(())
            }
            _ => self.parent.grant(grant).await,
        }
    }
    fn clear(&self, scope: GrantScope) {
        if scope == GrantScope::Turn {
            self.turn.clear();
        }
    }
}

#[derive(Default)]
pub(crate) struct TurnGrants(std::sync::RwLock<Vec<base::permission::PermissionRule>>);

impl TurnGrants {
    fn now(&self) -> Vec<base::permission::PermissionRule> {
        self.0.read().unwrap_or_else(|e| e.into_inner()).clone()
    }
    fn add(&self, rule: base::permission::PermissionRule) {
        self.0.write().unwrap_or_else(|e| e.into_inner()).push(rule);
    }
    fn clear(&self) {
        self.0.write().unwrap_or_else(|e| e.into_inner()).clear();
    }
}

/// A grant as the allow rule it is, matched as a `settings.json` rule's
/// content is.
pub(crate) fn grant_rule(
    grant: base::interface::permission::Grant,
) -> base::permission::PermissionRule {
    base::permission::PermissionRule {
        source: base::permission::RuleSource::Command,
        behavior: base::permission::RuleBehavior::Allow,
        tool_name: grant.tool_name,
        rule_content: grant.content,
    }
}
