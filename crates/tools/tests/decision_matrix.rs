//! 权限判定的决策矩阵：每个内建工具 × 若干样例输入 × 5 种模式 × 5 种规则 ×
//! {有人值守, 无人值守} × {无 hook, hook approve}，每格一个结论，冻结成一份 golden。
//!
//! 每格照 runtime 的做法走一遍：输入先校验（不合法即 Deny），再带着 hook 的回答和
//! 在场性问 `Policy::from_settings`（生产路径），事实是工具自己声明的
//! （`ToolFacts::gather`）。"raw" 是在场性起作用之前的结论：因没人可问而拒的原本是一问，
//! hook 的 approve 也答不了的是只有人能答的一问。这份快照必须逐字不变：
//! `UPDATE_GOLDEN=1 cargo test -p tools --test integration decision_matrix::` 重新生成。
//!
//! 家目录用固定的 `/home/matrix`，不读环境；沙箱用一个恒报 Full 的桩，让无人值守的
//! Bash 放行通道不随运行机器变化。快照末尾另有一节：`sandbox.require_enforcement` 打开、
//! 沙箱只兑现一部分时 Bash 和 Monitor 的结论。

#![allow(clippy::unwrap_used, clippy::expect_used)]

use base::context::{EngineAttachment, SessionEnv, SessionState};
use base::interface::exec::{
    Confined, Enforcement, ExecProviders, ProcessSpec, Sandbox, SandboxMode, SandboxPolicy,
};
use base::interface::permission::{
    Answerable, Attendance, Delegate, HookAnswer, PermissionOutcome, PermissionPolicy,
    PermissionRequest, Stage, ToolFacts,
};
use base::permission::{PermissionMode, PermissionRule, RuleBehavior, RuleSource};
use base::settings::{PermissionAction, Settings};
use base::tool::{InMemoryToolRegistry, SandboxSettings, Tool, ToolContext};
use permissions::policy::Policy;
use serde_json::{json, Value};
use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;

const GOLDEN: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/golden/decision_matrix.txt"
);

#[derive(Debug)]
struct NoSearch;

#[async_trait::async_trait]
impl tools::web_search::SearchProvider for NoSearch {
    async fn search(
        &self,
        _: &str,
        _: usize,
        _: &str,
    ) -> Result<tools::web_search::SearchOutput, tools::web_search::SearchError> {
        unreachable!("the matrix never calls a tool")
    }
}

struct HoldsEverything;

impl Sandbox for HoldsEverything {
    fn confine(&self, spec: ProcessSpec, _: &SandboxPolicy) -> Confined {
        Confined {
            spec,
            mode: SandboxMode::LinuxBwrap,
            enforcement: Enforcement::Full,
            unmet: Vec::new(),
        }
    }
}

/// Holds a command only in part, under a host that demands it be held fully:
/// the cells `sandbox.require_enforcement` decides.
struct HoldsPartly;

impl Sandbox for HoldsPartly {
    fn confine(&self, spec: ProcessSpec, _: &SandboxPolicy) -> Confined {
        Confined {
            spec,
            mode: SandboxMode::LinuxBwrap,
            enforcement: Enforcement::Partial,
            unmet: vec!["the domain allowlist".into()],
        }
    }
}

fn builtin_tools() -> Arc<InMemoryToolRegistry> {
    let reg = Arc::new(InMemoryToolRegistry::new());
    tools::register_builtin_tools(&reg);
    tools::register_cron_tools(&reg, Arc::new(tools::cron::CronStore::new()));
    tools::register_worktree_tools(
        &reg,
        Arc::new(tools::worktree_tools::WorktreeRegistry::new()),
    );
    reg.register(Arc::new(tools::web_search::WebSearchTool::new(Box::new(
        NoSearch,
    ))));
    reg.register(Arc::new(tools::task_stop::TaskStopTool));
    reg.register(Arc::new(tools::task_output::TaskOutputTool));
    reg.register(Arc::new(tools::task_wait::TaskWaitTool));
    reg.register(Arc::new(tools::task_message::TaskMessageTool));
    reg
}

struct Sample {
    input: Value,
}

fn shell(commands: &[String]) -> Vec<Sample> {
    commands
        .iter()
        .map(|c| Sample {
            input: json!({ "command": c }),
        })
        .collect()
}

