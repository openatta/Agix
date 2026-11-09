//! Where memories live, how they are found, and who decides what to keep.
//!
//! Memory was one concrete struct with a fixed directory layout, one search
//! that was a substring scan, and one retriever that was an LLM call written
//! inline. Every part of that is a reasonable default and none of it was a
//! choice: a host with memories in a database, a deployment that wants a
//! vector index, a project whose recall should be deterministic in tests —
//! none had anywhere to plug in.
//!
//! Four seams, matching the four things that are actually separable:
//!
//! * [`MemoryStorage`] — where the memories are.
//! * [`MemoryRetriever`] — which ones this turn should see.
//! * [`RetrievalHook`] — a look at the question before, and the answer after.
//! * [`ExtractionPolicy`] — what the model is told about writing memories
//!   down, which is the whole of the current extraction strategy.
//!
//! # The defaults
//!
//! `MemoryStore` implements the storage trait over the session's memory
//! directories, and the shipped retriever is an LLM selection call. A session
//! that registers nothing gets those two.

use std::collections::HashSet;

use async_trait::async_trait;

use crate::memory::{DurableMemory, MemoryError, MemoryStore};

/// Where durable memories are kept.
///
/// Deliberately narrow: the operations the engine performs, not the ones the
/// file-backed implementation happens to expose. A backend that is not a
/// directory should not have to grow a concept of an index file to satisfy
/// this.
pub trait MemoryStorage: Send + Sync {
    /// Every memory, most recently seen first.
    fn load_all(&self) -> Vec<DurableMemory>;

    /// Memories matching `query`, however this backend matches. The shipped
    /// implementation is a substring scan; a backend with an index is
    /// expected to do better, and nothing depends on the ranking.
    fn search(&self, query: &str) -> Vec<DurableMemory>;

    /// Write these, returning how many landed.
    fn persist_batch(&self, memories: Vec<DurableMemory>) -> Result<usize, MemoryError>;

    /// Forget one by name; `false` if it was not there.
    fn remove(&self, name: &str) -> Result<bool, MemoryError>;
}

impl MemoryStorage for MemoryStore {
    fn load_all(&self) -> Vec<DurableMemory> {
        MemoryStore::load_all(self)
    }

    fn search(&self, query: &str) -> Vec<DurableMemory> {
        MemoryStore::search(self, query)
    }

    fn persist_batch(&self, memories: Vec<DurableMemory>) -> Result<usize, MemoryError> {
        MemoryStore::persist_batch(self, memories)
    }

    fn remove(&self, name: &str) -> Result<bool, MemoryError> {
        MemoryStore::remove(self, name)
    }
}

/// Memories held in memory. The second implementation, and what a test that
/// is about recall rather than about files should use.
#[derive(Default)]
pub struct InMemoryStorage {
    memories: std::sync::Mutex<Vec<DurableMemory>>,
}

impl InMemoryStorage {
    pub fn new(memories: Vec<DurableMemory>) -> Self {
        Self {
            memories: std::sync::Mutex::new(memories),
        }
    }
}

