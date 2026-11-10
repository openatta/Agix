//! 回合后记忆抽取与模型/场景提示的流向。

use crate::turn::memory_extraction::extract_memories_after_turn;
use base::interface::memory::{DurableMemory, MemoryStore, MemoryType};
use base::interface::model::{MessageRole, ModelContentBlock, ModelMessage};
use std::sync::Arc;

use std::sync::Mutex;

/// Records the `StreamParams`/request text it was called with instead of
/// hitting a real provider, and returns a canned "[]" response so
/// `extract_memories_after_turn` completes without persisting anything.
struct CapturingModel {
    captured: Mutex<Option<(base::interface::model::StreamParams, String)>>,
}

impl CapturingModel {
    fn new() -> Self {
        Self {
            captured: Mutex::new(None),
        }
    }
}

#[async_trait::async_trait]
impl base::interface::model::Model for CapturingModel {
    fn api_type(&self) -> base::provider::ApiType {
        base::provider::ApiType::Anthropic
    }
    async fn stream(
        &self,
        _prompt_blocks: Vec<base::interface::prompt::PromptBlock>,
        _tools: Vec<base::interface::model::ToolDef>,
        messages: Vec<ModelMessage>,
        params: base::interface::model::StreamParams,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError> {
        let prompt_text = messages
            .first()
            .and_then(|m| m.content.first())
            .and_then(|b| match b {
                ModelContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
            .unwrap_or_default();
        *self.captured.lock().unwrap() = Some((params, prompt_text));
        let events = vec![Ok(base::interface::model::ModelEvent::TextDelta {
            text: "[]".to_string(),
        })];
        Ok(Box::new(futures::stream::iter(events)))
    }
}

fn user_msg(text: &str) -> ModelMessage {
    ModelMessage {
        role: MessageRole::User,
        content: vec![ModelContentBlock::Text {
            text: text.to_string(),
        }],
    }
}

fn tmp_memory_store() -> Arc<MemoryStore> {
    let base = std::env::temp_dir().join(format!("agix-test-memstore-{}", uuid::Uuid::new_v4()));
    Arc::new(MemoryStore::new(base.join("user"), base.join("local")))
}

#[tokio::test]
async fn model_name_flows_into_stream_params() {
    let store = tmp_memory_store();
    let messages = vec![user_msg("hello"), user_msg("world")];
    let model = CapturingModel::new();

    extract_memories_after_turn(
        store.clone(),
        &messages,
        &model,
        "custom-vendor-model",
        None,
        &base::interface::environment::SystemEnvironment,
        "test-session",
    )
    .await;

    let (params, _) = model
        .captured
        .lock()
        .unwrap()
        .take()
        .expect("stream() was called");
    assert_eq!(params.model, "custom-vendor-model");
}

#[tokio::test]
async fn no_prompt_override_uses_default_codebase_intro() {
    let store = tmp_memory_store();
    let messages = vec![user_msg("hello"), user_msg("world")];
    let model = CapturingModel::new();

    extract_memories_after_turn(
        store.clone(),
        &messages,
        &model,
        "m",
        None,
        &base::interface::environment::SystemEnvironment,
        "test-session",
    )
    .await;

    let (_, prompt_text) = model
        .captured
        .lock()
        .unwrap()
        .take()
        .expect("stream() was called");
    assert!(prompt_text.contains("codebase or git history"));
}

#[tokio::test]
async fn scene_prompt_override_replaces_default_intro() {
    let store = tmp_memory_store();
    let messages = vec![user_msg("hello"), user_msg("world")];
    let model = CapturingModel::new();

    extract_memories_after_turn(
        store.clone(),
        &messages,
        &model,
        "m",
        Some("Extract facts about the user's chat preferences."),
        &base::interface::environment::SystemEnvironment,
        "test-session",
    )
    .await;

    let (_, prompt_text) = model
        .captured
        .lock()
        .unwrap()
        .take()
        .expect("stream() was called");
    assert!(prompt_text.contains("chat preferences"));
    assert!(!prompt_text.contains("codebase or git history"));
    // Fixed JSON-schema instructions must survive regardless of override.
    assert!(prompt_text.contains("kebab-case slug"));
}

/// Returns a canned memory extraction: one `project` memory and one `user`
/// memory, so the routing of each type can be observed on disk.
struct ExtractingModel;

#[async_trait::async_trait]
impl base::interface::model::Model for ExtractingModel {
    fn api_type(&self) -> base::provider::ApiType {
        base::provider::ApiType::Anthropic
    }
    async fn stream(
        &self,
        _prompt_blocks: Vec<base::interface::prompt::PromptBlock>,
        _tools: Vec<base::interface::model::ToolDef>,
        _messages: Vec<ModelMessage>,
        _params: base::interface::model::StreamParams,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError> {
        let json = r#"[
              {"name":"release-freeze","description":"merge freeze","content":"Freeze after 2026-03-05.","type":"project","confidence":0.9},
              {"name":"user-is-a-go-dev","description":"user background","content":"Ten years of Go.","type":"user","confidence":0.9}
            ]"#;
        let events = vec![Ok(base::interface::model::ModelEvent::TextDelta {
            text: json.to_string(),
        })];
        Ok(Box::new(futures::stream::iter(events)))
    }
}

/// The extraction prompt asks the model to classify each memory, and
/// `project` means "about this working directory". Persisting the whole
/// batch through the unscoped write path landed every project memory in the
/// global directory, where it followed the user into unrelated projects.
#[tokio::test]
async fn project_memories_are_extracted_into_the_local_scope() {
    let tmp = tempfile::tempdir().unwrap();
    let user_dir = tmp.path().join("user");
    let local_dir = tmp.path().join("local");
    let store = Arc::new(MemoryStore::new(user_dir.clone(), local_dir.clone()));
    let messages = vec![user_msg("hello"), user_msg("world")];

    extract_memories_after_turn(
        store.clone(),
        &messages,
        &ExtractingModel,
        "m",
        None,
        &base::interface::environment::SystemEnvironment,
        "test-session",
    )
    .await;

    assert!(
        local_dir.join("release-freeze.md").exists(),
        "type: project must be written to the project-local dir"
    );
    assert!(
        !user_dir.join("release-freeze.md").exists(),
        "project memory leaked into the global dir"
    );
    assert!(
        user_dir.join("user-is-a-go-dev.md").exists(),
        "type: user must be written to the global dir"
    );
    assert!(!local_dir.join("user-is-a-go-dev.md").exists());
    // Both indices reflect their own contents.
    let local_index =
        std::fs::read_to_string(local_dir.join(base::interface::memory::INDEX_FILE)).unwrap();
    assert!(local_index.contains("release-freeze"));
    assert!(!local_index.contains("user-is-a-go-dev"));
}

/// Hands back `self.0` as the extraction call's answer.
struct Answering(&'static str);

#[async_trait::async_trait]
impl base::interface::model::Model for Answering {
    fn api_type(&self) -> base::provider::ApiType {
        base::provider::ApiType::Anthropic
    }
    async fn stream(
        &self,
        _prompt_blocks: Vec<base::interface::prompt::PromptBlock>,
        _tools: Vec<base::interface::model::ToolDef>,
        _messages: Vec<ModelMessage>,
        _params: base::interface::model::StreamParams,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError> {
        let events = vec![Ok(base::interface::model::ModelEvent::TextDelta {
            text: self.0.to_string(),
        })];
        Ok(Box::new(futures::stream::iter(events)))
    }
}

async fn extract_with(store: &Arc<MemoryStore>, answer: &'static str) {
    extract_memories_after_turn(
        store.clone(),
        &[user_msg("hello"), user_msg("world")],
        &Answering(answer),
        "m",
        None,
        &base::interface::environment::SystemEnvironment,
        "test-session",
    )
    .await;
}

#[tokio::test]
async fn an_extracted_memory_holding_a_key_is_not_saved() {
    let store = tmp_memory_store();
    extract_with(
            &store,
            r#"[
              {"name":"deploy-key","description":"how to deploy","content":"use token sk-abcdefghijklmnopqrstuvwx","type":"project"},
              {"name":"deploy-day","description":"when to deploy","content":"Tuesdays.","type":"project"}
            ]"#,
        )
        .await;
    let names: Vec<String> = store.load_all().into_iter().map(|m| m.name).collect();
    assert_eq!(names, vec!["deploy-day".to_string()]);
}

fn old_memories(count: usize) -> Vec<DurableMemory> {
    (0..count)
        .map(|i| DurableMemory {
            name: format!("m{i}"),
            description: String::new(),
            memory_type: MemoryType::User,
            content: String::new(),
            source_session_id: String::new(),
            confidence: 0.9,
            last_seen: "2020-01-01T00:00:00Z".into(),
            recall_count: 0,
        })
        .collect()
}

fn names_in(store: &MemoryStore) -> std::collections::BTreeSet<String> {
    store.load_all().into_iter().map(|m| m.name).collect()
}

#[tokio::test]
async fn a_full_store_stops_extraction_and_loses_nothing() {
    const CAP: usize = base::interface::memory::MAX_MEMORIES;
    let store = tmp_memory_store();
    store.persist_batch(old_memories(CAP)).unwrap();
    let before = names_in(&store);
    extract_with(
        &store,
        r#"[{"name":"one-more","description":"d","content":"c","type":"user"}]"#,
    )
    .await;
    assert_eq!(names_in(&store), before);
}

#[tokio::test]
async fn extraction_fills_the_store_up_to_its_cap_and_no_further() {
    const CAP: usize = base::interface::memory::MAX_MEMORIES;
    let store = tmp_memory_store();
    store.persist_batch(old_memories(CAP - 1)).unwrap();
    extract_with(
        &store,
        r#"[
              {"name":"m0","description":"d","content":"updated","type":"user"},
              {"name":"first-new","description":"d","content":"c","type":"user"},
              {"name":"second-new","description":"d","content":"c","type":"user"}
            ]"#,
    )
    .await;
    let names = names_in(&store);
    assert_eq!(names.len(), CAP);
    assert!(names.contains("first-new") && !names.contains("second-new"));
    let m0 = store
        .load_all()
        .into_iter()
        .find(|m| m.name == "m0")
        .unwrap();
    assert_eq!(
        m0.content, "updated",
        "a memory already held is still updated"
    );
}

/// Sets its flag when dropped: the task holding it was either finished or
/// aborted, and is not running any more.
struct Gone(Arc<std::sync::atomic::AtomicBool>);

impl Drop for Gone {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

fn never_finishing(tasks: &mut tokio::task::JoinSet<()>) -> Arc<std::sync::atomic::AtomicBool> {
    let gone = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let guard = Gone(gone.clone());
    tasks.spawn(async move {
        let _guard = guard;
        std::future::pending::<()>().await;
    });
    gone
}

#[tokio::test]
async fn an_extraction_that_finishes_in_time_is_waited_for() {
    let mut tasks = tokio::task::JoinSet::new();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let marks = done.clone();
    tasks.spawn(async move {
        let _ = rx.await;
        marks.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    tx.send(()).unwrap();
    assert!(crate::turn::finish_within(&mut tasks, crate::turn::MEMORY_EXTRACTION_GRACE).await);
    assert!(done.load(std::sync::atomic::Ordering::SeqCst));
}

#[tokio::test]
async fn an_extraction_still_running_past_the_grace_is_aborted() {
    let mut tasks = tokio::task::JoinSet::new();
    let gone = never_finishing(&mut tasks);
    assert!(!crate::turn::finish_within(&mut tasks, std::time::Duration::ZERO).await);
    assert!(gone.load(std::sync::atomic::Ordering::SeqCst));
    assert!(tasks.is_empty());
}

/// A stopping engine does not leave an extraction it started running behind
/// it.
#[tokio::test]
async fn a_stopped_engine_leaves_no_extraction_running() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut agent, _rx, _tx) = crate::agent::Builder::new()
        .scene(Arc::new(scene::code::CodeScene::default()))
        .model(Arc::new(super::support::FixedUsageModel {
            usage: Default::default(),
        }))
        .settings(Arc::new(super::support::recall_test_settings(tmp.path())))
        .skip_warmup(true)
        .build()
        .expect("build should succeed");
    let gone = never_finishing(&mut agent.memory.extractions);
    let cancel = tokio_util::sync::CancellationToken::new();
    cancel.cancel();
    agent.run(cancel).await;
    assert!(gone.load(std::sync::atomic::Ordering::SeqCst));
}