fn samples(name: &str, cwd: &Path, home: &str) -> Vec<Sample> {
    let inside = |p: &str| cwd.join(p).display().to_string();
    let one = |input: Value| vec![Sample { input }];
    match name {
        "Bash" => shell(&[
            "ls -la".into(),
            "cat README.md".into(),
            "git status".into(),
            "git log --oneline -5 | head -3".into(),
            "echo hi > out.txt".into(),
            "rm -rf build".into(),
            "cargo build".into(),
            "cat /elsewhere/data.txt".into(),
            "ls /elsewhere".into(),
            format!("cat {home}/.ssh/id_rsa"),
            format!("cat {home}/.aws/credentials"),
            format!("cp {home}/.ssh/id_ed25519 key.txt"),
        ]),
        "Monitor" => [
            "tail -f log.txt",
            "cargo watch",
            "ls",
            "tail -f /elsewhere/log",
        ]
        .iter()
        .map(|c| Sample {
            input: json!({ "command": c, "description": "watch" }),
        })
        .collect(),
        "Read" => vec![
            Sample {
                input: json!({ "file_path": inside("src/lib.rs") }),
            },
            Sample {
                input: json!({ "file_path": "/elsewhere/data.txt" }),
            },
            Sample {
                input: json!({ "file_path": format!("{home}/.ssh/id_rsa") }),
            },
        ],
        "Write" => vec![
            Sample {
                input: json!({ "file_path": inside("a.txt"), "content": "x" }),
            },
            Sample {
                input: json!({ "file_path": "/elsewhere/a.txt", "content": "x" }),
            },
            Sample {
                input: json!({ "file_path": inside(".agix/skills/s/SKILL.md"), "content": "x" }),
            },
            Sample {
                input: json!({ "file_path": format!("{home}/.ssh/authorized_keys"), "content": "x" }),
            },
        ],
        "Edit" => vec![
            Sample {
                input: json!({
                    "file_path": inside("a.txt"), "old_string": "a", "new_string": "b"
                }),
            },
            Sample {
                input: json!({
                    "file_path": "/elsewhere/a.txt", "old_string": "a", "new_string": "b"
                }),
            },
        ],
        "NotebookEdit" => vec![
            Sample {
                input: json!({
                    "file_path": inside("n.ipynb"), "mode": "edit", "cell_index": 0,
                    "new_source": "x"
                }),
            },
            Sample {
                input: json!({
                    "file_path": "/elsewhere/n.ipynb", "mode": "edit", "cell_index": 0,
                    "new_source": "x"
                }),
            },
        ],
        "Grep" => vec![
            Sample {
                input: json!({ "pattern": "fn main", "path": cwd.display().to_string() }),
            },
            Sample {
                input: json!({ "pattern": "x", "path": "/elsewhere" }),
            },
        ],
        "Glob" => vec![
            Sample {
                input: json!({ "pattern": "**/*.rs", "path": cwd.display().to_string() }),
            },
            Sample {
                input: json!({ "pattern": "*", "path": "/elsewhere" }),
            },
        ],
        "WebFetch" => vec![
            Sample {
                input: json!({ "url": "https://example.com/page", "prompt": "x" }),
            },
            Sample {
                input: json!({ "url": "http://127.0.0.1:8080/", "prompt": "x" }),
            },
        ],
        "Ping" => one(json!({ "url": "https://example.com" })),
        "WebSearch" => one(json!({ "query": "rust" })),
        "CronCreate" => vec![
            Sample {
                input: json!({ "cron": "*/5 * * * *", "prompt": "check" }),
            },
            Sample {
                input: json!({ "cron": "0 9 * * *", "prompt": "report", "durable": true }),
            },
        ],
        "CronDelete" => one(json!({ "id": "cron-1" })),
        "CronList" => one(json!({})),
        "EnterWorktree" => one(json!({ "slug": "feature-x" })),
        "ExitWorktree" => one(json!({ "action": "keep" })),
        "EnterPlanMode" => one(json!({})),
        "ExitPlanMode" => one(json!({ "note": "ready" })),
        "AskUserQuestion" => one(json!({ "question": "which?" })),
        "GoalCreate" => one(json!({ "objective": "ship it" })),
        "GoalGet" => one(json!({})),
        "GoalUpdate" => one(json!({
            "goal_id": "g1", "revision": 1, "action": "complete"
        })),
        "PushNotification" => one(json!({ "message": "done" })),
        "Sleep" => one(json!({ "duration_ms": 10 })),
        "StructuredOutput" => one(json!({})),
        "TaskMessage" => one(json!({ "task_id": "t1", "message": "go" })),
        "TaskOutput" => one(json!({ "task_id": "t1" })),
        "TaskStop" => one(json!({ "task_id": "t1" })),
        "TaskWait" => one(json!({ "task_ids": ["t1"] })),
        "TodoWrite" => one(json!({
            "todos": [{ "content": "a", "status": "pending", "activeForm": "doing a" }]
        })),
        "ToolSearch" => one(json!({ "query": "select:Read" })),
        other => panic!("{other}: a built-in tool with no matrix samples"),
    }
}

