//! The catalog's `Builder` wiring, checked against running sessions: for each
//! point in `PROBES` a session is built through the `Builder` method the
//! catalog names, runs its warmup and one turn, and the implementation it was
//! given is shown to have been used. Every point the catalog says a `Builder`
//! method wires is either probed here or listed in `NOT_YET_PROBED`, so a
//! setter that stores a value nothing reads cannot claim a point unnoticed.
//!
//! What a delegate inherits is the session's `HostContracts`; the last case
//! shows the injected contracts are what the modules are handed, and the
//! agents crate shows a delegate runs on what it is handed
//! (`a_delegate_runs_on_its_parents_host_contracts`).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use base::engine_exec::ENGINE_GIT_OPTIONS;
use base::event::AgentEvent;
use base::interface::catalog::{self, Wiring};
use base::interface::exec::in_process::{
    InMemoryFileSystem, NoSandbox, OfflineNetwork, ScriptedProcess, ScriptedRun,
};
use base::interface::exec::{
    Confined, DirEntry, ExecError, ExecProviders, FileSystem, HttpRequest, HttpResponse, Metadata,
    Network, Origin, ProcessSpec, Sandbox, SandboxPolicy,
};
use base::interface::instruction_provider::RuleProvider;
use base::interface::model::{
    Model, ModelContentBlock, ModelError, ModelEvent, ModelMessage, ModelStream, StreamParams,
    ToolDef,
};
use base::interface::token_counter::TokenCounter;
use base::prompt::PromptBlock;
use base::rules::RuleEntry;
use base::settings::{LoadedSettings, PathSettings, Settings};
use runtime::agent::{Builder, HostContracts, InputMessage};
use runtime::module::{Module, ModuleError, SessionContext};
use tokio_util::sync::CancellationToken;

const MODEL_ENDPOINT: &str = "http://model.invalid/";
const RULE_PATH: &str = "/srv/rules/probe.md";

struct Probe {
    id: &'static str,
    builder: &'static str,
    install: fn(Builder, &Probes) -> Builder,
    used: fn(&Probes, &Ran) -> Result<(), String>,
}

const PROBES: &[Probe] = &[
    Probe {
        id: "exec.process",
        builder: "exec",
        install: |b, p| b.exec(p.exec.clone()),
        used: |p, _| {
            let asked = p.process.seen();
            asked
                .iter()
                .any(|argv| argv == &git("rev-parse --is-inside-work-tree"))
                .then_some(())
                .ok_or(format!("the engine's git never reached it: {asked:?}"))
        },
    },
    Probe {
        id: "exec.filesystem",
        builder: "exec",
        install: |b, p| b.exec(p.exec.clone()),
        used: |p, ran| {
            let dot_git = ran.cwd.join(".git");
            let reads = p.seen.reads.lock().unwrap().clone();
            if !reads.contains(&dot_git) {
                return Err(format!(
                    "{} was never read through it: {reads:?}",
                    dot_git.display()
                ));
            }
            let bound = ran
                .bound
                .as_ref()
                .ok_or("the session state has no providers")?;
            Arc::ptr_eq(&bound.filesystem, &p.exec.filesystem)
                .then_some(())
                .ok_or("the session state holds another filesystem".to_string())
        },
    },
    Probe {
        id: "exec.sandbox",
        builder: "exec",
        install: |b, p| b.exec(p.exec.clone()),
        used: |p, _| {
            let confined = p.seen.confined.lock().unwrap().clone();
            confined
                .iter()
                .any(|program| program == "git")
                .then_some(())
                .ok_or(format!(
                    "the engine's git was never confined by it: {confined:?}"
                ))
        },
    },
    Probe {
        id: "exec.network",
        builder: "exec",
        install: |b, p| b.exec(p.exec.clone()),
        used: |p, _| {
            let requests = p.seen.requests.lock().unwrap().clone();
            requests
                .iter()
                .any(|(url, origin)| url == MODEL_ENDPOINT && *origin == Origin::Operator)
                .then_some(())
                .ok_or(format!("the warmup request never reached it: {requests:?}"))
        },
    },
    Probe {
        id: "token.count",
        builder: "token_counter",
        install: |b, p| b.token_counter(p.counter.clone()),
        used: |p, _| {
            (p.seen.counted.load(Ordering::SeqCst) > 0)
                .then_some(())
                .ok_or("the turn never asked it to count a conversation".to_string())
        },
    },
    Probe {
        id: "rules.source",
        builder: "rule_provider",
        install: |b, p| b.rule_provider(p.rules.clone()),
        used: |p, ran| {
            if p.seen.rules_asked.load(Ordering::SeqCst) == 0 {
                return Err("the prompt was assembled without asking it".to_string());
            }
            let shown = format!("`{RULE_PATH}`");
            ran.prompts
                .iter()
                .any(|block| block.contains(&shown))
                .then_some(())
                .ok_or(format!(
                    "the model was not shown its rule: {:?}",
                    ran.prompts
                ))
        },
    },
];

