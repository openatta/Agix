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

Both are the same engine.

---

## Why Agix

**One engine for a rack of servers and a board on the shelf.** Agix is built
to be deployed by the hundred and embedded where every megabyte counts: a
single small binary, a predictable footprint, and a loop that keeps working
when no one is watching.

1. **Small enough to run anywhere.** `agixd` is one 11 MB binary with no
   runtime to install. It idles at about 10 MB of memory and starts in about
   30 ms. A real task — four sub-agents in parallel plus a program, 91 model
   calls over six minutes — peaked under 30 MB and used about 8 s of CPU (measured
   on an M2 Max, `release` build). Size the async workers to the hardware
   with `--worker-threads` — one is enough on a small board — and leave out
   whole modules at build time with the build tiers.
2. **Many sessions, one process.** A daemon serves dozens of sessions across
   projects and scenes, over a Unix socket, TCP or WebSocket. Idle sessions
   are evicted and come back from disk on demand. A crash is recorded,
   leftover commands are cleaned up on the next start, and the daemon runs
   under systemd or launchd like any other service.
3. **Safe to leave alone.** Mark a scene unattended and nothing ever waits on
   a human: what would need approval is refused with a reason, writes stay
   inside the roots you gave it, and `Bash` runs only inside the OS sandbox
   (Seatbelt on macOS, bubblewrap on Linux). Authority comes from settings,
   never from task text, so injected content cannot grant itself anything.
4. **More work per token.** With `RunCode` the model drives tools from a
   short program — in parallel, in loops, across sub-agents whose answers
   are checked against a JSON Schema — and only the result comes back. Goals
   keep a session going round after round until the work is done. The fixed
   prompt prefix is about 5.8K tokens, shared across sessions by the
   provider's cache, and long sessions compact themselves and carry on. On
   six engineering tasks, against another open agent harness on the same
   model, that came to **34% less wall time and 13% fewer prompt tokens, with
   every run correct** ([how it was measured](tests/live/tests/compare_dsh.rs)).
5. **Your product stays in charge.** Every session, task, sub-agent, goal and
   prompt is visible, stoppable and editable over one JSON-RPC protocol.
   45 extension points let you replace or intercept any subsystem from Rust
   or a QuickJS script without forking. Every session is an
   append-only log you can resume, fork, export or replay byte for byte
   with no network.

---

## Features

**Deployment**
- `agixd`: a daemon speaking JSON-RPC 2.0 (NDJSON) over a Unix socket, TCP
  or WebSocket, with token authentication off the local socket.
- Or embed the engine: one `Agent` from the builder, or the daemon's whole
  session pool in your own process.
- `release` builds an 11 MB binary; `release-fast` trades twice the size for
  a little speed and unwinding on panic.
- Build tiers (`minimal`, `coding`, `full`) and per-module features: MCP,
  sub-agents, skills, web tools, structured output, `RunCode`, WebSocket,
  scripts, plugins, the sandbox, OpenTelemetry.
- `--worker-threads` / `AGIX_WORKER_THREADS`; session cap with least-recently-used
  eviction; idle timeout; several scenes per process.
- Discovery files, so clients find a daemon by scene. Clean shutdown on
  SIGTERM, crash records, and process-group cleanup after a hard kill.

**Models**
- Anthropic and OpenAI-compatible APIs, so hosted and local models
  (llama.cpp, Ollama, vLLM, …) both work.
- Per-task routing (`main`, `subagent`, `compact`, `memory`, `web_search`,
  `web_fetch`),
  per-session providers, and model switching mid-session.
- Retry with backoff, plus recovery: switch model, compact and retry, or
  raise `max_tokens`.
- A cache-friendly prompt: per-session facts ride in the first message,
  rarely used tools are loaded on demand with `ToolSearch`, and compaction
  emits cache edits.

**Autonomy**
- Goals that run automatic rounds until done, blocked or out of budget.
- Cron-style schedules, plan mode with verification, and todo lists.
- Background shell commands, monitors, sub-agents and programs, each
  reporting back with a notification.
- `RunCode`: a JavaScript program (embedded QuickJS) that calls the
  session's tools, sub-agents included.

**Multi-agent**
- The `Agent` tool with typed agents: foreground or background, with their
  own models and tools, and results validated against a JSON Schema.
- Teams with a mailbox and shared memory.
- Sub-agents run in sidechain sessions linked to their parent.
- Concurrency and depth limits, and permissions, hooks and budgets that
  follow delegation.