const MODES: [(&str, PermissionMode); 5] = [
    ("default", PermissionMode::Default),
    ("acceptEdits", PermissionMode::AcceptEdits),
    ("plan", PermissionMode::Plan),
    ("dontAsk", PermissionMode::DontAsk),
    ("bypassPermissions", PermissionMode::BypassPermissions),
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum RuleCase {
    None,
    Allow,
    Ask,
    Deny,
    ManagedAsk,
}

const RULES: [(&str, RuleCase); 5] = [
    ("none", RuleCase::None),
    ("allow", RuleCase::Allow),
    ("ask", RuleCase::Ask),
    ("deny", RuleCase::Deny),
    ("managed-ask", RuleCase::ManagedAsk),
];

fn permission(tool: &str, case: RuleCase) -> Policy {
    let mut settings = base::settings::LoadedSettings::new(
        Settings::defaults_for("matrix-model"),
        base::settings::PathSettings::default(),
    );
    let mut extra = Vec::new();
    let user_rule = |behavior| PermissionRule {
        source: RuleSource::UserSettings,
        behavior,
        tool_name: tool.into(),
        rule_content: None,
    };
    match case {
        RuleCase::None => {}
        RuleCase::Allow => extra.push(user_rule(RuleBehavior::Allow)),
        RuleCase::Ask => extra.push(user_rule(RuleBehavior::Ask)),
        RuleCase::Deny => extra.push(user_rule(RuleBehavior::Deny)),
        RuleCase::ManagedAsk => {
            settings.managed = Some(base::managed_settings::ManagedSettings {
                origin: "matrix".into(),
                error: None,
                permission_rules: vec![base::settings::PermissionRule {
                    tool: tool.into(),
                    action: PermissionAction::Ask,
                    matching: Default::default(),
                }],
                disable_bypass_permissions_mode: false,
            });
        }
    }
    Policy::from_settings(&settings, extra)
}

fn word(outcome: &PermissionOutcome) -> &'static str {
    match outcome {
        PermissionOutcome::Permit { .. } => "Permit",
        PermissionOutcome::Deny { .. } => "Deny",
        PermissionOutcome::Prompt { .. } => "Prompt",
    }
}

/// What the policy concluded before the attendance was weighed, from what it
/// concluded without a hook and with a hook's approve.
fn raw(no_hook: &PermissionOutcome, approved: &PermissionOutcome) -> &'static str {
    match no_hook {
        PermissionOutcome::Deny { reason } if reason.stage == Stage::Attendance => {
            if approved.is_permit() {
                "Prompt"
            } else {
                "Prompt+managed"
            }
        }
        PermissionOutcome::Prompt {
            answerable: Answerable::PersonOnly,
            ..
        } => "Prompt+managed",
        other => word(other),
    }
}

fn session(cwd: &Path, mode: PermissionMode, unattended: bool) -> Arc<SessionState> {
    session_in(cwd, mode, unattended, Arc::new(HoldsEverything))
}

fn session_in(
    cwd: &Path,
    mode: PermissionMode,
    unattended: bool,
    sandbox: Arc<dyn Sandbox>,
) -> Arc<SessionState> {
    let state = SessionState::for_tests(cwd.to_path_buf());
    state.attach(EngineAttachment::new(
        SessionEnv {
            unattended,
            base_mode: mode,
            ..SessionEnv::new(cwd.to_path_buf())
        },
        ExecProviders {
            sandbox,
            ..platform::local_providers()
        },
        1,
    ));
    Arc::new(state)
}

