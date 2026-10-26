//! 内置 skills —— 15 个常用 prompt 模板。
//!
//! 不写盘、不需要用户设置；每 session 自动注入到 skill 列表。用户在
//! `~/.agix/skills/<name>/SKILL.md`（全局默认）、`~/.agix/scenes/<scene>/skills/<name>/SKILL.md`
//! （scene 覆盖）或 `<cwd>/.agents/skills/<name>/SKILL.md`（项目级）创建同名 skill 即覆盖
//! （disk 优先，因为 collect_skills 把 disk 后入但 dedup 不存在；同名时 `/<name>` slash 命中
//! 第一个 —— 即用户的）。
//!
//! # 技能列表
//!
//! | 名称 | 用途 |
//! |------|------|
//! | `simplify` | 审查代码变更，找复用 / 质量 / 效率问题并修复 |
//! | `verify` | 运行应用验证变更是否正常工作 |
//! | `debug` | 收集证据后诊断 bug / 失败测试 |
//! | `batch` | 规划机械性批量变更，在并行 worktree agent 中执行 |
//! | `stuck` | 多次失败后停下来记录已知信息并请求用户方向 |
//! | `loop` | 在固定/动态间隔重复执行任务（监视 build / 状态轮询） |
//! | `loremIpsum` | 生成指定结构的占位文本（原型 / fixture 用） |
//! | `remember` | 把事实 / 偏好 / 决策保存到跨会话 memory |
//! | `skillify` | 把刚跑过的工作流抽成可复用的 SKILL.md |
//! | `updateConfig` | 安全编辑 settings.json（permissions / hooks / mcp_servers） |
//! | `keybindings-help` | 展示 CLI/TUI 键盘快捷键参考 |
//! | `init` | 引导用户创建新项目的 AGENTS.md 文件 |
//! | `security-review` | 审查代码变更中的安全漏洞 |
//! | `rename` | 重命名当前对话会话标题 |
//! | `import` | 检测并从 Claude Code/Codex/Cursor 导入配置到 AGENTS.md/.agents/.agix |
//!
//! 还有几个我们刻意不带：它们要么依赖 Anthropic SaaS / Chrome 扩展，要么是内部子流程。
//!
//! 架构照搬自参考实现，prompt 文本重写以贴 agix 工具命名 + 不依赖 TS 特定能力。

use base::frozen::{SkillEntry, SkillSource};
use std::path::PathBuf;

