//! Differential check: what a recording holds must equal what actually
//! crossed the model boundary.
//!
//! Every other test in this crate asserts the recorder against its own idea of
//! what it wrote. This one asserts it against an independent witness. A tap is
//! spliced *between* the recorder and the model it wraps:
//!
//! ```text
//! caller → RecorderModel → TapModel → model
//!               ↓ writes         ↓ observes
//!          calls.jsonl      what really went out / came back
//! ```
//!
//! The recorder forwards verbatim, so whatever the tap sees is exactly what
//! left for the model, and whatever the tap emits is exactly what came back.
//! Comparing the two afterwards catches anything the recorder drops, reorders,
//! merges, or invents — the failure modes that a self-consistent recorder
//! cannot detect about itself.
//!
//! The traffic is scripted rather than live because the recorder cannot tell
//! the difference: it sees a `Model` either way. The script is shaped like real
//! traffic instead — token-sized deltas, thinking blocks, tool arguments
//! arriving in fragments — so the comparison exercises the paths that matter.
//! To run the same check against a real provider, point `INNER` at a live model
//! (see `tests/runner` for building one from `.env`); nothing else changes.

use base::interface::model::{
    CallOrigin, Model, ModelContentBlock, ModelError, ModelEvent, ModelMessage, ModelStream,
    StreamParams, ToolDef, Usage,
};
use base::interface::prompt::{BlockRole, CacheStrategy, PromptBlock};
use base::interface::settings::{Divergence, RecorderConfig, RecorderMode, ThinkingMode};
use base::provider::ApiType;
use futures::StreamExt;
use std::sync::{Arc, Mutex};
use telemetry::recorder::reader;
use telemetry::recorder::RecorderModel;
use tokio_util::sync::CancellationToken;

/// One observation of the model boundary.
#[derive(Debug, Clone)]
struct Observed {
    prompt_blocks: Vec<PromptBlock>,
    tools: Vec<ToolDef>,
    messages: Vec<ModelMessage>,
    params: StreamParams,
    emitted: Vec<ModelEvent>,
}

/// Sits between the recorder and the model, remembering what passed through.
struct TapModel {
    script: Vec<Vec<ModelEvent>>,
    observed: Arc<Mutex<Vec<Observed>>>,
    calls: Mutex<usize>,
}

#[async_trait::async_trait]
impl Model for TapModel {
    fn api_type(&self) -> ApiType {
        ApiType::Anthropic
    }

    async fn stream(
        &self,
        prompt_blocks: Vec<PromptBlock>,
        tools: Vec<ToolDef>,
        messages: Vec<ModelMessage>,
        params: StreamParams,
        _cancel: CancellationToken,
    ) -> Result<ModelStream, ModelError> {
        let index = {
            let mut calls = self.calls.lock().unwrap();
            let i = *calls;
            *calls += 1;
            i
        };
        let emitted = self.script.get(index).cloned().unwrap_or_default();

        self.observed.lock().unwrap().push(Observed {
            prompt_blocks,
            tools,
            messages,
            params,
            emitted: emitted.clone(),
        });

        let events: Vec<Result<ModelEvent, ModelError>> = emitted.into_iter().map(Ok).collect();
        Ok(Box::new(futures::stream::iter(events)))
    }
}

/// A response shaped like a real one: thinking that streams in fragments, a
/// tool call whose arguments arrive as partial JSON, and prose in token-sized
/// pieces — including two adjacent fragments that would read identically if
/// anything joined them.
fn realistic_response(tool_id: &str) -> Vec<ModelEvent> {
    let mut events = vec![
        ModelEvent::ContentBlockStart {
            index: 0,
            block: ModelContentBlock::Text {
                text: String::new(),
            },
        },
        ModelEvent::ThinkingDelta {
            text: "The user wants".into(),
        },
        ModelEvent::ThinkingDelta {
            text: " a file read".into(),
        },
        ModelEvent::ThinkingDelta {
            text: ". Let me check".into(),
        },
        ModelEvent::ThinkingSignature {
            signature: "sig-abc123".into(),
        },
        ModelEvent::ContentBlockStop { index: 0 },
    ];
    for fragment in ["I'll", " read", " that", " file", " for", " you", ".", "."] {
        events.push(ModelEvent::TextDelta {
            text: fragment.into(),
        });
    }
    for fragment in [r#"{"pa"#, r#"th": "/tm"#, r#"p/notes"#, r#".md"}"#] {
        events.push(ModelEvent::ToolArgsDelta {
            id: tool_id.into(),
            partial_json: fragment.into(),
        });
    }
    events.push(ModelEvent::ToolUse {
        id: tool_id.into(),
        name: "Read".into(),
        input: serde_json::json!({"path": "/tmp/notes.md"}),
    });
    events.push(ModelEvent::EndTurn {
        stop_reason: "tool_use".into(),
        usage: Usage {
            input_tokens: 1024,
            output_tokens: 57,
            // A cached read is priced differently from a fresh one, so a
            // recording that lost it would misprice every replay.
            cache_creation_input_tokens: 128,
            cache_read_input_tokens: 896,
        },
    });
    events
}