async fn decide(
    policy: &Policy,
    tool: &Arc<dyn Tool>,
    input: &Value,
    cwd: &Path,
    mode: PermissionMode,
    unattended: bool,
) -> PermissionOutcome {
    let session = session(cwd, mode, unattended);
    let ctx = context_like_the_runtime(&session, cwd);
    let facts = ToolFacts::gather(tool.as_ref(), input, &ctx).await;
    policy
        .decide(&PermissionRequest::new(
            tool.name(),
            Some(tool),
            input,
            &facts,
            &ctx,
            &session,
        ))
        .await
}

/// The facts of a call to `tool`, or of a name nothing answers to.
async fn facts_of(tool: Option<&Arc<dyn Tool>>, input: &Value, ctx: &ToolContext) -> ToolFacts {
    match tool {
        Some(tool) => ToolFacts::gather(tool.as_ref(), input, ctx).await,
        None => ToolFacts::unknown_tool(),
    }
}

pub(crate) async fn render() -> (String, usize) {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().canonicalize().unwrap();
    let home = "/home/matrix".to_string();
    let cwd_text = cwd.display().to_string();
    let label = |input: &Value| {
        input
            .to_string()
            .replace(&cwd_text, "$CWD")
            .replace(&home, "$HOME")
    };

    let registry = builtin_tools();
    let mut all = registry.list();
    all.sort_by(|a, b| a.name().cmp(b.name()));

    let mut out = String::from(
        "# tool | input | mode | rule: attended raw -> final (no hook / hook approve); \
         unattended raw -> final (no hook / hook approve)\n",
    );
    let mut cells = 0usize;
    for tool in &all {
        cells += table(&mut out, tool, &cwd, &home, &label, Sandboxing::Holds).await;
    }
    let _ = writeln!(out, "\n# cells: {cells}");

    let _ = writeln!(
        out,
        "\n# `sandbox.require_enforcement` on, and a sandbox that holds a command only in part"
    );
    let mut confinement = 0usize;
    for tool in all
        .iter()
        .filter(|t| matches!(t.name(), "Bash" | "Monitor"))
    {
        confinement += table(
            &mut out,
            tool,
            &cwd,
            &home,
            &label,
            Sandboxing::HoldsPartlyAsDemanded,
        )
        .await;
    }
    let _ = writeln!(out, "\n# confinement cells: {confinement}");
    (out, cells + confinement)
}

/// How the session's sandbox holds a command.
#[derive(Clone, Copy)]
enum Sandboxing {
    Holds,
    /// Only in part, and `sandbox.require_enforcement` is on.
    HoldsPartlyAsDemanded,
}

/// Every sample of `tool` in every cell, printed; the number of cells.
async fn table(
    out: &mut String,
    tool: &Arc<dyn Tool>,
    cwd: &Path,
    home: &str,
    label: &dyn Fn(&Value) -> String,
    sandboxing: Sandboxing,
) -> usize {
    let mut cells = 0usize;
    let _ = writeln!(out, "\n## {}", tool.name());
    for sample in samples(tool.name(), cwd, home) {
        let _ = writeln!(out, "\n{}", label(&sample.input));
        for (mode_name, mode) in MODES {
            for (rule_name, rule) in RULES {
                let policy = permission(tool.name(), rule);
                let at = |unattended| Setup {
                    mode,
                    unattended,
                    sandboxing,
                };
                let [a_raw, a_none, a_hook] =
                    rendered(&policy, tool, &sample.input, cwd, at(false)).await;
                let [u_raw, u_none, u_hook] =
                    rendered(&policy, tool, &sample.input, cwd, at(true)).await;
                let _ = writeln!(
                    out,
                    "{mode_name:<17} {rule_name:<11} | att {a_raw} -> {a_none} / {a_hook} \
                     | unatt {u_raw} -> {u_none} / {u_hook}",
                );
                cells += 4;
            }
        }
    }
    cells
}

/// The session a cell's call is made in.
#[derive(Clone, Copy)]
struct Setup {
    mode: PermissionMode,
    unattended: bool,
    sandboxing: Sandboxing,
}

impl Setup {
    fn holding(mode: PermissionMode, unattended: bool) -> Self {
        Self {
            mode,
            unattended,
            sandboxing: Sandboxing::Holds,
        }
    }

