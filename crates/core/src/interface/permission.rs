//! Tool authorization, in two parts: [`PermissionPolicy`] decides whether a
//! call runs, [`PermissionGrants`] keeps the allows a person or a skill hands
//! out along the way. A session holds the two together as a [`PermissionSet`].
//! What a policy weighs about a call is its [`ToolFacts`].

use async_trait::async_trait;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;

use crate::context::SessionState;
use crate::interface::tool::{CallTraits, Tool, ToolContext, ToolOrigin};
use crate::permission::{PermissionMode, PermissionRule};

mod profile;
pub use profile::{Approval, ExecProfile, Obligations};

/// How the permission modes treat a tool's calls: `ToolSpec::stance`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum ModeStance {
    #[default]
    Normal,
    /// A coordination primitive that touches no file, shell or network: a
    /// self-contained call of it is spared `Plan` and `DontAsk`.
    Coordination,
    /// The call that takes the session out of plan mode, which plan mode
    /// would otherwise refuse as a change of state.
    LeavesPlan,
}

/// What a call names, as permission rules are matched against it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Subject {
    #[default]
    None,
    /// Compared to rule patterns verbatim.
    Opaque(String),
    /// A path read; `Read(...)` rules apply to it as well as the tool's own.
    ReadPath(String),
    /// A path written; `Edit(...)` rules apply to it as well as the tool's own.
    WritePath(String),
    /// A command line, matched one simple command at a time.
    Shell(String),
    Url(String),
}

impl Subject {
    pub fn content(&self) -> Option<&str> {
        match self {
            Self::None => None,
            Self::Opaque(s)
            | Self::ReadPath(s)
            | Self::WritePath(s)
            | Self::Shell(s)
            | Self::Url(s) => Some(s),
        }
    }
}

/// How far a call reaches beyond the session itself.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Reach {
    /// Only the session's own state: todos, tasks, the mailbox, the plan.
    SessionOnly,
    /// Starts other calls, each judged on its own.
    Delegates,
    /// Only endpoints the operator configured.
    Operator,
    /// Within the roots the session works in without asking: the readable
    /// ones for a read, the writable ones for a write.
    Inside,
    /// A path outside those roots.
    Outside,
    /// A host the model chose; `listed` when `sandbox.allowed_domains` names it.
    Host {
        listed: bool,
    },
    /// An MCP server's tool, whose word about itself does not count.
    Server,
    /// Leaves something behind that acts later on its own.
    Persists(&'static str),
    /// The tool knows this input must not run, and says why.
    Refused(String),
    Unknown,
}

/// The call runs a command inside the session's sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Runs {
    /// It keeps running after the call returns.
    pub background: bool,
}

impl Runs {
    pub fn foreground() -> Self {
        Self { background: false }
    }
    pub fn background() -> Self {
        Self { background: true }
    }
}

/// What a tool says about one call for the permission policy to weigh:
/// [`Tool::permission_facts`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PermissionFacts {
    pub subject: Subject,
    pub reach: Reach,
    pub runs: Option<Runs>,
    /// What to put to a person when the call is asked about.
    pub question: Option<String>,
}

impl PermissionFacts {
    /// Claims nothing, so the call is asked about.
    pub fn unknown() -> Self {
        Self::new(Subject::None, Reach::Unknown)
    }

    pub fn new(subject: Subject, reach: Reach) -> Self {
        Self {
            subject,
            reach,
            runs: None,
            question: None,
        }
    }

    pub fn with_runs(mut self, runs: Runs) -> Self {
        self.runs = Some(runs);
        self
    }

    pub fn with_question(mut self, question: impl Into<String>) -> Self {
        self.question = Some(question.into());
        self
    }
}

/// Everything the permission policy reads about one call: the tool's spec,
/// its [`CallTraits`] for the input and its [`PermissionFacts`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ToolFacts {
    pub subject: Subject,
    pub traits: CallTraits,
    pub stance: ModeStance,
    pub origin: ToolOrigin,
    pub reach: Reach,
    pub runs: Option<Runs>,
    pub question: Option<String>,
}

impl ToolFacts {
    /// Gathered once per call, by the session that runs it and with the
    /// context it runs with.
    pub async fn gather(tool: &dyn Tool, input: &Value, ctx: &ToolContext) -> Self {
        let spec = tool.spec();
        let facts = tool.permission_facts(input, ctx).await;
        Self {
            subject: facts.subject,
            traits: tool.call_traits(input),
            stance: spec.stance,
            origin: spec.origin,
            reach: facts.reach,
            runs: facts.runs,
            question: facts.question,
        }
    }