/// Points a `Builder` method wires that no probe above runs yet.
const NOT_YET_PROBED: &[&str] = &[
    "tool.registry",
    "tool.around",
    "tool.result",
    "prompt.block",
    "prompt.context",
    "prompt.variable",
    "prompt.assembler",
    "prompt.assemble",
    "prompt.submit",
    "event.sink",
    "health.check",
    "elicitation.ask",
    "permission.check",
    "permission.grants",
    "scene",
    "model",
    "model.request",
    "model.message",
    "history.store",
    "memory.retriever",
    "memory.retrieval_hook",
    "instruction.source",
    "turn.policy",
    "model.recovery",
    "budget",
    "environment",
    "compaction",
    "script.carrier",
    "hooks",
];

fn git(args: &str) -> String {
    format!("git {} {args}", ENGINE_GIT_OPTIONS.join(" "))
}

#[derive(Default)]
struct Seen {
    reads: Mutex<Vec<PathBuf>>,
    requests: Mutex<Vec<(String, Origin)>>,
    confined: Mutex<Vec<String>>,
    counted: AtomicUsize,
    rules_asked: AtomicUsize,
}

/// One recorder per contract, fresh for each session.
struct Probes {
    seen: Arc<Seen>,
    process: ScriptedProcess,
    exec: ExecProviders,
    counter: Arc<dyn TokenCounter>,
    rules: Arc<dyn RuleProvider>,
}

impl Probes {
    fn new() -> Self {
        let seen = Arc::new(Seen::default());
        let process = ScriptedProcess::new().with(
            git("rev-parse --is-inside-work-tree"),
            ScriptedRun::ok("true\n"),
        );
        let exec = ExecProviders {
            process: Arc::new(process.clone()),
            filesystem: Arc::new(RecordingFs(seen.clone(), InMemoryFileSystem::new())),
            network: Arc::new(RecordingNetwork(seen.clone(), OfflineNetwork::new())),
            sandbox: Arc::new(RecordingSandbox(seen.clone())),
        };
        Probes {
            counter: Arc::new(CountingCounter(seen.clone())),
            rules: Arc::new(CountingRules(seen.clone())),
            seen,
            process,
            exec,
        }
    }
}

struct RecordingFs(Arc<Seen>, InMemoryFileSystem);

#[async_trait]
impl FileSystem for RecordingFs {
    async fn read(&self, path: &Path) -> Result<Vec<u8>, ExecError> {
        self.0.reads.lock().unwrap().push(path.to_path_buf());
        self.1.read(path).await
    }
    async fn write(&self, path: &Path, bytes: &[u8]) -> Result<(), ExecError> {
        self.1.write(path, bytes).await
    }
    async fn create_dir_all(&self, path: &Path) -> Result<(), ExecError> {
        self.1.create_dir_all(path).await
    }
    async fn read_dir(&self, path: &Path) -> Result<Vec<DirEntry>, ExecError> {
        self.1.read_dir(path).await
    }
    async fn remove_dir_all(&self, path: &Path) -> Result<(), ExecError> {
        self.1.remove_dir_all(path).await
    }
    async fn metadata(&self, path: &Path) -> Result<Metadata, ExecError> {
        self.1.metadata(path).await
    }
    async fn canonicalize(&self, path: &Path) -> Result<PathBuf, ExecError> {
        self.1.canonicalize(path).await
    }
}

struct RecordingNetwork(Arc<Seen>, OfflineNetwork);

