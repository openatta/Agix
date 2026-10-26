//! Recorder — records every LLM call and replays it deterministically.
//!
//! A `Model` decorator sitting on `Model::stream()`, the one place in the
//! engine that sees a complete request and a complete response at once: the
//! turn loop assembles the request but hands the stream away, and the stream
//! consumer sees the response but not what produced it.
//!
//! What it produces is diagnostic data, not session state. It takes no part in
//! resume, fork, or context reconstruction — deleting a recording leaves the
//! session working exactly as before. `crates/history` remains the sole
//! authority on what a conversation is.
//!
//! Replay matches by **position**, not by hashing the request: the k-th live
//! call replays the k-th recorded one. Hash-keyed lookup makes every
//! environment-dependent byte in a prompt a correctness problem, and when it
//! misses it can only report that it missed. Recording the whole request
//! instead means a mismatch can name the field that moved.
//!
//! **A recording holds everything the model saw, in the clear.** System
//! prompts, the whole conversation, tool arguments and results — verbatim, and
//! deliberately not passed through [`crate::RedactionPolicy`], which exists for
//! telemetry events and would defeat the point here: a recording is worth
//! keeping only if it says what was actually sent. So a recording carries
//! whatever secrets the prompt carried, and its directory deserves the same
//! handling as the transcript it came from. Recording is off unless configured.

pub mod blob;
pub mod format;
pub mod override_doc;
pub mod pack;
pub mod reader;
pub mod rerun;
pub mod writer;

use async_trait::async_trait;
use base::interface::model::{
    Model, ModelError, ModelEvent, ModelMessage, ModelStream, StreamParams, ToolDef, Usage,
};
use base::interface::prompt::PromptBlock;
use base::interface::settings::{Divergence, RecorderConfig, RecorderMode};
use base::provider::ApiType;
use std::collections::HashMap;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use tokio_util::sync::CancellationToken;

use blob::BlobId;
use format::{
    now_ms, CallRecord, ChunkRecord, EndRecord, Header, Outcome, Record, RecordedError,
    RecordedParams, FORMAT_VERSION,
};
use pack::RunPacker;
use reader::Recording;
use writer::RecordingWriter;

/// The recording a call belongs to, or nothing.
///
/// There used to be a shared `unattributed` name for calls with no session
/// identity. It filed genuinely unrelated calls together, and since a writer
/// truncates the file it opens, two concurrent sessions using it erased each
/// other. A call that cannot name its session has no recording to belong to,
/// and saying so is better than inventing one.
fn recording_name_of(config: &RecorderConfig, params: &StreamParams) -> Option<String> {
    if let Some(name) = &config.name {
        return Some(name.clone());
    }
    params.origin.as_ref().map(|o| o.session_id.clone())
}

/// Everything a set of recordings shares: the open writers and the replay
/// cursors, keyed by recording name.
///
/// Held behind an `Arc` and shared by every [`RecorderModel`] that records into
/// the same root. Multi-provider routing hands out one `Model` per provider,
/// and each of them needs wrapping to be recorded at all — but they must not
/// each open their own writer for the same file, or the second one to write
/// truncates the first one's recording out from under it. One shared state, one
/// writer per name, appends serialized by that writer's lock.
#[derive(Default)]
pub struct Recorder {
    writers: Mutex<HashMap<String, Arc<RecordingWriter>>>,
    replays: Mutex<HashMap<String, ReplayState>>,
}

impl Recorder {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Report any replay that did not run to the end of what was recorded.
    ///
    /// A run that made *fewer* calls than the recording holds passes every
    /// other check — nothing diverged, nothing ran out — while having quietly
    /// stopped doing part of the work. Overshooting is already an error at the
    /// call that overshoots; this is the other direction, and it can only be
    /// asked at the end. Returns one line per unfinished session, empty when
    /// every cursor was drained.
    pub fn unconsumed(&self) -> Vec<String> {
        let replays = self.replays.lock().unwrap_or_else(|e| e.into_inner());
        let mut out = Vec::new();
        for (name, state) in replays.iter() {
            for recorded in &state.recorded_sessions {
                let total = state.calls_of(recorded).len();
                let used = state.cursor(recorded);
                if used < total {
                    out.push(format!(
                        "recording {name:?} session {recorded:?}: replayed {used} of {total} call(s)"
                    ));
                }
            }
        }
        out
    }
}

pub struct RecorderModel {
    inner: Arc<dyn Model>,
    config: Option<RecorderConfig>,
    engine_version: String,
    /// The provider id this instance fronts, as named in settings.json. The
    /// api type alone cannot tell two OpenAI-compatible endpoints apart.
    provider_id: Option<String>,
    shared: Arc<Recorder>,
}

/// One recording being replayed, sliced per session.
///
/// A single cursor over the whole file works only when the file holds one
/// session. When a parent and its sub-agents record under one name their calls
/// interleave, and the order they interleave in is a scheduling detail that
/// will not repeat — so each live session walks only the calls that were made
/// by its counterpart, and the interleaving is allowed to differ.
struct ReplayState {
    recording: Recording,
    /// Recorded session ids in order of first appearance, which is the order
    /// live sessions bind to them when the ids themselves don't match.
    recorded_sessions: Vec<String>,
    /// Live session id → the recorded session it replays against.
    bound: HashMap<String, String>,
    /// Recorded session id → how many of its calls have been handed out.
    ///
    /// Kept apart from `bound` so advancing a cursor needs no second lookup
    /// into `bound` — one that could only be written as an `unwrap()` resting
    /// on an invariant held across two functions with nothing enforcing it.
    consumed: HashMap<String, usize>,
}

impl ReplayState {
    fn new(recording: Recording) -> Self {
        let mut recorded_sessions: Vec<String> = Vec::new();
        for call in &recording.calls {
            let session = call
                .request
                .session_id
                .clone()
                .unwrap_or_else(|| recording.header.session_id.clone());
            if !recorded_sessions.contains(&session) {
                recorded_sessions.push(session);
            }
        }
        Self {
            recording,
            recorded_sessions,
            bound: HashMap::new(),
            consumed: HashMap::new(),
        }
    }