/// 返回内置 skills（in-memory；不动 disk）。在 `load_session_skills` 之后
/// 追加，所以用户的同名 disk skill 仍然在前。
pub fn bundled_skills() -> Vec<SkillEntry> {
    vec![
        SkillEntry {
            name: "simplify".into(),
            description: "Review changed code for reuse, quality, and efficiency; then fix what's found.".into(),
            when_to_use: Some("After making non-trivial changes; before opening a PR.".into()),
            source: SkillSource::User, // 视为 user-level（用户级"自带"）
            path: PathBuf::from("(bundled:simplify)"),
            ..Default::default()
        },
        SkillEntry {
            name: "verify".into(),
            description: "Verify a change does what it should by running the app and exercising the new path.".into(),
            when_to_use: Some("After implementing a feature/fix; before reporting it as done.".into()),
            source: SkillSource::User,
            path: PathBuf::from("(bundled:verify)"),
            ..Default::default()
        },
        SkillEntry {
            name: "debug".into(),
            description: "Diagnose a bug or failing test by gathering evidence before forming a hypothesis.".into(),
            when_to_use: Some("When something is broken or behaving unexpectedly.".into()),
            source: SkillSource::User,
            path: PathBuf::from("(bundled:debug)"),
            ..Default::default()
        },
        SkillEntry {
            name: "batch".into(),
            description: "Plan a sweeping mechanical change (rename, refactor, migration) and run it in parallel worktree-isolated agents.".into(),
            when_to_use: Some("When the change is mechanical, decomposable, and affects many files.".into()),
            source: SkillSource::User,
            path: PathBuf::from("(bundled:batch)"),
            ..Default::default()
        },
        SkillEntry {
            name: "stuck".into(),
            description: "When you're not making progress: stop, write down what you know, what you've tried, what you'd try next, and ask the user for direction.".into(),
            when_to_use: Some("After 3+ failed attempts at the same goal; when going in circles.".into()),
            source: SkillSource::User,
            path: PathBuf::from("(bundled:stuck)"),
            ..Default::default()
        },
        // -3 **: 5 more skills added (skipping SaaS-bound ones).
        SkillEntry {
            name: "loop".into(),
            description: "Run a prompt or task on a recurring interval (poll status / monitor process)."
                .into(),
            when_to_use: Some(
                "When the user asks to keep running something every N minutes, watch a build, etc."
                    .into(),
            ),
            source: SkillSource::User,
            path: PathBuf::from("(bundled:loop)"),
            ..Default::default()
        },
        SkillEntry {
            name: "loremIpsum".into(),
            description: "Generate placeholder text of a requested length / structure for prototypes."
                .into(),
            when_to_use: Some("When prototyping UI / docs and any placeholder text will do.".into()),
            source: SkillSource::User,
            path: PathBuf::from("(bundled:loremIpsum)"),
            ..Default::default()
        },
        SkillEntry {
            name: "remember".into(),
            description: "Save a fact / preference / decision to cross-session memory (~/.agix/memory).".into(),
            when_to_use: Some(
                "When the user explicitly asks to remember something, or you learn a stable preference."
                    .into(),
            ),
            source: SkillSource::User,
            path: PathBuf::from("(bundled:remember)"),
            ..Default::default()
        },
        SkillEntry {
            name: "skillify".into(),
            description: "Turn a recently-executed workflow into a reusable SKILL.md."
                .into(),
            when_to_use: Some(
                "After a multi-step task that the user might run again — capture the playbook."
                    .into(),
            ),
            source: SkillSource::User,
            path: PathBuf::from("(bundled:skillify)"),
            ..Default::default()
        },
        SkillEntry {
            name: "updateConfig".into(),
            description: "Make a precise edit to settings.json (permissions / hooks / mcp_servers) with backup.".into(),
            when_to_use: Some(
                "When the user wants to add a permission rule, register a hook, or wire an MCP server."
                    .into(),
            ),
            source: SkillSource::User,
            path: PathBuf::from("(bundled:updateConfig)"),
            ..Default::default()
        },
        // -- Phase 4 additions: 4 more skills --
        SkillEntry {
            name: "keybindings-help".into(),
            description: "Show keyboard shortcuts and key bindings reference for the CLI/TUI.".into(),
            when_to_use: Some(
                "When the user asks about keyboard shortcuts, key bindings, or how to navigate the CLI/TUI."
                    .into(),
            ),
            source: SkillSource::User,
            path: PathBuf::from("(bundled:keybindings-help)"),
            ..Default::default()
        },
        SkillEntry {
            name: "init".into(),
            description: "Guide the user through initializing a new AGENTS.md file for a project.".into(),
            when_to_use: Some(
                "When starting a new project that lacks documentation, or when the user asks to create an AGENTS.md."
                    .into(),
            ),
            source: SkillSource::User,
            path: PathBuf::from("(bundled:init)"),
            ..Default::default()
        },
        SkillEntry {
            name: "security-review".into(),
            description: "Review pending code changes for security vulnerabilities and report findings.".into(),
            when_to_use: Some(
                "Before opening a PR or merging; when handling secrets, auth, crypto, or user-supplied data."
                    .into(),
            ),
            source: SkillSource::User,
            path: PathBuf::from("(bundled:security-review)"),
            ..Default::default()
        },
        SkillEntry {
            name: "rename".into(),
            description: "Rename the current conversation session with a descriptive title.".into(),
            when_to_use: Some(
                "When the user says 'rename this session' or 'give this conversation a title'."
                    .into(),
            ),
            source: SkillSource::User,
            path: PathBuf::from("(bundled:rename)"),
            ..Default::default()
        },
        SkillEntry {
            name: "import".into(),
            description: "Detect and import configuration from Claude Code, Codex, or Cursor into this project's AGENTS.md/.agents/.agix layout.".into(),
            when_to_use: Some(
                "When the user asks to import/migrate settings from Claude Code, Codex, or Cursor, or runs /import."
                    .into(),
            ),
            source: SkillSource::User,
            path: PathBuf::from("(bundled:import)"),
            ..Default::default()
        },
    ]
}