    fn session(&self, cwd: &Path) -> Arc<SessionState> {
        let sandbox: Arc<dyn Sandbox> = match self.sandboxing {
            Sandboxing::Holds => Arc::new(HoldsEverything),
            Sandboxing::HoldsPartlyAsDemanded => Arc::new(HoldsPartly),
        };
        session_in(cwd, self.mode, self.unattended, sandbox)
    }

    fn context(&self, session: &Arc<SessionState>, cwd: &Path) -> ToolContext {
        let mut ctx = context_like_the_runtime(session, cwd);
        if let Sandboxing::HoldsPartlyAsDemanded = self.sandboxing {
            ctx.sandbox.require_enforcement = true;
            ctx.confinement = tools::bash::confinement_report(
                &ctx.sandbox,
                &ctx.exec,
                ctx.config.dangerously_disable_sandbox,
                cwd,
                &ctx.session_id,
            );
        }
        ctx
    }
}

#[tokio::test]
async fn every_builtin_tool_is_decided_the_same_in_every_cell() {
    let (actual, _) = render().await;

    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(Path::new(GOLDEN).parent().unwrap()).unwrap();
        std::fs::write(GOLDEN, &actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(GOLDEN)
        .unwrap_or_else(|e| panic!("{GOLDEN}: {e}; generate it with UPDATE_GOLDEN=1"));
    if actual != expected {
        let first = actual
            .lines()
            .zip(expected.lines())
            .position(|(a, e)| a != e)
            .unwrap_or(0);
        panic!(
            "decisions drifted from {GOLDEN} at line {}: {:?} != {:?}; if intended, \
             regenerate with UPDATE_GOLDEN=1",
            first + 1,
            actual.lines().nth(first),
            expected.lines().nth(first),
        );
    }
}

/// The context the runtime gives a call of `session`'s.
fn context_like_the_runtime(session: &Arc<SessionState>, cwd: &Path) -> ToolContext {
    let settings = base::settings::LoadedSettings::new(
        Settings::defaults_for("matrix-model"),
        base::settings::PathSettings::default(),
    );
    let mut ctx = ToolContext::from_engine_ctx_with(
        cwd.to_path_buf(),
        tokio_util::sync::CancellationToken::new(),
        session.exec().unwrap(),
    );
    ctx.session_id = "matrix".into();
    ctx.sandbox = SandboxSettings {
        allow_read: settings.values.sandbox.allow_read.clone(),
        deny_read: settings.values.sandbox.deny_read.clone(),
        allowed_domains: settings.values.sandbox.allowed_domains.clone(),
        network_mode: settings.values.sandbox.network_mode,
        deny_write: settings.values.sandbox.deny_write.clone(),
        allow_write: settings.values.sandbox.allow_write.clone(),
        paths: Arc::new(base::paths::SensitivePaths::for_paths(
            &base::paths::ConfigPaths::from_settings(&settings.paths),
        )),
        require_enforcement: settings.values.sandbox.require_enforcement,
        unattended_requires_network_policy: settings
            .values
            .sandbox
            .unattended_requires_network_policy,
    };
    ctx.session = session.clone();
    ctx.permission_mode = session.permission_mode();
    let env = session.env();
    ctx.additional_writable_dirs = env.additional_writable_dirs.clone();
    ctx.attendance = if env.unattended {
        Attendance::Unattended
    } else {
        Attendance::Attended
    };
    ctx.confinement = tools::bash::confinement_report(
        &ctx.sandbox,
        &ctx.exec,
        ctx.config.dangerously_disable_sandbox,
        cwd,
        &ctx.session_id,
    );
    ctx
}

/// What the runtime makes of a call: `None` when its input is refused before
/// anyone judges it, else the policy's conclusion on what the tool states.
async fn concluded(
    policy: &Policy,
    tool: &Arc<dyn Tool>,
    input: &Value,
    cwd: &Path,
    setup: Setup,
    hook: &HookAnswer,
) -> Option<PermissionOutcome> {
    let session = setup.session(cwd);
    let ctx = setup.context(&session, cwd);
    if tool.validate_input(input, &ctx).await.is_err() {
        return None;
    }
    let facts = ToolFacts::gather(tool.as_ref(), input, &ctx).await;
    let request = PermissionRequest::new(tool.name(), Some(tool), input, &facts, &ctx, &session)
        .with_hook(hook)
        .with_attendance(ctx.attendance);
    Some(policy.decide(&request).await)
}

/// One attendance of one cell, as the golden prints it.
async fn rendered(
    policy: &Policy,
    tool: &Arc<dyn Tool>,
    input: &Value,
    cwd: &Path,
    setup: Setup,
) -> [&'static str; 3] {
    let approve = HookAnswer::Approve {
        hook: "matrix".into(),
    };
    let none = HookAnswer::None;
    let no_hook = concluded(policy, tool, input, cwd, setup, &none).await;
    let hook = concluded(policy, tool, input, cwd, setup, &approve).await;
    match (no_hook, hook) {
        (Some(no_hook), Some(hook)) => [raw(&no_hook, &hook), word(&no_hook), word(&hook)],
        _ => ["Deny"; 3],
    }
}

/// F-8: a durable cron job outlives the session and acts on its own, so it is
/// asked about where a session-only one runs.
#[tokio::test]
async fn a_durable_cron_job_is_asked_about_where_a_session_only_one_runs() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().canonicalize().unwrap();
    let cron = builtin_tools().get("CronCreate").unwrap();
    let policy = permission("CronCreate", RuleCase::None);
    let job =
        |durable: bool| json!({ "cron": "0 9 * * *", "prompt": "report", "durable": durable });
    let at = |unattended| Setup::holding(PermissionMode::Default, unattended);
    let none = HookAnswer::None;
    let outcome = |input: Value, unattended| {
        let (policy, cron, cwd, none) = (&policy, &cron, &cwd, &none);
        async move {
            word(
                &concluded(policy, cron, &input, cwd, at(unattended), none)
                    .await
                    .unwrap(),
            )
        }
    };
    assert_eq!(outcome(job(false), false).await, "Permit");
    assert_eq!(outcome(job(true), false).await, "Prompt");
    assert_eq!(outcome(job(true), true).await, "Deny");
}