    /// Which recorded session `live` replays against.
    ///
    /// An id that appears in the recording binds to itself — a replay driven
    /// with the same session ids it was recorded with is exact. Otherwise
    /// sessions bind in the order they first call, which is what a live run
    /// can guarantee: the parent necessarily streams before it can delegate.
    /// Returns the recorded session id, or `None` when the run has introduced
    /// more distinct sessions than the recording holds.
    fn bind(&mut self, live: &str) -> Option<String> {
        if let Some(recorded) = self.bound.get(live) {
            return Some(recorded.clone());
        }
        let recorded = if self.recorded_sessions.iter().any(|s| s == live) {
            live.to_string()
        } else {
            self.recorded_sessions
                .iter()
                .find(|s| !self.bound.values().any(|taken| taken == *s))?
                .clone()
        };
        self.bound.insert(live.to_string(), recorded.clone());
        Some(recorded)
    }

    fn cursor(&self, recorded_session: &str) -> usize {
        self.consumed.get(recorded_session).copied().unwrap_or(0)
    }

    fn advance(&mut self, recorded_session: &str) {
        *self
            .consumed
            .entry(recorded_session.to_string())
            .or_insert(0) += 1;
    }

    fn calls_of(&self, recorded_session: &str) -> Vec<&reader::RecordedCall> {
        self.recording
            .calls
            .iter()
            .filter(|c| {
                c.request
                    .session_id
                    .as_deref()
                    .unwrap_or(&self.recording.header.session_id)
                    == recorded_session
            })
            .collect()
    }
}

impl RecorderModel {
    pub fn new(
        inner: Arc<dyn Model>,
        config: Option<RecorderConfig>,
        engine_version: &str,
    ) -> Self {
        Self::shared(inner, config, engine_version, Recorder::new())
    }

    /// Wrap `inner` sharing `shared` with every other model recording into the
    /// same root — the constructor multi-provider routing uses, so a session's
    /// calls land in one recording however many providers served them.
    pub fn shared(
        inner: Arc<dyn Model>,
        config: Option<RecorderConfig>,
        engine_version: &str,
        shared: Arc<Recorder>,
    ) -> Self {
        Self {
            inner,
            config: config.or_else(Self::env_config),
            engine_version: engine_version.to_string(),
            provider_id: None,
            shared,
        }
    }

    pub fn with_provider_id(mut self, provider_id: impl Into<String>) -> Self {
        self.provider_id = Some(provider_id.into());
        self
    }

    /// `AGIX_RECORD=<name>` / `AGIX_REPLAY=<name>` with `AGIX_RECORDINGS_DIR`
    /// for the root. Absent all of them, the decorator is a pass-through.
    fn env_config() -> Option<RecorderConfig> {
        let root = std::env::var("AGIX_RECORDINGS_DIR")
            .ok()
            .map(PathBuf::from)?;
        let on_divergence = Self::default_divergence();
        if let Ok(name) = std::env::var("AGIX_RECORD") {
            Some(RecorderConfig {
                mode: RecorderMode::Record,
                name: Some(name),
                root,
                on_divergence,
            })
        } else if let Ok(name) = std::env::var("AGIX_REPLAY") {
            Some(RecorderConfig {
                mode: RecorderMode::Replay,
                name: Some(name),
                root,
                on_divergence,
            })
        } else {
            None
        }
    }

    fn writer_for(
        &self,
        config: &RecorderConfig,
        name: &str,
        params: &StreamParams,
    ) -> Arc<RecordingWriter> {
        let mut writers = self
            .shared
            .writers
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        writers
            .entry(name.to_string())
            .or_insert_with(|| {
                Arc::new(RecordingWriter::create(
                    &config.root,
                    Header {
                        version: FORMAT_VERSION,
                        name: name.to_string(),
                        session_id: params
                            .origin
                            .as_ref()
                            .map(|o| o.session_id.clone())
                            .unwrap_or_default(),
                        // Fixed by the first call, which is why a sub-agent has
                        // to know its lineage at spawn time rather than
                        // discovering it later.
                        parent: params
                            .origin
                            .as_ref()
                            .and_then(|o| o.parent_session_id.clone()),
                        agent_type: params.origin.as_ref().and_then(|o| o.agent_type.clone()),
                        created_at: now_ms(),
                        engine_version: self.engine_version.clone(),
                    },
                ))
            })
            .clone()
    }
}

fn flag(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| v != "0" && !v.is_empty())
}

/// The environment facts that decide the default divergence policy. Split from
/// the lookups so the policy is a pure function that can be tested without
/// mutating process-global environment variables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DivergenceInputs {
    forced_strict: bool,
    forced_warn: bool,
    in_test: bool,
    in_ci: bool,
}

fn resolve_divergence(inputs: DivergenceInputs) -> Divergence {
    if inputs.forced_strict {
        return Divergence::Strict;
    }
    if inputs.forced_warn {
        return Divergence::Warn;
    }
    // Strict wherever a divergence means a broken test; lenient only for a
    // human driving a replay by hand, so a brand-new case still runs before
    // anyone has recorded it.
    if inputs.in_test || inputs.in_ci {
        Divergence::Strict
    } else {
        Divergence::Warn
    }
}

impl RecorderModel {
    /// The divergence policy a harness should ask for rather than re-derive.
    ///
    /// Public because a harness that builds an explicit [`RecorderConfig`]
    /// never reaches [`RecorderModel::env_config`], and re-deriving this is
    /// how the equivalent flag once shipped as a silent no-op.
    ///
    /// Overrides, highest precedence first: `AGIX_REPLAY_STRICT=1`, then
    /// `AGIX_REPLAY_WARN=1`.
    pub fn default_divergence() -> Divergence {
        resolve_divergence(DivergenceInputs {
            forced_strict: flag("AGIX_REPLAY_STRICT"),
            forced_warn: flag("AGIX_REPLAY_WARN"),
            in_test: std::env::var("CARGO_TEST_RUNNER").is_ok()
                || std::env::var("AGIX_RECORDER_AUTO_DETECT").is_ok(),
            in_ci: flag("CI"),
        })
    }
}

/// The parts of a request that identify it, as blob ids. Computed for both a
/// live request and a recorded one so replay can compare them without loading
/// content that matches.
struct RequestShape {
    params: RecordedParams,
    system: Vec<BlobId>,
    tools: BlobId,
    messages: Vec<BlobId>,
}

