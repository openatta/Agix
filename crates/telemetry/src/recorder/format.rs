//! The on-disk vocabulary of `calls.jsonl`.
//!
//! One recording is a header line followed by records describing what left for
//! the model and what came back, in arrival order. A call's request is written
//! *before* the stream opens, so a recording torn by a crash still answers the
//! question a crash raises — what was in flight when it died.
//!
//! Bulky request parts are [`BlobId`] references (see [`super::blob`]); the
//! response is stored as the model events themselves rather than a parallel
//! representation, so there is no second shape that can drift from the first.

use base::interface::model::{InputMap, ModelError, ModelEvent, Usage};
use base::interface::settings::ThinkingMode;
use base::provider::ApiType;
use serde::{Deserialize, Serialize};

use super::blob::BlobId;

/// Bumped only for a change that makes existing recordings unreadable. Adding
/// a record type is not such a change — readers skip what they do not know
/// (see [`Record::Unknown`]).
pub const FORMAT_VERSION: u32 = 1;

/// One line of `calls.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Record {
    Recording(Header),
    /// Boxed: a `CallRecord` is three times the size of a `ChunkRecord`, and a
    /// `Record` is built once per streamed token — so an unboxed variant would
    /// make every chunk pay for the largest thing the enum can hold.
    Call(Box<CallRecord>),
    Chunk(ChunkRecord),
    TextChunks(TextRun),
    ThinkingChunks(TextRun),
    ToolArgsChunks(ToolArgsRun),
    End(EndRecord),

    /// A record type this build does not know. A recording is diagnostic data,
    /// not session state: reading the parts we understand beats refusing the
    /// whole file. The reader warns with the line number, because a genuinely
    /// corrupt line lands here too.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Header {
    pub version: u32,
    pub name: String,
    pub session_id: String,
    /// The session that spawned this one. With `created_at` and the per-record
    /// timestamps, this is what turns a directory of recordings into a single
    /// tree on one timeline instead of a pile of unrelated files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// Which kind of agent this session runs, when it is a sub-agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    pub created_at: u64,
    pub engine_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallRecord {
    pub seq: u64,
    pub ts: u64,
    /// Which session issued this call. The header names one session, but a
    /// recording configured with an explicit name holds a parent's calls and
    /// its sub-agents' interleaved, and replay hands each live session only its
    /// own — so the file has to say per call, not once at the top.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// The lineage of `session_id`, repeated on every call for the same reason
    /// `session_id` is: the header can only describe one session, and a
    /// recording written under an explicit name holds several. Without this a
    /// sub-agent's parentage is simply absent from any recording but its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    pub turn: u32,
    pub step: u32,
    /// Present for a call made outside any turn — `compact`, `memory`, and so
    /// on; the conventional values are
    /// [`base::interface::model::call_purpose`]'s constants. Absent
    /// for turn work, where `turn`/`step` are the coordinates that mean
    /// something. Deliberately not part of the replay divergence check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purpose: Option<String>,
    pub provider: String,
    pub api_type: ApiType,
    pub params: RecordedParams,
    /// One entry per assembled prompt block, in order. Kept per block rather
    /// than joined: the boundaries are what tell the system prompt, the skills
    /// inventory, and MCP instructions apart.
    pub system: Vec<BlobId>,
    pub tools: BlobId,
    pub messages: Vec<BlobId>,
    /// What the turn's user message is made of, as coordinates over
    /// `messages`. Absent for a call that started no turn, or when compaction
    /// folded the message away before this call went out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_map: Option<InputMap>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordedParams {
    pub model: String,
    pub max_tokens: u32,
    pub thinking_mode: ThinkingMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_model: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cache_edits: Vec<String>,
    /// `StreamParams::provider_search`'s `max_uses`, when the call asked the
    /// provider to search.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_max_uses: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkRecord {
    pub seq: u64,
    pub ts: u64,
    pub call: u64,
    pub chunk: ModelEvent,
}

