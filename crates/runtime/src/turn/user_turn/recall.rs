//! 回合开场的记忆召回：后台预取，限时等待后注入本回合。

use crate::agent::Agent;
use crate::turn::attachments::truncate_at_char_boundary;
use base::interface::model::MessageRole;
use base::interface::model::ModelContentBlock;
use base::interface::model::ModelMessage;

use crate::turn::recall::{Recall, RecallQuery};

pub(super) struct PendingRecall {
    handle: tokio_util::task::AbortOnDropHandle<Recall>,
    started_at: std::time::Instant,
}

impl Agent {
    pub(super) fn start_memory_recall(&self, query: &str) -> Option<PendingRecall> {
        // Memory prefetch: fire LLM-based relevant memory selection as a
        // background task. The handle aborts it when dropped, so a recall that
        // misses its budget stops spending on the selection call.
        let handle: Option<tokio_util::task::AbortOnDropHandle<Recall>> =
            if !self.settings.values.memory_enabled {
                None
            } else {
                let ask = RecallQuery {
                    query: query.to_string(),
                    already_surfaced: self.prompt.surfaced().cloned().unwrap_or_default(),
                    recent_tools: self.tooling.tools.names(),
                    model_name: self.settings.values.model.model_name.clone(),
                    session_id: self.session.session_id.clone(),
                };
                Some(self.memory.start_recall(self.llm.model.clone(), ask))
            };
        let started_at = std::time::Instant::now();
        handle.map(|handle| PendingRecall { handle, started_at })
    }

    pub(super) async fn inject_recalled_memories(&mut self, pending: PendingRecall) {
        // Collected before the first API call, not after tool execution, so a
        // turn the model answers without tools still sees its memories. The
        // wait is dead air before the user's first token, hence the short
        // latency budget. In the common case there is no wait at all —
        // `select_memories_from` returns without a model call when the
        // store holds no more candidates than the requested maximum.
        //
        // `already_surfaced` / `recall_count` are updated here and only here,
        // at the point the memories are actually placed in the transcript —
        // recording a memory as "shown to the model" when it was not
        // permanently suppresses a legitimate future surfacing of it.
        const MEMORY_RECALL_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);
        /// Per-entry cap on inlined memory content. Recall carries at most 5
        /// entries, so the whole injection stays bounded; the cap only exists
        /// so one oversized memory cannot dominate the turn.
        const MEMORY_RECALL_CONTENT_CAP: usize = 1024;
        let (prefetch_names, all) =
            match tokio::time::timeout(MEMORY_RECALL_BUDGET, pending.handle).await {
                Ok(Ok(recall)) => {
                    tracing::debug!(
                        count = recall.0.len(),
                        latency_ms = pending.started_at.elapsed().as_millis(),
                        "memory recall completed"
                    );
                    recall
                }
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, "memory recall task failed");
                    Default::default()
                }
                Err(_) => {
                    tracing::warn!(
                        budget_ms = MEMORY_RECALL_BUDGET.as_millis(),
                        "memory recall exceeded its latency budget"
                    );
                    Default::default()
                }
            };
        let relevant: Vec<base::memory::DurableMemory> = {
            let surfaced = self.prompt.surfaced();
            prefetch_names
                .iter()
                .filter_map(|name| all.iter().find(|m| &m.name == name).cloned())
                .filter(|m| !surfaced.is_some_and(|s| s.contains(&m.name)))
                .take(5)
                .collect()
        };
        if !relevant.is_empty() {
            // Mark injected memories as surfaced to avoid repeated injection.
            self.prompt
                .mark_surfaced(relevant.iter().map(|m| m.name.as_str()));
            self.memory
                .record_recalled(relevant.iter().map(|m| m.name.clone()).collect());

            // Carry the memory body, not just its one-line description:
            // the description exists to decide relevance, and a model that
            // only gets the description has to spend a Read round trip to
            // learn anything. At ≤5 entries this is the cheaper option.
            let mut mem_text =
                String::from("<system-reminder>\nRelevant memories for this query:\n");
            for m in &relevant {
                mem_text.push_str(&format!("\n## {}\n{}\n", m.name, m.description));
                let body = m.content.trim();
                if !body.is_empty() {
                    let shown = truncate_at_char_boundary(body, MEMORY_RECALL_CONTENT_CAP);
                    mem_text.push_str(shown);
                    if shown.len() < body.len() {
                        mem_text.push_str(&format!(
                            "\n… (truncated — read {}.md for the full memory)",
                            m.name
                        ));
                    }
                    mem_text.push('\n');
                }
            }
            mem_text.push_str("\nUse these memories to inform your response.\n</system-reminder>");
            self.session.push_message(ModelMessage {
                role: MessageRole::User,
                content: vec![ModelContentBlock::Text { text: mem_text }],
            });
        }
    }
}