impl RequestShape {
    fn of(
        prompt_blocks: &[PromptBlock],
        tools: &[ToolDef],
        messages: &[ModelMessage],
        params: &StreamParams,
    ) -> Self {
        Self {
            params: RecordedParams {
                model: params.model.clone(),
                max_tokens: params.max_tokens,
                thinking_mode: params.thinking_mode.clone(),
                fallback_model: params.fallback_model.clone(),
                cache_edits: params.cache_edits.clone(),
            },
            system: prompt_blocks.iter().map(BlobId::of_value).collect(),
            tools: BlobId::of_value(&tools),
            messages: messages.iter().map(BlobId::of_value).collect(),
        }
    }
}

/// Field-level differences between what was recorded and what the live run
/// produced. Empty means the two agree.
fn diverges(recorded: &CallRecord, live: &RequestShape) -> Vec<String> {
    let mut out = Vec::new();
    if recorded.params != live.params {
        out.push(format!(
            "params: recorded {:?}, live {:?}",
            recorded.params, live.params
        ));
    }
    if recorded.system.len() != live.system.len() {
        out.push(format!(
            "system: recorded {} blocks, live {}",
            recorded.system.len(),
            live.system.len()
        ));
    } else {
        for (i, (a, b)) in recorded.system.iter().zip(&live.system).enumerate() {
            if a != b {
                out.push(format!("system[{i}]: recorded {a}, live {b}"));
            }
        }
    }
    if recorded.tools != live.tools {
        out.push(format!(
            "tools: recorded {}, live {}",
            recorded.tools, live.tools
        ));
    }
    if recorded.messages.len() != live.messages.len() {
        out.push(format!(
            "messages: recorded {} messages, live {}",
            recorded.messages.len(),
            live.messages.len()
        ));
    } else {
        for (i, (a, b)) in recorded.messages.iter().zip(&live.messages).enumerate() {
            if a != b {
                out.push(format!("messages[{i}]: recorded {a}, live {b}"));
            }
        }
    }
    out
}

#[async_trait]
impl Model for RecorderModel {
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
        let Some(config) = self.config.clone() else {
            return self
                .inner
                .stream(prompt_blocks, tools, messages, params, cancel)
                .await;
        };
        let Some(name) = recording_name_of(&config, &params) else {
            return self
                .inner
                .stream(prompt_blocks, tools, messages, params, cancel)
                .await;
        };

        match config.mode {
            RecorderMode::Replay => {
                let shape = RequestShape::of(&prompt_blocks, &tools, &messages, &params);
                let live_session = params
                    .origin
                    .as_ref()
                    .map(|o| o.session_id.as_str())
                    .unwrap_or_default();
                self.replay(&config, &name, live_session, shape)
            }
            RecorderMode::Record => {
                let writer = self.writer_for(&config, &name, &params);
                self.record(writer, prompt_blocks, tools, messages, params, cancel)
                    .await
            }
            // Rerun drives the provider from a recording rather than from a
            // live session, so it does not sit on this path — see
            // [`rerun::rerun_one`]. A session that somehow runs under this mode
            // just talks to the model, which is the only honest thing to do
            // here: intercepting would mean answering a live request with a
            // response recorded for some other request.
            RecorderMode::Rerun => {
                self.inner
                    .stream(prompt_blocks, tools, messages, params, cancel)
                    .await
            }
        }
    }
}

impl RecorderModel {
    async fn record(
        &self,
        writer: Arc<RecordingWriter>,
        prompt_blocks: Vec<PromptBlock>,
        tools: Vec<ToolDef>,
        messages: Vec<ModelMessage>,
        params: StreamParams,
        cancel: CancellationToken,
    ) -> Result<ModelStream, ModelError> {
        let call_seq = writer.next_seq();
        let started = std::time::Instant::now();

        // Written before the stream opens, and flushed: a process killed
        // mid-response still leaves behind the request that was in flight.
        if let Err(e) = self.write_call(
            &writer,
            call_seq,
            &prompt_blocks,
            &tools,
            &messages,
            &params,
        ) {
            tracing::warn!(error = %e, "recorder: failed to write call record, continuing unrecorded");
        }

        let result = self
            .inner
            .stream(prompt_blocks, tools, messages, params, cancel)
            .await;

        match result {
            Ok(inner) => Ok(Box::new(RecordingStream {
                inner,
                writer,
                packer: RunPacker::new(),
                call_seq,
                started,
                stop_reason: String::new(),
                usage: Usage::default(),
                finished: false,
            })),
            Err(e) => {
                write_end(
                    &writer,
                    call_seq,
                    Outcome::Error {
                        error: RecordedError::from(&e),
                    },
                    None,
                    None,
                    started,
                );
                Err(e)
            }
        }
    }

    fn write_call(
        &self,
        writer: &RecordingWriter,
        call_seq: u64,
        prompt_blocks: &[PromptBlock],
        tools: &[ToolDef],
        messages: &[ModelMessage],
        params: &StreamParams,
    ) -> std::io::Result<()> {
        let blobs = writer.blobs();
        let system = prompt_blocks
            .iter()
            .map(|b| blobs.put(b))
            .collect::<Result<Vec<_>, _>>()?;
        let tools_id = blobs.put(&tools)?;
        let message_ids = messages
            .iter()
            .map(|m| blobs.put(m))
            .collect::<Result<Vec<_>, _>>()?;

        let (turn, step) = params
            .origin
            .as_ref()
            .map(|o| o.turn_step())
            .unwrap_or((0, 0));

        writer.append(&Record::Call(Box::new(CallRecord {
            seq: call_seq,
            ts: now_ms(),
            session_id: params.origin.as_ref().map(|o| o.session_id.clone()),
            parent_session_id: params
                .origin
                .as_ref()
                .and_then(|o| o.parent_session_id.clone()),
            agent_type: params.origin.as_ref().and_then(|o| o.agent_type.clone()),
            turn,
            step,
            purpose: params
                .origin
                .as_ref()
                .and_then(|o| o.purpose())
                .map(str::to_string),
            // The provider as configured, when routing named one. Falling back
            // to the api type keeps single-provider recordings readable, but it
            // cannot distinguish two OpenAI-compatible endpoints — which is why
            // the routed path supplies the id.
            provider: self.provider_id.clone().unwrap_or_else(|| {
                match self.inner.api_type() {
                    ApiType::Anthropic => "anthropic",
                    ApiType::OpenAICompatible => "openai_compatible",
                }
                .into()
            }),
            api_type: self.inner.api_type(),
            params: RecordedParams {
                model: params.model.clone(),
                max_tokens: params.max_tokens,
                thinking_mode: params.thinking_mode.clone(),
                fallback_model: params.fallback_model.clone(),
                cache_edits: params.cache_edits.clone(),
            },
            input_map: params.input_map.clone(),
            system,
            tools: tools_id,
            messages: message_ids,
        })))?;
        writer.flush()
    }

