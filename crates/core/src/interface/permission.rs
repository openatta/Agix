//! Tool authorization, in two parts: [`PermissionPolicy`] decides whether a
//! call runs, [`PermissionGrants`] keeps the allows a person or a skill hands
//! out along the way. A session holds the two together as a [`PermissionSet`].
//! What a policy weighs about a call is its [`ToolFacts`].

use async_trait::async_trait;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::context::SessionState;
use crate::interface::tool::{CallTraits, PermissionDecision, Tool, ToolContext, ToolOrigin};
use crate::permission::{PermissionMode, PermissionRule};

/// What a tool's permission match content is, which decides how rules are
/// matched against it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionContentKind {
    /// Compared to rule patterns verbatim.
    Opaque,
    /// A path the tool reads. Resolved against the session's cwd before
    /// matching, and `Read(...)` rules apply to it as well as the tool's own.
    ReadPath,
    /// A path the tool writes. Resolved like `ReadPath`; `Edit(...)` rules
    /// apply to it as well as the tool's own.
    WritePath,
    /// A shell command line, matched one simple command at a time.
    ShellCommand,
}

/// What the permission engine reads off a tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ToolPermissionSpec {
    /// How the permission engine reads [`Tool::permission_match_content`].
    pub content_kind: PermissionContentKind,
    /// Does this tool's own `Allow` from [`Tool::check_permissions`] survive a
    /// *mode-level* denial (`Plan`, `DontAsk`)?
    ///
    /// `false` by default, which is what makes plan mode mean anything:
    /// `Write` and `Edit` allow themselves for any in-project path, and that
    /// verdict is a statement about *path safety*, not a claim that writing
    /// files is fine while the user asked for a plan.
    ///
    /// `true` only when the self-`Allow` is a statement about the tool's own
    /// nature rather than about its arguments — a coordination primitive that
    /// touches no file, no shell and no network. The team mailbox's
    /// `SendMessage` is the case in point: team members run under `Plan` by
    /// default, and a plan-mode member that cannot talk to its coordinator is
    /// a broken one.
    ///
    /// Rule-based denials and the bypass-immune path list are **not**
    /// affected: an explicit `deny` always wins.
    pub self_allow_overrides_mode: bool,
    /// Is this the call that takes the session out of plan mode?
    ///
    /// Plan mode refuses every tool that is not read-only, and leaving plan
    /// mode changes the session's state, so the way out would be refused too.
    /// A tool that says yes is spared that one refusal and is decided as in
    /// any mode that asks: a matching rule, else the host's answer.
    pub leaves_plan_mode: bool,
}

impl Default for ToolPermissionSpec {
    fn default() -> Self {
        Self {
            content_kind: PermissionContentKind::Opaque,
            self_allow_overrides_mode: false,
            leaves_plan_mode: false,
        }
    }
}

impl ToolPermissionSpec {
    /// The [`ModeStance`] these two flags amount to.
    pub fn stance(&self) -> ModeStance {
        if self.leaves_plan_mode {
            ModeStance::LeavesPlan
        } else if self.self_allow_overrides_mode {
            ModeStance::Coordination
        } else {
            ModeStance::Normal
        }
    }
}

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
    pub fn of(kind: PermissionContentKind, content: Option<String>) -> Self {
        let Some(content) = content else {
            return Self::None;
        };
        match kind {
            PermissionContentKind::Opaque => Self::Opaque(content),
            PermissionContentKind::ReadPath => Self::ReadPath(content),
            PermissionContentKind::WritePath => Self::WritePath(content),
            PermissionContentKind::ShellCommand => Self::Shell(content),
        }
    }

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

    /// The facts a tool's `check_permissions`, `permission_match_content`,
    /// `accept_edits_allows` and `spec().permission` amount to, so that a tool
    /// written against those is judged as before it stated facts of its own.
    ///
    /// - `Deny` is [`Reach::Refused`] with its reason.
    /// - `Allow` is contained: [`Reach::Inside`] when the subject is a path or
    ///   a command line, [`Reach::SessionOnly`] otherwise.
    /// - `Ask` is [`Reach::Unknown`], with its message as the question —
    ///   unless `accept_edits_allows` says yes, which makes it
    ///   `Host { listed: true }`: the one reach `acceptEdits` lets through
    ///   that is not contained.
    ///
    /// `runs` stays `None`: whatever a tool's own allow made of the sandbox is
    /// already in its answer.
    pub async fn bridged<T: Tool + ?Sized>(tool: &T, input: &Value, ctx: &ToolContext) -> Self {
        let subject = Subject::of(
            tool.spec().permission.content_kind,
            tool.permission_match_content(input),
        );
        match tool.check_permissions(input, ctx).await {
            PermissionDecision::Deny { reason, .. } => {
                Self::new(subject, Reach::Refused(reason.unwrap_or_default()))
            }
            PermissionDecision::Allow { .. } => {
                let reach = match subject {
                    Subject::None | Subject::Opaque(_) => Reach::SessionOnly,
                    _ => Reach::Inside,
                };
                Self::new(subject, reach)
            }
            PermissionDecision::Ask { message, .. } => {
                let reach = if tool.accept_edits_allows(input, ctx) {
                    Reach::Host { listed: true }
                } else {
                    Reach::Unknown
                };
                Self::new(subject, reach).with_question(message)
            }
        }
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

    /// [`Self::gather`] with what the tool's old answers amount to
    /// ([`PermissionFacts::bridged`]) in place of what it states.
    pub async fn bridged(tool: &dyn Tool, input: &Value, ctx: &ToolContext) -> Self {
        let spec = tool.spec();
        let facts = PermissionFacts::bridged(tool, input, ctx).await;
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

/// How a permitted command is confined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum ExecProfile {
    #[default]
    Session,
    /// Let through on its own word: held tighter than one a person approved.
    Strict,
    /// Nobody attends the session.
    Unattended,
}

/// What the caller owes once a permitted call has run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct Obligations {
    pub scan_nested_repos: bool,
}

