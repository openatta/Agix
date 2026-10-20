//! A `Model` that reports what the calls no turn made have cost.
//!
//! `api_request` was emitted from one place, inside the turn loop, so it
//! described exactly the calls a turn makes and nothing else. The engine
//! makes others: it extracts memories once a turn is over, summarizes for
//! compaction, answers a `"type": "prompt"` hook, classifies an intent. Every
//! one of them is a real request to a real provider that somebody pays for,
//! and a host reading this telemetry saw none of them — a session with memory
//! enabled undercounted by one call per turn, silently.
//!
//! They cannot be reported from where they are made. Two of those call sites
//! live in `base`, which cannot depend on this crate, and threading a handle
//! to each one would leave the next one to be added unreported again. What
//! they have in common is the model they call, so that is where the counting
//! goes: one decorator, and any call carrying an auxiliary [`CallOrigin`] is
//! accounted for by the fact that it happened.
//!
//! The turn's own calls stay with the turn loop. It knows things this does
//! not — which turn, how many messages went out, how many tools were offered
//! — and an event reported from both places would be counted twice. Which is
//! also the rule for wrapping: exactly once, around the model a session was
//! built with. A model fetched from somewhere else — the task router hands
//! out pool-level ones, shared and built before any session existed — is
//! outside that wrapping and has to be wrapped by whoever fetched it. There
//! is one such fetcher, `runtime::turn::Turn::accounted`.

use std::sync::Arc;
use std::time::Instant;

use base::interface::model::{
    CallKind, Model, ModelError, ModelEvent, ModelMessage, ModelStream, StreamParams, ToolDef,
};
use base::interface::prompt::PromptBlock;
use base::provider::ApiType;
use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::events::{ApiRequestPayload, TelemetryEvent};
use crate::handle::TelemetryHandle;

pub struct TelemetryModel {
    inner: Arc<dyn Model>,
    handle: TelemetryHandle,
    /// The model this session was configured with, so a call that went
    /// somewhere else can say so. Auxiliary work often does — memory
    /// extraction runs on a small one on purpose.
    configured_model: String,
}

impl TelemetryModel {
    pub fn new(inner: Arc<dyn Model>, handle: TelemetryHandle, configured_model: String) -> Self {
        Self {
            inner,
            handle,
            configured_model,
        }
    }
}

#[async_trait::async_trait]
impl Model for TelemetryModel {
    fn api_type(&self) -> ApiType {
        self.inner.api_type()
    }

    fn provenance(&self) -> Option<&str> {
        self.inner.provenance()
    }

    async fn stream(
        &self,
        prompt_blocks: Vec<PromptBlock>,
        tools: Vec<ToolDef>,
        messages: Vec<ModelMessage>,
        params: StreamParams,
        cancel: CancellationToken,
    ) -> Result<ModelStream, ModelError> {
        let auxiliary = params
            .origin
            .as_ref()
            .and_then(|origin| match &origin.kind {
                CallKind::Auxiliary { purpose } => {
                    Some((origin.session_id.clone(), purpose.clone()))
                }
                _ => None,
            });
        let Some((session_id, purpose)) = auxiliary else {
            return self
                .inner
                .stream(prompt_blocks, tools, messages, params, cancel)
                .await;
        };

        let started = Instant::now();
        let model = params.model.clone();
        let default_model = model == self.configured_model;
        let message_count = messages.len();
        let tool_count = tools.len();
        let stream = self
            .inner
            .stream(prompt_blocks, tools, messages, params, cancel)
            .await?;

        // The event is emitted from inside the stream because that is where
        // the answer is: usage arrives on the last event, and a caller that
        // abandons the stream early never gets one — the same call would then
        // be reported by the turn loop as having spent nothing either.
        let handle = self.handle.clone();
        let observed = stream.map(move |item| {
            if let Ok(ModelEvent::EndTurn { stop_reason, usage }) = &item {
                let _ = handle.record(TelemetryEvent::api_request(
                    &session_id,
                    0,
                    None,
                    ApiRequestPayload {
                        model: model.clone(),
                        input_tokens: usage.input_tokens as u64,
                        output_tokens: usage.output_tokens as u64,
                        cache_creation_tokens: usage.cache_creation_input_tokens as u64,
                        cache_read_tokens: usage.cache_read_input_tokens as u64,
                        latency_ms: started.elapsed().as_millis() as u64,
                        stop_reason: stop_reason.clone(),
                        input_message_count: message_count,
                        tool_count,
                        default_model,
                        purpose: Some(purpose.clone()),
                    },
                ));
            }
            item
        });
        Ok(Box::new(Box::pin(observed)))
    }
}