fn system_blocks(turn: u32) -> Vec<PromptBlock> {
    vec![
        PromptBlock {
            role: BlockRole::System,
            content: "You are a coding agent.".into(),
            cache_strategy: Some(CacheStrategy::Ephemeral),
            name: Some("identity".into()),
            origin: Default::default(),
        },
        PromptBlock {
            role: BlockRole::System,
            content: "## Available Skills\n- agix-implement\n- agix-review".into(),
            cache_strategy: None,
            name: Some(base::prompt::names::SKILLS_CATALOG.into()),
            origin: Default::default(),
        },
        // Differs per turn, so the blob set has to grow rather than collapse
        // to one — which is what proves dedup is content-driven and not just
        // "everything after the first call points at the first call".
        PromptBlock {
            role: BlockRole::System,
            content: format!("<env>turn={turn}</env>"),
            cache_strategy: None,
            name: Some("env".into()),
            origin: Default::default(),
        },
    ]
}

fn tool_table() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "Read".into(),
            description: "Read a file".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }),
            source: Some("builtin".into()),
        },
        ToolDef {
            name: "Bash".into(),
            description: "Run a shell command".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {"command": {"type": "string"}}
            }),
            source: Some("builtin".into()),
        },
    ]
}

fn message(role: base::interface::model::MessageRole, text: &str) -> ModelMessage {
    ModelMessage {
        role,
        content: vec![ModelContentBlock::Text { text: text.into() }],
    }
}

#[tokio::test]
async fn every_recorded_call_matches_what_crossed_the_boundary() {
    use base::interface::model::MessageRole;

    let root = tempfile::tempdir().unwrap();
    let observed = Arc::new(Mutex::new(Vec::new()));

    let script: Vec<Vec<ModelEvent>> = (0..3)
        .map(|i| realistic_response(&format!("call_{i}")))
        .collect();

    let tap = Arc::new(TapModel {
        script,
        observed: Arc::clone(&observed),
        calls: Mutex::new(0),
    });

    let recorder = RecorderModel::new(
        tap,
        Some(RecorderConfig {
            mode: RecorderMode::Record,
            name: Some("wire".into()),
            root: root.path().to_path_buf(),
            on_divergence: Divergence::Strict,
        }),
        "test",
    );

    // Three calls with a growing conversation, so message dedup and the
    // per-call message list both get exercised.
    let mut messages = vec![message(MessageRole::User, "read /tmp/notes.md")];
    for step in 0..3u32 {
        let stream = recorder
            .stream(
                system_blocks(step),
                tool_table(),
                messages.clone(),
                StreamParams {
                    model: "claude-opus-5".into(),
                    max_tokens: 8192,
                    thinking_mode: ThinkingMode::Off,
                    fallback_model: Some("claude-sonnet-5".into()),
                    cache_edits: vec![],
                    origin: Some(
                        CallOrigin::turn("S1", 1, step)
                            .with_lineage(Some("P0".into()), Some("code-reviewer".into())),
                    ),
                    input_map: None,
                },
                CancellationToken::new(),
            )
            .await
            .expect("stream opens");
        let _drained: Vec<_> = stream.collect().await;

        messages.push(message(MessageRole::Assistant, &format!("reply {step}")));
        messages.push(message(MessageRole::User, &format!("follow-up {step}")));
    }
    drop(recorder);

    let recording = reader::load(&root.path().join("wire")).expect("recording loads");
    let observed = observed.lock().unwrap().clone();
    let blobs = telemetry::recorder::blob::BlobStore::new(&root.path().join("wire"));

    assert_eq!(recording.damaged, 0, "recording must be fully readable");
    assert_eq!(
        recording.calls.len(),
        observed.len(),
        "a recorded call per call that crossed the boundary"
    );

    for (i, (recorded, actual)) in recording.calls.iter().zip(&observed).enumerate() {
        // ── request: system blocks, per block, in order ──
        let recorded_system: Vec<PromptBlock> = recorded
            .request
            .system
            .iter()
            .map(|id| {
                blobs
                    .get::<PromptBlock>(id)
                    .expect("blob readable")
                    .unwrap_or_else(|| panic!("call {i}: system blob {id} missing"))
            })
            .collect();
        assert_eq!(
            recorded_system, actual.prompt_blocks,
            "call {i}: recorded system blocks differ from what was sent"
        );

        // ── request: the tool table ──
        let recorded_tools: Vec<ToolDef> = blobs
            .get(&recorded.request.tools)
            .expect("blob readable")
            .unwrap_or_else(|| panic!("call {i}: tools blob missing"));
        assert_eq!(
            format!("{recorded_tools:?}"),
            format!("{:?}", actual.tools),
            "call {i}: recorded tool table differs from what was sent"
        );

        // ── request: every message, in order ──
        let recorded_messages: Vec<ModelMessage> = recorded
            .request
            .messages
            .iter()
            .map(|id| {
                blobs
                    .get::<ModelMessage>(id)
                    .expect("blob readable")
                    .unwrap_or_else(|| panic!("call {i}: message blob {id} missing"))
            })
            .collect();
        assert_eq!(
            format!("{recorded_messages:?}"),
            format!("{:?}", actual.messages),
            "call {i}: recorded messages differ from what was sent"
        );

        // ── request: call configuration ──
        assert_eq!(recorded.request.params.model, actual.params.model);
        assert_eq!(recorded.request.params.max_tokens, actual.params.max_tokens);
        assert_eq!(
            recorded.request.params.fallback_model,
            actual.params.fallback_model
        );
        let origin = actual.params.origin.as_ref().unwrap();
        assert_eq!(
            (recorded.request.turn, recorded.request.step),
            origin.turn_step()
        );
        assert_eq!(recorded.request.purpose.as_deref(), origin.purpose());

        // ── response: every event, in order, verbatim ──
        assert_eq!(
            format!("{:?}", recorded.response),
            format!("{:?}", actual.emitted),
            "call {i}: recorded response differs from what came back"
        );
    }
}

