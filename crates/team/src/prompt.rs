//! The team-context block a `TeamCreate` member's task opens with.

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
    /// [`crate::team_memory::TeamMemoryStore::build_team_memory_prompt`], or empty.
    pub team_memory: &'a str,
}

/// Prepend a minimal team-context block to a worker's task prompt.
///
/// Without it a worker gets `agent_spec.prompt` verbatim, with no idea it is
/// part of a team, which stage it is in, or where the shared scratchpad
/// lives — and the coordinator prompt would have to keep telling the lead
/// that "every worker prompt must be self-contained".
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
            "Running alongside (in parallel; reach them with SendMessage / ReadMail): {}\n",
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
         under your label, which is what later stages and the team's caller read.\n",
    );
    block.push_str("</team-context>\n\n");
    if !ctx.team_memory.is_empty() {
        block.push_str(ctx.team_memory.trim_end());
        block.push_str("\n\n");
    }
    block.push_str(task);
    block
}

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
            team_memory: "",
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
        assert!(out.contains("SendMessage"), "peers are reachable: {out}");
        assert!(!out.contains("cannot see"), "{out}");
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