    fn replay(
        &self,
        config: &RecorderConfig,
        name: &str,
        live_session: &str,
        shape: RequestShape,
    ) -> Result<ModelStream, ModelError> {
        let mut replays = self
            .shared
            .replays
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let state = match replays.get_mut(name) {
            Some(state) => state,
            None => {
                let dir = config.root.join(name);
                let mut recording = reader::load(&dir).map_err(|e| {
                    ModelError::Internal(format!("recorder: cannot replay {name:?}: {e}"))
                })?;
                if recording.damaged > 0 {
                    tracing::warn!(
                        name,
                        damaged = recording.damaged,
                        "recorder: replaying a recording with unreadable lines"
                    );
                }
                // A hand-written override is deliberate, so a broken one fails
                // the replay instead of being skipped — an edit that silently
                // did nothing is worse than one that says why it could not.
                if let Some(doc) = override_doc::load(&dir)
                    .map_err(|e| ModelError::Internal(format!("recorder: {name:?}: {e}")))?
                {
                    let mut responses: Vec<Vec<ModelEvent>> =
                        recording.calls.iter().map(|c| c.response.clone()).collect();
                    override_doc::apply(&doc, &mut responses)
                        .map_err(|e| ModelError::Internal(format!("recorder: {name:?}: {e}")))?;
                    for (call, response) in recording.calls.iter_mut().zip(responses) {
                        call.response = response;
                    }
                    tracing::info!(name, "recorder: replaying with an override applied");
                }
                replays
                    .entry(name.to_string())
                    .or_insert(ReplayState::new(recording))
            }
        };

        let sessions = state.recorded_sessions.len();
        let Some(recorded_session) = state.bind(live_session) else {
            return Err(ModelError::Internal(format!(
                "recorder: recording {name:?} holds {sessions} session(s), \
                 live run introduced another ({live_session:?})"
            )));
        };
        let cursor = state.cursor(&recorded_session);

        let calls = state.calls_of(&recorded_session);
        let Some(call) = calls.get(cursor).copied().cloned() else {
            return Err(ModelError::Internal(format!(
                "recorder: recording {:?} has {} call(s) for session {:?}, \
                 live run asked for #{}",
                name,
                calls.len(),
                recorded_session,
                cursor + 1
            )));
        };
        state.advance(&recorded_session);

        let differences = diverges(&call.request, &shape);
        if !differences.is_empty() {
            let report = format!(
                "recorder: call #{} of session {:?} diverges from recording {:?}:\n  {}",
                cursor + 1,
                recorded_session,
                name,
                differences.join("\n  ")
            );
            match config.on_divergence {
                Divergence::Strict => return Err(ModelError::Internal(report)),
                Divergence::Warn => tracing::warn!("{report}"),
            }
        }

        if let Some(end) = &call.end {
            if let Outcome::Error { error } = &end.outcome {
                return Err(error.clone().into_model_error());
            }
        }

        let events: Vec<Result<ModelEvent, ModelError>> =
            call.response.iter().cloned().map(Ok).collect();
        Ok(Box::new(futures::stream::iter(events)))
    }
}

fn write_end(
    writer: &RecordingWriter,
    call_seq: u64,
    outcome: Outcome,
    stop_reason: Option<String>,
    usage: Option<Usage>,
    started: std::time::Instant,
) {
    let record = Record::End(EndRecord {
        seq: writer.next_seq(),
        ts: now_ms(),
        call: call_seq,
        outcome,
        stop_reason,
        usage,
        duration_ms: started.elapsed().as_millis() as u64,
    });
    if let Err(e) = writer.append(&record).and_then(|()| writer.flush()) {
        tracing::warn!(error = %e, "recorder: failed to write end record");
    }
}

/// Forwards the model's stream downstream while writing every event.
///
/// Recording happens on the way through rather than after collection: buffering
/// the whole response before returning it would make the recorder visible in
/// the agent's own latency, and would lose everything if the process died
/// mid-stream.
struct RecordingStream {
    inner: ModelStream,
    writer: Arc<RecordingWriter>,
    packer: RunPacker,
    call_seq: u64,
    started: std::time::Instant,
    stop_reason: String,
    usage: Usage,
    finished: bool,
}

impl RecordingStream {
    fn write(&mut self, mut records: Vec<Record>) {
        for record in &mut records {
            let from = self.writer.next_seq_block(pack::seq_width(record));
            pack::stamp_seqs(record, from);
            if let Err(e) = self.writer.append(record) {
                tracing::warn!(error = %e, "recorder: failed to write chunk");
                return;
            }
        }
    }

    fn finish(&mut self, outcome: Outcome) {
        if self.finished {
            return;
        }
        self.finished = true;
        let tail = self.packer.flush();
        self.write(tail);
        let stop_reason = (!self.stop_reason.is_empty()).then(|| self.stop_reason.clone());
        write_end(
            &self.writer,
            self.call_seq,
            outcome,
            stop_reason,
            Some(self.usage),
            self.started,
        );
    }
}