#[tokio::test]
async fn token_boundaries_and_argument_fragments_survive_the_round_trip() {
    use base::interface::model::MessageRole;

    let root = tempfile::tempdir().unwrap();
    let observed = Arc::new(Mutex::new(Vec::new()));
    let tap = Arc::new(TapModel {
        script: vec![realistic_response("call_0")],
        observed: Arc::clone(&observed),
        calls: Mutex::new(0),
    });

    let recorder = RecorderModel::new(
        tap,
        Some(RecorderConfig {
            mode: RecorderMode::Record,
            name: Some("wire".into()),
            root: root.path().to_path_buf(),
            on_divergence: Divergence::Strict,
        }),
        "test",
    );
    let stream = recorder
        .stream(
            system_blocks(0),
            tool_table(),
            vec![message(MessageRole::User, "go")],
            StreamParams {
                model: "m".into(),
                max_tokens: 100,
                thinking_mode: ThinkingMode::Off,
                fallback_model: None,
                cache_edits: vec![],
                origin: None,
                input_map: None,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let _drained: Vec<_> = stream.collect().await;
    drop(recorder);

    let recording = reader::load(&root.path().join("wire")).unwrap();
    let response = &recording.calls[0].response;

    let texts: Vec<&str> = response
        .iter()
        .filter_map(|e| match e {
            ModelEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        texts,
        vec!["I'll", " read", " that", " file", " for", " you", ".", "."],
        "adjacent deltas must stay separate — the two trailing '.' prove nothing was joined"
    );

    let fragments: Vec<&str> = response
        .iter()
        .filter_map(|e| match e {
            ModelEvent::ToolArgsDelta { partial_json, .. } => Some(partial_json.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        fragments,
        vec![r#"{"pa"#, r#"th": "/tm"#, r#"p/notes"#, r#".md"}"#],
        "tool argument fragments must survive unjoined"
    );

    assert!(
        response
            .iter()
            .any(|e| matches!(e, ModelEvent::ThinkingSignature { .. })),
        "thinking signatures must be recorded, not dropped"
    );
}

#[tokio::test]
async fn replaying_a_recording_reproduces_the_boundary_traffic() {
    use base::interface::model::MessageRole;

    let root = tempfile::tempdir().unwrap();
    let observed = Arc::new(Mutex::new(Vec::new()));
    let tap = Arc::new(TapModel {
        script: vec![realistic_response("call_0")],
        observed: Arc::clone(&observed),
        calls: Mutex::new(0),
    });

    let config = |mode| RecorderConfig {
        mode,
        name: Some("wire".into()),
        root: root.path().to_path_buf(),
        on_divergence: Divergence::Strict,
    };
    let params = || StreamParams {
        model: "m".into(),
        max_tokens: 100,
        thinking_mode: ThinkingMode::Off,
        fallback_model: None,
        cache_edits: vec![],
        origin: None,
        input_map: None,
    };

    let recorder = RecorderModel::new(tap, Some(config(RecorderMode::Record)), "test");
    let live: Vec<ModelEvent> = recorder
        .stream(
            system_blocks(0),
            tool_table(),
            vec![message(MessageRole::User, "go")],
            params(),
            CancellationToken::new(),
        )
        .await
        .unwrap()
        .filter_map(|e| async { e.ok() })
        .collect()
        .await;
    drop(recorder);

    // A replayer whose inner model would panic if reached.
    let unreachable = Arc::new(TapModel {
        script: vec![],
        observed: Arc::new(Mutex::new(Vec::new())),
        calls: Mutex::new(0),
    });
    let replayer = RecorderModel::new(unreachable, Some(config(RecorderMode::Replay)), "test");
    let replayed: Vec<ModelEvent> = replayer
        .stream(
            system_blocks(0),
            tool_table(),
            vec![message(MessageRole::User, "go")],
            params(),
            CancellationToken::new(),
        )
        .await
        .unwrap()
        .filter_map(|e| async { e.ok() })
        .collect()
        .await;

    assert_eq!(
        format!("{replayed:?}"),
        format!("{live:?}"),
        "replay must reproduce the live event stream exactly"
    );
}