**Tools** — 45 built in: files, shell, web, tasks, planning, goals,
scheduling, git worktrees, skills, teams, MCP, questions, notifications,
monitors and structured output ([table below](#tools--45-built-in)).

**Context**
- An LLM summary first, then cheaper fallbacks, behind a predictive
  trigger, a circuit breaker and a configurable threshold.
- Session memory with recall.
- A skill catalog.
- Token and output budgets.

**Safety**
- A fixed permission order: the tool's own check, then rules (a deny always
  wins), then path safety, then the mode.
- Modes `default`, `acceptEdits`, `plan`, `dontAsk` and `bypassPermissions`.
- An OS sandbox for shell commands, with git hooks, git config and agent
  settings read-only inside it; the engine's own git runs there too.
- Unattended scenes that refuse instead of asking.
- Permission prompts that time out closed.
- Worktree isolation.
- Workspace trust: a project the user has not trusted gets plugin-level
  authority — its own settings can only tighten, and its hooks, MCP servers,
  skill commands and grants, looser agent types, and the settings that pick
  where data goes (providers, endpoints, the instruction file) wait for trust
  (granted in the global tier or over RPC; trusted by default; the release that flips this is announced separately,
  [ARCHITECTURE.md](docs/ARCHITECTURE.md) §6.6).

**Host control**
- Seventy RPC methods: create, resume, fork, rebuild and export sessions;
  read history and the exact context of the next model call; answer
  permission and question prompts.
- Start, message, stop and follow tasks, reading their output by byte offset.
- Switch the model or prompt; manage scenes, MCP servers, plugins,
  schedules and which projects are trusted.

**Extensibility**
- 45 published extension points: 30 contracts, 9 interceptions and 6
  registrations.
- Rust plugins compiled into the host, and QuickJS scripts, installable as
  packages with install-time disclosure.
- Hooks at thirty moments, with command, prompt, HTTP and agent backends
  ([`docs/hooks.md`](docs/hooks.md)).
- An MCP client (stdio, SSE, streamable HTTP, WebSocket), declarative scenes,
  and custom agent types.
- MCP authorization (the 2025-11-25 spec, OAuth 2.1): discovery, dynamic
  client registration, PKCE, refresh; sign-in driven by the host over RPC with
  a loopback or host-owned redirect; client credentials and enterprise-managed
  authorization for unattended servers; tokens in 0600 files or the host's own
  store ([`docs/mcp_auth.md`](docs/mcp_auth.md)).
- An enterprise MCP policy: allow and deny lists by server name, command or
  URL, read from a system path users cannot write (or `--mcp-policy`); an
  unreadable policy denies every server
  ([ARCHITECTURE.md](docs/ARCHITECTURE.md) §6.4).
- Managed settings for the deployment, read from a system path users cannot
  write (or `--managed-settings`): their deny and ask rules hold even under
  `bypassPermissions`, and they can switch that mode off; an unreadable file
  refuses every tool call ([ARCHITECTURE.md](docs/ARCHITECTURE.md) §6.1).

**Records**
- An append-only JSONL history that can be resumed, forked or rebuilt after
  a crash.
- 21 telemetry event types, with OpenTelemetry export
  ([`docs/telemetry.md`](docs/telemetry.md)).
- A recorder for byte-exact offline replay.
- Session export as a zip archive or a directory, sub-agents included.

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
              │                plugin packages, the module list; the JSON-RPC server and discovery
              │
 modules    agents (+team) · mcp · skills · script-host · plugin
              │                enter a session through runtime::module::Module
 kernel     runtime ────────── turn loop, prompt assembly, tool dispatch, the Module interface
              │
            model · scene · history · session · compaction · telemetry · hooks · permissions · tools
              │
 machine    platform ───────── this machine's execution providers, crash hook, process identity
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
  sub-agents and teams, MCP, skills, the script carrier and plugin packages. Only the
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

`crates/runtime/src/turn/` is the main loop. **The order of the steps is the
skeleton and is fixed; the decision at each step is open.**

```
user message ─ UserPromptSubmit hook · slash commands · memory recall · pending notices
loop:
  1. stop?           TurnPolicy::before_model_call
  2. compact?        Compactor::should_compact
  3. hard cap        ContextBudget::hard_cap
  4. prompt          PromptAssembler · registered blocks · assembly interceptors
  5. model call      Model + ModelInterceptor + BackoffPolicy
                     ├─ tools run as the reply streams: Permission → ToolMiddleware → Tool → ToolResultTransformer
                     └─ on error: RecoveryPolicy — switch model / compact and retry / fail
  6. review          TurnPolicy::after_model_call
  7. account         BudgetPolicy::on_usage
  8. tool calls      deliver messages and notices that arrived during the step, then loop
  9. otherwise       RecoveryPolicy::on_early_stop — raise max_tokens / continue a pause_turn;
                     BudgetPolicy::on_output_target decides whether to continue
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
outside a Rust build:

| Route | Reaches | Cost |
|---|---|---|
| **Scripts** (QuickJS, default) | nine points: four that write the prompt, two around a tool call, two around a model call, memory recall | a `.js` file and one line of config; microseconds per call |
| **Plugin packages** | MCP servers, scripts, a declarative scene, agent types | a zip with a `plugin.toml`; everything it will show the model is disclosed at install |
| **Hooks** | Thirty named moments in a turn (`HookEvent` (30)), four backends: command, prompt, HTTP, agent | observe, rewrite an input, block a tool call, end a turn |

And anything that speaks MCP is already an extension. Scripts run in the same
embedded QuickJS as `RunCode`: their limits bound cost, not hostile code, so
what a script may do follows from where it came from. There is no
WebAssembly runtime; a host that wants one builds it on the Rust interfaces.

### Build tiers

| tier | what it assembles |
|---|---|
| `minimal` (`--no-default-features`) | the kernel only: models, the turn loop, scenes, sessions, telemetry, hooks, file and shell tools |
| `coding` | + MCP, sub-agents and teams, skills, the web tools, structured output, programmatic tool calling (`RunCode`) |
| `full` (default, with `scripts`, `plugin-packages` and `sandbox`) | + the WebSocket listener |

`daemon.ping` reports the modules a build carries; a method belonging to one it
does not carry answers `METHOD_NOT_FOUND` naming the module.

### Crate map

| Layer | Crate | Responsibility |
|---|---|---|
| contract | `core` (`base`) | traits, settings, messages, prompt types, the extension catalog |
| machine | `platform` | the local process, filesystem, network and OS sandbox behind the execution contracts |
| kernel | `runtime` | turn loop, streaming tool executor, `Builder`, `Module` |
| kernel | `model` | Anthropic and OpenAI-compatible adapters, retry and backoff |
| kernel | `tools` | built-in tools (`web` and `structured-output` are features) |
| kernel | `scene` | built-in scenes, `SceneRegistry`, declarative scenes |
| kernel | `history` · `session` · `compaction` | transcript log, session state, context compaction |
| kernel | `permissions` · `hooks` · `telemetry` | rule engine, lifecycle hooks, telemetry and the recorder |
| module | `agents` · `team` | the `Agent` tool, agent types, teams |
| module | `mcp` | MCP client and tool adapters, MCP OAuth authorization |
| module | `skills` | skill catalog and the `Skill` tool |
| module | `script-host` | the QuickJS carrier |
| kernel | `ptc` | `RunCode`: the model's programs, run in an embedded QuickJS ([`docs/ptc.md`](docs/ptc.md)) |
| module | `plugin` | plugin packages: manifest, install, cache, disclosure |
| assembly | `assembly` | session pool, configuration, provider routing, the JSON-RPC server |
| shell | `daemon` | the `agixd` binary |

20 crates under `crates/`, plus `daemon/`, the five test members and the
example plugin in `examples/plugins/rust` — 27 workspace members in total. The full picture — sessions and scenes, the history
log, execution, locks, disk layout — is in
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

---

## Quick start

### Run the daemon

```sh
git clone https://github.com/openatta/agix && cd agix
export ANTHROPIC_AUTH_TOKEN=sk-...        # or ANTHROPIC_API_KEY; ANTHROPIC_BASE_URL for a compatible endpoint
cargo run --release -p daemon --bin agixd # listens on ~/.agix/scenes/code/daemon.sock
```

Talk to it — one JSON object per line:

```sh
sock=$HOME/.agix/scenes/code/daemon.sock
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
(`session.subscribe`) and drives it from another. Seventy methods are
documented in [`docs/daemon_rpc_protocol.md`](docs/daemon_rpc_protocol.md) —
sessions, history and context, tasks and sub-agents, scenes, configuration,
project trust, MCP, plugins, scheduling — and every documented response shape is compared
against a real daemon's answer by a test.

Useful flags: `--scene` / `--scenes` (which scenes to serve), `--listen` /
`--listen-ws` with `--token` (TCP / WebSocket, authenticated), `--session-cap`,
`--worker-threads`, `--permission-prompt-timeout`, `--mcp-policy`, `--managed-settings`. Each running daemon writes a discovery file
under `~/.agix/daemon/instances.d/`, so a client finds it by scene instead of
by path.

### Embed the engine

As a library — one agent, driven directly:

```rust
let (mut agent, mut events, _input) = runtime::agent::Builder::new()
    .scene(Arc::new(scene::code::CodeScene) as Arc<dyn Scene>)
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
    &load_daemon_config("claude-sonnet-4-6", 8192, None, "code", &paths),
    Arc::new(scene::code::CodeScene),
    daemon::Assembly { scenes: vec![my_scene], ..Default::default() },
)
.await?;
```

`agixd` is this call plus a command line and its listeners; a test fails the
build if anything in the workspace assembles a pool a second way.

### Extend it

- A new behavior for one point → a script: a `.js` file and one line in
  `settings.json` ([`docs/extending_quickjs.md`](docs/extending_quickjs.md)).
- A packaged capability — MCP servers, scripts, a scene, agent types — → a
  plugin package ([`docs/plugin_packages.md`](docs/plugin_packages.md)).
- Tools, interceptors or prompt contributions in your own build → a Rust
  plugin (`runtime::plugin::Plugin`), added to the builder or to every session
  through `Assembly`. Where a Rust plugin and a script share a point, Rust runs
  first and the script has the last word.
  [`docs/plugin_development.md`](docs/plugin_development.md) walks through both
  kinds, with a working example of each in `examples/plugins/`.
- A different subsystem → implement its trait
  ([`docs/extension_points.md`](docs/extension_points.md) lists every one, with
  what it costs and who may use it).

---

## Capabilities

### Tools — 45 built in

| Category | Tools |
|---|---|
| **Filesystem** | `Read` `Write` `Edit` `Glob` `Grep` |
| **Shell** | `Bash` (sandboxed, path-safe, timeout-controlled) |
| **Web** | `WebFetch` `WebSearch` |
| **Task** (running sub-agents, shell commands, monitors) | `TaskOutput` `TaskWait` `TaskStop` `TaskMessage` |
| **Planning** | `EnterPlanMode` `ExitPlanMode` `TodoWrite` |
| **Goals** | `GoalCreate` `GoalGet` `GoalUpdate` (a goal keeps the session working across automatic rounds) |
| **Scheduling** | `CronCreate` `CronDelete` `CronList` |
| **Workspace** | `EnterWorktree` `ExitWorktree` (isolated git worktree per session) |
| **Collaboration** | `Skill` `Agent` `TeamCreate` `TeamList` `TeamDelete` |
| **Team members** (given only to a team's members) | `SendMessage` `ReadMail` `ListPeers` `TeamMemorySave` `TeamMemoryRead` |
| **Protocol** | `MCP` `ListMcpResources` `ReadMcpResource`, plus one `mcp__<server>__<tool>` per discovered MCP tool |
| **Interaction** | `AskUserQuestion` `PushNotification` `Monitor` `StructuredOutput` |
| **Programs** | `RunCode` (a JavaScript program that calls the session's tools; only its return value comes back) |
| **Editor / Meta / Diagnostics** | `NotebookEdit` · `ToolSearch` · `Ping` `Sleep` |

`tools::register_builtin_tools` installs the 21 self-contained ones; the rest
need per-session state and are registered by the builder or by a module.
Deferred tools are advertised by name and fetched with `ToolSearch` when needed.
The `code` scene leaves out `NotebookEdit`, the worktree tools, `Ping`, `Sleep`
and `PushNotification`; `StructuredOutput` is there only
when the host sets an output schema. A scene is interactive (someone answers
prompts) or unattended: `harnix` and `mini` are unattended, `code` is created
either way. An unattended session has no `AskUserQuestion` or plan mode, refuses
what would need approval instead of asking, and runs `Bash` only inside the OS
sandbox — see `docs/ARCHITECTURE.md` §3.2.3 and §6.3.

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

Task keys `main`, `subagent`, `compact`, `memory`, `web_search` and `web_fetch`
route each kind of call; a session can also choose its own provider. `WebSearch`
searches through the session's own model (the provider's built-in search) unless
`task_models.web_search` names another provider. `WebFetch` returns the page as
plain text (truncated at 50,000 characters); only when `task_models.web_fetch`
names a provider does that provider's model read the page and answer the call's
`prompt` instead. It never falls back to the session's own model.

### Context, telemetry, recording

- **Compaction**: an LLM summary first, then cheaper fallbacks (snip,
  micro-compact, collapse), aimed at what the threshold leaves beside the
  system prompt and tool definitions. Opt-in in the `compaction` settings: a
  predictive pass with a circuit breaker (`reactive`), and hourly clearing that
  emits cache edits so the prompt cache is not thrown away
  (`cached_microcompact`).
- **Telemetry**: 21 structured event types (`EventPayload` (21)) with
  OpenTelemetry export, four token counts per call, and the calls no turn asked
  for — compaction, memory, hooks, search — counted too.
- **Recorder**: wrap any `Model` and a session becomes a self-contained
  directory; replay is byte-exact and needs no network. A recording is not
  redacted — it holds the prompts, tool arguments and results as sent — so keep
  it the way you keep the transcript.

---

## Running `agixd`

| profile | command | on panic |
|---|---|---|
| `release` (the default) | `cargo build --release -p daemon --bin agixd` | aborts the process; about 11 MB |
| `release-fast` | `cargo build --profile release-fast -p daemon --bin agixd` | unwinds; a panic inside a task ends that task; twice the size, a little faster |

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
generated [JSON Schema](docs/schemas/settings.schema.json). Values are taken
literally: `$VAR` / `${VAR}` / `${VAR:-default}` are expanded only inside an
MCP server's `command`, `args`, `env`, `url` and `headers`.

A few switches whose effect the rest of this README does not mention:

| Setting | Default | Effect |
|---|---|---|
| `model.fallback_model` | none | the model a call is retried on when the main one stays overloaded (529) |
| `memory_enabled` | `true` | file-based memory: the memory prompt, the index in each turn's reminder, recall |
| `telemetry_enabled` | `true` | one telemetry JSONL file per session under the state directory's `telemetry/` |
| `telemetry_url` | none | OTLP endpoint (else `OTEL_EXPORTER_OTLP_ENDPOINT`); only in a build with the `otel` feature |
| `instruction_file` | none | one more instruction file, read when the session is built and given to the model like `AGENTS.md` |
| `hooks_config` | none | lifecycle hooks: event name → list of hooks ([docs/extension_points.md](docs/extension_points.md)) |
| `disable_skill_shell_execution` | `false` | a skill body's `` !`cmd` `` is replaced by a notice instead of run |
| `execution.team_enabled` | `false` | gives `TeamCreate` / `TeamList` / `TeamDelete` to sessions whose scene supports teams (`code`, `harnix`) |
| `execution.team_member_idle_timeout_secs` | `1800` | a team member idle this long is stopped |
| `sandbox.network_mode` | `unrestricted` | outbound network of sandboxed commands: `unrestricted`, `deny_all`, or `allowlist` (`sandbox.allowed_domains`) |
| `session_dir` | — | not read; transcripts always go under the state directory |

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
cargo test  --workspace                                      # everything offline
cargo test  -p live --features real-model --test real_model  # the live tier, against a real model
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
| [docs/hooks.md](docs/hooks.md) | the hook protocol: configuration, the four backends, exit codes, timeouts, payloads, answers, and when each event fires |
| [docs/telemetry.md](docs/telemetry.md) | the telemetry files, envelope and redaction, and every event type with its fields |
| [docs/daemon_rpc_protocol.md](docs/daemon_rpc_protocol.md) | the JSON-RPC reference — methods, events, error codes, auth |
| [docs/plugin_development.md](docs/plugin_development.md) | writing Rust and QuickJS plugins, with the examples in `examples/plugins/` |
| [docs/extending_quickjs.md](docs/extending_quickjs.md) | writing script extensions |
| [docs/plugin_packages.md](docs/plugin_packages.md) | the plugin package format, install and trust |
| [docs/ptc.md](docs/ptc.md) | `RunCode`: what a model's program can call, its permissions, limits and prompt |
| [docs/mcp_auth.md](docs/mcp_auth.md) | OAuth for remote MCP servers: configuration, sign-in flows, RPCs and events, credential storage, security |
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
