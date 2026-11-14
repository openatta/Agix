//! 会话的持久记忆：回合开场的召回任务、召回记账，以及回合结束后的后台抽取。

use super::memory_extraction::{extract_memories_after_turn, finish_within};
use base::interface::memory_contracts::{MemoryRetriever, RetrievalHook};
use base::interface::model::{Model, ModelMessage};
use base::memory::MemoryStore;
use std::collections::HashSet;
use std::sync::Arc;

pub(super) type Recall = (Vec<String>, Vec<base::memory::DurableMemory>);

/// What a recall is asked, besides the store it reads.
pub(super) struct RecallQuery {
    pub(super) query: String,
    pub(super) already_surfaced: HashSet<String>,
    pub(super) recent_tools: Vec<String>,
    pub(super) model_name: String,
    pub(super) session_id: String,
}

/// What an extraction reads and calls, besides the store it writes.
pub(super) struct Extraction {
    pub(super) messages: Vec<ModelMessage>,
    pub(super) model: Arc<dyn Model>,
    pub(super) model_name: String,
    pub(super) prompt_intro: Option<String>,
    pub(super) environment: Arc<dyn base::interface::environment::Environment>,
    pub(super) session_id: String,
}

/// Durable memory and how a turn recalls from it.
pub(crate) struct Memory {
    memory_store: Arc<MemoryStore>,
    memory_retriever: Arc<dyn MemoryRetriever>,
    retrieval_hooks: Arc<Vec<Arc<dyn RetrievalHook>>>,
    extract_after_turn: bool,
    /// Extractions started at the end of a turn, finished or abandoned when
    /// the session stops.
    extractions: tokio::task::JoinSet<()>,
}

impl Memory {
    pub(crate) fn new(
        memory_store: Arc<MemoryStore>,
        memory_retriever: Arc<dyn MemoryRetriever>,
        retrieval_hooks: Vec<Arc<dyn RetrievalHook>>,
        extract_after_turn: bool,
    ) -> Self {
        Self {
            memory_store,
            memory_retriever,
            retrieval_hooks: Arc::new(retrieval_hooks),
            extract_after_turn,
            extractions: tokio::task::JoinSet::new(),
        }
    }

    pub(crate) fn store(&self) -> &Arc<MemoryStore> {
        &self.memory_store
    }

    pub(super) fn start_recall(
        &self,
        model: Arc<dyn Model>,
        ask: RecallQuery,
    ) -> tokio_util::task::AbortOnDropHandle<Recall> {
        let store = self.memory_store.clone();
        let retriever = Arc::clone(&self.memory_retriever);
        let hooks = Arc::clone(&self.retrieval_hooks);
        tokio_util::task::AbortOnDropHandle::new(tokio::spawn(async move {
            let memories = tokio::task::spawn_blocking(move || store.load_all())
                .await
                .unwrap_or_default();
            // The retriever reads what was just loaded rather than
            // the store, whose reads block.
            let snapshot =
                base::interface::memory_contracts::InMemoryStorage::new(memories.clone());
            let names = base::interface::memory_contracts::retrieve_with_hooks(
                retriever.as_ref(),
                &hooks,
                &snapshot,
                model.as_ref(),
                base::interface::memory_contracts::RetrievalRequest {
                    query: ask.query,
                    limit: 5,
                    already_surfaced: ask.already_surfaced,
                    recent_tools: ask.recent_tools,
                    model_name: ask.model_name,
                    session_id: Some(ask.session_id),
                },
            )
            .await;
            (names, memories)
        }))
    }

    pub(super) fn record_recalled(&self, names: Vec<String>) {
        let store = self.memory_store.clone();
        tokio::task::spawn_blocking(move || {
            if let Err(e) = store.record_recalled(&names) {
                tracing::debug!(error = %e, "recording a memory recall failed");
            }
        });
    }

    pub(super) fn extracts_after_turn(&self) -> bool {
        self.extract_after_turn
    }

    pub(super) fn spawn_extraction(&mut self, job: Extraction) {
        let store = self.memory_store.clone();
        while self.extractions.try_join_next().is_some() {}
        self.extractions.spawn(async move {
            extract_memories_after_turn(
                store,
                &job.messages,
                job.model.as_ref(),
                &job.model_name,
                job.prompt_intro.as_deref(),
                &*job.environment,
                &job.session_id,
            )
            .await;
        });
    }

    /// Wait up to `grace` for the extractions still running, then abandon them.
    pub(crate) async fn finish(&mut self, grace: std::time::Duration) -> bool {
        finish_within(&mut self.extractions, grace).await
    }

    #[cfg(test)]
    pub(crate) fn extractions_mut(&mut self) -> &mut tokio::task::JoinSet<()> {
        &mut self.extractions
    }
}