    /// For a name no tool answers to: claims nothing, not even a read.
    pub fn unknown_tool() -> Self {
        Self {
            subject: Subject::None,
            traits: CallTraits::default(),
            stance: ModeStance::Normal,
            origin: ToolOrigin::default(),
            reach: Reach::Unknown,
            runs: None,
            question: None,
        }
    }
}

/// What a `PreToolUse` hook answered about the call.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum HookAnswer {
    #[default]
    None,
    Approve {
        hook: String,
    },
    Block {
        hook: String,
        reason: String,
    },
}

/// Whether someone can be asked about the call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Attendance {
    Attended,
    Unattended,
    /// Nothing could reach a person even if one were there, and why.
    NoOneToAsk(&'static str),
}

/// What a policy concluded about one call.
#[derive(Debug, Clone)]
pub enum PermissionOutcome {
    Permit {
        basis: Basis,
        profile: ExecProfile,
        obligations: Obligations,
    },
    Deny {
        reason: Reason,
    },
    /// Put to whoever can answer: the engine emits
    /// `AgentEvent::PermissionPrompt` and waits for the answer.
    Prompt {
        prompt_id: String,
        message: String,
        paths: Vec<PathBuf>,
        reason: Reason,
        answerable: Answerable,
    },
}

impl PermissionOutcome {
    pub fn deny(stage: Stage, message: impl Into<String>) -> Self {
        Self::Deny {
            reason: Reason::new(stage, message),
        }
    }

    /// A question anyone who can answer may answer, under a fresh id.
    pub fn prompt(stage: Stage, message: impl Into<String>) -> Self {
        let message = message.into();
        Self::Prompt {
            prompt_id: uuid::Uuid::new_v4().to_string(),
            reason: Reason::new(stage, message.clone()),
            message,
            paths: Vec::new(),
            answerable: Answerable::HookOrPerson,
        }
    }

    pub fn is_permit(&self) -> bool {
        matches!(self, Self::Permit { .. })
    }
}

/// On whose word a call runs.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Basis {
    Rule(PermissionRule),
    /// It stays where the session works without asking.
    Contained,
    AcceptEdits,
    Bypass,
    /// A `PreToolUse` hook approved it, by name.
    HookApproved(String),
    /// A command nobody can be asked about, which the session's sandbox holds.
    UnattendedConfined,
    Person,
}

impl Basis {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Rule(_) => "rule",
            Self::Contained => "contained",
            Self::AcceptEdits => "accept_edits",
            Self::Bypass => "bypass",
            Self::HookApproved(_) => "hook_approved",
            Self::UnattendedConfined => "unattended_confined",
            Self::Person => "person",
        }
    }
}

/// Who may answer a question a policy asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Answerable {
    HookOrPerson,
    /// The deployment's managed layer asks it: a hook's approve does not
    /// answer it.
    PersonOnly,
}

/// Where in the policy's order a call was refused or asked about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Stage {
    ManagedUnreadable,
    UnknownTool,
    Credentials,
    Hook,
    ToolRefusal,
    /// The session's sandbox cannot hold the command as fully as
    /// `sandbox.require_enforcement` demands.
    Confinement,
    Rule,
    ManagedRule,
    /// A rule the delegate runs under: its type's deny rules.
    DelegateRule,
    BypassImmune,
    PlanMode,
    DontAsk,
    /// A write to the agent's or the repository's configuration, which the
    /// tool's own word does not let through.
    Configuration,
    ModeDefault,
    Attendance,
}

impl Stage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ManagedUnreadable => "managed_unreadable",
            Self::UnknownTool => "unknown_tool",
            Self::Credentials => "credentials",
            Self::Hook => "hook",
            Self::ToolRefusal => "tool_refusal",
            Self::Confinement => "confinement",
            Self::Rule => "rule",
            Self::ManagedRule => "managed_rule",
            Self::DelegateRule => "delegate_rule",
            Self::BypassImmune => "bypass_immune",
            Self::PlanMode => "plan_mode",
            Self::DontAsk => "dont_ask",
            Self::Configuration => "configuration",
            Self::ModeDefault => "mode_default",
            Self::Attendance => "attendance",
        }
    }
}

/// Why a call was refused or asked about.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Reason {
    pub stage: Stage,
    pub rule: Option<PermissionRule>,
    pub message: String,
}

impl Reason {
    pub fn new(stage: Stage, message: impl Into<String>) -> Self {
        Self {
            stage,
            rule: None,
            message: message.into(),
        }
    }

