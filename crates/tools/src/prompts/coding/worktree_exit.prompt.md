End a worktree session created by EnterWorktree. The session's working directory never changed, so after this, paths simply stop pointing into the worktree.

## Scope

This tool ONLY operates on worktrees created by EnterWorktree in this session. It will NOT touch:
- Worktrees you created manually with `git worktree add`
- Worktrees from a previous session (even if created by EnterWorktree then)
- The directory you're in if EnterWorktree was never called

If called outside an EnterWorktree session, the tool is a **no-op**: it reports that no worktree session is active and takes no action. Filesystem state is unchanged.

## When to Use

- The user explicitly asks to "exit the worktree", "leave the worktree", "go back", or otherwise end the worktree session
- Do NOT call this proactively — only when the user asks

## Parameters

- `action` (`"keep"` or `"remove"`, default `"remove"`)
  - `"keep"` — end the worktree session and leave the worktree directory and branch intact on disk. Use this if the user wants to come back to the work later, or if there are changes to preserve. Afterwards EnterWorktree can start a new session.
  - `"remove"` — delete the worktree directory and its branch. Use this for a clean exit when the work is done or abandoned.
- `discard_changes` (optional, default false): only meaningful with `action: "remove"`. If the worktree has uncommitted files or commits that no other branch has, the tool REFUSES to remove it unless this is set to `true`; the worktree session stays active. If the tool returns such an error, confirm with the user before re-invoking with `discard_changes: true`.