impl Obligations {
    pub fn scanning_nested_repos(yes: bool) -> Self {
        Self {
            scan_nested_repos: yes,
        }
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
    pub cwd: &'a Path,
    pub session_id: &'a str,
    /// The live state of the session the policy belongs to: its permission
    /// mode as it is now, what it has read.
    pub session: &'a Arc<SessionState>,
    /// Set when the call is a delegate's — a sub-agent's, a team member's —
    /// relayed to this session's policy.
    pub delegate: Option<Delegate<'a>>,
    /// What the tool says about the call, gathered with `ctx`.
    pub facts: Option<&'a ToolFacts>,
    /// The context the call runs with: the judged session's.
    pub ctx: Option<&'a ToolContext>,
    pub hook: Option<&'a HookAnswer>,
    pub attendance: Option<Attendance>,
}

impl<'a> PermissionRequest<'a> {
    pub fn new(
        tool_name: &'a str,
        tool: Option<&'a Arc<dyn Tool>>,
        input: &'a serde_json::Value,
        cwd: &'a Path,
        session_id: &'a str,
        session: &'a Arc<SessionState>,
    ) -> Self {
        Self {
            tool_name,
            tool,
            input,
            cwd,
            session_id,
            session,
            delegate: None,
            facts: None,
            ctx: None,
            hook: None,
            attendance: None,
        }
    }

    pub fn for_delegate(mut self, delegate: Delegate<'a>) -> Self {
        self.delegate = Some(delegate);
        self
    }

    pub fn with_facts(mut self, facts: &'a ToolFacts) -> Self {
        self.facts = Some(facts);
        self
    }

    pub fn with_ctx(mut self, ctx: &'a ToolContext) -> Self {
        self.ctx = Some(ctx);
        self
    }

    pub fn with_hook(mut self, hook: &'a HookAnswer) -> Self {
        self.hook = Some(hook);
        self
    }