    pub fn with_rule(mut self, rule: PermissionRule) -> Self {
        self.rule = Some(rule);
        self
    }
}

impl std::fmt::Display for Reason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// One tool call put to a [`PermissionPolicy`].
#[derive(Clone, Copy)]
#[non_exhaustive]
pub struct PermissionRequest<'a> {
    pub tool_name: &'a str,
    /// The tool the call dispatches to, looked up in the registry dispatch
    /// uses. `None` is a name nothing answers to.
    pub tool: Option<&'a Arc<dyn Tool>>,
    pub input: &'a serde_json::Value,
    /// What the tool says about the call, gathered with `ctx`.
    pub facts: &'a ToolFacts,
    /// The context the call runs with: the judged session's.
    pub ctx: &'a ToolContext,
    /// The live state of the session the policy belongs to: its permission
    /// mode as it is now, what it has read.
    pub session: &'a Arc<SessionState>,
    /// Set when the call is a delegate's — a sub-agent's, a team member's —
    /// relayed to this session's policy.
    pub delegate: Option<Delegate<'a>>,
    pub hook: &'a HookAnswer,
    pub attendance: Attendance,
}

const NO_HOOK: &HookAnswer = &HookAnswer::None;

impl<'a> PermissionRequest<'a> {
    /// A call no hook answered, as attended as `ctx` says.
    pub fn new(
        tool_name: &'a str,
        tool: Option<&'a Arc<dyn Tool>>,
        input: &'a serde_json::Value,
        facts: &'a ToolFacts,
        ctx: &'a ToolContext,
        session: &'a Arc<SessionState>,
    ) -> Self {
        Self {
            tool_name,
            tool,
            input,
            facts,
            ctx,
            session,
            delegate: None,
            hook: NO_HOOK,
            attendance: ctx.attendance,
        }
    }

    pub fn for_delegate(mut self, delegate: Delegate<'a>) -> Self {
        self.delegate = Some(delegate);
        self
    }

    pub fn with_hook(mut self, hook: &'a HookAnswer) -> Self {
        self.hook = hook;
        self
    }

    pub fn with_attendance(mut self, attendance: Attendance) -> Self {
        self.attendance = attendance;
        self
    }

    /// The state the call is judged against: the delegate's when there is
    /// one, the session's otherwise.
    pub fn judged(&self) -> &'a Arc<SessionState> {
        self.delegate.map_or(self.session, |d| d.session)
    }
}

/// A delegate's call as the session that relays it hands it on: judged
/// against the delegate's state, in `mode`, and held to `rules` on top of the
/// policy's own.
#[derive(Clone, Copy)]
#[non_exhaustive]
pub struct Delegate<'a> {
    /// The session the call runs in.
    pub session: &'a Arc<SessionState>,
    /// The delegate's mode as every session between it and the policy holds
    /// it (`PermissionMode::for_delegate`, once per relaying session). The
    /// policy holds it to its own session's mode in turn.
    pub mode: PermissionMode,
    /// What the delegation itself adds: its `deny` rules refuse the call in
    /// any mode; its `allow` rules are the delegate's own grants, which answer
    /// only the question the mode would put — never a deny, an ask rule, or
    /// a question only a person may answer.
    pub rules: &'a [PermissionRule],
}

impl<'a> Delegate<'a> {
    /// `session`'s call in its own live mode, with nothing added.
    pub fn of(session: &'a Arc<SessionState>) -> Self {
        Self::new(session, session.permission_mode(), &[])
    }

    pub fn new(
        session: &'a Arc<SessionState>,
        mode: PermissionMode,
        rules: &'a [PermissionRule],
    ) -> Self {
        Self {
            session,
            mode,
            rules,
        }
    }
}

/// Whether a tool call runs. The engine's own is
/// `permissions::policy::Policy` (rules, modes, the
/// tools' own verdicts).
#[async_trait]
pub trait PermissionPolicy: Send + Sync {
    async fn decide(&self, req: &PermissionRequest<'_>) -> PermissionOutcome;
}

/// How long a [`Grant`] lasts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum GrantScope {
    /// Until the next user turn starts: a skill's `allowed-tools`.
    Turn,
    /// For the rest of the session: "always allow" answered for this session.
    Session,
    /// For this project, kept in its `settings.local.json`; a session with no
    /// project keeps it for the session.
    Project,
}

