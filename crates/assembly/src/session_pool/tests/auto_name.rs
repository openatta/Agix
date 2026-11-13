//! `auto_name_tests` 原样搬来的用例。

use super::mcp_for_project::test_pool;
use super::*;
use base::interface::model::{
    call_purpose, Model, ModelError, ModelEvent, ModelMessage, ModelStream, StreamParams, ToolDef,
    Usage,
};
use base::prompt::PromptBlock;
use base::provider::{ApiType, ResolvedModel, TaskRouter};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Answers every call: a naming call with `title`, anything else with a
/// line of text. Remembers what each call was for and which model it
/// asked for.
struct Answering {
    title: &'static str,
    calls: std::sync::Mutex<Vec<(Option<String>, String)>>,
}

impl Answering {
    fn new(title: &'static str) -> Arc<Self> {
        Arc::new(Self {
            title,
            calls: Default::default(),
        })
    }

    fn naming_calls(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(purpose, _)| purpose.as_deref() == Some(call_purpose::TITLE))
            .map(|(_, model)| model.clone())
            .collect()
    }
}

#[async_trait::async_trait]
impl Model for Answering {
    fn api_type(&self) -> ApiType {
        ApiType::Anthropic
    }

    async fn stream(
        &self,
        _: Vec<PromptBlock>,
        _: Vec<ToolDef>,
        _: Vec<ModelMessage>,
        params: StreamParams,
        _: CancellationToken,
    ) -> Result<ModelStream, ModelError> {
        let purpose = params
            .origin
            .as_ref()
            .and_then(|o| o.purpose())
            .map(str::to_string);
        let text = if purpose.as_deref() == Some(call_purpose::TITLE) {
            self.title
        } else {
            "好的，这就整理。"
        };
        self.calls.lock().unwrap().push((purpose, params.model));
        let events = vec![
            Ok(ModelEvent::TextDelta { text: text.into() }),
            Ok(ModelEvent::EndTurn {
                stop_reason: "end_turn".into(),
                usage: Usage::default(),
            }),
        ];
        Ok(Box::new(futures::stream::iter(events)))
    }
}

/// What a replay must never reach: fails every call, and counts them.
#[derive(Default)]
struct Unreachable {
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl Model for Unreachable {
    fn api_type(&self) -> ApiType {
        ApiType::Anthropic
    }

    async fn stream(
        &self,
        _: Vec<PromptBlock>,
        _: Vec<ToolDef>,
        _: Vec<ModelMessage>,
        _: StreamParams,
        _: CancellationToken,
    ) -> Result<ModelStream, ModelError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(ModelError::auth("invalid x-api-key"))
    }
}

/// Serve `project` on `scene` with `model`, and `task_router` when given,
/// in place of what its settings would build.
async fn bind(
    pool: &SessionPool,
    project: &Path,
    scene: &str,
    model: Arc<dyn Model>,
    task_router: Option<TaskRouter>,
) {
    let mut settings = (*pool.bindings.pool_settings().await).clone();
    settings.values.memory_enabled = false;
    let mut binding = pool.bindings.binding_from(settings);
    binding.model = model;
    binding.task_router = task_router.map(Arc::new);
    pool.bindings
        .insert((Some(project.to_path_buf()), scene.to_string()), binding)
        .await;
}

fn recorder(mode: &str, root: &Path) -> Option<crate::session_pool::api::RecorderOptions> {
    Some(crate::session_pool::api::RecorderOptions {
        mode: mode.into(),
        name: Some("naming".into()),
        dir: Some(root.display().to_string()),
        strict: mode == "replay",
    })
}

async fn harnix_session(
    pool: &SessionPool,
    project: &Path,
    recorder: Option<crate::session_pool::api::RecorderOptions>,
) -> String {
    pool.activate_scene("harnix")
        .await
        .expect("harnix is built in");
    let options = SessionOptions {
        recorder,
        ..Default::default()
    };
    pool.create_session(
        Some("harnix"),
        ProjectSelector::Path(project.to_path_buf()),
        Some(&options),
    )
    .await
    .expect("a harnix session")
    .session_id
}

