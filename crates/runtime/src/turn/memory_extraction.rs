//! 回合结束后的持久记忆抽取。

use base::interface::memory::{DurableMemory, MemoryStore, MemoryType};
use base::interface::model::{MessageRole, ModelContentBlock, ModelMessage};
use std::sync::Arc;

/// How long a stopping session waits for its memory extractions. Under the
/// daemon's grace for a stopping engine, so a session brought back right away
/// is not refused for one.
pub(crate) const MEMORY_EXTRACTION_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

/// Wait up to `grace` for `tasks` to finish, then abort what is left.
/// Returns whether they all finished.
pub(crate) async fn finish_within(
    tasks: &mut tokio::task::JoinSet<()>,
    grace: std::time::Duration,
) -> bool {
    let finished =
        tokio::time::timeout(grace, async { while tasks.join_next().await.is_some() {} })
            .await
            .is_ok();
    if !finished {
        tracing::warn!(
            still_running = tasks.len(),
            "memory extraction had not finished when the session stopped; abandoned"
        );
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    }
    finished
}

/// Extract durable memories from recent conversation messages using a lightweight
/// model call, then persist them to the MemoryStore. Runs asynchronously after
/// each complete turn — failures are silently logged, never block the user.
///
/// `model`/`model_name` are resolved by the caller — normally via
/// `TaskRouter::model_for("memory")` / `model_name_for("memory")` when a
/// `task_models.memory` override is configured, falling
/// back to the main conversation model otherwise, mirroring
/// `AgentTool::Inner::model_for_subagent`'s pattern for the `"subagent"`
/// task type. `prompt_intro` is the scene's `memory_extraction_prompt()`
/// override, if any — see `Scene::memory_extraction_prompt`.
pub(crate) async fn extract_memories_after_turn(
    store: Arc<MemoryStore>,
    messages: &[ModelMessage],
    model: &dyn base::interface::model::Model,
    model_name: &str,
    prompt_intro: Option<&str>,
    environment: &dyn base::interface::environment::Environment,
    session_id: &str,
) {
    // Only extract if there are messages worth analyzing
    if messages.len() < 2 {
        return;
    }

    const CAP: usize = base::interface::memory::MAX_MEMORIES;
    let counting = store.clone();
    let held = tokio::task::spawn_blocking(move || counting.count())
        .await
        .unwrap_or(0);
    if held >= CAP {
        tracing::warn!(
            held,
            cap = CAP,
            "the memory store is full; no new memories are extracted until some are removed"
        );
        return;
    }

    // Build a lightweight prompt asking the model to extract memories
    let recent: Vec<&ModelMessage> = messages.iter().rev().take(20).collect();
    let messages_text: String = recent
        .iter()
        .rev()
        .filter_map(|m| {
            let texts: Vec<&str> = m
                .content
                .iter()
                .filter_map(|b| {
                    if let ModelContentBlock::Text { text } = b {
                        Some(text.as_str())
                    } else {
                        None
                    }
                })
                .collect();
            if texts.is_empty() {
                None
            } else {
                Some(texts.join("\n"))
            }
        })
        .collect::<Vec<_>>()
        .join("\n---\n");

    if messages_text.is_empty() {
        return;
    }

    const DEFAULT_MEMORY_EXTRACTION_INTRO: &str = "\
Extract any durable memories from this conversation excerpt. A durable \
memory is a fact about the user, project, or workflow that should persist \
across sessions. Only extract memories that are NOT derivable from the \
current codebase or git history.";
    let intro = prompt_intro.unwrap_or(DEFAULT_MEMORY_EXTRACTION_INTRO);
    let prompt = format!(
        "{intro}\n\n\
For each memory, return a JSON object with:\n\
- name: short kebab-case slug\n\
- description: 1-line summary used to decide relevance during recall\n\
- content: the fact; for feedback/project, follow with **Why:** and **How to apply:** lines\n\
- type: one of [user, feedback, project, reference]\n\
- confidence: 0.0-1.0 (default 0.8)\n\n\
Return only a JSON array of memories. If nothing is worth saving, return []."
    );

    use base::interface::settings::ThinkingMode;
    let request_messages = vec![ModelMessage {
        role: MessageRole::User,
        content: vec![
            ModelContentBlock::Text { text: prompt },
            ModelContentBlock::Text {
                text: messages_text,
            },
        ],
    }];
    let params = base::interface::model::StreamParams {
        model: model_name.to_string(),
        max_tokens: 2000,
        thinking_mode: ThinkingMode::Off,
        effort: None,
        fallback_model: None,
        cache_edits: vec![],
        // A call the session pays for, outside any turn. Saying so is what
        // lets it be counted; the recall half of this feature has always
        // said it (`base::memory`), and this half never did.
        origin: Some(base::interface::model::CallOrigin::auxiliary(
            session_id,
            base::interface::model::call_purpose::MEMORY,
        )),
        input_map: None,
        provider_search: None,
    };
    let mut full_text = String::new();
    let stream_result = model
        .stream(
            vec![],
            vec![],
            request_messages,
            params,
            tokio_util::sync::CancellationToken::new(),
        )
        .await;

    let Ok(mut stream) = stream_result else {
        return;
    };

    use futures::StreamExt;
    while let Some(Ok(event)) = stream.next().await {
        if let base::interface::model::ModelEvent::TextDelta { text } = event {
            full_text.push_str(&text);
        }
    }

    // Parse extracted memories
    let memories: Vec<DurableMemory> =
        if let Ok(parsed) = serde_json::from_str::<Vec<serde_json::Value>>(&full_text) {
            parsed
                .iter()
                .filter_map(|mem| {
                    let name = mem["name"].as_str()?.to_string();
                    let description = mem["description"].as_str().unwrap_or("").to_string();
                    let content = mem["content"].as_str().unwrap_or("").to_string();
                    let memory_type = match mem["type"].as_str().unwrap_or("user") {
                        "feedback" => MemoryType::Feedback,
                        "project" => MemoryType::Project,
                        "reference" => MemoryType::Reference,
                        _ => MemoryType::User,
                    };
                    let confidence = mem["confidence"].as_f64().unwrap_or(0.8);
                    if name.is_empty() || confidence < 0.3 {
                        return None;
                    }
                    let timestamp = environment
                        .now()
                        .format(&time::format_description::well_known::Iso8601::DEFAULT)
                        .unwrap_or_else(|_| "2026-01-01T00:00:00Z".to_string());
                    Some(DurableMemory {
                        name,
                        description,
                        memory_type,
                        content,
                        source_session_id: String::new(),
                        confidence,
                        last_seen: timestamp,
                        recall_count: 0,
                    })
                })
                .collect()
        } else {
            Vec::new()
        };

    // A conversation that quoted a key is no reason to keep the key.
    let memories: Vec<DurableMemory> = memories
        .into_iter()
        .filter(|memory| {
            let found = base::interface::memory::scan_for_secrets(memory);
            if !found.is_empty() {
                let kinds: Vec<&str> = found.iter().map(|f| f.kind).collect();
                tracing::warn!(memory = %memory.name, ?kinds, "not saving an extracted memory that holds a secret");
            }
            found.is_empty()
        })
        .collect();
    if memories.is_empty() {
        return;
    }

    // Route by the type the model was asked to classify each memory with.
    // `project` memories are about *this* working directory ("the merge
    // freeze starts Thursday", "the auth rewrite is compliance-driven") and
    // belong in the project-local directory; everything else describes the
    // user or external resources and is global. Persisting the whole batch
    // globally — which is what the unscoped `persist_batch` does for new
    // names — leaked one project's context into every other project.
    let scoped: Vec<base::interface::memory::ScopedMemory> = memories
        .into_iter()
        .map(|memory| {
            let scope = match memory.memory_type {
                MemoryType::Project => base::interface::memory::MemoryScope::Local,
                _ => base::interface::memory::MemoryScope::User,
            };
            base::interface::memory::ScopedMemory { memory, scope }
        })
        .collect();
    let persisted = tokio::task::spawn_blocking(move || {
        let held: std::collections::HashSet<String> =
            store.load_all().into_iter().map(|m| m.name).collect();
        let mut room = CAP.saturating_sub(held.len());
        let (admitted, refused): (Vec<_>, Vec<_>) = scoped.into_iter().partition(|sm| {
            held.contains(&sm.memory.name)
                || (room > 0 && {
                    room -= 1;
                    true
                })
        });
        if !refused.is_empty() {
            tracing::warn!(
                refused = refused.len(),
                cap = CAP,
                "the memory store is full; new memories past the cap are not saved"
            );
        }
        store.persist_batch_scoped(admitted)?;
        Ok::<_, base::interface::memory::MemoryError>(())
    })
    .await;
    match persisted {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::debug!(error = %e, "auto memory extraction: persist failed"),
        Err(e) => tracing::debug!(error = %e, "auto memory extraction: persist task failed"),
    }
}
