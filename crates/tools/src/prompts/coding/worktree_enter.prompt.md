Use this tool ONLY when the user explicitly asks to work in a worktree. This tool creates an isolated git worktree for the current session to work in.

## When to Use

- The user explicitly says "worktree" (e.g., "start a worktree", "work in a worktree", "create a worktree", "use a worktree")
- AGENTS.md or memory instructions direct you to work in a worktree for the current task

## When NOT to Use

- The user asks to create a branch, switch branches, or work on a different branch — use git commands instead
- The user asks to fix a bug or work on a feature — use normal git workflow unless worktrees are explicitly requested by the user or project instructions

## Requirements

- Must be in a git repository; outside one the tool fails
- Must not already be in a worktree session — call ExitWorktree first

## Behavior

- Creates a new git worktree at `.agix/worktrees/<slug>` in the repository root, on a new branch `agix/worktree-<slug>` that starts from your current local HEAD (uncommitted changes in the main checkout are not carried over)
- The session's working directory does NOT change: relative paths and new shell commands still resolve against the original directory. Use the absolute worktree path the tool returns for file operations, and `cd` into it in Bash commands
- Use ExitWorktree to end the worktree session (keep or remove). A worktree still active when the session ends is left on disk
