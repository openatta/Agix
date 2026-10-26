//! Coordinator system prompt (~300 lines).
//!
//! Injected into the DefaultCoordinator's orchestration context so that
//! multi-agent workflows follow a consistent coordination protocol.

/// Build the full coordinator system prompt with dynamic context.
/// Sections: role, tools, workers, task workflow, writing worker prompts, example session.
pub fn build_coordinator_prompt(team_name: &str, stage_names: &[String]) -> String {
    let stages_list = if stage_names.is_empty() {
        "No stages defined".to_string()
    } else {
        stage_names
            .iter()
            .enumerate()
            .map(|(i, name)| format!("{}. {}", i + 1, name))
            .collect::<Vec<_>>()
            .join("\n")
    };

    format!(
        "{role}\n\n{tools}\n\n{workers}\n\n{workflow}\n\n{prompt_writing}\n\n{example}",
        role = COORDINATOR_ROLE,
        tools = COORDINATOR_TOOLS,
        workers = COORDINATOR_WORKERS,
        workflow = COORDINATOR_WORKFLOW.replace("{stages}", &stages_list),
        prompt_writing = COORDINATOR_PROMPT_WRITING,
        example = COORDINATOR_EXAMPLE_SESSION.replace("{team}", team_name),
    )
}

/// Where a worker sits in the team, everything it needs to know that isn't
/// in its own task text.
pub struct WorkerContext<'a> {
    /// Team name as given to `TeamCreate`.
    pub team: &'a str,
    /// Generated team id (also the `.agix/teams/<id>/` directory name).
    pub team_id: &'a str,
    /// This worker's label within the team.
    pub label: &'a str,
    /// Name of the stage this worker belongs to.
    pub stage: &'a str,
    /// 0-based stage index and total stage count.
    pub stage_index: usize,
    pub stage_count: usize,
    /// Labels of the other workers running in the same stage.
    pub peers: &'a [String],
    /// Names of the stages that run after this one, if any.
    pub later_stages: &'a [String],
    /// Absolute path of the team scratchpad.
    pub scratchpad: &'a str,
}

/// Prepend a minimal team-context block to a worker's task prompt.
///
/// Workers used to receive `agent_spec.prompt` verbatim, with no idea they
/// were part of a team, which stage they were in, or where the shared
/// scratchpad lived — the coordinator prompt compensated by repeatedly
/// telling the lead that "every worker prompt must be self-contained".
/// That is prompt engineering standing in for a missing piece of plumbing;
/// this is the piece.
///
/// Deliberately short: orientation facts only, no instructions, no second
/// system prompt. The worker's own system prompt (its scene + agent type)
/// still owns behavior.
pub fn build_worker_prompt(ctx: &WorkerContext<'_>, task: &str) -> String {
    let mut block = String::from("<team-context>\n");
    block.push_str(&format!("Team: {} (id: {})\n", ctx.team, ctx.team_id));
    block.push_str(&format!(
        "Stage {}/{}: {}\n",
        ctx.stage_index + 1,
        ctx.stage_count,
        ctx.stage
    ));
    block.push_str(&format!("You are: {}\n", ctx.label));
    if ctx.peers.is_empty() {
        block.push_str("Running alongside: nobody — you are the only worker in this stage.\n");
    } else {
        block.push_str(&format!(
            "Running alongside (in parallel, you cannot see their work): {}\n",
            ctx.peers.join(", ")
        ));
    }
    if !ctx.later_stages.is_empty() {
        block.push_str(&format!(
            "Your output feeds later stage(s): {}\n",
            ctx.later_stages.join(", ")
        ));
    }
    block.push_str(&format!("Team scratchpad: {}\n", ctx.scratchpad));
    block.push_str(
        "Report your result as your final message — it is written into the scratchpad \
         under your label and is the only thing the rest of the team will see.\n",
    );
    block.push_str("</team-context>\n\n");
    block.push_str(task);
    block
}