/// How an allow's content meets a call's.
#[derive(
    Clone,
    Copy,
    PartialEq,
    Eq,
    Debug,
    Default,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum GrantMatch {
    /// As a `settings.json` rule's content is: `*`, `prefix:*`, path and URL
    /// patterns.
    #[default]
    Pattern,
    /// Only a call whose content is the same text.
    Exact,
}

impl GrantMatch {
    pub fn is_pattern(&self) -> bool {
        *self == Self::Pattern
    }
}

/// An allow for calls to one tool.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Grant {
    pub tool_name: String,
    /// `None` allows every call to the tool. Otherwise matched as `matching`
    /// says.
    ///
    /// A grant relayed from project-controlled data — a skill's frontmatter —
    /// must not be `None`: anyone who can write a file in the repository could
    /// then switch off confirmation for `Bash` wholesale.
    pub content: Option<String>,
    pub scope: GrantScope,
    /// A [`GrantScope::Turn`] grant is a skill's rule pattern; a
    /// [`GrantScope::Session`] or [`GrantScope::Project`] one is a person's
    /// answer about one call, so it covers that call's content exactly and
    /// not whatever the text happens to match as a pattern.
    pub matching: GrantMatch,
}

impl Grant {
    pub fn new(tool_name: impl Into<String>, content: Option<String>, scope: GrantScope) -> Self {
        Self {
            tool_name: tool_name.into(),
            content,
            matching: match scope {
                GrantScope::Turn => GrantMatch::Pattern,
                GrantScope::Session | GrantScope::Project => GrantMatch::Exact,
            },
            scope,
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum GrantError {
    /// Kept for the session, but not written where it would outlast it.
    #[error("kept for this session only, not written to {}: {reason}", .path.display())]
    NotPersisted { path: PathBuf, reason: String },
}

/// Where the allows handed out during a session are kept.
#[async_trait]
pub trait PermissionGrants: Send + Sync {
    async fn grant(&self, grant: Grant) -> Result<(), GrantError>;
    /// Drop every grant of `scope`.
    fn clear(&self, scope: GrantScope);
}

/// A session's permission: the policy its calls are judged by and where the
/// allows it is handed are kept. With no `grants` nothing is kept, so nobody
/// is offered an "allow every time".
#[derive(Clone)]
pub struct PermissionSet {
    pub policy: Arc<dyn PermissionPolicy>,
    pub grants: Option<Arc<dyn PermissionGrants>>,
}

impl PermissionSet {
    /// One value that both decides and keeps grants, so what it is granted
    /// changes what it decides.
    pub fn of<P: PermissionPolicy + PermissionGrants + 'static>(p: Arc<P>) -> Self {
        Self {
            policy: p.clone(),
            grants: Some(p),
        }
    }

    pub fn without_grants(policy: Arc<dyn PermissionPolicy>) -> Self {
        Self {
            policy,
            grants: None,
        }
    }
}

#[cfg(test)]
mod facts_tests {
    use super::*;
    use crate::error::ToolError;
    use crate::interface::tool::{PathStyle, ProgressSender, ToolResult, ToolSpec};
    use serde_json::json;

    struct Silent;

    #[async_trait]
    impl Tool for Silent {
        fn name(&self) -> &str {
            "Silent"
        }
        fn description(&self) -> &str {
            "states nothing about its calls"
        }
        fn spec(&self) -> ToolSpec {
            ToolSpec::new().read_only(true)
        }
        fn input_schema(&self, _: PathStyle) -> Value {
            json!({"type": "object"})
        }
        async fn call(
            &self,
            _: Value,
            _: ToolContext,
            _: ProgressSender,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::text("ok"))
        }
    }

    struct Declares {
        spec: ToolSpec,
        facts: PermissionFacts,
    }

    impl Declares {
        fn stating(facts: PermissionFacts) -> Self {
            Self {
                spec: ToolSpec::new().origin(ToolOrigin::plugin("p")),
                facts,
            }
        }
    }

    #[async_trait]
    impl Tool for Declares {
        fn name(&self) -> &str {
            "Declares"
        }
        fn description(&self) -> &str {
            "states its own facts"
        }
        fn spec(&self) -> ToolSpec {
            self.spec.clone()
        }
        fn input_schema(&self, _: PathStyle) -> Value {
            json!({"type": "object"})
        }
        fn call_traits(&self, input: &Value) -> CallTraits {
            self.spec.traits.read_only(input.get("peek").is_some())
        }
        async fn permission_facts(&self, _: &Value, _: &ToolContext) -> PermissionFacts {
            self.facts.clone()
        }
        async fn call(
            &self,
            _: Value,
            _: ToolContext,
            _: ProgressSender,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::text("ok"))
        }
    }

