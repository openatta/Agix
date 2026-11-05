Launch a new agent to handle complex, multi-step tasks autonomously.

The Agent tool launches specialized agents (subprocesses) that autonomously handle complex tasks. Each agent type has specific capabilities and tools available to it.

Available agent types and the tools they have access to:
- claude: Catch-all for any task that doesn't fit a more specific agent. FleetView's default when no agent name is typed. (Tools: All tools)
- general-purpose: General-purpose agent for researching complex questions, searching for code, and executing multi-step tasks. (Tools: All tools)
- explore: Fast read-only search agent for locating code. Uses Read/Grep/Glob/WebSearch/WebFetch/LSP. (Tools: Read, Grep, Glob, WebSearch, WebFetch, LSP)
- plan: Software architect agent for designing implementation plans. Uses Read/Grep/Glob/WebSearch/WebFetch/Write. (Tools: Read, Grep, Glob, WebSearch, WebFetch, Write)
- code-reviewer: Code review specialist. Uses Read/Grep/Glob/LSP/Bash for read-only inspection. (Tools: Read, Grep, Glob, LSP, Bash)

When using the Agent tool, specify a subagent_type parameter to select which agent type to use. If omitted, the general-purpose agent is used.

When NOT to use the Agent tool:
- If you want to read a specific file path, use the Read tool or Glob instead of the Agent tool, to find the match more quickly
- If you are searching for a specific class definition like "class Foo", use Glob instead, to find the match more quickly
- If you are searching for code within a specific file or set of 2-3 files, use the Read tool instead of the Agent tool, to find the match more quickly
- Other tasks that are not related to the agent descriptions above

Usage notes:
- Always include a short description (3-5 words) summarizing what the agent will do
- When the agent is done, it will return a single message back to you. The result returned by the agent is not visible to the user. To show the user the result, you should send a text message back to the user with a concise summary of the result.
- Each Agent invocation starts fresh — provide a complete task description.
- The agent's outputs should generally be trusted
- Clearly tell the agent whether you expect it to write code or just to do research (search, file reads, web fetches, etc.), since it is not aware of the user's intent
- If the agent description mentions that it should be used proactively, then you should try your best to use it without the user having to ask for it first. Use your judgement.
- If the user specifies that they want you to run agents "in parallel", you MUST send a single message with multiple Agent tool use content blocks. For example, if you need to launch both a build-validator agent and a test-runner agent in parallel, send a single message with both tool calls.

## Writing the prompt

Brief the agent like a smart colleague who just walked into the room — it hasn't seen this conversation, doesn't know what you've tried, doesn't understand why this task matters.
- Explain what you're trying to accomplish and why.
- Describe what you've already learned or ruled out.
- Give enough context about the surrounding problem that the agent can make judgment calls rather than just following a narrow instruction.
- If you need a short response, say so ("report in under 200 words").
- Lookups: hand over the exact command. Investigations: hand over the question — prescribed steps become dead weight when the premise is wrong.

Terse command-style prompts produce shallow, generic work.

**Never delegate understanding.** Don't write "based on your findings, fix the bug" or "based on the research, implement it." Those phrases push synthesis onto the agent instead of doing it yourself. Write prompts that prove you understood: include file paths, line numbers, what specifically to change.

## Foreground vs Background

You can optionally run agents in the background using the `background` parameter.

- **Foreground agent** (default): Use when you need the agent's results before you can proceed with the next step of your task. The agent runs synchronously and returns its summary directly.
- **Background agent** (`background: true`): Use when you have genuinely independent work to do in parallel with other tasks. The agent returns a task ID immediately, and you can continue working. Check on it with `TaskOutput` when you actually need the result — there is no automatic push notification when it finishes, so don't tell the user you'll "let them know" the moment it's done; check back on it instead.

**IMPORTANT**: Launch multiple agents in a single message to run them in parallel. Background agents are ideal for fan-out work where you need multiple independent analyses simultaneously.

## Joining a persistent team (`team_name` + `name`)

Only available when team coordination is enabled for this session (otherwise these two fields are rejected — check whether `TeamCreate`/`TeamList`/`TeamDelete` are in your tool list). Give both `team_name` and `name` together to talk to a **persistent** team member instead of a one-shot agent:

- If `(team_name, name)` doesn't exist yet, this spawns a new persistent member and sends it `prompt` as its first message.
- If it already exists (you or another turn spawned it earlier), `prompt` is queued as its **next** message — the member remembers everything from its own earlier turns. Reusing the same `name` is how you keep talking to the same member; it isn't a naming collision to avoid.
- This always behaves like `background: true` — you get a task id back immediately, poll it with `TaskOutput` when you need the reply. It cannot run in the foreground, because a persistent member's reply isn't ready until its queued turn actually runs, and there's no push notification for it either (see the note on backgrounding above) — if you need to know the moment it's done, check back on the task id yourself.
- Members don't go away on their own until either you (or another turn) explicitly stop them via `TeamDelete`, or the runtime reclaims them for being idle too long / the team's total member count getting too large — don't assume a member is still there if a long time has passed since you last messaged it; if unsure, check `TeamList` first rather than guessing. `TeamList` also shows how long an idle member has been waiting, since there's no other way to find out.
- A persistent member does not currently have any way to message *you* back mid-conversation, or to talk to other team members — it only responds to the specific message you send it, when you send it. For agents that need to coordinate with each other while actually running at the same time, use `TeamCreate`'s batch mode instead (see that tool's own guidance).
- **`permission_mode`** (optional): the permission grant this member runs under for its whole lifetime, e.g. `"bypassPermissions"` to let it actually make changes, `"plan"` (the default if you don't specify anything) to restrict it to read-only tools, `"dontAsk"` to allow nothing beyond structurally-safe read-only tools. Set it once, on whichever call spawns the *first* member under a `team_name` — every later member spawned under that same name inherits it automatically, you don't need to repeat it. `"auto"` and `"bubble"` are rejected here; they don't work for a persistent member (see this tool's `permission_mode` field description for why).
