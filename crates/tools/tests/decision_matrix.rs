//! 权限判定的决策矩阵：每个内建工具 × 若干样例输入 × 5 种模式 × 5 种规则 ×
//! {有人值守, 无人值守} × {无 hook, hook approve}，每格一个结论，冻结成一份 golden。
//!
//! 判定走 `RuleSetPermission::from_settings`（生产路径）；runtime 里
//! `turn/tool_exec.rs` 的 R6（hook approve 只能回答非 managed 的 Prompt）和 R7
//! （无人值守时 Prompt 转 Deny）在这里照现行逻辑复刻。判定管线重构之后，这份
//! 快照必须逐字不变：`UPDATE_GOLDEN=1 cargo test -p tools --test integration
//! decision_matrix::` 重新生成。
//!
//! 家目录用固定的 `/home/matrix`，不读环境；沙箱用一个恒报 Full 的桩，让无人值守的
//! Bash 放行通道不随运行机器变化。
//!
//! 自己声明 `permission_facts` 的工具另有一条等价断言：它的事实和从旧方法桥接推出的
//! 事实，经同一个按中心判定顺序写的 `conclusion` 得出相同结论，且与 gate 的结论一致。

#![allow(clippy::unwrap_used, clippy::expect_used)]

use base::context::SessionState;
use base::interface::exec::{
    Confined, Enforcement, ExecProviders, ProcessSpec, Sandbox, SandboxMode, SandboxPolicy,
};
use base::interface::permission::{
    ModeStance, PermissionFacts, PermissionOutcome, PermissionPolicy, PermissionRequest, Reach,
    ToolFacts,
};
use base::permission::{PermissionMode, PermissionRule, RuleBehavior, RuleSource};
use base::settings::{PermissionAction, Settings};
use base::tool::{InMemoryToolRegistry, SandboxSettings, Tool, ToolContext};
use permissions::rule_set_permission::RuleSetPermission;
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

fn permission(tool: &str, case: RuleCase) -> RuleSetPermission {
    let mut settings = Settings::defaults_for("matrix-model");
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
                    tier: Some(base::interface::config_source::Tier::Managed),
                }],
                disable_bypass_permissions_mode: false,
            });
        }
    }
    RuleSetPermission::from_settings(&settings, extra)
}

/// What `turn/tool_exec.rs` makes of the policy's outcome: R6, then R7.
fn after_runtime(
    outcome: &PermissionOutcome,
    hook_approved: bool,
    unattended: bool,
) -> &'static str {
    match outcome {
        PermissionOutcome::Permit => "Permit",
        PermissionOutcome::Deny { .. } => "Deny",
        PermissionOutcome::Prompt { managed, .. } => {
            if hook_approved && !managed {
                "Permit"
            } else if unattended {
                "Deny"
            } else {
                "Prompt"
            }
        }
    }
}

fn raw(outcome: &PermissionOutcome) -> &'static str {
    match outcome {
        PermissionOutcome::Permit => "Permit",
        PermissionOutcome::Deny { .. } => "Deny",
        PermissionOutcome::Prompt { managed: false, .. } => "Prompt",
        PermissionOutcome::Prompt { managed: true, .. } => "Prompt+managed",
    }
}

fn session(cwd: &Path, mode: PermissionMode, unattended: bool) -> Arc<SessionState> {
    let mut state = SessionState::new(cwd.to_path_buf()).with_permission_mode(mode);
    state.unattended = unattended;
    state.bind_exec(ExecProviders {
        sandbox: Arc::new(HoldsEverything),
        ..platform::local_providers()
    });
    Arc::new(state)
}