#[async_trait]
impl Network for RecordingNetwork {
    async fn send(&self, req: HttpRequest, origin: Origin) -> Result<HttpResponse, ExecError> {
        self.0
            .requests
            .lock()
            .unwrap()
            .push((req.url.clone(), origin));
        self.1.send(req, origin).await
    }
    fn permits(&self, host: &str, origin: Origin) -> bool {
        self.1.permits(host, origin)
    }
}

struct RecordingSandbox(Arc<Seen>);

impl Sandbox for RecordingSandbox {
    fn confine(&self, spec: ProcessSpec, policy: &SandboxPolicy) -> Confined {
        self.0.confined.lock().unwrap().push(spec.program.clone());
        NoSandbox.confine(spec, policy)
    }
}

struct CountingCounter(Arc<Seen>);

impl TokenCounter for CountingCounter {
    fn count_text(&self, text: &str) -> usize {
        text.len()
    }
    fn count_block(&self, _: &ModelContentBlock) -> usize {
        1
    }
    fn count_messages(&self, messages: &[ModelMessage]) -> usize {
        self.0.counted.fetch_add(1, Ordering::SeqCst);
        messages.len()
    }
}

struct CountingRules(Arc<Seen>);

impl RuleProvider for CountingRules {
    fn id(&self) -> &str {
        "probe"
    }
    fn rules(&self) -> Vec<RuleEntry> {
        self.0.rules_asked.fetch_add(1, Ordering::SeqCst);
        vec![RuleEntry {
            name: "probe".into(),
            description: "the probe's rule".into(),
            path: RULE_PATH.into(),
        }]
    }
}

/// Answers at once, keeping the system prompt it was sent.
struct KeepsPrompt(Arc<Mutex<Vec<String>>>);

#[async_trait]
impl Model for KeepsPrompt {
    fn api_type(&self) -> base::provider::ApiType {
        base::provider::ApiType::Anthropic
    }
    async fn stream(
        &self,
        prompt_blocks: Vec<PromptBlock>,
        _tools: Vec<ToolDef>,
        _messages: Vec<ModelMessage>,
        _params: StreamParams,
        _cancel: CancellationToken,
    ) -> Result<ModelStream, ModelError> {
        self.0
            .lock()
            .unwrap()
            .extend(prompt_blocks.into_iter().map(|b| b.content));
        let events: Vec<Result<ModelEvent, ModelError>> = vec![
            Ok(ModelEvent::TextDelta { text: "ok".into() }),
            Ok(ModelEvent::EndTurn {
                stop_reason: "end_turn".into(),
                usage: Default::default(),
            }),
        ];
        Ok(Box::new(futures::stream::iter(events)))
    }
}

fn settings(dir: &Path) -> LoadedSettings {
    let mut values = Settings::defaults_for("test-model");
    values.model.base_url = MODEL_ENDPOINT.into();
    values.memory_enabled = false;
    values.telemetry_enabled = false;
    LoadedSettings::new(
        values,
        PathSettings {
            user_data_dir: dir.join("user"),
            global_data_dir: dir.join("global"),
            workspace: base::settings::Workspace::project_of(dir),
            local_data_dir: dir.to_path_buf(),
            scope: "code".into(),
        },
    )
}

fn builder(dir: &Path, prompts: Arc<Mutex<Vec<String>>>) -> Builder {
    Builder::new()
        .scene(Arc::new(scene::code::spec(
            scene::code::Interaction::Interactive,
        )))
        .model(Arc::new(KeepsPrompt(prompts)))
        .settings(Arc::new(settings(dir)))
}

/// What a session left behind for the probes to look at.
struct Ran {
    cwd: PathBuf,
    bound: Option<ExecProviders>,
    prompts: Vec<String>,
}

/// Warmup and one turn, through `Agent::run` as a host drives it.
async fn run_session(install: fn(Builder, &Probes) -> Builder, probes: &Probes) -> Ran {
    let dir = tempfile::tempdir().unwrap();
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let (mut agent, mut events, input) = install(builder(dir.path(), prompts.clone()), probes)
        .build()
        .expect("the session builds");
    let cwd = agent.settings().paths.working_dir().to_path_buf();
    let bound = agent.session_state().exec();
    let session = CancellationToken::new();
    let engine = tokio::spawn({
        let session = session.clone();
        async move { agent.run(session).await }
    });
    input
        .send(InputMessage::User {
            content: "go".into(),
            attachments: vec![],
            turn_id: "t1".into(),
        })
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        while let Some(event) = events.recv().await {
            if matches!(event, AgentEvent::TurnComplete { .. }) {
                return;
            }
        }
        panic!("the session ended before its turn completed");
    })
    .await
    .expect("the turn completes");
    session.cancel();
    drop(input);
    engine.await.unwrap();
    let prompts = prompts.lock().unwrap().clone();
    Ran {
        cwd,
        bound,
        prompts,
    }
}