impl MemoryStorage for InMemoryStorage {
    fn load_all(&self) -> Vec<DurableMemory> {
        self.memories
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn search(&self, query: &str) -> Vec<DurableMemory> {
        self.load_all()
            .into_iter()
            .filter(|m| {
                m.name.contains(query) || m.description.contains(query) || m.content.contains(query)
            })
            .collect()
    }

    fn persist_batch(&self, memories: Vec<DurableMemory>) -> Result<usize, MemoryError> {
        let mut held = self.memories.lock().unwrap_or_else(|e| e.into_inner());
        let n = memories.len();
        for m in memories {
            match held.iter_mut().find(|h| h.name == m.name) {
                Some(existing) => *existing = m,
                None => held.push(m),
            }
        }
        Ok(n)
    }

    fn remove(&self, name: &str) -> Result<bool, MemoryError> {
        let mut held = self.memories.lock().unwrap_or_else(|e| e.into_inner());
        let before = held.len();
        held.retain(|m| m.name != name);
        Ok(held.len() != before)
    }
}

/// What a turn is asking for.
#[derive(Debug, Clone)]
pub struct RetrievalRequest {
    /// What the user said, which is what recall is about.
    pub query: String,
    pub limit: usize,
    /// Already shown this session. Surfacing one twice spends context to tell
    /// the model something it was told.
    pub already_surfaced: HashSet<String>,
    /// Tool names in play, so a memory whose name collides with a tool is not
    /// pulled in by the ambiguity.
    pub recent_tools: Vec<String>,
    pub model_name: String,
    pub session_id: Option<String>,
}

/// Which memories this turn should see.
#[async_trait]
pub trait MemoryRetriever: Send + Sync {
    /// Returns memory *names*, best first. Names rather than whole memories
    /// because the caller re-reads them from storage anyway, and a retriever
    /// backed by an index has no reason to carry the bodies around.
    async fn retrieve(
        &self,
        storage: &dyn MemoryStorage,
        model: &dyn crate::interface::model::Model,
        request: &RetrievalRequest,
    ) -> Vec<String>;
}

/// The engine's own retriever: ask the model which memories are relevant.
///
/// The default because it is what the engine has always done, and it is
/// genuinely good at the job — relevance here is a judgement about meaning,
/// which is the thing a model is for. It costs a model call, which is why the
/// seam exists.
pub struct LlmRetriever;

#[async_trait]
impl MemoryRetriever for LlmRetriever {
    async fn retrieve(
        &self,
        storage: &dyn MemoryStorage,
        model: &dyn crate::interface::model::Model,
        request: &RetrievalRequest,
    ) -> Vec<String> {
        crate::memory::select_memories_from(
            &storage.load_all(),
            &request.query,
            model,
            request.limit,
            &request.already_surfaced,
            &request.recent_tools,
            &request.model_name,
            request.session_id.as_deref(),
        )
        .await
    }
}

/// Matches on substrings and spends nothing.
///
/// The second implementation, and the one a test wants: recall that is a pure
/// function of the store is recall a test can assert on, where the model-based
/// default is a judgement that may reasonably differ between runs.
pub struct SubstringRetriever;

#[async_trait]
impl MemoryRetriever for SubstringRetriever {
    async fn retrieve(
        &self,
        storage: &dyn MemoryStorage,
        _model: &dyn crate::interface::model::Model,
        request: &RetrievalRequest,
    ) -> Vec<String> {
        storage
            .search(&request.query)
            .into_iter()
            .map(|m| m.name)
            .filter(|name| !request.already_surfaced.contains(name))
            .filter(|name| {
                !request
                    .recent_tools
                    .iter()
                    .any(|t| name.eq_ignore_ascii_case(t))
            })
            .take(request.limit)
            .collect()
    }
}

/// A look at the question before it is asked, and the answer before it is used.
///
/// Both halves are useful for the same reason and in opposite directions: a
/// deployment knows things about its own vocabulary that the retriever does
/// not (expand an acronym, add the current file's name), and knows things
/// about its own policy that the retriever should not have to (drop anything
/// from a scope this session must not see).
pub trait RetrievalHook: Send + Sync {
    /// Change the question. The default does nothing.
    fn before_retrieve(&self, _request: &mut RetrievalRequest) {}