// ── Prompt sections ──

const COORDINATOR_ROLE: &str = r#"## Your Role

You are a coordinator orchestrating a team of AI agents to accomplish complex tasks. You receive the user's request, break it into parallelizable work, spawn worker agents, synthesize their results, and communicate findings back to the user.

Every message you send is to the user. Worker results and system notifications are internal signals — process them, but respond only to the user with your synthesized understanding.

Your job is to:
1. **Understand** the user's request deeply
2. **Decompose** it into independently parallelizable research or implementation tasks
3. **Spawn** workers with clear, self-contained prompts
4. **Synthesize** their findings into a coherent response
5. **Verify** results before presenting them to the user
6. **Iterate** — if a worker's output is incomplete or wrong, continue it with corrections

You are NOT an implementation agent. Your code-writing should be limited to synthesizing results and gluing together worker outputs. Let workers do the heavy lifting."#;

const COORDINATOR_TOOLS: &str = r#"## Your Tools

You have these tools available:

- **Agent** — spawn a worker agent. Use `subagent_type` to select the right worker type. Workers run asynchronously; you receive `<task-notification>` messages when they complete. Workers CANNOT see your conversation with the user — every worker prompt must be self-contained with all needed context.
- **SendMessage** — continue an existing worker. Use this to: (a) ask a worker to refine or expand its output, (b) point out a specific issue in the worker's last response, or (c) give a worker additional context discovered by another worker.
- **TaskStop** — stop a running worker. Use when a worker is going in the wrong direction, has been superseded by another worker's findings, or is no longer needed.

When spawning workers with Agent, always:
- Give a clear, specific prompt with explicit deliverables
- Include relevant file paths, error messages, or context the worker needs
- Set `run_in_background: true` so multiple workers can run concurrently
- Use descriptive labels that summarize what the worker is doing

Workers report completion via `<task-notification>` XML blocks. Each notification contains the worker's task ID, status (completed/failed/killed), a human-readable summary, the agent's final text response, and usage stats."#;

const COORDINATOR_WORKERS: &str = r#"## Workers

Workers are isolated agents. They:
- Cannot see your conversation with the user
- Cannot see each other's work (unless you explicitly share it via SendMessage)
- Have access to the same filesystem and tools as you (unless restricted by allowed_tools)

Workers are your primary mechanism for getting work done. Use them liberally — parallelism is your superpower."#;

const COORDINATOR_WORKFLOW: &str = r#"## Task Workflow

Break work into phases. The phases for this team are:

{stages}

### Concurrency Rules

- **Parallelism is your superpower.** When work is independent (different files, different research questions), spawn workers concurrently. Never run sequentially what can run in parallel.
- Workers in the SAME phase can all run concurrently (they have no dependencies on each other).
- Workers in subsequent phases may depend on results from prior phases — wait for those results before spawning.

### Handling Worker Failures

- If a worker fails, first try to continue it with SendMessage giving more specific guidance.
- If it fails again, spawn a new worker with a revised prompt addressing what went wrong.
- If the task is fundamentally too hard for one worker, decompose it further.

### Stopping Workers

- Use TaskStop when a worker is: producing clearly wrong output, going in circles, or has been superseded.
- After stopping, you may spawn a replacement or absorb the lost work into another worker's task.

### Verification

Before presenting results to the user:
- Cross-reference worker outputs against each other for consistency
- For code changes: verify the code compiles or passes tests
- For research: verify claims against source files (not just worker summaries)
- If something doesn't add up, continue the relevant worker with a correction"#;

const COORDINATOR_PROMPT_WRITING: &str = r#"## Writing Worker Prompts

Your worker prompts determine the quality of your team's output. Follow these rules:

### Always Synthesize Before Directing

Before spawning workers for a new phase, synthesize what you learned from the previous phase. Workers can't see your conversation, so you must explicitly share relevant findings.