fn stage_of(outcome: &PermissionOutcome) -> Option<Stage> {
    match outcome {
        PermissionOutcome::Deny { reason } | PermissionOutcome::Prompt { reason, .. } => {
            Some(reason.stage)
        }
        PermissionOutcome::Permit { .. } => None,
    }
}

async fn stage_with(
    policy: &Policy,
    name: Option<&str>,
    input: &Value,
    cwd: &Path,
    hook: &HookAnswer,
    attendance: Attendance,
) -> Option<Stage> {
    let session = session(cwd, PermissionMode::Default, false);
    let registry = builtin_tools();
    let tool = name.and_then(|n| registry.get(n));
    let ctx = context_like_the_runtime(&session, cwd);
    let facts = facts_of(tool.as_ref(), input, &ctx).await;
    let request = PermissionRequest::new(
        name.unwrap_or("Nope"),
        tool.as_ref(),
        input,
        &facts,
        &ctx,
        &session,
    )
    .with_hook(hook)
    .with_attendance(attendance);
    stage_of(&policy.decide(&request).await)
}

/// A delegate's call, held to `rules` on top of `policy`'s own.
async fn delegated(
    policy: &Policy,
    name: &str,
    input: &Value,
    cwd: &Path,
    rules: &[PermissionRule],
) -> Option<Stage> {
    let parent = session(cwd, PermissionMode::Default, false);
    let delegate = session(cwd, PermissionMode::Default, false);
    let tool = builtin_tools().get(name);
    let ctx = context_like_the_runtime(&delegate, cwd);
    let facts = facts_of(tool.as_ref(), input, &ctx).await;
    let request = PermissionRequest::new(name, tool.as_ref(), input, &facts, &ctx, &parent)
        .for_delegate(Delegate::new(&delegate, PermissionMode::Default, rules));
    stage_of(&policy.decide(&request).await)
}

