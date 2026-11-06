# Agix

[![CI](https://github.com/openatta/agix/actions/workflows/ci.yml/badge.svg)](https://github.com/openatta/agix/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/openatta/agix?display_name=tag&sort=semver)](https://github.com/openatta/agix/releases)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](#license)
[![Rust](https://img.shields.io/badge/rust-1.80%2B-orange.svg)](#build-and-test)

**中文版:[docs/README.zh-CN.md](docs/README.zh-CN.md)**

**An agent engine you build products on, not an assistant you talk to.**

---

## Purpose

Agix is the engine underneath an agent product: it runs a *session* — assembles
the prompt, calls the model, executes tools, keeps the context within budget,
writes the history, delegates to sub-agents — and leaves every decision along
the way open to the product built on top of it. There is no user interface
here; that is the host's job.

It is meant for people who build on it:

- **A Rust application** links the engine as a library and drives an `Agent`
  directly.
- **Anything else** — an IDE plugin, a web front end, an orchestrator in another
  language — runs `agixd`, the reference host in this repository, and speaks
  JSON-RPC to it over a Unix socket, TCP or WebSocket.

Both are the same engine. What makes it worth building on:

- **Every decision is a seam.** 45 extension points in a published catalog —
  replace a subsystem, contribute to a collection, or intercept something in
  flight — reachable from Rust, from a script, or from a WebAssembly plugin.
- **An extension that fails costs its own contribution and nothing else.** A
  script that throws or times out leaves its point exactly as it found it.
- **Policy follows delegation.** Permissions, hooks and budgets apply to
  sub-agents, team members and background tasks the same way they apply to the
  session that started them.
- **The host sees and steers what runs.** Tasks, sub-agents, prompts, the
  context the next call will carry — all inspectable, stoppable and editable
  over the same protocol.
- **Nothing is claimed that is not checked.** The protocol reference, the
  extension catalog, the settings schema and this file's numbers are compared
  against the code by tests.

---

## Architecture

### Layers

A Rust workspace in five layers. Dependencies point downward only, and a test
(`daemon/tests/dependency_direction.rs`) fails the build if one points the
other way.

```
 shell      daemon ─────────── agixd: command line, process lifecycle (lock, signals, crash cleanup)
              │
 assembly   assembly ───────── session pool, (project, scene) bindings, provider routing,
              │                carriers, the module list; the JSON-RPC server and discovery
              │
 modules    agents (+team) · mcp · skills · script-host · plugin / plugin-host / wasm-host
              │                enter a session through runtime::module::Module
 kernel     runtime ────────── turn loop, prompt assembly, tool dispatch, the Module interface
              │
            model · scene · history · session · compaction · telemetry · hooks · permissions · tools
              │
 contract   core (crate `base`) — every trait and every shared data type
```

- **Contract.** `core` defines the traits (`Model`, `Scene`, `Permission`, `Tool`,
  `HistoryStore`, `Compactor`, …) and the data types, and depends on no other
  crate in the workspace. Replacing a subsystem means implementing a trait from
  `core`, never reaching into another crate's internals.
- **Kernel.** Everything a session needs to run a turn: the turn loop
  (`runtime`), the model adapters (Anthropic Messages and OpenAI-compatible Chat
  Completions), the built-in tools, scenes, the history log, compaction,
  permissions, hooks, telemetry. Kernel crates never depend on a module crate.
- **Modules.** Optional capabilities that plug into a session at build time:
  sub-agents and teams, MCP, skills, and the two extension carriers. Only the
  assembly layer and the shell know which modules exist.
- **Assembly.** Turns configuration into running sessions: merges the settings
  tiers, resolves provider routing, binds each session to its project and scene,
  pools sessions, and serves them over JSON-RPC. A host that wants the daemon's
  engine in its own process calls the same entry point `agixd` does.
- **Shell.** `agixd` itself: arguments, the instance lock, signals, and cleaning
  up after a crash.

### How a session is built

```rust
let (mut agent, events, input) = runtime::agent::Builder::new()
    .scene(scene)          // prompt, tool whitelist, budgets — Arc<dyn Scene>
    .model(model)          // any backend — Arc<dyn Model>
    .settings(settings)
    .module(Arc::new(mcp::McpModule::new(manager)))   // optional, in this order:
    .module(Arc::new(agents::AgentsModule::new()))    //   mcp → agents → skills
    .module(Arc::new(skills::SkillsModule::new()))
    .build()?;
```

`build()` narrows the module list by the scene, checks each module's declared
requirements, builds the session's own tool registry, event bus, permissions
and hooks, then lets each module contribute (`on_session`). Module tools are
subject to the same scene whitelist as built-in ones. Two things are
*late-bound* because what reads them is built before what provides them: the
agent spawner (provided by `agents`, used by agent-type hooks and forked skills)
and the skill catalog (provided by `skills`). A sub-agent is assembled from its
parent's narrowed module list, so nesting works the same at every depth.

### What a turn is

`crates/runtime/src/turn.rs` is the main loop. **The order of the steps is the
skeleton and is fixed; the decision at each step is open.**

```
user message ─ UserPromptSubmit hook · slash commands · memory recall · pending notices
loop:
  1. stop?           TurnPolicy::before_model_call
  2. compact?        Compactor::should_compact
  3. hard cap        ContextBudget::hard_cap
  4. prompt          PromptAssembler · registered blocks · assembly interceptors
  5. model call      Model + ModelInterceptor + BackoffPolicy
                     └─ on error: RecoveryPolicy — switch model / compact and retry / raise max_tokens / fail
  6. review          TurnPolicy::after_model_call
  7. account         BudgetPolicy::on_usage
  8. tools           Permission → ToolMiddleware → Tool → ToolResultTransformer
                     └─ then deliver messages and notices that arrived during the step
  9. more tool calls → loop; otherwise BudgetPolicy::on_output_target decides whether to continue
exit: finish_turn — persist, then announce TurnComplete
```

A turn has exactly one exit: whatever ended it — a hook, a budget, a
cancellation, an error — the transcript is written and `TurnComplete` announced
in one place, so a host can treat `TurnComplete` as "idle, and on disk".
Messages that arrive while a turn runs are delivered at step boundaries, never
between a `tool_use` and its `tool_result`.

### Sessions, scenes, history

- **Scenes** are stateless behavior: system prompt, tool whitelist, deferred
  tools, budgets, which modules may run. One process serves several; each
  session picks one and resolves its settings by `(project, scene)`. A scene can
  be Rust or declarative (`scene.json` + `prompt.md`).
- **History is a log.** A session's state is an append-only JSONL transcript
  that a projection turns into the messages the model sees. Extensions append
  their own entries to the same timeline; unknown ones are skipped, so
  uninstalling a plugin never breaks an old session. A session can be resumed,
  forked, or rebuilt from its log after a restart.
- **Sub-agents run in sidechain sessions** linked to their parent, listed under
  it and readable through the same API.

### Concurrency

Every agent — the session's own, each foreground or background sub-agent, each
team member — is its own task on a multi-threaded tokio runtime. Agents never
share mutable state; they talk through channels:

| Direction | Channel |
|---|---|
| parent → child | the child's inbox, read at its next step boundary (`TaskMessage`, `session.messageTask`) |
| child → parent | a `<task-notification>` posted when it ends, read at the parent's next step boundary |
| parent waits | `TaskWait`, `TaskOutput` |
| between members | team mailbox and shared team memory |

Limits live in one place each: delegation depth, concurrent sub-agents per
session tree (default 6), team stage concurrency. Exceeding one is refused with
an error the model can see, never queued.

### Execution and safety

Tools reach the machine through four contracts — `Process`, `FileSystem`,
`Network`, `Sandbox` — with a local provider by default and an in-process one
for tests. Every command leads its own process group and is stopped as a group.
Permissions run in a fixed order: the tool's own verdict, then rules (**a deny
always wins**, whichever tier it came from), path safety, then the permission
mode. A sub-agent's question reaches the host on its parent's session.

### The extension surface

The engine publishes a catalog of its seams, generated from the code: **45
extension points** — 30 *contracts* (replace a subsystem), 9 *interceptions* (sit
in the path of something in flight), 6 *registrations* (contribute to a
collection). From Rust, implement the trait and hand it to the builder. From
outside a Rust build, three carriers reach a subset:

| Carrier | Reaches | Cost |
|---|---|---|
| **Scripts** (QuickJS, default) | nine points: four that write the prompt, two around a tool call, two around a model call, memory recall | a `.js` file and one line of config; microseconds per call |
| **WebAssembly plugins** | five contribution points: tools, MCP servers, hook subscriptions, scenes, agent types | sandboxed components with declared capabilities and install-time disclosure |
| **Hooks** | Thirty named moments in a turn (`HookEvent` (30)), five backends: command, prompt, HTTP, agent, WASM | observe, rewrite an input, block a tool call, end a turn |

And anything that speaks MCP is already an extension. A build carries one
script/plugin carrier at most — `scripts` (default) or `plugins` — and the two
refuse to compile together.

### Build tiers

| tier | what it assembles |
|---|---|
| `minimal` (`--no-default-features`) | the kernel only: models, the turn loop, scenes, sessions, telemetry, hooks, file and shell tools |
| `coding` | + MCP, sub-agents and teams, skills, the web tools, structured output |
| `full` (default, with `scripts` and `plugin-packages`) | + the WebSocket listener |

`daemon.ping` reports the modules a build carries; a method belonging to one it
does not carry answers `METHOD_NOT_FOUND` naming the module.

### Crate map

| Layer | Crate | Responsibility |
|---|---|---|
| contract | `core` (`base`) | traits, settings, messages, prompt types, the extension catalog |
| kernel | `runtime` | turn loop, streaming tool executor, `Builder`, `Module` |
| kernel | `model` | Anthropic and OpenAI-compatible adapters, retry and backoff |
| kernel | `tools` | built-in tools (`web` and `structured-output` are features) |
| kernel | `scene` | built-in scenes, `SceneRegistry`, declarative scenes |
| kernel | `history` · `session` · `compaction` | transcript log, session state, context compaction |
| kernel | `permissions` · `hooks` · `telemetry` | rule engine, lifecycle hooks, telemetry and the recorder |
| kernel | `task` | background task lifecycle |
| module | `agents` · `team` | the `Agent` tool, agent types, teams |
| module | `mcp` · `auth` | MCP client and tool adapters, OAuth 2.0 PKCE |
| module | `skills` | skill catalog and the `Skill` tool |
| module | `script-host` | the QuickJS carrier |
| module | `plugin` · `plugin-host` · `wasm-host` · `plugin-compiler` | plugin packages and the WebAssembly carrier |
| assembly | `assembly` | session pool, configuration, provider routing, the JSON-RPC server |
| shell | `daemon` | the `agixd` binary |

23 crates under `crates/`, plus `daemon/` and the four test members — 28
workspace members in total. The full picture — sessions and scenes, the history
log, execution, locks, disk layout — is in
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

---

## Quick start

### Run the daemon

```sh
git clone https://github.com/openatta/agix && cd agix
export ANTHROPIC_AUTH_TOKEN=sk-...        # or ANTHROPIC_API_KEY; ANTHROPIC_BASE_URL for a compatible endpoint
cargo run --release -p daemon --bin agixd # listens on ~/.agix/scenes/coding/daemon.sock
```

Talk to it — one JSON object per line:

```sh
sock=$HOME/.agix/scenes/coding/daemon.sock
echo '{"jsonrpc":"2.0","id":1,"method":"session.create","params":{"project_root":"'$PWD'"}}' \
  | socat - UNIX-CONNECT:$sock
# → {"jsonrpc":"2.0","id":1,"result":{"session_id":"…",…}}

echo '{"jsonrpc":"2.0","id":2,"method":"session.run_turn",
       "params":{"session_id":"<id>","message":"List the crates in this workspace"}}' \
  | socat - UNIX-CONNECT:$sock
```

While the turn runs, `session.event` frames stream back (text deltas, tool
calls, tool results, prompts, task progress); the response arrives when the turn
ends. A typical client subscribes to a session on one connection
(`session.subscribe`) and drives it from another. Fifty-nine methods are
documented in [`docs/daemon_rpc_protocol.md`](docs/daemon_rpc_protocol.md) —
sessions, history and context, tasks and sub-agents, scenes, configuration,
MCP, plugins, scheduling — and every documented response shape is compared
against a real daemon's answer by a test.

Useful flags: `--scene` / `--scenes` (which scenes to serve), `--listen` /
`--listen-ws` with `--token` (TCP / WebSocket, authenticated), `--session-cap`,
`--permission-prompt-timeout`. Each running daemon writes a discovery file
under `~/.agix/daemon/instances.d/`, so a client finds it by scene instead of
by path.

### Embed the engine

As a library — one agent, driven directly:

```rust
let (mut agent, mut events, _input) = runtime::agent::Builder::new()
    .scene(Arc::new(scene::coding::CodingScene) as Arc<dyn Scene>)
    .model(Arc::new(model::adapter::AnthropicModel::new(client)))
    .settings(Arc::new(settings))
    .build()?;

tokio::spawn(async move {
    while let Some(event) = events.recv().await {
        if let AgentEvent::TextDelta { text, .. } = event { print!("{text}"); }
    }
});
agent.run_turn("List the files here".into(), "turn-1".into(), CancellationToken::new()).await?;
```

A scene, a model and settings are required; everything else has a working
default, and every default is a trait you can replace. The complete program is
[`crates/runtime/examples/minimal.rs`](crates/runtime/examples/minimal.rs):

```sh
ANTHROPIC_API_KEY=... cargo run --release -p runtime --example minimal -- "list the files here"
```

As a session pool — the daemon's engine in your own process, with the same
settings tiers, routing, history layout and modules, and your own scenes:

```rust
let pool = daemon::assemble::pool(
    &load_daemon_config("claude-sonnet-4-6", 8192, None, "coding", &paths),
    Arc::new(scene::coding::CodingScene),
    daemon::Assembly { scenes: vec![my_scene], ..Default::default() },
)
.await?;
```

`agixd` is this call plus a command line and its listeners; a test fails the
build if anything in the workspace assembles a pool a second way.

### Extend it

- A new behavior for one point → a script: a `.js` file and one line in
  `settings.json` ([`docs/extending_quickjs.md`](docs/extending_quickjs.md)).
- A packaged capability — tools, a scene, agent types — → a WebAssembly plugin
  ([`docs/extending_wasm.md`](docs/extending_wasm.md)).
- A different subsystem → implement its trait
  ([`docs/extension_points.md`](docs/extension_points.md) lists every one, with
  what it costs and who may use it).

---

## Capabilities

### Tools — 41 built in

| Category | Tools |
|---|---|
| **Filesystem** | `Read` `Write` `Edit` `Glob` `Grep` |
| **Shell** | `Bash` (sandboxed, path-safe, timeout-controlled) |
| **Web** | `WebFetch` `WebSearch` |
| **Task** | `TaskCreate` `TaskList` `TaskGet` `TaskUpdate` `TaskStop` `TaskOutput` `TaskWait` `TaskMessage` |
| **Planning** | `EnterPlanMode` `ExitPlanMode` `VerifyPlanExecution` `TodoWrite` |
| **Scheduling** | `CronCreate` `CronDelete` `CronList` |
| **Workspace** | `EnterWorktree` `ExitWorktree` (isolated git worktree per session) |
| **Collaboration** | `Skill` `Agent` `TeamCreate` `TeamList` `TeamDelete` |
| **Protocol** | `MCP` `ListMcpResources` `ReadMcpResource`, plus one `mcp__<server>__<tool>` per discovered MCP tool |
| **Interaction** | `AskUserQuestion` `PushNotification` `Monitor` `StructuredOutput` |
| **Editor / Meta / Diagnostics** | `NotebookEdit` · `ToolSearch` · `Ping` `Sleep` |

`tools::register_builtin_tools` installs the 23 self-contained ones; the rest
need per-session state and are registered by the builder or by a module.
Deferred tools are advertised by name and fetched with `ToolSearch` when needed.

### Model routing

```json
{
  "providers": {
    "anthropic": { "api_type": "anthropic", "default_model": "claude-sonnet-4-6" },
    "local":     { "api_type": "openai_compatible", "base_url": "http://localhost:11434",
                   "default_model": "qwen3" }
  },
  "default_provider": "anthropic",
  "task_models": { "subagent": "local", "compact": "local" }
}
```

Task keys `main`, `subagent`, `compact`, `memory` and `web_search` route each kind
of call; a session can also choose its own provider. `WebSearch` searches
through the session's own model (the provider's built-in search) unless
`task_models.web_search` names another provider.

### Context, telemetry, recording

- **Compaction**: an LLM summary first, then cheaper fallbacks (snip,
  micro-compact, collapse), behind a predictive trigger and a circuit breaker,
  emitting cache edits so compaction does not throw away the prompt cache.
- **Telemetry**: 37 structured event types (`EventPayload` (37)) with
  OpenTelemetry export, four token counts per call, and the calls no turn asked
  for — compaction, memory, hooks, search — counted too.
- **Recorder**: wrap any `Model` and a session becomes a self-contained
  directory; replay is byte-exact and needs no network.

---

## Running `agixd`

| profile | command | on panic |
|---|---|---|
| `release` | `cargo build --release -p daemon --bin agixd` | unwinds; a panic inside a task ends that task |
| `release-small` | `cargo build --profile release-small -p daemon --bin agixd` | aborts the process; about half the size |

Commands the agent runs live in their own process groups and outlive a crashed
daemon, so the daemon cleans up twice: its panic hook stops them before
aborting, and the next start of the same instance stops what a killed one
(`SIGKILL`, OOM) left behind. Crash records go to `~/.agix/daemon/crashes/`.
The daemon does not restart itself — run it under launchd or systemd with
restart on failure; under systemd use `KillMode=process`, so the daemon, not
systemd, decides which background commands to stop.

Configuration is layered, lowest priority first: built-in defaults →
`~/.agix/settings.json` → `~/.agix/scenes/<scene>/settings.json` →
`<project>/.agix/settings.json`, each with an optional `settings.local.json`;
`permission_rules` from every tier apply side by side. The full surface is the
generated [JSON Schema](docs/schemas/settings.schema.json).

| Variable | Purpose |
|---|---|
| `ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_API_KEY` | model credentials (or the host supplies its own credential source) |
| `ANTHROPIC_BASE_URL` | a compatible endpoint |
| `AGIX_DAEMON_TOKEN` | TCP / WebSocket handshake token |
| `AGIX_CONFIG_HOME` | global root, default `~/.agix` |

---

## Build and test

Rust 1.80+. The test suite needs no network and no key.

```sh
cargo build --workspace
cargo test  --workspace                                   # everything offline
cargo test  -p live --features real-model --test 'live_*' # the live tier, against a real model
```

Six layers, from unit tests to the assembled product: crate tests, a behavior
net that runs a real `Agent` against written-down model answers, `daemon/tests`
over a real socket, and `tests/live` — the product driven over JSON-RPC, once
against a scripted model server and once against a real model behind a
recording proxy, from the host's side as well as the model's. How each layer
works and how to run it: [`tests/README.md`](tests/README.md).

---

## Documentation

| Document | For |
|---|---|
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | how the engine is put together — layers, modules, the turn, sessions and scenes, history, execution, concurrency, disk layout |
| [docs/extension_points.md](docs/extension_points.md) | every seam: what you can replace, contribute to or intercept, what it costs, who may |
| [docs/daemon_rpc_protocol.md](docs/daemon_rpc_protocol.md) | the JSON-RPC reference — methods, events, error codes, auth |
| [docs/extending_quickjs.md](docs/extending_quickjs.md) | writing script extensions |
| [docs/extending_wasm.md](docs/extending_wasm.md) | writing WebAssembly plugins |
| [docs/migrating_to_0.3.md](docs/migrating_to_0.3.md) | upgrading an embedder or client from 0.2.x |
| [docs/schemas/settings.schema.json](docs/schemas/settings.schema.json) | the generated schema of `settings.json` |
| [tests/README.md](tests/README.md) | the test layers and the live tier |

## Design principles

1. **Library-first.** The daemon is a reference host, not the product.
2. **The engine owns mechanism; you own policy.** Every decision is behind a trait.
3. **Extensions cannot break the engine.** A failure costs its own contribution.
4. **Safe by default.** You opt into less safety, never into more by accident.
5. **Nothing is claimed that is not checked** — including by these documents.

## License

Apache-2.0
