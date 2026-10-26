# Agix

[![CI](https://github.com/openatta/agix/actions/workflows/ci.yml/badge.svg)](https://github.com/openatta/agix/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/openatta/agix?display_name=tag&sort=semver)](https://github.com/openatta/agix/releases)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](#license)
[![Rust](https://img.shields.io/badge/rust-1.80%2B-orange.svg)](#prerequisites)

**An agent engine you build products on, not an assistant you talk to.**

Agix is the runtime underneath a coding assistant: the turn loop, the tool
surface, permissions, context compaction, sub-agents, MCP, and the seams to
change any of it. Ship it as a Rust library inside your app, or run it as a
JSON-RPC daemon your IDE plugin talks to. Both are the same engine.

What makes it worth looking at is the last part — **44 extension points**, three
different ways to reach them, and a rule that runs through all of them: an
extension that fails costs its own contribution and nothing else.

---

## Sixty seconds

```sh
git clone https://github.com/openatta/agix && cd agix
export ANTHROPIC_AUTH_TOKEN=sk-...          # or ANTHROPIC_API_KEY
cargo run --release -p daemon               # listens on ~/.agix/scenes/coding/daemon.sock
```

```sh
echo '{"jsonrpc":"2.0","id":1,"method":"session.run_turn",
       "params":{"message":"Write a TCP echo server in Rust"}}' \
  | socat - UNIX-CONNECT:$HOME/.agix/scenes/coding/daemon.sock
```

Tokens stream back as `session.event` frames while the turn runs; the response
arrives when it ends. Forty-two methods are documented in
[`docs/daemon_rpc_protocol.md`](docs/daemon_rpc_protocol.md), and every
documented shape is compared against a real daemon's answer by a test — the
document cannot drift from the code.

Or skip the process boundary entirely:

```rust
let (mut agent, mut events, input) = runtime::agent::Builder::new()
    .scene(Arc::new(scene::scene::coding::CodingScene))
    .model(model)
    .settings(settings)
    .build()?;

tokio::spawn(async move { agent.run(cancel).await });

input.send(InputMessage::User {
    content: "Write a TCP echo server".into(),
    attachments: vec![],
    turn_id,
})?;
while let Some(event) = events.recv().await {
    match event {
        AgentEvent::TextDelta { text, .. } => print!("{text}"),
        AgentEvent::TurnComplete { .. } => break,
        _ => {}
    }
}
```

Three things are required — a scene, a model, settings. Everything else has a
working default, and every default is a trait you can replace.

Between the two, a host that wants the daemon's engine inside its own process
hands over a configuration and gets one back, assembled:

```rust
let pool = daemon::assemble::pool(
    &load_daemon_config("claude-sonnet-4-6", 2000, None, "coding", &paths),
    Arc::new(scene::scene::coding::CodingScene),
    daemon::Assembly::default(),
)
.await?;
```

Provider routing, credentials, the memory store, the history layout and its
migration, plugin components, MCP connection, session expiry — the order and
the defaults are the engine's knowledge, not yours. `agixd` is this call
plus a command line and its listeners, and a test fails the build if anything
in the workspace assembles a pool a second way.

---

## Why this one

| | |
|---|---|
| **Extend it without forking it** | 44 points in a published catalog, each with its cost and its trust rules. A `.js` file and one line of config reaches nine of them; a WebAssembly plugin reaches five; a Rust trait reaches all of them. |
| **A failing extension is a non-event** | Every adapter computes its change fully before applying any of it. A script that throws, times out, exhausts its quota or answers in the wrong shape leaves its point exactly as it found it — a prompt half-edited by something that died mid-pass is worse than an unedited one. |
| **And you can find out that it did** | Which makes "nothing happened" the failure everyone hits, and the four ways of doing nothing identical from inside the engine. Every call is written down and `session.get` reports it, so "why is my script not working" has an answer that is not a log line on somebody else's stderr. |
| **Policy survives delegation** | The rings around tool and model calls follow sub-agents, team members and background tasks. A rule that stopped at the first `Agent` call would be one model decision away from being bypassed. |
| **Context is handled, not hoped about** | Four compaction strategies behind a predictive trigger and a circuit breaker, with cache-aware edits so compaction does not invalidate the prompt cache it just paid for. |
| **Tools run while the model is still typing** | The streaming executor dispatches concurrency-safe tools during generation, with sibling abort on failure. |
| **Every call is recordable and replayable** | Wrap any `Model` in the recorder and a session becomes a self-contained directory — system blocks, tool table, messages, response down to token boundaries. Replay is byte-exact and needs no network. |
| **Safe by default, not by discipline** | Three-tier permissions, Unicode-normalized path safety, sandboxed execution, capability-gated plugins that default to nothing. You opt into less. |
| **One engine, two integrations** | Library mode or JSON-RPC daemon over Unix socket / TCP / WebSocket. Same turn loop underneath. |

---

## The extension surface

Most engines have a plugin API. This one publishes a **catalog**: every seam,
what it costs, and who is allowed to use it — generated from the code, so the
table cannot go stale. Three questions decide which shape you need:

| You want to… | You need | Example |
|---|---|---|
| **replace** how the engine does something | a **contract** — implement a trait, hand it to the builder | your own `Model`, `Permission`, `Compactor`, `HistoryStore` |
| **add** to what it already does | a **registration** — contribute something named, ordered, withdrawable | a prompt block, a tool, an MCP server, a scene |
| **see or change** something in flight | an **interception** — sit in the path | rewrite a tool result, refuse a call before dispatch, narrow a recall query |

And three carriers to reach them from outside a Rust build:

### Scripts — the cheap tier (default)

A `.js` file and one line of configuration. QuickJS is embedded in the daemon;
a call costs microseconds, no build step, no subprocess, no network.

```json
{ "scripts": [
  { "path": ".agix/scripts/house_style.js", "point": "prompt.assemble", "entry": "onAssemble" }
] }
```

**Nine points**: four that write the prompt (`prompt.assemble`, `prompt.block`,
`prompt.context`, `prompt.variable`), two around a tool call (`tool.around`,
`tool.result`), two around a model call (`model.request`, `model.message`), and
both ends of memory recall (`memory.retrieval_hook`).

What a script may do follows **where its file is**, not what its binding claims
— because a declaration is exactly what a script from outside would lie in. One
inside the project is the operator's own code and may rewrite the prompt; one
that arrived from elsewhere may add to it and no more, and a refused edit is
reported and counted rather than dropped, so "being held back" reads differently
from "doing nothing". Budgets are per binding, per turn: 100 ms and 1000 calls
by default, with the clock enforced inside the interpreter so a `while (true)`
stops too.

Every call a script makes is recorded — which point, which turn, and whether
the point took the answer, found nothing to take, never got one, or refused it
— and `session.get` reports it per session.

Full guide: [`docs/extending_quickjs.md`](docs/extending_quickjs.md).

### Plugins — the sandboxed tier

WebAssembly components (component model, WIT world `agix:plugin@0.1.0`), not
dynamic libraries and not scripts:

```wit
world plugin {
  import host;      // log, progress, now-ms, http-request, secret, kv-get, kv-set
  export tools;     // list-tools, call-tool
  export events;    // on-event — optional
}
```

- **Capabilities default to nothing.** A manifest declares what it needs; the
  host resolves the declaration into an actual capability set at load.
- **Install-time disclosure is not skippable.** Sandboxing governs what a plugin
  *executes* and does nothing about what it *says* — and text reaching the model
  is the one attack isolation cannot address. Every model-visible string is
  presented for review at install, under hard caps. Over the cap **refuses the
  install**; it does not warn and continue.
- **Five contribution points, and the number is the point**: tools, MCP servers,
  hook subscriptions, scenes, agent types. Adding a sixth is a decision to argue
  for.
- **Hook subscriptions are a whitelist of 6 events**, not the 30 available to
  local hooks, enforced when the manifest is parsed.
- **A plugin gets its own scene, never anyone else's.** Rewriting the prompt of a
  scene the user picked for other reasons is hijacking.
- **Faults cost one call.** A component that traps fails that invocation, not the
  process; three consecutive traps set it aside. Timeouts and cancellations do
  not count — those say something about the work.

Full guide: [`docs/extending_wasm.md`](docs/extending_wasm.md).

### Hooks — the lifecycle tier

Thirty named moments in a turn, five backends (command, prompt, HTTP, agent,
WASM). A hook can observe, rewrite an input, block a tool call, or end a turn,
depending on the event.

### And one more door

Anything that speaks MCP is already an extension. `bridges/agix-dsh-bridge` runs
an external JavaScript plugin as an MCP server, so a plugin written for a
framework this engine knows nothing about reaches the model through the same
path as any other server.

### The OS sandbox — optional, and off

Separate from the three above, and about something else: the carriers sandbox
*somebody else's code*, this constrains the shell commands **the model itself**
runs. `Bash` is its only consumer.

```sh
cargo build -p daemon --features sandbox     # macOS sandbox-exec / Linux bubblewrap
```

Off by default, deliberately. On a machine whose owner is sitting at it, the
permission system is the control surface and this is defence in depth; where an
agent acts for somebody else, it is a boundary worth compiling in. With the
feature on, a policy denies writes outside the working directory, re-denies
writes to `settings.json` even inside it — that file is where the permission
rules live — and refuses reads of the usual credential stores. Windows has no
backend.

The rule the design turns on: **a policy that asked for constraint must never
silently become an unconstrained run.** A backend reports how much it could
deliver, and `sandbox.require_enforcement` decides whether a shortfall refuses
the command or proceeds with it. A build without the feature reports the
shortfall the same way, naming the missing feature.

The credential deny-read list is not really sandboxing and stays either way:
the command classifier uses it to keep `cat ~/.ssh/id_rsa` from being treated
as a harmless read whose output goes to a model provider.

### One carrier per build — but packages are not a carrier

`scripts` and `plugins` are mutually exclusive features, and `scripts` is the
default. `plugin-packages` — reading, verifying, unpacking, disclosing and
managing a package — needs no runtime and is exclusive with neither, so it is
on by default too:

```sh
cargo build -p daemon                                                   # QuickJS + packages
cargo build -p daemon --no-default-features --features plugins          # WebAssembly + packages
cargo build -p daemon --no-default-features --features plugin-packages  # packages, no carrier
cargo build -p daemon --no-default-features                             # neither
```

Every configuration is checked on every push, including that the two carriers
still refuse to compile together and that the default build links no
WebAssembly. With plugins off, `PluginHost` is `None` and every call site is an
`if let Some(..)` that does nothing — no `#[cfg]` scattered through the engine,
and no behavior to reason about beyond "there are no components".

A package whose content is an MCP server or a script runs on the default build;
one carrying WebAssembly components still installs there, and its disclosure
says the components will not run.

The whole catalog, with costs and trust rules per point:
[`docs/extension_points.md`](docs/extension_points.md).

---

## Architecture

A strictly-layered Rust workspace. Dependencies flow upward only. No cycles.

```
                          ┌──────────────────────────┐
                          │     Your Application     │
                          │  IDE · CLI · GUI · Server│
                          └──────────┬───────────────┘
                                     │
                          ┌──────────▼───────────────┐
                          │  L5  plugin-host         │  ← optional, compile-time
                          │      plugin-compiler     │     removable
                          ├──────────────────────────┤
                          │  L4  runtime             │
                          │      Agent loop · Builder│
                          │      Streaming executor  │
                          ├──────────────────────────┤
                          │  L3  tools · skills      │
                          │      scene · team · task │
                          ├──────────────────────────┤
                          │  L2  model · history     │
                          │      permissions · mcp   │
                          │      compaction · session│
                          ├──────────────────────────┤
                          │  L1  core · wasm-host    │
                          │      script-host         │
                          │      traits · types · ID │
                          ├──────────────────────────┤
                          │  L0  auth · hooks        │
                          │      plugin · telemetry  │
                          └──────────────────────────┘
```

`plugin-host` sits *above* `runtime` because it wires plugin contributions into
the engine's registries — which is exactly why the whole tier drops out without
the layers below it noticing.

**L0 — cross-cutting** (no internal deps): `auth` (OAuth 2.0 PKCE), `hooks` (30
events, 5 backends), `plugin` (marketplace, dependency resolution, disclosure),
`telemetry` (37 event types, OpenTelemetry export, recorder).
**L1 — foundation**: `core` (every shared trait and type: `Model`, `AgentScene`,
`Permission`, `Tool`, `Id`, `EngineConfig`, `FrozenContext`), and the two
carriers — `wasm-host` and `script-host` — each depending on `core` and nothing
else, so a build that wants neither links neither.
**L2 — infrastructure**: `model` (Anthropic + OpenAI-compatible), `history`
(JSONL), `permissions`, `mcp`, `compaction`, `session`.
**L3 — domain**: `tools` (40), `skills`, `scene`, `team`, `task`.
**L4 — runtime**: the Agent, the Builder, the turn loop, the streaming executor,
sub-agent spawning, slash commands.
**L5 — plugins**: loader and ahead-of-time component compiler, both optional.

Concepts and how they fit: [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

---

## Capabilities

### Tools — 40 built in

| Category | Tools |
|---|---|
| **Filesystem** | `Read` `Write` `Edit` `Glob` `Grep` |
| **Shell** | `Bash` (sandboxed, path-safe, timeout-controlled) |
| **Web** | `WebFetch` `WebSearch` |
| **Task** | `TaskCreate` `TaskList` `TaskGet` `TaskUpdate` `TaskStop` `TaskOutput` |
| **Planning** | `EnterPlanMode` `ExitPlanMode` `VerifyPlanExecution` `TodoWrite` |
| **Scheduling** | `CronCreate` `CronDelete` `CronList` `ScheduleWakeup` |
| **Workspace** | `EnterWorktree` `ExitWorktree` (isolated git worktree per session) |
| **Collaboration** | `Skill` `Agent` `TeamCreate` `TeamList` `TeamDelete` |
| **Protocol** | `MCP` `ListMcpResources` `ReadMcpResource`, plus one `mcp__<server>__<tool>` per discovered MCP tool |
| **Interaction** | `AskUserQuestion` `PushNotification` `Monitor` `StructuredOutput` |
| **Editor / Meta / Diagnostics** | `NotebookEdit` · `ToolSearch` · `Ping` `Sleep` |

Registration is split on purpose. `tools::register_builtin_tools` installs the
24 self-contained ones; the rest need per-session state, so the builder
registers them into a registry it creates fresh for that session — `Agent` only
under the sub-agent depth limit, the MCP tools only when servers are configured,
`Team*` only when the setting **and** the scene's `supports_team()` both say yes.
A scene that does not do teamwork never shows the model tools it would then be
refused.

**Deferred tools** stay callable but are advertised by name and one line only;
the model pulls a full schema on demand with `ToolSearch`. On the coding scene
that is ~17k tokens of schemas that stop shipping on every call.

### Scenes — behavior is a trait, not a constant

System prompt, tool whitelist, token budget, execution limits live behind
[`AgentScene`](crates/core/src/interface/scene.rs). Four ship, and three of them
double as a ladder you can copy from:

| Scene | Depth |
|---|---|
| `CodingScene` | Full reference (~850 lines) — cache-optimized, multi-section prompt |
| `ChatScene` | Complete but compact — every method implemented, each with a "why" comment. **Copy this one.** |
| `DemoScene` | Minimal skeleton — only the 6 required methods; everything optional left at its default |
| `ResearchScene` | A working non-coding scene — evidence the trait is not coding-shaped |

One process runs several at once: `--scene` sets the default, `--scenes` activates
more, `scene.activate`/`scene.deactivate` change the set at runtime, and each
session picks one. A request that touches a session belonging to another scene is
refused with `SCENE_MISMATCH` rather than executed across the boundary.

### Model routing — per task, not per engine

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

Four task keys are wired — `main`, `subagent`, `compact`, `memory` — and anything
unrouted falls back to the default provider. A session can also pick its own
provider (`options.provider`, or `session.setModel` mid-session); its
sub-agents and compaction keep the routing above. A key need not sit in the
file: `api_key_env` names the variable to read it from, and an embedding host
can hand over its own credential source instead. Both `api_type` values have a real
protocol implementation; `openai_compatible` speaks
`POST <base_url>/v1/chat/completions`, which is what reaches OpenAI, vLLM, Ollama
and the gateways that only expose that shape. An unknown provider or a missing
`default_model` fails the daemon **at boot**, not mid-session. Routing is
inspectable at runtime via `daemon.doctor`, and editable without touching JSON via
`config.getProvider` / `config.setProvider` — both hot-reload the router.

### Context compaction

```
Budget warning (80%) → reactive trigger → micro-compact (cache-aware)
        ↓                                        ↓
  circuit breaker ← collapse (full) ← LLM summarize (cost-aware)
                                            ↓
                                    post-compact recovery
                     (re-inject files, skills, plan state, task summaries)
```

**Micro-compact** drops stale tool results while preserving the prompt cache.
**Collapse** merges consecutive blocks. **LLM summarize** delegates to a cheaper
model. **Reactive** predicts exhaustion from token velocity and triggers before
the wall. **Circuit breaker** detects compression loops and falls back. Edits are
emitted as `cache_edits` so compaction does not throw away the cache it just
paid for.

### Permissions

```
tool's own verdict (a deny stands; an allow waits)
        ↓
rules: deny > ask > allow, across every tier
        ↓
path safety (Unicode NFC/NFD normalization, system-directory blocklist)
        ↓
mode: dontAsk · plan · default · acceptEdits · auto / yolo (classifier) · bypass
```

**A deny is a deny.** Any matching deny refuses the call, then any ask asks,
and only then does an allow count. Which tier a rule came from and how specific
it is decide nothing — so an allow committed to a repository's
`.agix/settings.json`, contributed by a plugin or passed in over RPC cannot
reopen what the user's own settings deny. Every tier's `permission_rules` apply
side by side; none replaces another.

Rules match what a call actually touches. Paths are made absolute against the
session's directory, with `..` and `~` resolved on both sides; `*` stays inside
one path segment and `**` crosses them; a `Read(...)` rule covers `Grep` and
`Glob` too. A `Bash(...)` rule is matched against every simple command in the
line — quoting understood, `&&`, `;`, pipes and `$(...)` split out — so
`true; rm -rf build` does not slip past a deny on `rm:*`.

### Multi-agent

```
Coordinator → [Agent A] [Agent B] [Agent C]
     ↕            ↕         ↕         ↕
  Mailbox  ←  messages  →  Mailbox  ←→ Mailbox
     ↕
  Shared memory (files, YAML frontmatter, [[wikilinks]], staleness scoring)
```

Sub-agents spawn with type selection, optional git-worktree isolation and
background execution. What a delegate inherits from its parent is **one list** —
a source scan fails if a spawn site takes some of it and not the rest.

### MCP

Five transports: **stdio** (subprocess lifecycle, auto-restart), **SSE**,
**Streamable HTTP**, **WebSocket**, and **in-process** — the one that matters for
embedders: register your own `McpClient`, name it in `mcp_servers`, and its tools
reach the model over the same path as any external server with no IPC in between.
MCP tools adapt to the native `Tool` trait; servers can also register as skills.

### Telemetry and the recorder

37 structured event types over the whole lifecycle, with OpenTelemetry export.
Spending is reported at both scales: one `api_request` event per model call,
carrying that call's tokens, latency and the model it went to — which is what a
cost is priced from, and the only thing that can say where the money went when a
turn switches models partway — and the turn's total on `TurnOutcome` and on the
event a daemon client watches. A turn is several calls; the last one is not the
bill.

Two things make that total the one a host is actually billed for. **Four token
counts per call, not two**: a cache read costs a fraction of ordinary input and
a cache write a premium on it, so folding all three into one number cannot be
turned back into a price. And **the calls no turn asked for are counted too** —
memory extraction after a turn ends, compaction's summarizer, a `"type":
"prompt"` hook. Those are made from places that cannot reach a telemetry handle,
so the counting sits in the model they all share: any call carrying an auxiliary
origin is reported with a `purpose`, which a reader can sum in or tell apart.

And the recorder: wrap any `Model`, and every call is written to a
self-contained directory — the assembled system blocks (per block, not joined),
the full tool table, every message, the response down to token boundaries, and
failures recorded as failures so an overload that switched to the fallback model
replays as an overload. Replay matches the k-th live call to the k-th recorded
one, so a mismatch names the field that moved instead of reporting a hash miss.

---

## Embedding

### Daemon mode

For IDE plugins and multi-process setups. Unix socket, TCP, or WebSocket — the
transport changes framing and nothing else, which is checked by running one
exchange over all three.

- **Session pool** with capacity, LRU eviction and idle timeout
- **Discovery** via one JSON file per daemon under `~/.agix/daemon/instances.d/`,
  written at startup and removed on exit — a client reads the directory and
  picks by scene rather than reconstructing a socket path
- **Graceful shutdown** that lets in-flight turns finish
- **TCP and WebSocket require a `daemon.auth` handshake** (constant-time token
  comparison) as the first message on every connection; Unix sockets rely on
  filesystem permissions instead
- **Several clients, one session**: every connection watching a session sees the
  same stream, any of them can answer a permission prompt, and closing one
  changes nothing for the others

### Library mode

```rust
Builder::new()
    .scene(my_scene)              // Arc<dyn AgentScene>    — prompt, tools, budgets
    .model(my_model)              // Arc<dyn Model>         — any backend
    .permission(my_permission)    // Arc<dyn Permission>    — your authorization
    .compactor(my_compactor)      // Arc<dyn Compactor>     — your compaction
    .history_store(my_store)      // Arc<dyn HistoryStore>  — your persistence
    .plugin_host(my_host)         // Arc<dyn PluginHost>
    .task_router(my_router)       // Arc<TaskRouter>
    .tools(my_registry)           // Arc<dyn ToolRegistry>  — your own registry
    .hooks(my_hook_runner)        // Arc<HookRunner>        ← concrete
    .build()?;
```

Eight take a trait object, so an external crate can supply the implementation —
`ToolRegistry` among them, and it carries the whole surface a contributor needs:
`all`, `find`, `register`, `replace`, and removal by name, each registration
handing back something that withdraws exactly itself.

One does not: `.hooks(...)` takes the concrete runner — customization there
happens through hook *configuration* rather than by replacing the runner, though
the two executor traits it delegates to are injectable.

---

## Configuration

Layers, lowest priority first: built-in defaults and CLI arguments (`--model`,
`--max-tokens`, used only where no file sets them) → `$HOME/.agix/settings.json`
→ `$HOME/.agix/scenes/<scene>/settings.json` → `<project>/.agix/settings.json`.
Each tier may also carry an untracked `settings.local.json` on top of its own
file. Later layers override earlier ones key by key — except
`permission_rules`, which every layer adds to.

`$HOME/.agix` is the global root, flat and shared by every scene; each scene
gets its own subtree under `scenes/<scene>/` for the state that should not
cross between them.

The authoritative surface is the generated
[JSON Schema](docs/schemas/settings.schema.json) — it is regenerated from the
Rust types by a test, so it cannot describe a shape the code does not have.

| Variable | Purpose |
|---|---|
| `ANTHROPIC_AUTH_TOKEN` | **Required** unless `ANTHROPIC_API_KEY` is set or the embedding host supplies credentials — checked first, fails at boot if none is present |
| `ANTHROPIC_API_KEY` | Fallback for the above |
| `ANTHROPIC_BASE_URL` | Custom endpoint (proxies, compatible providers) |
| `AGIX_DAEMON_TOKEN` | TCP / WebSocket handshake token |
| `AGIX_CONFIG_HOME` | Global root (default `$HOME/.agix`); the scene's own root is `{it}/scenes/<scene>` |
| `AGIX_RECORD` / `AGIX_REPLAY` | Recorder mode, with `AGIX_RECORDINGS_DIR` |

**Identifiers** are BASE58(UUID v4) — 22 characters, URL-safe, one generation
path (`base::id::Id::new()`), a `#[sqlx(transparent)]` newtype over `[u8; 16]`
that maps to `TEXT` in Postgres and SQLite alike.

---

## Testing

Five layers, each answering a different question, none of them needing a
network or an API key — and all of it runs on every push.

| Layer | What it answers | How |
|---|---|---|
| `mod tests` in each crate | one function, one type | `cargo test -p <crate>` |
| `crates/*/tests` | does this crate keep its promise through its public seam | `cargo test` |
| `daemon/tests` | a real server, a real socket, real JSON-RPC | `cargo test -p daemon` |
| `tests/runner/tests` | a real Agent doing real work, with the model's answers written down | `cargo test -p test-runner` |
| `tests/daemon_harness/tests` | the assembled product: in through JSON-RPC, out through an HTTP provider — run twice, in-process and against a spawned `agixd` | `cargo test -p daemon-harness` |

Plus `tests/scripts/*.sh`, which asserts things about the build itself — that a
no-plugin build links no WebAssembly, for one.

Three things about it are worth stealing:

**The model is faked at three depths, and the depth is what each one buys.** A
scripted `Model` for "what does the engine decide given these answers" —
including the 529s and truncations no provider will produce on request. A
scripted HTTP client *below* the Anthropic adapter, so the tool loop, the
permission gate and the streaming frames are all really running and only the
network is not. And an HTTP server speaking Anthropic SSE, below that again,
which puts request serialization, SSE decoding and backoff back in the test —
and is the only one of the three a *separate process* can use, because
`ANTHROPIC_BASE_URL` crosses a process boundary where an `Arc<dyn
AnthropicClient>` cannot. That last one is why the same cases can run twice,
in-process and against a real spawned daemon; a case that passes in one mode
only has found a startup step being skipped rather than performed.

The recorder is deliberately not on that list. It is a product
capability — an operator records their own session to debug it — and this
repository records nothing and commits no recordings.

**Documents are tested.** The RPC method list, the response shapes in it, the
extension-point table, the settings schema — and this file's own counts, down to
whether every tool its table names still exists: each is compared against the
code by a test, so a document that drifts fails a build rather than misleading a
reader.

**Nothing in it needs a network or a key.** Every fake is a fake all the way
down and the answers are written down, so the whole suite runs anywhere in
seconds — which is what makes gating on it possible at all.

---

## Crate map

| Layer | Crate | Responsibility | Key exports |
|---|---|---|---|
| L0 | `auth` | OAuth 2.0 PKCE | `OAuth2Client`, `TokenStore`, `PkceVerifier` |
| L0 | `hooks` | Lifecycle hooks | `HookRunner`, `HookConfig` (5 backends), `HookEvent` (30) |
| L0 | `plugin` | Marketplace, resolution, disclosure | `PluginManifest`, `PluginResolver`, `Disclosure`, `Capabilities` |
| L0 | `telemetry` | Telemetry + recorder | `TelemetryHandle`, `EventPayload` (37), `RecorderModel` |
| L1 | `core` (`base`) | Shared traits and types | `Model`, `AgentScene`, `Permission`, `Tool`, `Id`, `EngineConfig` |
| L1 | `wasm-host` | WASM component runtime | `API_VERSION`, capability resolver, health tracking |
| L1 | `script-host` | QuickJS carrier | `QuickJsEngine`, `bindings::bind_into` / `bind_lenient` |
| L2 | `model` | Anthropic + OpenAI adapters | `AnthropicModel`, `OpenAICompatibleModel`, `ModelEvent` |
| L2 | `history` | JSONL persistence | `HistoryStore`, `JsonlHistoryStore`, `LogEntry` |
| L2 | `permissions` | Rule engine | `RuleSet`, `PermissionGate`, `LlmClassifier`, `WritePolicy` |
| L2 | `mcp` | MCP client | `McpManager`, `McpClient`, `McpToolAdapter`, `McpOAuthResolver` |
| L2 | `compaction` | Context compression | `Compactor`, `DefaultCompactor`, reactive/cached strategies |
| L2 | `session` | Session state | `SessionManager`, `SessionSummary` |
| L3 | `tools` | 40 built-in tools | `BashTool`, `FileReadTool`, `SearchProvider`, … |
| L3 | `skills` | Skill loader + watcher | `SkillManager`, `SkillWatcher`, `build_skills_from_mcp` |
| L3 | `scene` | Built-in scenes | `SceneRegistry`, `CodingScene`, `ChatScene`, `DemoScene`, `ResearchScene` |
| L3 | `team` | Multi-agent coordination | `Coordinator`, `TeamRegistry`, `Mailbox` |
| L3 | `task` | Background task lifecycle | `TaskStore`, `RunningTaskStore`, `DreamTask` |
| L4 | `runtime` | Agent runtime + turn loop | `Agent`, `Builder`, `PluginHost`, `CommandRegistry` |
| L5 | `plugin-host` | Loads plugins, wires contributions | `InstalledPlugins`, `PluginScene` |
| L5 | `plugin-compiler` | AOT component compile | `agix-plugin-compile` |
| — | `daemon` | JSON-RPC 2.0 server | `DaemonServer`, `SessionPool`, `run_doctor` |

21 crates under `crates/`, plus `daemon/` and the four test members — 26
workspace members in total.

---

## Build and test

### Prerequisites

Rust 1.80+, and an Anthropic-compatible API key for anything that talks to a
model. The test suite needs neither.

```sh
cargo build --workspace              # full build
cargo test  --workspace              # the whole suite: no network, no key
cargo test  -p daemon                # the daemon's own end-to-end suite
cargo test  -p base --features sandbox   # the OS sandbox backends
```

A clean build is ~13 GB, and that is not waste: every integration test file is a
separate binary statically linking the whole dependency graph. What *is* waste is
that cargo never reclaims the superseded copy — run `cargo clean` after a version
bump and `cargo sweep --time 7` weekly. `tests/scripts/disk_report.sh` says what
is reclaimable and why.

---

## Documentation

| Document | Audience |
|---|---|
| [ARCHITECTURE.md](docs/ARCHITECTURE.md) | **The concepts and how they fit** — crates, a turn's skeleton, sessions and scenes, history, execution, locks, disk layout |
| [extension_points.md](docs/extension_points.md) | **Every seam in the engine** — what you can replace, contribute to or intercept, what it costs, who may. Start here to build on Agix |
| [daemon_rpc_protocol.md](docs/daemon_rpc_protocol.md) | JSON-RPC reference — methods, fields, error codes, auth handshake. Start here to write a client |
| [extending_quickjs.md](docs/extending_quickjs.md) | Script extensions — the nine points, the API, the budgets, the failure rules |
| [extending_wasm.md](docs/extending_wasm.md) | WebAssembly plugins — manifest, capabilities, contribution points |
| [testing_scripts.md](docs/testing_scripts.md) | How the script carrier is tested, and what a case must satisfy to count |
| [tests/README.md](tests/README.md) | The five test layers, how to run each, why recordings are not committed |
| [schemas/settings.schema.json](docs/schemas/settings.schema.json) | Generated JSON Schema for `settings.json` |

There is no prose API reference for library mode yet; the entry points are
`runtime::agent::Builder` and the extension-point catalog, which lists every
trait the builder accepts with a minimal example for each.

## Design principles

1. **Library-first.** The daemon is a reference application, not the product.
2. **Trait injection.** The engine owns mechanism; you own policy.
3. **Extensions cannot break the engine.** A failure costs its own contribution.
4. **Safe by default.** You opt into less safety, never into more by accident.
5. **Nothing is claimed that is not checked** — including by these documents.

## License

Apache-2.0
