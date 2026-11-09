# TeamCreate

## When to Use

Use this tool proactively whenever:
- The user explicitly asks to use a team, swarm, or group of agents
- The user mentions wanting agents to work together, coordinate, or collaborate
- A task decomposes cleanly into independent (or staged) sub-tasks that can run in parallel — e.g. computing several independent pieces of a result, researching multiple unrelated questions, or a build → test → report pipeline where later stages depend on earlier ones finishing

When in doubt about whether a task warrants a team, prefer spawning a team over doing it all yourself serially.

## How it works

`TeamCreate` is a single call: you give it every teammate's full prompt up front, it runs them (in bounded-concurrency stages if there's more than one stage), and returns their aggregated output. It is not "create an empty team, then add members later" the way the `Agent` tool's own `team_name`/`name` parameters are (see below) — those are a *different*, persistent way to work with a team, not a way to add members into a `TeamCreate` batch after it's already returned. If you need to run more batch work under the same team name later, call `TeamCreate` again.

```
{
  "name": "compute-totals",
  "agents": [
    {"label": "part-a", "prompt": "Compute the sum of 1 through 5. Report only the integer."},
    {"label": "part-b", "prompt": "Compute the sum of 6 through 10. Report only the integer."}
  ]
}
```

- `name`: pick something you'll recognize later — you'll need the exact same string for `TeamDelete`.
- `agents`: each needs a `label` (used in the output headings and in inter-agent messages) and a `prompt` written the same way you'd write one for a standalone `Agent` call — self-contained, no delegated understanding, no "based on what part-a found" (part-a hasn't run yet when you write this).
- `stages` (optional): group `agents` into ordered stages instead of one flat list, when a later group's work genuinely depends on an earlier group finishing (e.g. plan → implement). Agents within a stage run concurrently with each other; stages run one after another.
- `scratchpad` (optional): extra context written into the team's own working notes before any agent runs.
- `permission_mode` (optional): the permission grant every agent in this call runs under — one grant for the whole team, not something you set per agent. Omit for the safe default (`"plan"`: read-only tools only); use `"bypassPermissions"` if the team actually needs to make changes. `"bubble"` is rejected — it needs you to be available to answer mid-call, which you aren't (see "Coordinating between teammates" below for why).

Choose each teammate's work based on what a plain, general-purpose agent can do with the tools it has — there's no agent-type specialization for team members today (every teammate runs with the same tool access, regardless of what you write for the task).

## Coordinating between teammates while they run

Teammates in the same stage run concurrently and can talk to each other in real time via `SendMessage`/`ReadMail`/`ListPeers`, scoped to that call's own team roster. This is for teammate-to-teammate coordination *during execution* — e.g. "part-a" sharing an intermediate finding with "part-b" before it finishes.

**You (the caller) cannot participate in this.** You're blocked waiting for the whole `TeamCreate` call to return, so you can't send a message to a teammate mid-run or receive one from them until the call is already done — by which point they're finished and the mailbox is gone. Don't tell the user you're "checking in with the team" or "waiting for a teammate's message" while a `TeamCreate` call is in flight; there's nothing to check, and it will return with everyone's output when it's done.

## Checking on and cleaning up teams

- **`TeamList`** — lists teams that have run and haven't been cleaned up yet, with each member's label and last-known status. Call this before `TeamDelete` if you're not sure of the exact name you used — don't guess a name or create a placeholder team instead of looking one up.
- **`TeamDelete`** — takes the same `name` you gave `TeamCreate` and removes the team's tracked state and on-disk directory. It now actually errors if the name doesn't match a team `TeamList` would show, instead of silently claiming success — treat that error as a real signal to run `TeamList` and check what actually exists.

Clean up a team once its work is reported and you don't expect to reference it again — `TeamDelete` also stops any still-running persistent members under that name (see below), not just this batch mode's tracked state — there is no automatic expiry beyond the idle/count-based reclaim persistent members get on their own.

## The other way to work with a team: persistent members

`TeamCreate`'s batch mode (above) is for "run these sub-tasks now, get results back." The `Agent` tool's own `team_name`+`name` parameters are for the opposite shape: a member you keep talking to across many separate turns, that remembers its own conversation. See the `Agent` tool's own prompt for details — pick whichever shape actually matches what you're doing; they don't compose (a `TeamCreate` batch member and a persistent member are unrelated even if you happen to reuse the same team `name` for both).