async fn decide(
    policy: &RuleSetPermission,
    tool: &Arc<dyn Tool>,
    input: &Value,
    cwd: &Path,
    mode: PermissionMode,
    unattended: bool,
) -> PermissionOutcome {
    let session = session(cwd, mode, unattended);
    policy
        .decide(&PermissionRequest::new(
            tool.name(),
            Some(tool),
            input,
            cwd,
            "matrix",
            &session,
        ))
        .await
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
        let _ = writeln!(out, "\n## {}", tool.name());
        for sample in samples(tool.name(), &cwd, &home) {
            let _ = writeln!(out, "\n{}", label(&sample.input));
            for (mode_name, mode) in MODES {
                for (rule_name, rule) in RULES {
                    let policy = permission(tool.name(), rule);
                    let attended = decide(&policy, tool, &sample.input, &cwd, mode, false).await;
                    let unattended = decide(&policy, tool, &sample.input, &cwd, mode, true).await;
                    let _ = writeln!(
                        out,
                        "{mode_name:<17} {rule_name:<11} | att {} -> {} / {} | unatt {} -> {} / {}",
                        raw(&attended),
                        after_runtime(&attended, false, false),
                        after_runtime(&attended, true, false),
                        raw(&unattended),
                        after_runtime(&unattended, false, true),
                        after_runtime(&unattended, true, true),
                    );
                    cells += 4;
                }
            }
        }
    }
    let _ = writeln!(out, "\n# cells: {cells}");
    (out, cells)
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

/// What `RuleSetPermission` hands a tool's own checks, for the same session.
fn context_like_the_gates(session: &Arc<SessionState>, cwd: &Path) -> ToolContext {
    let settings = Settings::defaults_for("matrix-model");
    let mut ctx = ToolContext::from_engine_ctx_with(
        cwd.to_path_buf(),
        tokio_util::sync::CancellationToken::new(),
        session.exec().unwrap(),
    );
    ctx.session_id = "matrix".into();
    ctx.sandbox = SandboxSettings {
        allow_read: settings.sandbox.allow_read.clone(),
        deny_read: settings.sandbox.deny_read.clone(),
        allowed_domains: settings.sandbox.allowed_domains.clone(),
        network_mode: settings.sandbox.network_mode,
        deny_write: settings.sandbox.deny_write.clone(),
        allow_write: settings.sandbox.allow_write.clone(),
        paths: Arc::new(base::paths::SensitivePaths::for_paths(
            &base::paths::ConfigPaths::from_settings(&settings.paths),
        )),
        require_enforcement: settings.sandbox.require_enforcement,
        unattended_requires_network_policy: settings.sandbox.unattended_requires_network_policy,
    };
    ctx.session = session.clone();
    ctx.permission_mode = session.permission_mode();
    ctx.additional_writable_dirs = session.additional_writable_dirs.clone();
    ctx.dangerously_disable_sandbox = session.sandbox_disabled;
    ctx
}

#[derive(Clone, Copy)]
struct Cell {
    mode: PermissionMode,
    rule: RuleCase,
    unattended: bool,
    hook_approved: bool,
    /// The session's sandbox fully holds a command, under the network
    /// condition `sandbox.unattended_requires_network_policy` sets.
    confined: bool,
}

/// The final conclusion a call's facts lead to, weighed in the order the
/// central policy weighs them. Credentials, an unreadable managed layer and
/// the bypass-immune paths are judged on the subject before any of this and
/// are left out.
fn conclusion(facts: &ToolFacts, cell: Cell) -> &'static str {
    use PermissionMode::*;
    let answer = |person_only: bool| {
        if cell.hook_approved && !person_only {
            "Permit"
        } else if cell.unattended {
            "Deny"
        } else {
            "Prompt"
        }
    };
    let managed = cell.rule == RuleCase::ManagedAsk;
    if cell.mode == BypassPermissions {
        return if managed { answer(true) } else { "Permit" };
    }
    if matches!(facts.reach, Reach::Refused(_)) || cell.rule == RuleCase::Deny {
        return "Deny";
    }
    let contained = matches!(
        facts.reach,
        Reach::SessionOnly | Reach::Delegates | Reach::Operator | Reach::Inside
    );
    let spared = contained && facts.stance == ModeStance::Coordination;
    if cell.mode == Plan
        && !facts.traits.read_only
        && facts.stance != ModeStance::LeavesPlan
        && !spared
    {
        return "Deny";
    }
    if cell.mode == DontAsk && cell.rule != RuleCase::Allow && !spared {
        return "Deny";
    }
    if matches!(cell.rule, RuleCase::Ask | RuleCase::ManagedAsk) {
        return answer(managed);
    }
    let writes_configuration = !facts.traits.read_only
        && facts.subject.content().is_some_and(|s| {
            s.split_whitespace()
                .any(base::paths::SensitivePaths::names_configuration)
        });
    let background = facts.runs.is_some_and(|r| r.background);
    if contained && !background && !writes_configuration {
        return "Permit";
    }
    if cell.rule == RuleCase::Allow {
        return "Permit";
    }
    if cell.mode == AcceptEdits
        && facts.traits.read_only
        && matches!(
            facts.reach,
            Reach::SessionOnly | Reach::Inside | Reach::Host { listed: true }
        )
    {
        return "Permit";
    }
    // `Inside` as well as `Unknown`: a background command that stays where the
    // session may read is no further-reaching than one nobody vouched for, and
    // unattended it runs today.
    if cell.unattended
        && cell.confined
        && facts.runs.is_some()
        && matches!(facts.reach, Reach::Unknown | Reach::Inside)
        && !writes_configuration
    {
        return "Permit";
    }
    answer(false)
}