    fn ctx() -> ToolContext {
        ToolContext::for_test_with(
            PathBuf::from("/tmp"),
            crate::interface::exec::ExecProviders::in_process(),
        )
    }

    /// SEC F-6: a tool that says nothing about a call is asked about, even
    /// one that only reads.
    #[tokio::test]
    async fn a_tool_that_states_nothing_claims_nothing() {
        assert_eq!(
            Silent.permission_facts(&json!({}), &ctx()).await,
            PermissionFacts::unknown()
        );
        let facts = ToolFacts::gather(&Silent, &json!({}), &ctx()).await;
        assert_eq!(facts.reach, Reach::Unknown);
        assert_eq!(facts.subject, Subject::None);
        assert!(facts.traits.read_only);
    }

    #[tokio::test]
    async fn gathered_facts_carry_the_spec_and_the_traits_of_this_input() {
        let mut tool = Declares::stating(PermissionFacts::new(
            Subject::ReadPath("src/lib.rs".into()),
            Reach::Inside,
        ));
        tool.spec = ToolSpec::new()
            .origin(ToolOrigin::mcp("srv"))
            .stance(ModeStance::Coordination)
            .concurrency_safe(true);
        let ctx = ctx();

        let peek = ToolFacts::gather(&tool, &json!({"peek": 1}), &ctx).await;
        assert_eq!(peek.subject, Subject::ReadPath("src/lib.rs".into()));
        assert_eq!(peek.reach, Reach::Inside);
        assert_eq!(peek.stance, ModeStance::Coordination);
        assert_eq!(peek.origin, ToolOrigin::mcp("srv"));
        assert!(peek.traits.read_only && peek.traits.concurrency_safe);

        let write = ToolFacts::gather(&tool, &json!({}), &ctx).await;
        assert!(!write.traits.read_only);
    }

    #[tokio::test]
    async fn a_tool_that_states_its_facts_is_taken_at_its_word() {
        let stated = PermissionFacts::new(Subject::Shell("tail -f log".into()), Reach::Unknown)
            .with_runs(Runs::background())
            .with_question("watch it?");
        let facts = ToolFacts::gather(&Declares::stating(stated.clone()), &json!({}), &ctx()).await;
        assert_eq!(facts.subject, stated.subject);
        assert_eq!(facts.reach, stated.reach);
        assert_eq!(facts.runs, Some(Runs { background: true }));
        assert_eq!(facts.question.as_deref(), Some("watch it?"));
        assert_eq!(facts.origin, ToolOrigin::plugin("p"));
        assert_eq!(facts.stance, ModeStance::Normal);
    }

    #[test]
    fn an_unknown_tool_claims_nothing() {
        let facts = ToolFacts::unknown_tool();
        assert_eq!(facts.reach, Reach::Unknown);
        assert_eq!(facts.subject, Subject::None);
        assert!(!facts.traits.read_only);
        assert_eq!(facts.runs, None);
    }

    #[test]
    fn a_subject_keeps_its_content_whatever_its_kind() {
        for subject in [
            Subject::Opaque("x".into()),
            Subject::ReadPath("x".into()),
            Subject::WritePath("x".into()),
            Subject::Shell("x".into()),
            Subject::Url("x".into()),
        ] {
            assert_eq!(subject.content(), Some("x"));
        }
        assert_eq!(Subject::None.content(), None);
    }

    #[test]
    fn a_request_is_attended_as_its_context_and_unanswered_by_a_hook() {
        let session = Arc::new(SessionState::for_tests(PathBuf::from("/tmp")));
        let input = json!({});
        let facts = ToolFacts::unknown_tool();
        let mut ctx = ctx();
        ctx.attendance = Attendance::Unattended;
        let bare = PermissionRequest::new("Silent", None, &input, &facts, &ctx, &session);
        assert_eq!(bare.hook, &HookAnswer::None);
        assert_eq!(bare.attendance, Attendance::Unattended);
        assert!(bare.delegate.is_none());

        let hook = HookAnswer::Approve {
            hook: "audit".into(),
        };
        let full = bare
            .with_hook(&hook)
            .with_attendance(Attendance::NoOneToAsk("a program's call"));
        let copy = full;
        assert_eq!(copy.facts, &facts);
        assert_eq!(copy.ctx.session_id, "test");
        assert_eq!(copy.hook, &hook);
        assert_eq!(copy.attendance, Attendance::NoOneToAsk("a program's call"));
    }
}