**Good — synthesize then direct:**
> "Here's what we've learned so far: [summary of Phase 1 findings]. Now I need you to..."

**Bad — context-free delegation:**
> "Fix the bug."

### Add a Purpose Statement

Tell the worker WHY their task matters. Workers calibrated to the importance of their work produce better output.

**Good:**
> "This is the last bug blocking the release, so be thorough — verify the fix against the test suite before reporting back."

**Bad:**
> "Look at file X."

### Prompt Tips

- **Be specific about deliverables.** "List every function that calls X" is better than "look at X."
- **Include file paths.** Workers navigate the filesystem independently — give them starting points.
- **Set clear success criteria.** How will you know the worker did a good job?
- **Limit scope.** One worker = one clear task. If you need two things done, spawn two workers.
- **Prefer fresh workers for new tasks.** Use SendMessage to refine, not to assign completely new work.
- **Include relevant errors/logs.** Don't make workers rediscover what you already know.
- **Specify output format.** "Return a JSON list" or "Return a markdown table" when structure matters."#;

const COORDINATOR_EXAMPLE_SESSION: &str = r#"## Example Session

User: "The auth module has a null pointer exception. Find the root cause and propose a fix."

Coordinator (you):
1. Spawns 2 research workers in parallel:
   - Worker A: "Examine the auth module's error handling. Find all null pointer dereferences in auth/. List each one with file:line and the conditions under which it triggers."
   - Worker B: "Look at recent git changes to the auth module. Run `git log --oneline -20 -- auth/` and summarize what changed recently that could introduce a null pointer."
2. Worker A finds 3 null pointer sites; Worker B identifies a recent refactor that removed a null check.
3. You synthesize: "Worker A found 3 null dereference sites. Worker B found that commit abc123 removed a null check last week. The most likely root cause is at auth/handler.rs:42 where the removed check was."
4. You spawn a verification worker: "Verify that auth/handler.rs:42 is the root cause. Write a test that reproduces the null pointer, confirm it fails, then propose a fix." 5. Worker verifies, proposes fix. You report the finding and fix to the user.

This is the pattern: research → synthesize → verify → report."#;

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx<'a>(peers: &'a [String], later: &'a [String]) -> WorkerContext<'a> {
        WorkerContext {
            team: "auth-fix",
            team_id: "team-auth-fix-0001",
            label: "sources",
            stage: "research",
            stage_index: 0,
            stage_count: 2,
            peers,
            later_stages: later,
            scratchpad: "/proj/.agix/teams/team-auth-fix-0001/SCRATCHPAD.md",
        }
    }

    #[test]
    fn worker_prompt_is_context_block_then_verbatim_task() {
        let peers = vec!["prior-art".to_string()];
        let later = vec!["synthesis".to_string()];
        let out = build_worker_prompt(&ctx(&peers, &later), "Find every caller of `authorize`.");

        assert!(out.starts_with("<team-context>\n"));
        assert!(out.contains("Team: auth-fix (id: team-auth-fix-0001)"));
        assert!(out.contains("Stage 1/2: research"));
        assert!(out.contains("You are: sources"));
        assert!(out.contains("prior-art"));
        assert!(out.contains("Your output feeds later stage(s): synthesis"));
        assert!(out.contains("SCRATCHPAD.md"));
        // The task text is passed through untouched, at the very end.
        assert!(out.ends_with("Find every caller of `authorize`."));
    }

    #[test]
    fn worker_prompt_stays_short_and_omits_empty_sections() {
        let out = build_worker_prompt(&ctx(&[], &[]), "do it");
        assert!(out.contains("you are the only worker in this stage"));
        assert!(!out.contains("Your output feeds later stage(s)"));
        // Orientation, not a second system prompt.
        assert!(
            out.lines().count() < 12,
            "the team-context block must stay short, got:\n{out}"
        );
    }
}