async fn bridged(tool: &dyn Tool, input: &Value, ctx: &ToolContext) -> ToolFacts {
    let mut facts = ToolFacts::gather(tool, input, ctx).await;
    let bridged = PermissionFacts::bridged(tool, input, ctx).await;
    facts.subject = bridged.subject;
    facts.reach = bridged.reach;
    facts.runs = bridged.runs;
    facts.question = bridged.question;
    facts
}

/// One cell of one call: the conclusion the tool's own facts lead to, the one
/// the facts bridged from its old answers lead to, and the gate's where it did
/// not refuse the sample before reading the tool.
struct Compared {
    at: String,
    cell: Cell,
    stated: &'static str,
    old: &'static str,
    gate: Option<&'static str>,
}

async fn compare_stated_with_bridged(name: &str, input: &Value, cwd: &Path) -> Vec<Compared> {
    let registry = builtin_tools();
    let tool = registry.get(name).unwrap();
    let refused_before_facts = matches!(
        decide(
            &permission(name, RuleCase::None),
            &tool,
            input,
            cwd,
            PermissionMode::BypassPermissions,
            false,
        )
        .await,
        PermissionOutcome::Deny { .. }
    );
    let mut out = Vec::new();
    for (mode_name, mode) in MODES {
        for unattended in [false, true] {
            let session = session(cwd, mode, unattended);
            let ctx = context_like_the_gates(&session, cwd);
            let stated = ToolFacts::gather(tool.as_ref(), input, &ctx).await;
            let old = bridged(tool.as_ref(), input, &ctx).await;
            assert_eq!(stated.subject, old.subject, "{name} {input}");
            assert_eq!(stated.traits, old.traits, "{name} {input}");
            assert_eq!(stated.stance, old.stance, "{name} {input}");
            assert_eq!(stated.origin, old.origin, "{name} {input}");
            let confined = !ctx.dangerously_disable_sandbox
                && (!ctx.sandbox.unattended_requires_network_policy
                    || !matches!(
                        ctx.sandbox.network_mode,
                        base::context::config::NetworkModeConfig::Unrestricted
                    ));
            for (rule_name, rule) in RULES {
                let outcome =
                    decide(&permission(name, rule), &tool, input, cwd, mode, unattended).await;
                for hook_approved in [false, true] {
                    let cell = Cell {
                        mode,
                        rule,
                        unattended,
                        hook_approved,
                        confined,
                    };
                    out.push(Compared {
                        at: format!(
                            "{name} {input} {mode_name} {rule_name} unattended={unattended} \
                             hook={hook_approved}: stated {stated:?}, bridged {old:?}"
                        ),
                        cell,
                        stated: conclusion(&stated, cell),
                        old: conclusion(&old, cell),
                        gate: (!refused_before_facts)
                            .then(|| after_runtime(&outcome, hook_approved, unattended)),
                    });
                }
            }
        }
    }
    out
}

/// A durable cron job outlives the session and acts on its own, which is the
/// one thing a tool's facts make asked about that its old answer let through.
fn leaves_something_behind(name: &str, input: &Value) -> bool {
    name == "CronCreate" && input["durable"] == json!(true)
}

/// The gate denies a coordination tool under `dontAsk` when an `ask` rule
/// matches, where `conclusion` spares it and asks. The facts are not what
/// differs: `TaskMessage` is declared the same either way.
fn gate_differs_from_the_model(name: &str, c: &Compared) -> bool {
    name == "TaskMessage"
        && c.cell.mode == PermissionMode::DontAsk
        && matches!(c.cell.rule, RuleCase::Ask | RuleCase::ManagedAsk)
}

