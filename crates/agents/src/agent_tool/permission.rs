//! A delegate's permission. `DelegateRelay` hands each of its calls to the
//! parent's policy, `ParentElicitation` puts the questions that leaves open to
//! the parent's host, and `ParentGrants` keeps the person's "always allow"
//! with the parent. The delegate's own runtime asks and waits, as for any
//! session's question.

use super::*;
use base::interface::elicitation::{ElicitKind, ElicitOutcome, ElicitRequest, Elicitation};
use base::interface::permission::{
    Attendance, Delegate, GrantError, GrantScope, PermissionGrants, PermissionOutcome,
    PermissionPolicy, PermissionRequest, Stage,
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
    /// What the delegation adds on top of the parent's rules.
    pub(crate) rules: Vec<base::permission::PermissionRule>,
}

const NO_ONE_TO_ASK: &str = "a sub-agent started from a program cannot ask for the user's \
                             approval — leave this step out of the program and run it directly";

#[async_trait]
impl PermissionPolicy for DelegateRelay {
    async fn decide(&self, req: &PermissionRequest<'_>) -> PermissionOutcome {
        let own = req.session.permission_mode();
        let (session, mode, rules) = match req.delegate {
            Some(delegate) => (
                delegate.session,
                own.for_delegate(delegate.mode),
                [delegate.rules, &self.rules].concat(),
            ),
            None => (req.session, own, self.rules.clone()),
        };
        let mut relayed = req.for_delegate(Delegate::new(session, mode, &rules));
        relayed.session = &self.parent_session;
        if self.asks == AskRoute::Refuse && relayed.attendance != Some(Attendance::Unattended) {
            relayed.attendance = Some(Attendance::NoOneToAsk(NO_ONE_TO_ASK));
        }
        match (self.parent.decide(&relayed).await, relayed.attendance) {
            // A host's own policy may not weigh attendance; the engine's has
            // refused this already.
            (PermissionOutcome::Prompt { message, .. }, Some(Attendance::NoOneToAsk(why))) => {
                PermissionOutcome::deny(Stage::Attendance, format!("{message} — {why}"))
            }
            (outcome, _) => outcome,
        }
    }
}

/// A delegate's questions, put to the parent's host: registered in the
/// parent's pending table and emitted on the parent's channel, the pair the
/// parent's own questions use, so `session.respondToPrompt` answers them with
/// no plumbing of their own. The parent's input demultiplexer takes the answer
/// ahead of turn dispatch, so it lands even while the parent's turn is blocked
/// in the call that spawned the delegate.
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
        let ElicitKind::Authorization {
            tool_name, paths, ..
        } = request.kind
        else {
            return ElicitOutcome::declined(
                "a sub-agent can put only permission questions to the person, through its \
                 parent session",
            );
        };
        let (tx, rx) = tokio::sync::oneshot::channel();
        let _open = OpenQuestion::register(self, &request.id, tx);
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
            return ElicitOutcome::declined(format!(
                "the parent session closed before it could answer. Original prompt: {}",
                request.message
            ));
        }
        match rx.await {
            Ok(decision) => ElicitOutcome::answered(&decision),
            Err(_) => ElicitOutcome::declined("permission request closed without an answer"),
        }
    }

    fn can_clarify(&self) -> bool {
        false
    }
}

/// A question registered with the parent and its task shown as waiting, both
/// undone when the ask ends however it ends.
struct OpenQuestion<'a> {
    asker: &'a ParentElicitation,
    prompt_id: &'a str,
}

impl<'a> OpenQuestion<'a> {
    fn register(
        asker: &'a ParentElicitation,
        prompt_id: &'a str,
        tx: tokio::sync::oneshot::Sender<runtime::agent::PermissionDecision>,
    ) -> Self {
        asker
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(prompt_id.to_string(), tx);
        if let Some(task) = &asker.task {
            task.begin_waiting(prompt_id);
        }
        Self { asker, prompt_id }
    }
}

impl Drop for OpenQuestion<'_> {
    fn drop(&mut self) {
        self.asker
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(self.prompt_id);
        if let Some(task) = &self.asker.task {
            task.end_waiting(self.prompt_id);
        }
    }
}

/// A delegate's grants, kept by its parent: an answer the person gave lasts
/// as long there as it would have here. A turn's grant — a skill's
/// `allowed-tools` — is not passed up, and a delegate clears nothing of its
/// parent's.
pub(crate) struct ParentGrants(pub(crate) Arc<dyn PermissionGrants>);

#[async_trait]
impl PermissionGrants for ParentGrants {
    async fn grant(&self, grant: base::interface::permission::Grant) -> Result<(), GrantError> {
        match grant.scope {
            GrantScope::Turn => Err(GrantError::NotKept),
            _ => self.0.grant(grant).await,
        }
    }
    fn clear(&self, _: GrantScope) {}
}