/// PERM-8: every stage the policy refuses or asks at is reached by a call
/// here, so a stage added to the policy is added to the matrix too.
#[tokio::test]
async fn every_stage_of_the_policy_is_reached() {
    use PermissionMode::*;
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().canonicalize().unwrap();
    let registry = builtin_tools();
    let inside = |p: &str| cwd.join(p).display().to_string();
    let bash = |command: &str| json!({ "command": command });
    let plain = |rule| permission("Bash", rule);
    let reached = |policy: Policy, name: &str, input: Value, mode| {
        let tool = registry.get(name).unwrap();
        let cwd = cwd.clone();
        async move { stage_of(&decide(&policy, &tool, &input, &cwd, mode, false).await) }
    };
    let attended = Attendance::Attended;
    let no_hook = HookAnswer::None;

    let mut unreadable = base::settings::LoadedSettings::new(
        Settings::defaults_for("matrix-model"),
        base::settings::PathSettings::default(),
    );
    unreadable.managed = Some(base::managed_settings::ManagedSettings {
        origin: "matrix".into(),
        error: Some("not JSON".into()),
        permission_rules: Vec::new(),
        disable_bypass_permissions_mode: false,
    });
    let deny_bash = [PermissionRule {
        source: RuleSource::Session,
        behavior: RuleBehavior::Deny,
        tool_name: "Bash".into(),
        rule_content: None,
    }];
    let block = HookAnswer::Block {
        hook: "guard".into(),
        reason: "not today".into(),
    };
    let build = bash("cargo build");

    let seen = [
        (
            Stage::ManagedUnreadable,
            reached(
                Policy::from_settings(&unreadable, []),
                "Bash",
                bash("ls"),
                Default,
            )
            .await,
        ),
        (
            Stage::UnknownTool,
            stage_with(
                &plain(RuleCase::None),
                None,
                &json!({}),
                &cwd,
                &no_hook,
                attended,
            )
            .await,
        ),
        (
            Stage::Credentials,
            reached(
                plain(RuleCase::None),
                "Read",
                json!({ "file_path": "/home/matrix/.ssh/id_rsa" }),
                BypassPermissions,
            )
            .await,
        ),
        (
            Stage::Hook,
            stage_with(
                &plain(RuleCase::None),
                Some("Bash"),
                &build,
                &cwd,
                &block,
                attended,
            )
            .await,
        ),
        (
            Stage::ToolRefusal,
            reached(
                permission("Write", RuleCase::None),
                "Write",
                json!({ "file_path": "/usr/bin/evil", "content": "x" }),
                Default,
            )
            .await,
        ),
        (
            Stage::Confinement,
            concluded(
                &plain(RuleCase::None),
                &registry.get("Bash").unwrap(),
                &build,
                &cwd,
                Setup {
                    mode: Default,
                    unattended: false,
                    sandboxing: Sandboxing::HoldsPartlyAsDemanded,
                },
                &no_hook,
            )
            .await
            .as_ref()
            .and_then(stage_of),
        ),
        (
            Stage::Rule,
            reached(plain(RuleCase::Deny), "Bash", bash("ls"), Default).await,
        ),
        (
            Stage::ManagedRule,
            reached(
                plain(RuleCase::ManagedAsk),
                "Bash",
                bash("ls"),
                BypassPermissions,
            )
            .await,
        ),
        (
            Stage::DelegateRule,
            delegated(
                &plain(RuleCase::None),
                "Bash",
                &bash("ls"),
                &cwd,
                &deny_bash,
            )
            .await,
        ),
        (
            Stage::BypassImmune,
            reached(
                plain(RuleCase::None),
                "Bash",
                bash("cat /etc/passwd"),
                Default,
            )
            .await,
        ),
        (
            Stage::PlanMode,
            reached(plain(RuleCase::None), "Bash", build.clone(), Plan).await,
        ),
        (
            Stage::DontAsk,
            reached(plain(RuleCase::None), "Bash", build.clone(), DontAsk).await,
        ),
        (
            Stage::Configuration,
            reached(
                permission("Write", RuleCase::None),
                "Write",
                json!({ "file_path": inside(".agix/skills/s/SKILL.md"), "content": "x" }),
                Default,
            )
            .await,
        ),
        (
            Stage::ModeDefault,
            reached(plain(RuleCase::None), "Bash", build.clone(), Default).await,
        ),
        (
            Stage::Attendance,
            stage_with(
                &plain(RuleCase::None),
                Some("Bash"),
                &build,
                &cwd,
                &no_hook,
                Attendance::NoOneToAsk("nobody here"),
            )
            .await,
        ),
    ];
    for (expected, got) in seen {
        assert_eq!(got, Some(expected));
    }
}