    pub fn with_attendance(mut self, attendance: Attendance) -> Self {
        self.attendance = Some(attendance);
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
    /// any mode.
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

/// An allow for calls to one tool.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Grant {
    pub tool_name: String,
    /// `None` allows every call to the tool. For [`GrantScope::Turn`] this is
    /// a rule pattern, matched as a `settings.json` rule's content is; for
    /// [`GrantScope::Session`] and [`GrantScope::Project`] it is the
    /// approved call's own match content, kept literally so the allow covers
    /// that call and not whatever its text happens to match as a pattern.
    ///
    /// A grant relayed from project-controlled data — a skill's frontmatter —
    /// must not be `None`: anyone who can write a file in the repository could
    /// then switch off confirmation for `Bash` wholesale.
    pub content: Option<String>,
    pub scope: GrantScope,
}

impl Grant {
    pub fn new(tool_name: impl Into<String>, content: Option<String>, scope: GrantScope) -> Self {
        Self {
            tool_name: tool_name.into(),
            content,
            scope,
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum GrantError {
    /// This permission keeps no allows of its own.
    #[error("this permission keeps no grants")]
    NotKept,
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

/// Keeps nothing: [`PermissionGrants::grant`] answers
/// [`GrantError::NotKept`], so a caller can tell an allow was not kept.
pub struct NoGrants;

#[async_trait]
impl PermissionGrants for NoGrants {
    async fn grant(&self, _: Grant) -> Result<(), GrantError> {
        Err(GrantError::NotKept)
    }
    fn clear(&self, _: GrantScope) {}
}

/// A session's permission: the policy its calls are judged by and where the
/// allows it is handed are kept.
#[derive(Clone)]
pub struct PermissionSet {
    pub policy: Arc<dyn PermissionPolicy>,
    pub grants: Arc<dyn PermissionGrants>,
}

impl PermissionSet {
    /// One value that both decides and keeps grants, so what it is granted
    /// changes what it decides.
    pub fn of<P: PermissionPolicy + PermissionGrants + 'static>(p: Arc<P>) -> Self {
        Self {
            policy: p.clone(),
            grants: p,
        }
    }

    pub fn without_grants(policy: Arc<dyn PermissionPolicy>) -> Self {
        Self {
            policy,
            grants: Arc::new(NoGrants),
        }
    }
}

#[cfg(test)]
mod facts_tests {
    use super::*;
    use crate::error::ToolError;
    use crate::interface::tool::{PathStyle, ProgressSender, ToolResult, ToolSpec};
    use serde_json::json;

    struct Probe {
        spec: ToolSpec,
        content: Option<&'static str>,
        decision: PermissionDecision,
        accept_edits: bool,
    }

    impl Probe {
        fn says(decision: PermissionDecision) -> Self {
            Self {
                spec: ToolSpec::new(),
                content: None,
                decision,
                accept_edits: false,
            }
        }
        fn names(mut self, kind: PermissionContentKind, content: &'static str) -> Self {
            self.spec = self.spec.permission_content(kind);
            self.content = Some(content);
            self
        }
        fn accept_edits(mut self) -> Self {
            self.accept_edits = true;
            self
        }
    }

    #[async_trait]
    impl Tool for Probe {
        fn name(&self) -> &str {
            "Probe"
        }
        fn description(&self) -> &str {
            "answers what it is told to"
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
        fn permission_match_content(&self, _: &Value) -> Option<String> {
            self.content.map(str::to_string)
        }
        fn accept_edits_allows(&self, _: &Value, _: &ToolContext) -> bool {
            self.accept_edits
        }
        async fn check_permissions(&self, _: &Value, _: &ToolContext) -> PermissionDecision {
            self.decision.clone()
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

    struct Declares(PermissionFacts);

    #[async_trait]
    impl Tool for Declares {
        fn name(&self) -> &str {
            "Declares"
        }
        fn description(&self) -> &str {
            "states its own facts"
        }
        fn spec(&self) -> ToolSpec {
            ToolSpec::new().origin(ToolOrigin::plugin("p"))
        }
        fn input_schema(&self, _: PathStyle) -> Value {
            json!({"type": "object"})
        }
        async fn permission_facts(&self, _: &Value, _: &ToolContext) -> PermissionFacts {
            self.0.clone()
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

    async fn facts_of(tool: &dyn Tool) -> PermissionFacts {
        tool.permission_facts(&json!({}), &ctx()).await
    }

    #[tokio::test]
    async fn a_tool_that_allows_itself_is_contained() {
        let shell = Probe::says(PermissionDecision::allow())
            .names(PermissionContentKind::ShellCommand, "ls");
        assert_eq!(
            facts_of(&shell).await,
            PermissionFacts::new(Subject::Shell("ls".into()), Reach::Inside)
        );
        let path = Probe::says(PermissionDecision::allow())
            .names(PermissionContentKind::WritePath, "a.txt");
        assert_eq!(facts_of(&path).await.reach, Reach::Inside);

        let opaque = Probe::says(PermissionDecision::allow())
            .names(PermissionContentKind::Opaque, "anything");
        assert_eq!(
            facts_of(&opaque).await,
            PermissionFacts::new(Subject::Opaque("anything".into()), Reach::SessionOnly)
        );
        let bare = Probe::says(PermissionDecision::allow());
        assert_eq!(facts_of(&bare).await.reach, Reach::SessionOnly);
    }

    #[tokio::test]
    async fn a_tool_that_denies_itself_refuses_with_its_reason() {
        let tool = Probe::says(PermissionDecision::deny("system path"))
            .names(PermissionContentKind::WritePath, "/etc/x");
        assert_eq!(
            facts_of(&tool).await,
            PermissionFacts::new(
                Subject::WritePath("/etc/x".into()),
                Reach::Refused("system path".into())
            )
        );
    }

    #[tokio::test]
    async fn a_tool_that_asks_is_unknown_unless_accept_edits_lets_it_through() {
        let asks = Probe::says(PermissionDecision::ask("Probe?"))
            .names(PermissionContentKind::ReadPath, "/elsewhere");
        let facts = facts_of(&asks).await;
        assert_eq!(facts.subject, Subject::ReadPath("/elsewhere".into()));
        assert_eq!(facts.reach, Reach::Unknown);
        assert_eq!(facts.question.as_deref(), Some("Probe?"));
        assert_eq!(facts.runs, None);

        let listed = Probe::says(PermissionDecision::ask("Probe?")).accept_edits();
        assert_eq!(
            facts_of(&listed).await,
            PermissionFacts::new(Subject::None, Reach::Host { listed: true })
                .with_question("Probe?")
        );
    }

    #[tokio::test]
    async fn the_default_facts_are_the_bridged_ones() {
        let tool = Probe::says(PermissionDecision::ask("Probe?"))
            .names(PermissionContentKind::ShellCommand, "make");
        let ctx = ctx();
        let input = json!({});
        assert_eq!(
            tool.permission_facts(&input, &ctx).await,
            PermissionFacts::bridged(&tool, &input, &ctx).await
        );
        let as_dyn: &dyn Tool = &tool;
        assert_eq!(
            PermissionFacts::bridged(as_dyn, &input, &ctx).await,
            PermissionFacts::bridged(&tool, &input, &ctx).await
        );
    }

    #[tokio::test]
    async fn gathered_facts_carry_the_spec_and_the_traits_of_this_input() {
        let mut tool = Probe::says(PermissionDecision::allow())
            .names(PermissionContentKind::ReadPath, "src/lib.rs");
        tool.spec = tool
            .spec
            .clone()
            .origin(ToolOrigin::mcp("srv"))
            .self_allow_overrides_mode(true)
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
        let facts = ToolFacts::gather(&Declares(stated.clone()), &json!({}), &ctx()).await;
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
        assert_eq!(PermissionFacts::unknown().reach, Reach::Unknown);
    }

    #[test]
    fn the_stance_and_the_two_flags_agree_whichever_is_set() {
        assert_eq!(ToolSpec::new().stance, ModeStance::Normal);
        let coordination = ToolSpec::new().self_allow_overrides_mode(true);
        assert_eq!(coordination.stance, ModeStance::Coordination);
        let leaves = ToolSpec::new().leaves_plan_mode(true);
        assert_eq!(leaves.stance, ModeStance::LeavesPlan);
        assert_eq!(
            ToolSpec::new().leaves_plan_mode(false).stance,
            ModeStance::Normal
        );

        let set = ToolSpec::new().stance(ModeStance::Coordination);
        assert!(set.permission.self_allow_overrides_mode && !set.permission.leaves_plan_mode);
        let set = set.stance(ModeStance::LeavesPlan);
        assert!(set.permission.leaves_plan_mode && !set.permission.self_allow_overrides_mode);
        let set = set.stance(ModeStance::Normal);
        assert_eq!(set.permission, ToolPermissionSpec::default());
        assert_eq!(set, ToolSpec::new());
    }

    #[test]
    fn a_subject_keeps_its_content_whatever_its_kind() {
        for kind in [
            PermissionContentKind::Opaque,
            PermissionContentKind::ReadPath,
            PermissionContentKind::WritePath,
            PermissionContentKind::ShellCommand,
        ] {
            assert_eq!(Subject::of(kind, Some("x".into())).content(), Some("x"));
            assert_eq!(Subject::of(kind, None), Subject::None);
        }
        assert_eq!(
            Subject::Url("https://a".into()).content(),
            Some("https://a")
        );
    }

    #[test]
    fn a_request_carries_the_new_inputs_only_when_given() {
        let session = Arc::new(SessionState::new(PathBuf::from("/tmp")));
        let input = json!({});
        let bare = PermissionRequest::new("Probe", None, &input, Path::new("/tmp"), "s", &session);
        assert!(bare.facts.is_none() && bare.ctx.is_none());
        assert!(bare.hook.is_none() && bare.attendance.is_none());

        let facts = ToolFacts::unknown_tool();
        let ctx = ctx();
        let hook = HookAnswer::Approve {
            hook: "audit".into(),
        };
        let full = bare
            .with_facts(&facts)
            .with_ctx(&ctx)
            .with_hook(&hook)
            .with_attendance(Attendance::NoOneToAsk("a program's call"));
        let copy = full;
        assert_eq!(copy.facts, Some(&facts));
        assert_eq!(copy.ctx.map(|c| c.session_id.as_str()), Some("test"));
        assert_eq!(copy.hook, Some(&hook));
        assert_eq!(
            copy.attendance,
            Some(Attendance::NoOneToAsk("a program's call"))
        );
        assert!(full.delegate.is_none());
    }
}