fn builder_wired() -> BTreeSet<&'static str> {
    catalog::all()
        .iter()
        .filter(|p| p.wired_by.iter().any(|w| matches!(w, Wiring::Builder(_))))
        .map(|p| p.id)
        .collect()
}

#[test]
fn every_point_a_builder_method_wires_is_probed_or_listed() {
    let probed: BTreeSet<&str> = PROBES.iter().map(|p| p.id).collect();
    assert_eq!(probed.len(), PROBES.len(), "a point is probed twice");
    let not_yet: BTreeSet<&str> = NOT_YET_PROBED.iter().copied().collect();
    assert_eq!(
        not_yet.len(),
        NOT_YET_PROBED.len(),
        "NOT_YET_PROBED repeats a point"
    );
    let both: Vec<&&str> = probed.intersection(&not_yet).collect();
    assert!(
        both.is_empty(),
        "probed and still in NOT_YET_PROBED: {both:?}"
    );
    let covered: BTreeSet<&str> = probed.union(&not_yet).copied().collect();
    assert_eq!(
        covered,
        builder_wired(),
        "the catalog's `Wiring::Builder` points are not the probed ones plus NOT_YET_PROBED"
    );
}

#[test]
fn each_probe_calls_the_method_the_catalog_names() {
    for probe in PROBES {
        let point = catalog::find(probe.id)
            .unwrap_or_else(|| panic!("`{}` is not in the catalog", probe.id));
        assert!(
            point.wired_by.contains(&Wiring::Builder(probe.builder)),
            "the catalog does not say `Builder::{}` wires `{}`: {:?}",
            probe.builder,
            probe.id,
            point.wired_by
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_builder_point_is_used_by_the_session_it_was_given_to() {
    let mut unused = Vec::new();
    for probe in PROBES {
        let probes = Probes::new();
        let ran = run_session(probe.install, &probes).await;
        if let Err(why) = (probe.used)(&probes, &ran) {
            unused.push(format!(
                "{} (`Builder::{}`): {why}",
                probe.id, probe.builder
            ));
        }
    }
    assert!(
        unused.is_empty(),
        "given through the Builder and never used:\n  {}",
        unused.join("\n  ")
    );
}

/// Captures what the session hands its modules, which is what a delegate is
/// built on.
struct HandedToModules(Mutex<Option<HostContracts>>);

impl Module for HandedToModules {
    fn id(&self) -> &'static str {
        "handed-to-modules"
    }
    fn on_session(&self, cx: &mut SessionContext<'_>) -> Result<(), ModuleError> {
        *self.0.lock().unwrap() = Some(cx.host.clone());
        Ok(())
    }
}

#[test]
fn a_delegate_is_built_on_the_contracts_the_builder_was_given() {
    let dir = tempfile::tempdir().unwrap();
    let probes = Probes::new();
    let module = Arc::new(HandedToModules(Mutex::new(None)));
    let _session = builder(dir.path(), Arc::default())
        .exec(probes.exec.clone())
        .token_counter(probes.counter.clone())
        .rule_provider(probes.rules.clone())
        .module(module.clone())
        .skip_warmup(true)
        .build()
        .expect("the session builds");
    let host = module.0.lock().unwrap().take().expect("the module was run");
    assert!(Arc::ptr_eq(&host.exec.process, &probes.exec.process));
    assert!(Arc::ptr_eq(&host.exec.filesystem, &probes.exec.filesystem));
    assert!(Arc::ptr_eq(&host.exec.network, &probes.exec.network));
    assert!(Arc::ptr_eq(&host.exec.sandbox, &probes.exec.sandbox));
    assert!(Arc::ptr_eq(&host.token_counter, &probes.counter));
    assert!(Arc::ptr_eq(&host.rules, &probes.rules));
}