/// 把 `(bundled:<name>)` 路径形式解析回 prompt body。`SkillEntry::read_body` 在
/// 看到这种路径时调这里而不是读盘。
pub fn bundled_body(name: &str) -> Option<String> {
    let body = match name {
        "simplify" => SIMPLIFY,
        "verify" => VERIFY,
        "debug" => DEBUG,
        "batch" => BATCH,
        "stuck" => STUCK,
        "loop" => LOOP,
        "loremIpsum" => LOREM_IPSUM,
        "remember" => REMEMBER,
        "skillify" => SKILLIFY,
        "updateConfig" => UPDATE_CONFIG,
        "keybindings-help" => KEYBINDINGS_HELP,
        "init" => INIT,
        "security-review" => SECURITY_REVIEW,
        "rename" => RENAME,
        "import" => IMPORT,
        _ => return None,
    };
    Some(body.to_string())
}

const SIMPLIFY: &str = r#"# Simplify: Code Review and Cleanup

Review all changed files for reuse, quality, and efficiency. Fix any issues found.

## Phase 1: Identify Changes

Run `git diff` (or `git diff HEAD` if there are staged changes) to see what changed. If there are no git changes, review the most recently modified files that the user mentioned.

## Phase 2: Launch Three Review Agents in Parallel

Use the `Agent` tool to launch three agents concurrently in a single message — each in its own worktree (`worktree: "simplify-reuse"`, `"simplify-quality"`, `"simplify-efficiency"`). Pass each the full diff via the prompt so each has full context. Worktree isolation lets them run truly in parallel without stepping on each other.

### Agent 1: Code Reuse Review

For each change:
1. Search for existing utilities and helpers that could replace newly written code. Look for similar patterns in utility directories, shared modules, and files adjacent to the changed ones.
2. Flag any new function that duplicates existing functionality. Suggest the existing function to use instead.
3. Flag any inline logic that could use an existing utility (hand-rolled string manipulation, manual path handling, custom environment checks, ad-hoc type guards).

### Agent 2: Code Quality Review

Review for hacky patterns:
1. Redundant state — state that duplicates existing state, cached values that could be derived
2. Parameter sprawl — adding new parameters instead of generalizing or restructuring existing ones
3. Copy-paste with slight variation — near-duplicate blocks that should share an abstraction
4. Leaky abstractions — exposing internal details that should be encapsulated
5. Stringly-typed code — using raw strings where constants/enums already exist
6. Unnecessary comments — narrating WHAT instead of WHY; explaining the change or task

### Agent 3: Efficiency Review

Review for efficiency:
1. Unnecessary work — redundant computations, repeated file reads, duplicate API calls, N+1
2. Missed concurrency — independent ops run sequentially that could run in parallel
3. Hot-path bloat — new blocking work in startup / per-request paths
4. TOCTOU patterns — pre-checking existence before operating (operate and handle the error)
5. Memory — unbounded data structures, missing cleanup, listener leaks

## Phase 3: Fix

Wait for all three to complete. Aggregate findings, fix each directly. If a finding is a false positive, skip it — don't argue.
"#;

const VERIFY: &str = r#"# Verify: Confirm the Change Actually Works

Don't trust your own work. Run the change end-to-end and confirm the new behavior fires.

## Steps

1. **Identify the entry point** — what command / endpoint / UI action exercises the new code path?
2. **Run it** — use Bash to actually invoke. Don't assume it works because tests pass; tests miss things.
3. **Observe** — read stdout / logs / output files. Was the new behavior triggered? Is the output what the spec said?
4. **Adversarial check** — what's the obvious edge case? (empty input, missing file, slow network, concurrent caller). Run that case.
5. **Cleanup** — if you created test files, delete them.

## What to report

- Which command(s) you ran and their output (truncated).
- Whether the new path fired (cite a log line or output line).
- Any surprises — even if "fixed" by the change, mention them.
- If you couldn't actually run it (no test env, missing creds), say so explicitly. Don't pretend.
"#;

const DEBUG: &str = r#"# Debug: Evidence Before Hypothesis

When something is broken, resist the urge to immediately try fixes. Gather evidence first.

## Phase 1: Reproduce

1. Get the exact reproduction steps. If the user's report is vague, ask for the literal command they ran.
2. Run it yourself. Confirm you see the same failure.
3. If you can't reproduce, the bug isn't real until you can — say so.

## Phase 2: Read