/// Tools that state their own facts: what those lead to must be what the facts
/// bridged from their old answers lead to, in every cell. Where the gate does
/// not refuse a sample before reading the tool, both must also be what the
/// gate concludes, so the comparison is about the decisions and not only about
/// itself. Returns how many cells were compared, and how many of those
/// against the gate.
async fn assert_facts_conclude_as_bridged(names: &[&str]) -> (usize, usize) {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().canonicalize().unwrap();
    let (mut compared, mut against_the_gate) = (0usize, 0usize);
    for name in names {
        for sample in samples(name, &cwd, "/home/matrix") {
            if leaves_something_behind(name, &sample.input) {
                continue;
            }
            for c in compare_stated_with_bridged(name, &sample.input, &cwd).await {
                assert_eq!(c.stated, c.old, "{}", c.at);
                compared += 1;
                if let Some(gate) = c.gate.filter(|_| !gate_differs_from_the_model(name, &c)) {
                    assert_eq!(c.old, gate, "{}", c.at);
                    against_the_gate += 1;
                }
            }
        }
    }
    (compared, against_the_gate)
}

#[tokio::test]
async fn bash_and_monitor_facts_conclude_as_their_bridged_ones() {
    let (compared, against_the_gate) = assert_facts_conclude_as_bridged(&["Bash", "Monitor"]).await;
    assert_eq!(compared, (12 + 4) * 5 * 2 * 5 * 2);
    assert_eq!(against_the_gate, (9 + 4) * 5 * 2 * 5 * 2);
}

#[tokio::test]
async fn file_tools_facts_conclude_as_their_bridged_ones() {
    let (compared, against_the_gate) = assert_facts_conclude_as_bridged(&[
        "Read",
        "Write",
        "Edit",
        "NotebookEdit",
        "Grep",
        "Glob",
    ])
    .await;
    assert_eq!(compared, (3 + 4 + 2 + 2 + 2 + 2) * 5 * 2 * 5 * 2);
    assert!(against_the_gate > 0);
}

#[tokio::test]
async fn the_remaining_tools_facts_conclude_as_their_bridged_ones() {
    let (compared, against_the_gate) = assert_facts_conclude_as_bridged(&[
        "WebFetch",
        "Ping",
        "WebSearch",
        "CronCreate",
        "CronDelete",
        "CronList",
        "EnterWorktree",
        "ExitWorktree",
        "EnterPlanMode",
        "ExitPlanMode",
        "AskUserQuestion",
        "GoalCreate",
        "GoalGet",
        "GoalUpdate",
        "PushNotification",
        "Sleep",
        "StructuredOutput",
        "TaskMessage",
        "TaskOutput",
        "TaskStop",
        "TaskWait",
        "TodoWrite",
        "ToolSearch",
    ])
    .await;
    // 25 samples, less the durable cron one.
    assert_eq!(compared, 24 * 5 * 2 * 5 * 2);
    assert!(against_the_gate > 0);
}

/// F-8: a durable cron job is `Persists`, so it is asked about where its old
/// answer let it run. Every other cell is concluded as before.
#[tokio::test]
async fn a_durable_cron_job_is_asked_about_where_it_used_to_run() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().canonicalize().unwrap();
    let durable = json!({ "cron": "0 9 * * *", "prompt": "report", "durable": true });
    let cells = compare_stated_with_bridged("CronCreate", &durable, &cwd).await;
    let mut changed = 0usize;
    for c in &cells {
        assert_eq!(Some(c.old), c.gate, "{}", c.at);
        if c.stated == c.old {
            continue;
        }
        changed += 1;
        assert_eq!(c.old, "Permit", "{}", c.at);
        assert_eq!(
            c.stated,
            if c.cell.unattended { "Deny" } else { "Prompt" },
            "{}",
            c.at
        );
        assert!(!c.cell.hook_approved, "{}", c.at);
        assert_ne!(c.cell.mode, PermissionMode::BypassPermissions, "{}", c.at);
        assert!(c.cell.rule != RuleCase::Allow, "{}", c.at);
    }
    assert!(changed > 0);
    let attended_default = cells
        .iter()
        .find(|c| {
            c.cell.mode == PermissionMode::Default
                && c.cell.rule == RuleCase::None
                && !c.cell.unattended
                && !c.cell.hook_approved
        })
        .unwrap();
    assert_eq!(
        (attended_default.old, attended_default.stated),
        ("Permit", "Prompt")
    );
}