/// The first turn's `session.send` result.
async fn first_turn(pool: &SessionPool, sid: &str) -> serde_json::Value {
    let request = TurnRequest {
        session_id: Some(sid.to_string()),
        message: "把发布流程整理成一份清单".into(),
        attachments: vec![],
        turn_id: "t-1".into(),
        options: None,
        on_busy: OnBusy::Reject,
        initiator: Initiator::Client,
    };
    match pool.run_turn(request, None).await {
        Ok(TurnOutcome::Completed {
            session_id, name, ..
        }) => {
            serde_json::json!({"session_id": session_id, "name": name})
        }
        other => panic!("the turn did not complete: {other:?}"),
    }
}

fn naming_calls_in(root: &Path) -> Vec<String> {
    telemetry::recorder::reader::load(&root.join("naming"))
        .expect("a recording")
        .calls
        .into_iter()
        .filter(|c| c.request.purpose.as_deref() == Some(call_purpose::TITLE))
        .map(|c| c.request.params.model)
        .collect()
}

#[tokio::test]
async fn a_replay_whose_recording_never_named_leaves_the_session_unnamed_without_calling_out() {
    let (pool, dir) = test_pool().await;
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let recordings = dir.path().join("recordings");

    bind(&pool, &project, "harnix", Answering::new("发布清单"), None).await;
    let recorded = harnix_session(&pool, &project, recorder("record", &recordings)).await;
    pool.sessions
        .lock()
        .await
        .get_mut(&recorded)
        .unwrap()
        .named_by_user = true;
    first_turn(&pool, &recorded).await;
    assert!(naming_calls_in(&recordings).is_empty());

    let unreachable = Arc::new(Unreachable::default());
    bind(&pool, &project, "harnix", unreachable.clone(), None).await;
    bind(&pool, pool.cwd(), "code", unreachable.clone(), None).await;
    let replayed = harnix_session(&pool, &project, recorder("replay", &recordings)).await;
    let result = first_turn(&pool, &replayed).await;

    assert_eq!(result["name"], serde_json::Value::Null, "{result}");
    assert_eq!(unreachable.calls.load(Ordering::SeqCst), 0);
    let namer = pool.sessions.lock().await[&replayed].namer.clone();
    assert_eq!(
        namer.name("把发布流程整理成一份清单").await,
        Ok(None),
        "a naming call the recording lacks is skipped, not reported as a failure"
    );
    assert_eq!(unreachable.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_recorded_name_comes_back_on_replay_without_calling_out() {
    let (pool, dir) = test_pool().await;
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let recordings = dir.path().join("recordings");

    bind(&pool, &project, "harnix", Answering::new("发布清单"), None).await;
    let recorded = harnix_session(&pool, &project, recorder("record", &recordings)).await;
    let result = first_turn(&pool, &recorded).await;
    assert_eq!(result["name"], "发布清单", "{result}");
    assert_eq!(naming_calls_in(&recordings).len(), 1);

    let unreachable = Arc::new(Unreachable::default());
    bind(&pool, &project, "harnix", unreachable.clone(), None).await;
    bind(&pool, pool.cwd(), "code", unreachable.clone(), None).await;
    let replayed = harnix_session(&pool, &project, recorder("replay", &recordings)).await;
    let result = first_turn(&pool, &replayed).await;

    assert_eq!(result["name"], "发布清单", "{result}");
    assert_eq!(unreachable.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_session_is_named_through_its_own_projects_title_route() {
    let (pool, dir) = test_pool().await;
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).unwrap();

    let main = Answering::new("不该用这个");
    let titler = Answering::new("发布清单");
    let router = TaskRouter::new(
        HashMap::from([
            ("main".to_string(), main.clone() as Arc<dyn Model>),
            ("titler".to_string(), titler.clone() as Arc<dyn Model>),
        ]),
        HashMap::from([(
            call_purpose::TITLE.to_string(),
            ResolvedModel {
                provider_id: "titler".into(),
                model: "title-model".into(),
            },
        )]),
        main.clone(),
    );
    bind(&pool, &project, "harnix", main.clone(), Some(router)).await;
    let sid = harnix_session(&pool, &project, None).await;
    let result = first_turn(&pool, &sid).await;

    assert_eq!(result["name"], "发布清单", "{result}");
    assert_eq!(titler.naming_calls(), ["title-model"]);
    assert!(main.naming_calls().is_empty());
}
