//! 会话首轮命名找谁：项目配了 `title` 路由就走它，否则走会话的对话模型。

use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Where a session's first-turn naming call goes: the session's own `title`
/// route when its project has one, else its conversation model.
///
/// Taken from the clients the session was built with, so a recorded session
/// records the call and a replayed one is answered from the recording.
#[derive(Clone)]
pub(in crate::session_pool) struct SessionNamer {
    session_id: String,
    model: Arc<dyn base::interface::model::Model>,
    model_name: String,
}

impl SessionNamer {
    pub(in crate::session_pool) fn of(
        session_id: &str,
        task_router: Option<&base::provider::TaskRouter>,
        model: &Arc<dyn base::interface::model::Model>,
        model_name: &str,
    ) -> Self {
        let title = base::interface::model::call_purpose::TITLE;
        let routed = task_router.and_then(|router| {
            Some((
                router.model_for(title),
                router.model_name_for(title)?.to_string(),
            ))
        });
        let (model, model_name) = routed.unwrap_or_else(|| (model.clone(), model_name.to_string()));
        Self {
            session_id: session_id.to_string(),
            model,
            model_name,
        }
    }

    /// `Ok(None)` when a replay holds no naming call: the session stays
    /// unnamed rather than the call going anywhere else.
    pub(in crate::session_pool) async fn name(
        &self,
        prompt: &str,
    ) -> Result<Option<String>, String> {
        use base::interface::model::{
            CallOrigin, MessageRole, ModelContentBlock, ModelErrorKind, ModelEvent, ModelMessage,
            StreamParams,
        };
        use base::prompt::PromptBlock;
        use futures::StreamExt;

        let system = PromptBlock::system(
            "你是一个简洁的标题生成器。只输出 3-5 个词的中文标题，不要任何解释。",
        );
        let messages = vec![ModelMessage {
            role: MessageRole::User,
            content: vec![ModelContentBlock::Text {
                text: prompt.to_string(),
            }],
        }];
        let params = StreamParams {
            model: self.model_name.clone(),
            max_tokens: 50,
            thinking_mode: base::settings::ThinkingMode::Off,
            effort: None,
            fallback_model: None,
            cache_edits: vec![],
            origin: Some(CallOrigin::auxiliary(
                self.session_id.clone(),
                base::interface::model::call_purpose::TITLE,
            )),
            input_map: None,
            provider_search: None,
        };

        let stream = match self
            .model
            .stream(
                vec![system],
                vec![],
                messages,
                params,
                CancellationToken::new(),
            )
            .await
        {
            Ok(stream) => stream,
            Err(e) if e.kind == ModelErrorKind::NotRecorded => return Ok(None),
            Err(e) => return Err(format!("LLM name error: {e}")),
        };

        tokio::pin!(stream);
        let mut name = String::new();
        while let Some(event) = stream.next().await {
            match event {
                Ok(ModelEvent::TextDelta { text }) => name.push_str(&text),
                Ok(ModelEvent::EndTurn { .. }) => break,
                Err(e) => return Err(format!("LLM name stream error: {e}")),
                _ => {}
            }
        }

        let name = name.trim().trim_matches('"').trim().to_string();
        if name.is_empty() {
            Err("empty name generated".into())
        } else {
            Ok(Some(name))
        }
    }
}