/// A packed run of consecutive text or thinking deltas belonging to one call.
///
/// `texts` is never joined — token boundaries are evidence, not noise, and
/// keeping members separate is what makes the packing lossless regardless of
/// where content-block boundaries fell.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextRun {
    pub seq0: u64,
    pub ts0: u64,
    pub call: u64,
    pub texts: Vec<String>,
    /// Millisecond gaps between consecutive members; one shorter than `texts`.
    /// Signed because a wall clock can step backwards mid-stream.
    pub dt: Vec<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolArgsRun {
    pub seq0: u64,
    pub ts0: u64,
    pub call: u64,
    /// The tool_use id every member shares; a run with a mixed id never packs.
    pub id: String,
    pub args: Vec<String>,
    pub dt: Vec<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EndRecord {
    pub seq: u64,
    pub ts: u64,
    pub call: u64,
    pub outcome: Outcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Outcome {
    Ok,
    Cancelled,
    Error {
        #[serde(flatten)]
        error: RecordedError,
    },
}

/// A [`ModelError`] in a form a recording can hold. `ModelError` itself stays
/// free of serde so the model interface owes nothing to the recorder; replay
/// reconstructs the original through [`RecordedError::into_model_error`], which
/// is what lets a recorded failure — an overload that triggered the fallback
/// model, say — replay as a failure instead of being silently skipped.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "error", rename_all = "snake_case")]
pub enum RecordedError {
    Api { status: u16, message: String },
    Auth { message: String },
    RateLimited,
    Overloaded,
    Network { message: String },
    Cancelled,
    Internal { message: String },
    Unsupported { message: String },
}

impl From<&ModelError> for RecordedError {
    fn from(e: &ModelError) -> Self {
        match e {
            ModelError::Api { status, message } => Self::Api {
                status: *status,
                message: message.clone(),
            },
            ModelError::Auth(m) => Self::Auth { message: m.clone() },
            ModelError::RateLimited => Self::RateLimited,
            ModelError::Overloaded => Self::Overloaded,
            ModelError::Network(m) => Self::Network { message: m.clone() },
            ModelError::Cancelled => Self::Cancelled,
            ModelError::Internal(m) => Self::Internal { message: m.clone() },
            ModelError::Unsupported(m) => Self::Unsupported { message: m.clone() },
        }
    }
}

impl RecordedError {
    pub fn into_model_error(self) -> ModelError {
        match self {
            Self::Api { status, message } => ModelError::Api { status, message },
            Self::Auth { message } => ModelError::Auth(message),
            Self::RateLimited => ModelError::RateLimited,
            Self::Overloaded => ModelError::Overloaded,
            Self::Network { message } => ModelError::Network(message),
            Self::Cancelled => ModelError::Cancelled,
            Self::Internal { message } => ModelError::Internal(message),
            Self::Unsupported { message } => ModelError::Unsupported(message),
        }
    }
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_record_type_decodes_as_unknown() {
        let line = r#"{"type":"something_from_the_future","whatever":1}"#;
        assert!(matches!(
            serde_json::from_str::<Record>(line).unwrap(),
            Record::Unknown
        ));
    }

    #[test]
    fn call_record_round_trips() {
        let record = Record::Call(Box::new(CallRecord {
            seq: 3,
            ts: 1_755_500_000_123,
            session_id: Some("S1".into()),
            parent_session_id: None,
            agent_type: None,
            turn: 2,
            step: 1,
            purpose: None,
            provider: "anthropic".into(),
            api_type: ApiType::Anthropic,
            params: RecordedParams {
                search_max_uses: None,
                model: "claude-opus-5".into(),
                max_tokens: 8192,
                thinking_mode: ThinkingMode::Off,
                fallback_model: None,
                cache_edits: vec![],
            },
            system: vec![BlobId("aaaaaaaaaaaaaaaa".into())],
            tools: BlobId("bbbbbbbbbbbbbbbb".into()),
            messages: vec![BlobId("cccccccccccccccc".into())],
            input_map: None,
        }));
        let json = serde_json::to_string(&record).unwrap();
        assert!(json.contains(r#""type":"call""#));
        let back: Record = serde_json::from_str(&json).unwrap();
        let Record::Call(back) = back else {
            panic!("expected a call record")
        };
        assert_eq!(back.seq, 3);
        assert_eq!(back.system.len(), 1);
    }

    #[test]
    fn every_model_error_survives_the_round_trip() {
        let errors = [
            ModelError::Api {
                status: 529,
                message: "overloaded".into(),
            },
            ModelError::Auth("bad token".into()),
            ModelError::RateLimited,
            ModelError::Overloaded,
            ModelError::Network("timeout".into()),
            ModelError::Cancelled,
            ModelError::Internal("boom".into()),
        ];
        for error in &errors {
            let recorded = RecordedError::from(error);
            let json = serde_json::to_string(&recorded).unwrap();
            let back: RecordedError = serde_json::from_str(&json).unwrap();
            assert_eq!(recorded, back);
            assert_eq!(
                back.into_model_error().to_string(),
                error.to_string(),
                "{error:?}"
            );
        }
    }

    #[test]
    fn model_events_round_trip_through_a_chunk_record() {
        let events = [
            ModelEvent::TextDelta { text: "hi".into() },
            ModelEvent::ThinkingDelta { text: "hmm".into() },
            ModelEvent::ThinkingSignature {
                signature: "sig".into(),
            },
            ModelEvent::RedactedThinking { data: "enc".into() },
            ModelEvent::ToolUse {
                id: "t1".into(),
                name: "Read".into(),
                input: serde_json::json!({"path": "/tmp/x"}),
            },
            ModelEvent::ContentBlockStop { index: 0 },
            ModelEvent::EndTurn {
                stop_reason: "end_turn".into(),
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 20,
                    ..Default::default()
                },
            },
        ];
        for event in events {
            let record = Record::Chunk(ChunkRecord {
                seq: 0,
                ts: 1,
                call: 0,
                chunk: event.clone(),
            });
            let json = serde_json::to_string(&record).unwrap();
            let back: Record = serde_json::from_str(&json).unwrap();
            let Record::Chunk(back) = back else {
                panic!("expected a chunk record")
            };
            assert_eq!(format!("{:?}", back.chunk), format!("{event:?}"));
        }
    }
}