    /// Change the answer — filter, reorder, truncate. The default does
    /// nothing.
    fn after_retrieve(&self, _request: &RetrievalRequest, _names: &mut Vec<String>) {}
}

/// Run a retrieval with its hooks around it.
pub async fn retrieve_with_hooks(
    retriever: &dyn MemoryRetriever,
    hooks: &[std::sync::Arc<dyn RetrievalHook>],
    storage: &dyn MemoryStorage,
    model: &dyn crate::interface::model::Model,
    mut request: RetrievalRequest,
) -> Vec<String> {
    for hook in hooks {
        hook.before_retrieve(&mut request);
    }
    let mut names = retriever.retrieve(storage, model, &request).await;
    for hook in hooks {
        hook.after_retrieve(&request, &mut names);
    }
    names
}

/// What the model is told about writing memories down.
///
/// The engine's extraction strategy is a prompt: the model decides what is
/// worth remembering, and the only lever anyone has is the wording. Making the
/// wording replaceable is therefore the whole of "the extraction policy is
/// replaceable" — anything more would be inventing a mechanism that does not
/// exist to abstract over it.
pub trait ExtractionPolicy: Send + Sync {
    /// The instructions that go into the system prompt.
    fn instructions(&self, memory_dir: &std::path::Path) -> String;
}

/// The engine's own instructions.
pub struct DefaultExtraction;

impl ExtractionPolicy for DefaultExtraction {
    fn instructions(&self, memory_dir: &std::path::Path) -> String {
        crate::memory::build_memory_prompt(memory_dir)
    }
}

/// Says nothing, so the model is never told to write memories down.
///
/// The second implementation, and a real configuration: a deployment that
/// curates memories itself does not want the model appending to them, and
/// switching the feature off entirely also switches off recall of what is
/// already there.
pub struct NoExtraction;

impl ExtractionPolicy for NoExtraction {
    fn instructions(&self, _memory_dir: &std::path::Path) -> String {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn memory(name: &str, content: &str) -> DurableMemory {
        DurableMemory {
            name: name.to_string(),
            description: String::new(),
            memory_type: crate::memory::MemoryType::default(),
            content: content.to_string(),
            source_session_id: String::new(),
            confidence: 0.8,
            last_seen: String::new(),
            recall_count: 0,
        }
    }

    struct NeverCalledModel;

    #[async_trait]
    impl crate::interface::model::Model for NeverCalledModel {
        fn api_type(&self) -> crate::provider::ApiType {
            crate::provider::ApiType::Anthropic
        }
        async fn stream(
            &self,
            _p: Vec<crate::prompt::PromptBlock>,
            _t: Vec<crate::interface::model::ToolDef>,
            _m: Vec<crate::interface::model::ModelMessage>,
            _s: crate::interface::model::StreamParams,
            _c: tokio_util::sync::CancellationToken,
        ) -> Result<crate::interface::model::ModelStream, crate::interface::model::ModelError>
        {
            panic!("a retriever that costs nothing must not call the model")
        }
    }

    fn store() -> InMemoryStorage {
        InMemoryStorage::new(vec![
            memory("deploy-process", "we deploy on fridays"),
            memory("Bash", "a memory whose name collides with a tool"),
            memory("unrelated", "nothing to do with anything"),
        ])
    }

    #[tokio::test]
    async fn a_retriever_that_costs_nothing_can_replace_the_one_that_costs_a_call() {
        let names = SubstringRetriever
            .retrieve(
                &store(),
                &NeverCalledModel,
                &RetrievalRequest {
                    query: "deploy".into(),
                    limit: 5,
                    already_surfaced: HashSet::new(),
                    recent_tools: Vec::new(),
                    model_name: "test".into(),
                    session_id: None,
                },
            )
            .await;
        assert_eq!(names, ["deploy-process"]);
    }

    #[tokio::test]
    async fn recall_never_repeats_itself_or_shadows_a_tool() {
        let names = SubstringRetriever
            .retrieve(
                &store(),
                &NeverCalledModel,
                &RetrievalRequest {
                    query: "a".into(),
                    limit: 5,
                    already_surfaced: HashSet::from(["unrelated".to_string()]),
                    recent_tools: vec!["bash".into()],
                    model_name: "test".into(),
                    session_id: None,
                },
            )
            .await;
        assert!(!names.contains(&"unrelated".to_string()), "{names:?}");
        assert!(
            !names.contains(&"Bash".to_string()),
            "a memory named after a tool in play is an ambiguity, not a recall: {names:?}"
        );
    }

    #[tokio::test]
    async fn hooks_see_the_question_before_and_the_answer_after() {
        struct Expands;
        impl RetrievalHook for Expands {
            fn before_retrieve(&self, request: &mut RetrievalRequest) {
                if request.query == "ship" {
                    request.query = "deploy".into();
                }
            }
            fn after_retrieve(&self, _request: &RetrievalRequest, names: &mut Vec<String>) {
                names.retain(|n| n != "deploy-process");
            }
        }

        let hooks: Vec<Arc<dyn RetrievalHook>> = vec![Arc::new(Expands)];
        let names = retrieve_with_hooks(
            &SubstringRetriever,
            &hooks,
            &store(),
            &NeverCalledModel,
            RetrievalRequest {
                query: "ship".into(),
                limit: 5,
                already_surfaced: HashSet::new(),
                recent_tools: Vec::new(),
                model_name: "test".into(),
                session_id: None,
            },
        )
        .await;
        assert!(
            names.is_empty(),
            "the query rewrite found it and the result filter removed it: {names:?}"
        );

        // Without the filter the rewrite alone would have found it, which is
        // what makes the assertion above about both halves rather than one.
        let no_hooks: Vec<Arc<dyn RetrievalHook>> = Vec::new();
        let names = retrieve_with_hooks(
            &SubstringRetriever,
            &no_hooks,
            &store(),
            &NeverCalledModel,
            RetrievalRequest {
                query: "deploy".into(),
                limit: 5,
                already_surfaced: HashSet::new(),
                recent_tools: Vec::new(),
                model_name: "test".into(),
                session_id: None,
            },
        )
        .await;
        assert_eq!(names, ["deploy-process"]);
    }

    #[test]
    fn an_in_memory_store_round_trips_and_forgets() {
        let s = InMemoryStorage::default();
        assert_eq!(s.persist_batch(vec![memory("a", "x")]).unwrap(), 1);
        assert_eq!(s.load_all().len(), 1);
        assert_eq!(s.persist_batch(vec![memory("a", "y")]).unwrap(), 1);
        assert_eq!(s.load_all().len(), 1, "a rewrite is not a second memory");
        assert_eq!(s.load_all()[0].content, "y");
        assert!(s.remove("a").unwrap());
        assert!(!s.remove("a").unwrap(), "forgetting twice is not an error");
    }

    #[test]
    fn a_deployment_can_stop_the_model_being_told_to_write_memories() {
        let dir = std::path::Path::new("/tmp/memories");
        assert!(!DefaultExtraction.instructions(dir).is_empty());
        assert!(NoExtraction.instructions(dir).is_empty());
    }
}