Before changing any code:
1. Read the failing function and its callers.
2. Read related tests — what behavior is asserted?
3. Read recent git log on these files — what changed lately?
4. Look at the error message **literally**. What does each token mean? Don't paraphrase.

## Phase 3: Hypothesize

Now form a single specific hypothesis: "the bug is X because Y, evidence: Z." If you can't articulate X/Y/Z all three, you don't have a hypothesis — go back to Phase 2.

## Phase 4: Test the Hypothesis

The cheapest way is usually a print / log statement at the suspected point. Add it, re-run, check.

## Phase 5: Fix

Only after the hypothesis is confirmed: write the fix. Run the original repro again. Verify the fix doesn't break adjacent tests.

## Anti-patterns

- Trying random things ("maybe restart fixes it") — wastes time, hides root cause
- Adding try/catch to swallow errors — makes the bug invisible, not gone
- Reading documentation when the code is right there — read the code first
"#;

const BATCH: &str = r#"# Batch: Sweeping Parallel Changes

For mechanical changes that touch many files (renames, migrations, lint fixes), parallelize across worktree-isolated sub-agents.

## Phase 1: Plan

1. State the change in one sentence: "rename X to Y across the codebase".
2. Find all affected files: `rg --files-with-matches '<old>' --type rust` (or appropriate glob).
3. Group into independent units. Each unit = one worktree, one PR-ready slice. Aim for 5-15 units.
4. Confirm with the user the plan before launching.

## Phase 2: Launch

Use the `Agent` tool with `worktree: "<unit-slug>"` to spawn each sub-agent. Each gets:
- The exact files it owns
- The mechanical rule (find-replace pattern, AST edit, etc.)
- Instruction to run tests in its worktree before reporting
- Instruction to NOT push or commit — just leave the worktree clean

## Phase 3: Aggregate

Wait for all to finish (the parent turn pauses on each). Read each sub-agent's report.
- Conflicts? Triage manually.
- Test failures? Investigate per-unit.

## Phase 4: Land

Either:
- Open one big PR (use the `/pr` slash from your worktree-aggregated diff)
- Open per-unit PRs (chain `/pr` once per worktree)

## When NOT to use

If the change requires judgment (re-architecture, semantic refactor), don't batch — do it sequentially. Batching is for mechanical fan-out only.
"#;

const STUCK: &str = r#"# Stuck: Stop and Reset

You've tried 3+ approaches and nothing works. Don't try a 4th. Stop.

## What to do

