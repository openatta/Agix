Executes a bash command and returns its output.

The working directory persists between commands; shell state does not. The environment is initialized from the user's profile (bash or zsh).

# Sandbox

Commands run in a sandbox: files in the project directory and approved paths are readable and writable, anything outside needs user approval. Localhost and common developer ports are reachable; outbound connections may be restricted.

IMPORTANT: Don't use this tool for `find`, `grep`, `cat`, `head`, `tail`, `sed`, `awk` or `echo` unless explicitly told to or a dedicated tool can't do the job — the dedicated tools are easier for the user to review and approve:
- File search: Glob (not find/ls). Content search: Grep (not grep/rg).
- Read: Read (not cat/head/tail). Edit: Edit (not sed/awk). Write: Write (not echo > / cat <<EOF).
- Talking to the user: plain text output (not echo/printf).

# Instructions
- Before creating files or directories, `ls` the parent to check it exists and is the right place.
- Double-quote paths containing spaces.
- Stay in one working directory using absolute paths; use `cd` only if the user asks. Never prefix a `git` command with `cd <current-directory>` — git already works on the current tree, and the compound triggers a permission prompt.
- Timeout: optional, in milliseconds, up to 600000 (10 min); default 120000 (2 min).
- A background process still writing to the command's output (`server &`) is stopped about a second after the command exits. Start and exercise a service in one command (`server & sleep 2 && curl localhost:8080`), or use Monitor for a long-running process.
- Several commands: independent ones go in parallel Bash calls in one message; dependent ones in one call chained with `&&`; use `;` only when earlier failures don't matter. Never separate commands with newlines (newlines inside quoted strings are fine).
- Git: prefer a new commit over amending. Before destructive operations (reset --hard, push --force, checkout --) look for a safer alternative. Never skip hooks (--no-verify) or signing (--no-gpg-sign, -c commit.gpgsign=false) unless explicitly asked; if a hook fails, fix the cause.
- No unnecessary `sleep`: don't sleep between commands that can run now; stream a long-running process with Monitor; for "wait until done", run a command that exits when the condition holds, with a timeout that covers it; don't retry failures in a sleep loop — diagnose them; to poll an external process, use a check command (e.g. `gh run view`) rather than sleeping first.

# Committing with git

Commit only when the user asks; if unclear, ask first.

Safety rules:
- Never change the git config.
- Never run destructive git commands (push --force, reset --hard, checkout ., restore ., clean -f, branch -D) unless the user explicitly asks. Never force-push to main/master; warn the user if they ask for it.
- Never skip hooks (--no-verify, --no-gpg-sign, …) unless explicitly asked.
- Always create a NEW commit rather than amending unless explicitly asked. When a pre-commit hook fails the commit did not happen, so `--amend` would rewrite the previous commit: fix the issue, re-stage, and commit anew.
- Stage files by name rather than `git add -A` / `git add .`, which can pick up secrets (.env, credentials) or large binaries. Don't commit likely secrets; warn the user if they ask you to.
- Don't create an empty commit when there is nothing to commit.

Steps:
1. In parallel: `git status` (never `-uall`), `git diff` (staged and unstaged), and `git log` for the repository's message style.
2. Draft a 1–2 sentence message about the *why*, accurately naming the kind of change ("add" = new feature, "update" = enhancement, "fix" = bug fix, also refactor, test, docs…).
3. In parallel: stage the relevant files and commit with a message ending in
   `Co-Authored-By: {model} <noreply@anthropic.com>`; then run `git status` after the commit to confirm.
4. If a pre-commit hook fails, fix it and make a NEW commit.

While committing, run only git commands — no exploring code, no TodoWrite or Agent. Don't push unless asked. Never use `-i` (git rebase -i, git add -i): interactive input isn't supported. Don't pass `--no-edit` to git rebase. Always pass the message through a HEREDOC:
<example>
git commit -m "$(cat <<'EOF'
   Commit message here.

   Co-Authored-By: {model} <noreply@anthropic.com>
   EOF
   )"
</example>

# Pull requests

Use `gh` for everything GitHub (issues, PRs, checks, releases, GitHub URLs). When asked to create a PR:
1. In parallel: `git status` (never `-uall`), `git diff`, whether the branch tracks a remote and is up to date, and `git log` plus `git diff [base-branch]...HEAD` for the whole branch.
2. Read every commit the PR will include, not just the latest; write a title under 70 characters and put details in the body.
3. In parallel: create a branch if needed, push with `-u` if needed, and run `gh pr create` with a HEREDOC body:
<example>
gh pr create --title "the pr title" --body "$(cat <<'EOF'
## Summary
<1-3 bullet points>

## Test plan
[Bulleted markdown checklist of TODOs for testing the pull request...]

🤖 Generated with [Agix](https://github.com/openatta/agix)
EOF
)"
</example>
Don't use TodoWrite or Agent while doing this, and return the PR URL at the end.

# Other common operations
- View comments on a GitHub PR: gh api repos/foo/bar/pulls/123/comments