impl futures::Stream for RecordingStream {
    type Item = Result<ModelEvent, ModelError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_next(cx) {
            Poll::Ready(Some(Ok(event))) => {
                if let ModelEvent::EndTurn { stop_reason, usage } = &event {
                    this.stop_reason = stop_reason.clone();
                    this.usage = *usage;
                }
                let records = this.packer.push(ChunkRecord {
                    // Numbered on the way out, not here — see `pack::stamp_seqs`.
                    seq: 0,
                    ts: now_ms(),
                    call: this.call_seq,
                    chunk: event.clone(),
                });
                this.write(records);
                Poll::Ready(Some(Ok(event)))
            }
            Poll::Ready(Some(Err(e))) => {
                this.finish(Outcome::Error {
                    error: RecordedError::from(&e),
                });
                Poll::Ready(Some(Err(e)))
            }
            Poll::Ready(None) => {
                this.finish(Outcome::Ok);
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

/// A stream dropped before it ended was cancelled — the turn was interrupted,
/// or a caller stopped reading. Closing the call here is what keeps every
/// `call` record paired with an `end`.
impl Drop for RecordingStream {
    fn drop(&mut self) {
        self.finish(Outcome::Cancelled);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::interface::model::CallOrigin;
    use base::interface::prompt::BlockRole;
    use base::interface::settings::ThinkingMode;
    use futures::StreamExt;

    /// Replays a fixed script, or fails to open when `open_error` is set.
    struct ScriptedModel {
        events: Vec<ModelEvent>,
        open_error: bool,
    }

    impl ScriptedModel {
        fn ok(events: Vec<ModelEvent>) -> Arc<dyn Model> {
            Arc::new(Self {
                events,
                open_error: false,
            })
        }

        fn overloaded() -> Arc<dyn Model> {
            Arc::new(Self {
                events: vec![],
                open_error: true,
            })
        }
    }

    #[async_trait]
    impl Model for ScriptedModel {
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
            if self.open_error {
                return Err(ModelError::Overloaded);
            }
            let events: Vec<Result<ModelEvent, ModelError>> =
                self.events.iter().cloned().map(Ok).collect();
            Ok(Box::new(futures::stream::iter(events)))
        }
    }

    fn script() -> Vec<ModelEvent> {
        vec![
            ModelEvent::TextDelta { text: "Hel".into() },
            ModelEvent::TextDelta { text: "lo".into() },
            ModelEvent::TextDelta { text: "!".into() },
            ModelEvent::EndTurn {
                stop_reason: "end_turn".into(),
                usage: Usage {
                    input_tokens: 11,
                    output_tokens: 3,
                    ..Default::default()
                },
            },
        ]
    }

    fn config(
        mode: RecorderMode,
        root: &std::path::Path,
        on_divergence: Divergence,
    ) -> RecorderConfig {
        RecorderConfig {
            mode,
            name: Some("run".into()),
            root: root.to_path_buf(),
            on_divergence,
        }
    }

    fn prompt(text: &str) -> Vec<PromptBlock> {
        vec![PromptBlock {
            role: BlockRole::System,
            content: text.into(),
            cache_strategy: None,
            name: None,
            origin: Default::default(),
        }]
    }

    fn tools() -> Vec<ToolDef> {
        vec![ToolDef {
            name: "Bash".into(),
            description: "run".into(),
            input_schema: serde_json::json!({}),
            source: None,
        }]
    }

    fn params(session: &str, step: u32) -> StreamParams {
        StreamParams {
            model: "test-model".into(),
            max_tokens: 100,
            thinking_mode: ThinkingMode::Off,
            fallback_model: None,
            cache_edits: vec![],
            origin: Some(CallOrigin::turn(session, 0, step)),
            input_map: None,
        }
    }

    async fn drain(stream: ModelStream) -> Vec<ModelEvent> {
        stream.filter_map(|e| async { e.ok() }).collect().await
    }

    #[tokio::test]
    async fn unconfigured_recorder_writes_nothing() {
        let root = tempfile::tempdir().unwrap();
        let recorder = RecorderModel::new(ScriptedModel::ok(script()), None, "test");
        let stream = recorder
            .stream(
                prompt("sys"),
                tools(),
                vec![],
                params("S1", 0),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(drain(stream).await.len(), 4);
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn record_then_replay_reproduces_the_event_stream() {
        let root = tempfile::tempdir().unwrap();

        let recorder = RecorderModel::new(
            ScriptedModel::ok(script()),
            Some(config(
                RecorderMode::Record,
                root.path(),
                Divergence::Strict,
            )),
            "test",
        );
        let recorded = drain(
            recorder
                .stream(
                    prompt("sys"),
                    tools(),
                    vec![],
                    params("S1", 0),
                    CancellationToken::new(),
                )
                .await
                .unwrap(),
        )
        .await;
        drop(recorder);

        // The replaying model must never reach the inner one.
        let replayer = RecorderModel::new(
            ScriptedModel::ok(vec![]),
            Some(config(
                RecorderMode::Replay,
                root.path(),
                Divergence::Strict,
            )),
            "test",
        );
        let replayed = drain(
            replayer
                .stream(
                    prompt("sys"),
                    tools(),
                    vec![],
                    params("S1", 0),
                    CancellationToken::new(),
                )
                .await
                .unwrap(),
        )
        .await;

        assert_eq!(format!("{replayed:?}"), format!("{recorded:?}"));
        assert_eq!(replayed.len(), 4);
    }

    #[tokio::test]
    async fn token_boundaries_survive_a_record_replay_round_trip() {
        let root = tempfile::tempdir().unwrap();
        let recorder = RecorderModel::new(
            ScriptedModel::ok(script()),
            Some(config(
                RecorderMode::Record,
                root.path(),
                Divergence::Strict,
            )),
            "test",
        );
        drain(
            recorder
                .stream(
                    prompt("sys"),
                    tools(),
                    vec![],
                    params("S1", 0),
                    CancellationToken::new(),
                )
                .await
                .unwrap(),
        )
        .await;
        drop(recorder);

        let recording = reader::load(&root.path().join("run")).unwrap();
        let texts: Vec<&str> = recording.calls[0]
            .response
            .iter()
            .filter_map(|e| match e {
                ModelEvent::TextDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["Hel", "lo", "!"], "deltas must not be merged");
    }

    #[tokio::test]
    async fn a_recorded_failure_replays_as_a_failure() {
        let root = tempfile::tempdir().unwrap();
        let recorder = RecorderModel::new(
            ScriptedModel::overloaded(),
            Some(config(
                RecorderMode::Record,
                root.path(),
                Divergence::Strict,
            )),
            "test",
        );
        let err = recorder
            .stream(
                prompt("sys"),
                tools(),
                vec![],
                params("S1", 0),
                CancellationToken::new(),
            )
            .await
            .err()
            .expect("expected an error");
        assert!(matches!(err, ModelError::Overloaded));
        drop(recorder);

        let replayer = RecorderModel::new(
            ScriptedModel::ok(script()),
            Some(config(
                RecorderMode::Replay,
                root.path(),
                Divergence::Strict,
            )),
            "test",
        );
        let replayed = replayer
            .stream(
                prompt("sys"),
                tools(),
                vec![],
                params("S1", 0),
                CancellationToken::new(),
            )
            .await;
        assert!(
            matches!(replayed, Err(ModelError::Overloaded)),
            "a recorded overload must replay as an overload, not as a success"
        );
    }

    #[tokio::test]
    async fn divergence_names_the_field_that_moved() {
        let root = tempfile::tempdir().unwrap();
        let recorder = RecorderModel::new(
            ScriptedModel::ok(script()),
            Some(config(
                RecorderMode::Record,
                root.path(),
                Divergence::Strict,
            )),
            "test",
        );
        drain(
            recorder
                .stream(
                    prompt("original system prompt"),
                    tools(),
                    vec![],
                    params("S1", 0),
                    CancellationToken::new(),
                )
                .await
                .unwrap(),
        )
        .await;
        drop(recorder);

        let replayer = RecorderModel::new(
            ScriptedModel::ok(vec![]),
            Some(config(
                RecorderMode::Replay,
                root.path(),
                Divergence::Strict,
            )),
            "test",
        );
        let err = replayer
            .stream(
                prompt("a different system prompt"),
                tools(),
                vec![],
                params("S1", 0),
                CancellationToken::new(),
            )
            .await
            .err()
            .expect("expected an error");
        let message = err.to_string();
        assert!(
            message.contains("system[0]"),
            "the report must point at the field, got: {message}"
        );
    }

    #[tokio::test]
    async fn warn_mode_plays_the_recording_despite_divergence() {
        let root = tempfile::tempdir().unwrap();
        let recorder = RecorderModel::new(
            ScriptedModel::ok(script()),
            Some(config(
                RecorderMode::Record,
                root.path(),
                Divergence::Strict,
            )),
            "test",
        );
        drain(
            recorder
                .stream(
                    prompt("original"),
                    tools(),
                    vec![],
                    params("S1", 0),
                    CancellationToken::new(),
                )
                .await
                .unwrap(),
        )
        .await;
        drop(recorder);

        let replayer = RecorderModel::new(
            ScriptedModel::ok(vec![]),
            Some(config(RecorderMode::Replay, root.path(), Divergence::Warn)),
            "test",
        );
        let replayed = drain(
            replayer
                .stream(
                    prompt("changed"),
                    tools(),
                    vec![],
                    params("S1", 0),
                    CancellationToken::new(),
                )
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(replayed.len(), 4);
    }

    #[tokio::test]
    async fn a_stream_dropped_mid_response_records_a_cancelled_outcome() {
        let root = tempfile::tempdir().unwrap();
        let recorder = RecorderModel::new(
            ScriptedModel::ok(script()),
            Some(config(
                RecorderMode::Record,
                root.path(),
                Divergence::Strict,
            )),
            "test",
        );
        {
            let mut stream = recorder
                .stream(
                    prompt("sys"),
                    tools(),
                    vec![],
                    params("S1", 0),
                    CancellationToken::new(),
                )
                .await
                .unwrap();
            let _first = stream.next().await;
        }
        drop(recorder);

        let recording = reader::load(&root.path().join("run")).unwrap();
        let end = recording.calls[0]
            .end
            .as_ref()
            .expect("call must be closed");
        assert_eq!(end.outcome, Outcome::Cancelled);
    }

    #[tokio::test]
    async fn the_request_is_on_disk_before_the_response_arrives() {
        let root = tempfile::tempdir().unwrap();
        let recorder = RecorderModel::new(
            ScriptedModel::ok(script()),
            Some(config(
                RecorderMode::Record,
                root.path(),
                Divergence::Strict,
            )),
            "test",
        );
        let _stream = recorder
            .stream(
                prompt("sys"),
                tools(),
                vec![],
                params("S1", 0),
                CancellationToken::new(),
            )
            .await
            .unwrap();

        let content = std::fs::read_to_string(root.path().join("run").join("calls.jsonl")).unwrap();
        assert!(
            content.contains(r#""type":"call""#),
            "the request must be durable before the stream is consumed"
        );
    }

    #[tokio::test]
    async fn unchanged_request_parts_are_stored_once() {
        let root = tempfile::tempdir().unwrap();
        let recorder = RecorderModel::new(
            ScriptedModel::ok(script()),
            Some(config(
                RecorderMode::Record,
                root.path(),
                Divergence::Strict,
            )),
            "test",
        );
        for step in 0..3 {
            drain(
                recorder
                    .stream(
                        prompt("stable system prompt"),
                        tools(),
                        vec![],
                        params("S1", step),
                        CancellationToken::new(),
                    )
                    .await
                    .unwrap(),
            )
            .await;
        }
        drop(recorder);

        let recording = reader::load(&root.path().join("run")).unwrap();
        assert_eq!(recording.calls.len(), 3);
        // One blob for the single system block, one for the tool table.
        let blobs = std::fs::read_dir(root.path().join("run").join("blobs"))
            .unwrap()
            .count();
        assert_eq!(blobs, 2, "identical request parts must dedup across calls");
    }

    #[tokio::test]
    async fn each_session_records_into_its_own_directory() {
        let root = tempfile::tempdir().unwrap();
        let recorder = RecorderModel::new(
            ScriptedModel::ok(script()),
            Some(RecorderConfig {
                mode: RecorderMode::Record,
                name: None,
                root: root.path().to_path_buf(),
                on_divergence: Divergence::Strict,
            }),
            "test",
        );
        for session in ["parent", "subagent"] {
            drain(
                recorder
                    .stream(
                        prompt("sys"),
                        tools(),
                        vec![],
                        params(session, 0),
                        CancellationToken::new(),
                    )
                    .await
                    .unwrap(),
            )
            .await;
        }
        drop(recorder);

        assert!(root.path().join("parent").join("calls.jsonl").exists());
        assert!(root.path().join("subagent").join("calls.jsonl").exists());
        assert_eq!(
            reader::load(&root.path().join("parent"))
                .unwrap()
                .calls
                .len(),
            1
        );
    }

    /// The header is what lets a directory of recordings be read as one tree.
    #[tokio::test]
    async fn a_sub_agents_header_names_its_parent_and_kind() {
        let root = tempfile::tempdir().unwrap();
        let recorder = RecorderModel::new(
            ScriptedModel::ok(vec![]),
            Some(RecorderConfig {
                mode: RecorderMode::Record,
                name: None,
                root: root.path().to_path_buf(),
                on_divergence: Divergence::Strict,
            }),
            "test",
        );
        let mut child = params("child", 0);
        child.origin = child
            .origin
            .map(|o| o.with_lineage(Some("parent".into()), Some("code-reviewer".into())));
        drain(
            recorder
                .stream(
                    prompt("sys"),
                    tools(),
                    vec![],
                    child,
                    CancellationToken::new(),
                )
                .await
                .unwrap(),
        )
        .await;
        drop(recorder);

        let header = reader::load(&root.path().join("child")).unwrap().header;
        assert_eq!(header.parent.as_deref(), Some("parent"));
        assert_eq!(header.agent_type.as_deref(), Some("code-reviewer"));
    }

    /// An auxiliary call belongs to the session that made it, not to a shared
    /// name that concurrent sessions would then truncate out from under it.
    #[tokio::test]
    async fn an_auxiliary_call_is_filed_under_its_own_session() {
        let root = tempfile::tempdir().unwrap();
        let recorder = RecorderModel::new(
            ScriptedModel::ok(vec![]),
            Some(RecorderConfig {
                mode: RecorderMode::Record,
                name: None,
                root: root.path().to_path_buf(),
                on_divergence: Divergence::Strict,
            }),
            "test",
        );
        let mut aux = params("S9", 0);
        aux.origin = Some(CallOrigin::auxiliary("S9", "compact"));
        drain(
            recorder
                .stream(
                    prompt("sys"),
                    tools(),
                    vec![],
                    aux,
                    CancellationToken::new(),
                )
                .await
                .unwrap(),
        )
        .await;
        drop(recorder);

        assert!(
            !root.path().join("unattributed").exists(),
            "auxiliary calls must not land in a shared directory"
        );
        let call = &reader::load(&root.path().join("S9")).unwrap().calls[0].request;
        assert_eq!(call.purpose.as_deref(), Some("compact"));
        assert_eq!(
            (call.turn, call.step),
            (0, 0),
            "a call outside a turn has no turn coordinates to report"
        );
    }

    /// A parent and its sub-agent recording under one name interleave on disk,
    /// and the order they interleave in is scheduling luck. Replay must hand
    /// each session its own calls in its own order, not the file's order.
    #[tokio::test]
    async fn interleaved_sessions_replay_as_separate_slices() {
        let root = tempfile::tempdir().unwrap();
        let config = |mode| RecorderConfig {
            mode,
            // One shared name: this is the setup that interleaves.
            name: Some("run".into()),
            root: root.path().to_path_buf(),
            on_divergence: Divergence::Strict,
        };

        let recorder = RecorderModel::new(
            ScriptedModel::ok(vec![ModelEvent::TextDelta { text: "x".into() }]),
            Some(config(RecorderMode::Record)),
            "test",
        );
        // parent, child, child, parent — the child's calls land between the
        // parent's, exactly as a real delegation would.
        for (session, step) in [("P", 0), ("C", 0), ("C", 1), ("P", 1)] {
            let mut p = params(session, step);
            p.origin = Some(CallOrigin::turn(session, 0, step));
            drain(
                recorder
                    .stream(prompt("sys"), tools(), vec![], p, CancellationToken::new())
                    .await
                    .unwrap(),
            )
            .await;
        }
        drop(recorder);

        let recording = reader::load(&root.path().join("run")).unwrap();
        assert_eq!(recording.calls.len(), 4);

        // Replaying the child first must still get the child's two calls, even
        // though the parent's call is first in the file.
        let replayer = RecorderModel::new(
            ScriptedModel::ok(vec![]),
            Some(config(RecorderMode::Replay)),
            "test",
        );
        for step in [0, 1] {
            let mut p = params("C", step);
            p.origin = Some(CallOrigin::turn("C", 0, step));
            let events = drain(
                replayer
                    .stream(prompt("sys"), tools(), vec![], p, CancellationToken::new())
                    .await
                    .unwrap(),
            )
            .await;
            assert_eq!(events.len(), 1, "child call #{step} should replay");
        }
        // A third child call is one more than the child ever made.
        let mut extra = params("C", 2);
        extra.origin = Some(CallOrigin::turn("C", 0, 2));
        let Err(err) = replayer
            .stream(
                prompt("sys"),
                tools(),
                vec![],
                extra,
                CancellationToken::new(),
            )
            .await
        else {
            panic!("a third child call has nothing to replay against")
        };
        assert!(
            err.to_string().contains("2 call(s) for session"),
            "the error should name the session that ran out: {err}"
        );
    }

    /// Live session ids are freshly random every run, so the ids in a recording
    /// are never the ids of the run replaying it. Binding therefore falls back
    /// to first-call order — and since that is the *production* path, exact-id
    /// matching alone proving out is not enough.
    #[tokio::test]
    async fn sessions_with_new_ids_bind_in_first_call_order() {
        let root = tempfile::tempdir().unwrap();
        let config = |mode| RecorderConfig {
            mode,
            name: Some("run".into()),
            root: root.path().to_path_buf(),
            on_divergence: Divergence::Warn,
        };

        let recorder = RecorderModel::new(
            ScriptedModel::ok(vec![ModelEvent::TextDelta { text: "x".into() }]),
            Some(config(RecorderMode::Record)),
            "test",
        );
        // Parent calls first, then its child — the order a live run guarantees.
        for (session, step) in [("REC-parent", 0), ("REC-child", 0), ("REC-parent", 1)] {
            let mut p = params(session, step);
            p.origin = Some(CallOrigin::turn(session, 0, step));
            drain(
                recorder
                    .stream(prompt("sys"), tools(), vec![], p, CancellationToken::new())
                    .await
                    .unwrap(),
            )
            .await;
        }
        drop(recorder);

        let replayer = RecorderModel::new(
            ScriptedModel::ok(vec![]),
            Some(config(RecorderMode::Replay)),
            "test",
        );
        // Entirely different ids, same order of first call.
        let mut consumed = Vec::new();
        for (session, step) in [("LIVE-a", 0), ("LIVE-b", 0), ("LIVE-a", 1)] {
            let mut p = params(session, step);
            p.origin = Some(CallOrigin::turn(session, 0, step));
            let events = drain(
                replayer
                    .stream(prompt("sys"), tools(), vec![], p, CancellationToken::new())
                    .await
                    .unwrap(),
            )
            .await;
            consumed.push(events.len());
        }
        assert_eq!(
            consumed,
            vec![1, 1, 1],
            "each live session should have replayed its counterpart's calls"
        );

        // `LIVE-a` claimed the parent (2 calls), so a third call on it is one
        // past the end — proof it bound to the parent and not the child.
        let mut extra = params("LIVE-a", 2);
        extra.origin = Some(CallOrigin::turn("LIVE-a", 0, 2));
        let Err(err) = replayer
            .stream(
                prompt("sys"),
                tools(),
                vec![],
                extra,
                CancellationToken::new(),
            )
            .await
        else {
            panic!("the parent's slice holds only two calls")
        };
        assert!(
            err.to_string().contains("REC-parent"),
            "the error should name the recorded session it bound to: {err}"
        );
    }

    /// An override edits the replayed response without touching the recording,
    /// which is what makes "what if the model had said X" a reviewable file
    /// rather than a destroyed fixture.
    #[tokio::test]
    async fn an_override_replaces_a_response_and_leaves_the_recording_alone() {
        let root = tempfile::tempdir().unwrap();
        let config = |mode| RecorderConfig {
            mode,
            name: Some("run".into()),
            root: root.path().to_path_buf(),
            on_divergence: Divergence::Strict,
        };

        let recorder = RecorderModel::new(
            ScriptedModel::ok(vec![ModelEvent::TextDelta {
                text: "recorded".into(),
            }]),
            Some(config(RecorderMode::Record)),
            "test",
        );
        drain(
            recorder
                .stream(
                    prompt("sys"),
                    tools(),
                    vec![],
                    params("S1", 0),
                    CancellationToken::new(),
                )
                .await
                .unwrap(),
        )
        .await;
        drop(recorder);

        let before = std::fs::read_to_string(root.path().join("run").join("calls.jsonl")).unwrap();
        std::fs::write(
            root.path().join("run").join(override_doc::OVERRIDE_FILE),
            r#"{"patches":[{"at":0,"response":[{"kind":"text_delta","text":"edited"}]}]}"#,
        )
        .unwrap();

        let replayer = RecorderModel::new(
            ScriptedModel::ok(vec![]),
            Some(config(RecorderMode::Replay)),
            "test",
        );
        let events = drain(
            replayer
                .stream(
                    prompt("sys"),
                    tools(),
                    vec![],
                    params("S1", 0),
                    CancellationToken::new(),
                )
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(
            format!("{events:?}"),
            format!(
                "{:?}",
                vec![ModelEvent::TextDelta {
                    text: "edited".into()
                }]
            )
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("run").join("calls.jsonl")).unwrap(),
            before,
            "the recording itself must be untouched"
        );
    }

    /// A run that stops short of the recording passes every per-call check.
    /// The only place to notice is at the end.
    #[tokio::test]
    async fn a_replay_that_stops_short_is_reported_at_the_end() {
        let root = tempfile::tempdir().unwrap();
        let config = |mode| RecorderConfig {
            mode,
            name: Some("run".into()),
            root: root.path().to_path_buf(),
            on_divergence: Divergence::Strict,
        };

        let recorder = RecorderModel::new(
            ScriptedModel::ok(vec![]),
            Some(config(RecorderMode::Record)),
            "test",
        );
        for step in [0, 1] {
            drain(
                recorder
                    .stream(
                        prompt("sys"),
                        tools(),
                        vec![],
                        params("S1", step),
                        CancellationToken::new(),
                    )
                    .await
                    .unwrap(),
            )
            .await;
        }
        drop(recorder);

        let shared = Recorder::new();
        let replayer = RecorderModel::shared(
            ScriptedModel::ok(vec![]),
            Some(config(RecorderMode::Replay)),
            "test",
            shared.clone(),
        );
        // Only one of the two recorded calls.
        drain(
            replayer
                .stream(
                    prompt("sys"),
                    tools(),
                    vec![],
                    params("S1", 0),
                    CancellationToken::new(),
                )
                .await
                .unwrap(),
        )
        .await;

        let unconsumed = shared.unconsumed();
        assert_eq!(unconsumed.len(), 1, "got {unconsumed:?}");
        assert!(
            unconsumed[0].contains("replayed 1 of 2"),
            "got {unconsumed:?}"
        );
    }

    /// A call that cannot name a session has no recording to belong to, and
    /// must pass straight through rather than inventing one.
    #[tokio::test]
    async fn a_call_with_no_session_is_not_recorded() {
        let root = tempfile::tempdir().unwrap();
        let recorder = RecorderModel::new(
            ScriptedModel::ok(vec![]),
            Some(RecorderConfig {
                mode: RecorderMode::Record,
                name: None,
                root: root.path().to_path_buf(),
                on_divergence: Divergence::Strict,
            }),
            "test",
        );
        let mut anonymous = params("ignored", 0);
        anonymous.origin = None;
        drain(
            recorder
                .stream(
                    prompt("sys"),
                    tools(),
                    vec![],
                    anonymous,
                    CancellationToken::new(),
                )
                .await
                .unwrap(),
        )
        .await;
        drop(recorder);

        assert_eq!(
            std::fs::read_dir(root.path()).unwrap().count(),
            0,
            "nothing should have been written"
        );
    }

    #[tokio::test]
    async fn replaying_past_the_end_of_a_recording_says_so() {
        let root = tempfile::tempdir().unwrap();
        let recorder = RecorderModel::new(
            ScriptedModel::ok(script()),
            Some(config(
                RecorderMode::Record,
                root.path(),
                Divergence::Strict,
            )),
            "test",
        );
        drain(
            recorder
                .stream(
                    prompt("sys"),
                    tools(),
                    vec![],
                    params("S1", 0),
                    CancellationToken::new(),
                )
                .await
                .unwrap(),
        )
        .await;
        drop(recorder);

        let replayer = RecorderModel::new(
            ScriptedModel::ok(vec![]),
            Some(config(
                RecorderMode::Replay,
                root.path(),
                Divergence::Strict,
            )),
            "test",
        );
        for _ in 0..1 {
            drain(
                replayer
                    .stream(
                        prompt("sys"),
                        tools(),
                        vec![],
                        params("S1", 0),
                        CancellationToken::new(),
                    )
                    .await
                    .unwrap(),
            )
            .await;
        }
        let err = replayer
            .stream(
                prompt("sys"),
                tools(),
                vec![],
                params("S1", 1),
                CancellationToken::new(),
            )
            .await
            .err()
            .expect("expected an error");
        assert!(err.to_string().contains("asked for #2"), "{err}");
    }
}