1. **Stop coding.** Close any half-finished edits.
2. **Write down**:
   - What you're trying to do (one sentence)
   - What you've tried (each attempt + why it failed)
   - What you currently believe (the hypothesis you're operating under)
   - What you'd try next (and why)
3. **Show this to the user.** Ask if any of it sounds wrong, or if they have context you don't.

## Why this matters

Going in circles burns time and degrades context. The user almost always has information that changes the picture — but only if you stop and ask.

## Concrete output format

```
## What I'm trying to do
<one sentence>

## What I've tried
1. <attempt> — failed because <reason>
2. <attempt> — failed because <reason>
3. <attempt> — failed because <reason>

## Current hypothesis
<what I currently believe is wrong + evidence>

## What I'd try next
<one option> + why I think it'd help

## Question for you
<specific question that would unblock me>
```

Don't dress it up with confidence you don't have. The user needs to see the gaps.
"#;

const LOOP: &str = r#"# Loop: Recurring Task

Re-run a prompt or check on a fixed cadence (e.g. "watch this build", "every 5 min ping the deploy").

## How to apply

If the user gives an interval (`every 5 minutes`, `every hour`):

1. Confirm the cadence and what should happen each iteration.
2. After each run, schedule the next via the harness (`/loop` skill or `CronCreate` tool with the interval expression).
3. Each iteration should be **idempotent** — re-running shouldn't break state.
4. Report only when there's a state change (avoid spam).

If the user doesn't give an interval ("dynamic mode"):

- Use your judgement after each iteration on when to re-fire. Anchor to actual signals (build still running → 4-5 min; idle waiting → longer; quick check → shorter).
- Don't sleep in a tight poll loop. Pick a delay that reflects what you're waiting for.

## When to stop

- Loop the user explicitly cancelled
- The watched condition is no longer relevant
- 3+ consecutive iterations produce no useful new info → ask the user before continuing
"#;

const LOREM_IPSUM: &str = r#"# Lorem Ipsum: Placeholder Text Generator

Produce filler text of the requested shape (sentences / paragraphs / words; English or classical Latin lorem). For UI prototypes, demo fixtures, schema seed data, etc.

## Defaults

- Latin lorem ipsum unless the user asks for English-flavored placeholder
- Paragraphs default to ~50 words each
- Don't insert real-looking PII (no fake emails, names, addresses) unless asked — the goal is "obviously placeholder"

## Format options

- `length=words:N` → exactly N words
- `length=sentences:N` → exactly N sentences
- `length=paragraphs:N` → N paragraphs (default 50 words each)
- `style=english` → English-flavored placeholder
- `style=classical` → real Cicero passage from De Finibus (the original)

If unsure, ask: "How long, how many paragraphs, and English or Latin?" before generating.
"#;

const REMEMBER: &str = r#"# Remember: Save to Cross-Session Memory

Persist a fact / decision / preference to the per-project memory directory so future sessions see it.

## Where things go

The memory dir is `~/.agix/memory/<sha256(canonical_cwd)[..16]>/` (global, not scene-scoped) — already created and surfaced via the `# memory (cross-session)` system prompt block.

- **`MEMORY.md`** — one-line index. Each entry like:
  `- [Title](file.md) — short hook (under ~150 chars)`
- **`<topic>.md`** — the actual content. Frontmatter:
  ```
  ---
  name: descriptive name
  description: one line
  type: user | feedback | project | reference
  ---
  body
  ```

## When to apply

- User says "remember X" / "save this for later" → write file + index entry
- User corrects you in a way that should stick → save as `feedback`
- User shares a non-obvious fact about themselves / project → save as `user` or `project`

## What NOT to save

- Code patterns, file structure, git history (derivable from current state)
- One-off task details, ephemeral debugging steps
- Anything already in AGENTS.md
"#;

const SKILLIFY: &str = r#"# Skillify: Capture Workflow as a Skill

Turn the workflow you (or the user) just executed into a reusable `SKILL.md`.

## Output

Write `<cwd>/.agents/skills/<kebab-name>/SKILL.md` with frontmatter:

```
---
name: kebab-name
description: One-line, action-verb. (e.g. "Bisect a regression by running tests at midpoints.")
when_to_use: Concrete trigger ("when a test passes on main but fails on the current branch")
---
```

Body sections — keep terse:

1. **Goal** — one sentence
2. **Steps** — numbered, actionable
3. **Variables** — use `{ARGS}` placeholder for user-provided arg
4. **Example** — minimal worked example
5. **Failure modes** — common gotchas (only if non-obvious)

## Heuristics

- < 60 lines total. Linkable; not exhaustive.
- Steps describe *what to do*, not what was done. (Verbs in imperative.)
- If the workflow only ran once and might not repeat, **don't** skillify — rotate to memory or commit history.
"#;

const UPDATE_CONFIG: &str = r#"# UpdateConfig: Edit settings.json safely

Make a precise change to one of the settings.json layers without nuking adjacent fields.

## Pre-flight

1. Read the target file first. If absent, create with `{}` and proceed.
2. Identify which layer to change (low → high priority, higher wins):
   - Global (applies to every scene): `~/.agix/settings.json`
   - Scene-specific override: `~/.agix/scenes/<scene>/settings.json`
   - Project (committed): `<cwd>/.agix/settings.json`
   - Local override (gitignored): `<cwd>/.agix/settings.local.json`
3. Preserve `$schema` and any unknown fields verbatim — other tools may write fields we don't model.

## Common edits

- **Add permission rule**: append `{"tool": "Bash(git push:*)", "action": "allow"|"deny"|"ask"}` into the `permission_rules` array (don't replace whole array)
- **Register hook**: append into `hooks.<EventName>[]` (PreToolUse / PostToolUse / Notification / SubagentStop / etc.)
- **Wire MCP server**: add map entry under `mcp_servers.<name>` with `type` (`stdio` / `streamable_http`) + transport-specific fields

## Safety

- Always read → mutate parsed JSON in-place → write whole file. Don't string-edit.
- Verify by re-reading after write.
- If `schema_version` is present don't downgrade it.
"#;

const KEYBINDINGS_HELP: &str = r#"# Keybindings Help: CLI/TUI Keyboard Shortcuts

Reference for navigating the Claude Code / Agix CLI and TUI using keyboard shortcuts.

## Common Shortcuts

| Shortcut | Action |
|----------|--------|
| `Ctrl+C` | Cancel current operation / interrupt |
| `Ctrl+D` | End input / exit multi-line mode |
| `Ctrl+Z` | Suspend / background process |
| `Up` / `Ctrl+P` | Previous command from history |
| `Down` / `Ctrl+N` | Next command from history |
| `Ctrl+R` | Reverse search through command history |
| `Ctrl+A` | Move cursor to beginning of line |
| `Ctrl+E` | Move cursor to end of line |
| `Ctrl+U` | Delete text from cursor to beginning of line |
| `Ctrl+K` | Delete text from cursor to end of line |
| `Ctrl+W` | Delete word before cursor |
| `Ctrl+L` | Clear screen |
| `Tab` | Auto-complete (commands, file paths, slash commands) |
| `/?` | Open slash-command list |
| `Ctrl+T` | Open session / thread management |

## Multi-line Mode

| Shortcut | Action |
|----------|--------|
| `Alt+Enter` | Insert newline without submitting |
| `Shift+Enter` | Insert newline (same as Alt+Enter) |
| `Esc` then `Enter` | Submit the multi-line input |

## Prompt / Search

| Shortcut | Action |
|----------|--------|
| `Ctrl+F` | Toggle file-search mode |
| `Ctrl+G` | Search for symbol or file |
| `Ctrl+P` | Quick file picker (when not in history context) |

## Session Navigation

| Shortcut | Action |
|----------|--------|
| `Ctrl+[` / `Esc` | Escape from current context / menu |
| `Ctrl+N` | New session (when at top-level prompt) |
| `Ctrl+W` | Close current session (when at top-level) |

## Notes

- Key bindings can be customized via `~/.claude/keybindings.json` (Claude Code) or equivalent configuration.
- If a shortcut doesn't work, check whether a terminal emulator or tmux/screen is intercepting it first.
"#;

const INIT: &str = r#"# Init: New Project AGENTS.md

Guide the user through initializing a well-structured AGENTS.md (or equivalent project documentation) file for a new or existing project.

## When to use

- The working directory has no AGENTS.md, README.md, or CLAUDE.md yet
- The user explicitly asks "initialize this project" or "create documentation"
- The user starts a new codebase with no onboarding doc

## Process

1. **Scan the project** — list the top-level directory contents, check for config files (Cargo.toml, package.json, pyproject.toml, go.mod, Makefile, etc.), and identify the project type and language(s).

2. **Determine the file name** — check if the project convention calls for `AGENTS.md`, `CLAUDE.md`, or `README.md`. Default to `AGENTS.md` — it's the shared convention across Codex, this engine, and other agent tools.

3. **Gather key info**:
   - Project name and one-line purpose
   - Build system and key commands (`cargo build`, `npm test`, etc.)
   - Directory layout (top-level entries and what each does)
   - Any existing memory / preference files with user-defined instructions

4. **Generate** a file with these sections (omit what's not applicable):
   ```
   # Project Name

   One-paragraph description of what this project does.

   ## Quick Start

   ```sh
   git clone ...
   cd project
   make dev
   ```

   ## Commands

   | Command | Action |
   |---------|--------|
   | `make build` | Build the project |
   | `make test` | Run all tests |
   | `cargo run` | Start the dev server |

   ## Project Structure

   ```
   src/       — application source
   tests/     — integration tests
   docs/      — documentation
   scripts/   — utility scripts
   ```

   ## Tech Stack

   - Language / runtime
   - Frameworks
   - Database / services
   ```

5. **Write to disk** — use the appropriate tool to write the file. Confirm with the user before overwriting an existing file.

6. **Verify** — re-read the file to ensure it looks correct.

## Don't

- Don't overwrite a substantial existing file without user confirmation.
- Don't hallucinate commands or paths — only document what you can see.
- Don't include placeholder sections that you can't fill in (ask the user for missing info).
"#;

const SECURITY_REVIEW: &str = r#"# Security Review: Audit Pending Changes

Review the current uncommitted changes (and optionally the branch as a whole) for security vulnerabilities. This is a read-only review — file findings but do not fix them.

## Scope

1. **Secrets exposure** — have any secrets, API keys, tokens, credentials, or private keys been committed or printed to logs?
2. **Input validation** — are any user-supplied or untrusted inputs passed to dangerous functions (shell execution, file writes, SQL queries, eval, template injection) without sanitization?
3. **Authentication & authorization** — are new endpoints / commands / APIs properly gated? Are there missing auth checks or privilege escalations?
4. **Cryptographic misuse** — are custom crypto primitives used instead of established libraries? Hardcoded salts/keys? Weak algorithms (MD5, SHA-1 for certificates, ECB mode)?
5. **Injection vectors** — command injection, SQL injection, path traversal, server-side template injection, XSS (in rendered output).
6. **Temporary files** — are temp files created in predictable locations? Do they get cleaned up on all exit paths (including panics/crashes)?
7. **Error handling** — are sensitive details (stack traces, paths, DB schemas) leaked in error messages returned to the user?
8. **TOCTOU (time-of-check/time-of-use)** — does the code check a condition (file exists, user has role, etc.) and then act on it without revalidation? Are there race windows?

## Method

1. Run `git diff` (or `git diff main...HEAD` for branch review) to get the change set.
2. Read each changed file. Focus on new code, not moved code.
3. For each finding, record:
   - **Location**: file + line number
   - **Severity**: CRITICAL / HIGH / MEDIUM / LOW / INFO
   - **Issue**: what the security concern is
   - **Why it matters**: the potential impact if exploited
   - **Suggestion**: how to fix (without implementing the fix)

## Output format

```
## Security Review: <branch/summary>

### CRITICAL (N findings)
...

### HIGH (N findings)
...

### MEDIUM (N findings)
...

### LOW (N findings)
...

### INFO (N findings)
...

### Summary
N total findings. Recommend addressing CRITICAL and HIGH before merging.
```

## Boundaries

- This is a **read-only review** — do not implement fixes. The user or another skill (like `simplify` or `code-review`) will handle remediation.
- If you don't have the full context to judge a risk, flag it as INFO and explain what additional context you need.
- Version dependencies (known CVEs in dependencies) are out of scope unless the diff adds a new dependency.
"#;

const RENAME: &str = r#"# Rename: Set Conversation Session Title

Rename the current conversation session to a descriptive title that makes it easy to find later in session history.

## When to use

- The user says "rename this session" or "give this a title"
- The conversation has reached a natural milestone where a meaningful title emerges
- The user is about to start a multi-step task that should be recorded under a specific name

## How it works

1. If the user provides a title directly, use it.
2. If no title is given, review the conversation so far and propose 2-3 short (<60 chars) descriptive titles. Ask the user to pick one.
3. Apply the chosen title using the appropriate mechanism (shell command, API call, or settings update).

## Title guidelines

- Keep it under 60 characters
- Capture the goal, not the mechanism: "Fix login CSRF" not "Modify auth.rs line 42"
- Use sentence case: "Add user profile API" not "add user profile api"
- Be specific enough to distinguish from other sessions: "Q4 performance tuning — payment pipeline" not just "performance"

## Examples

| Conversation starting point | Good title |
|----------------------------|------------|
| User pastes a build error | "Debug Docker build failure on CI" |
| User asks to add a feature | "Add organization invite flow" |
| User asks for code review | "Review PR #142: caching layer" |
| User asks general design question | "Architecture discussion: offline sync" |

## Don't

- Don't use generic titles like "coding session" or "debugging"
- Don't include the current date or "session" in the title (it's implied)
- Don't change the title without user confirmation unless they gave an explicit one
"#;

const IMPORT: &str = r#"# Import: Migrate Config from Claude Code / Codex / Cursor

Bring project configuration left behind by another agent tool into this
engine's layout: `AGENTS.md` + `.agents/skills/` + `.agix/rules/`.

Do the work with the ordinary file tools. There is no `Import` tool — the
detection rules and output layout change often enough that they belong here,
in text you can edit, rather than compiled into the binary.

## When to use

- The user runs `/import`, or asks to import/migrate settings from Claude
  Code, Codex, or Cursor
- You notice `CLAUDE.md`, `.claude/skills/`, `.cursorrules`,
  `.cursor/rules/*.mdc`, or an `AGENTS.md` with no `.agents/skills/`, and the
  user has not already declined

## Process

1. **Detect.** Check for each source in the project root:
   - `claude_code` — `CLAUDE.md` exists, or `.claude/skills/` is non-empty
   - `cursor` — `.cursorrules` exists, or `.cursor/rules/*.mdc` matches
   - `codex` — `AGENTS.md` exists but `.agents/skills/` does not
2. **Nothing found** — say so and stop.
3. **Ask which one.** Present what you found and let the user pick exactly
   one. Never import two sources in one pass: their `AGENTS.md` sections would
   interleave and neither would be re-runnable.
4. **Execute** the steps for that source below.
5. **Report** what you wrote, by path.
6. **Declined** — acknowledge and stop. Do not record anything; the automatic
   detection path keeps its own marker.

## Merging into AGENTS.md

Imported prose goes inside HTML-comment markers so a re-run replaces its own
section instead of appending a second copy. Use the tag for the source
(`CLAUDE_CODE` or `CURSOR_RULES`):

```
<!-- IMPORTED_FROM_<TAG>_AT: <ISO-8601 UTC> -->
<!-- IMPORTED_FROM_<TAG>_BEGIN -->
...imported content...
<!-- IMPORTED_FROM_<TAG>_END -->
```

Read `AGENTS.md` first. If the pair of markers is already present, replace
only what is between them and keep everything after `_END` — that tail is the
user's own writing. If `AGENTS.md` does not exist, create it with the block
alone. If it exists without markers, put the block at the top and the existing
content below it.

Match this format exactly. The automatic import path
(`base::frozen::import`, used at daemon startup) writes the same markers, and
the two must be able to re-run over each other's output.

## Per source

**claude_code**
- Merge `CLAUDE.md` into `AGENTS.md` under tag `CLAUDE_CODE`
- Copy `.claude/skills/*` to `.agents/skills/*`. **Never overwrite** an
  existing project skill of the same name — skip it and say which you skipped

**cursor**
- Merge `.cursorrules` (plain text) into `AGENTS.md` under tag `CURSOR_RULES`
- Convert each `.cursor/rules/*.mdc` to `.agix/rules/<name>.md`
- An `.mdc` whose frontmatter has `alwaysApply: true` should also be
  referenced from `AGENTS.md`, so it is actually loaded

**codex**
- Nothing to migrate; Codex already uses `AGENTS.md`. Create an empty
  `.agents/skills/` if missing and say that was all that was needed

## Don't

- Don't invent a source the detection step did not find
- Don't import a source the user did not pick
- Don't overwrite existing project skills or rules
- Don't touch `.agix/.imported.json` — that belongs to the automatic path
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fifteen_bundled_skills_present() {
        let s = bundled_skills();
        assert_eq!(s.len(), 15);
        let names: Vec<&str> = s.iter().map(|e| e.name.as_str()).collect();
        for n in [
            "simplify",
            "verify",
            "debug",
            "batch",
            "stuck",
            "loop",
            "loremIpsum",
            "remember",
            "skillify",
            "updateConfig",
            "keybindings-help",
            "init",
            "security-review",
            "rename",
            "import",
        ] {
            assert!(names.contains(&n), "missing bundled skill: {n}");
        }
    }

    #[test]
    fn all_bundled_have_when_to_use() {
        for s in bundled_skills() {
            assert!(s.when_to_use.is_some(), "{} missing when_to_use", s.name);
        }
    }

    #[test]
    fn bundled_body_returns_for_known_names() {
        for name in &[
            "simplify",
            "verify",
            "debug",
            "batch",
            "stuck",
            "loop",
            "loremIpsum",
            "remember",
            "skillify",
            "updateConfig",
            "keybindings-help",
            "init",
            "security-review",
            "rename",
            "import",
        ] {
            assert!(bundled_body(name).is_some(), "{name} missing body");
            let body = bundled_body(name).unwrap();
            assert!(body.len() > 200, "{name} body suspiciously short");
        }
    }

    #[test]
    fn bundled_body_returns_none_for_unknown() {
        assert!(bundled_body("ghost").is_none());
    }

    #[test]
    fn paths_have_bundled_prefix() {
        for s in bundled_skills() {
            assert!(
                s.path.to_string_lossy().starts_with("(bundled:"),
                "{} path should be (bundled:...)",
                s.name
            );
        }
    }
}
