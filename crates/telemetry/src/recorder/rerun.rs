//! Rerun — send a recording's requests to a real provider and diff the answers.
//!
//! Replay proves a run is reproducible. It cannot answer the other question a
//! recording invites: *what would the model have said if the prompt were
//! different?* Replay compares blob **ids** and hands back the recorded
//! response, so editing a blob changes nothing it observes, and the response it
//! returns was never a function of the request anyway.
//!
//! Rerun reads the request back out of the blobs — which is where a hand edit
//! takes effect — issues it for real, and reports what moved. The recording is
//! the input; nothing here writes to it.
//!
//! Workflow this exists for: record a session, edit one system block or drop
//! one message, rerun, read the diff.

use base::interface::model::{
    CallOrigin, Model, ModelError, ModelEvent, ModelMessage, StreamParams, ToolDef,
};
use base::prompt::PromptBlock;
use futures::StreamExt;
use std::path::Path;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use super::blob::{BlobId, BlobStore};
use super::format::CallRecord;
use super::reader::{self, RecordedCall};

#[derive(Debug, thiserror::Error)]
pub enum RerunError {
    #[error("cannot read recording: {0}")]
    Read(#[from] super::reader::ReadError),
    #[error("call #{index} is missing blob {id} — the recording directory is incomplete")]
    MissingBlob { index: usize, id: BlobId },
    #[error("call #{index}: blob {id} no longer parses as {what}: {source}")]
    UnreadableBlob {
        index: usize,
        id: BlobId,
        what: &'static str,
        source: std::io::Error,
    },
    #[error("recording has {len} call(s), asked for #{index}")]
    NoSuchCall { index: usize, len: usize },
}

/// One request, loaded back from a recording and ready to send again.
pub struct LoadedRequest {
    pub prompt_blocks: Vec<PromptBlock>,
    pub tools: Vec<ToolDef>,
    pub messages: Vec<ModelMessage>,
    pub params: StreamParams,
    /// What the model said the first time, for comparison.
    pub recorded_response: Vec<ModelEvent>,
    /// The `call` record this was rebuilt from — session, turn, purpose.
    ///
    /// Carried here rather than left for the caller to fetch separately: a
    /// second `load` plus index-matching is a parallel array waiting to go out
    /// of step, and it would go out of step by panicking on an index.
    pub record: CallRecord,
}

/// Rebuild call `index`'s request from the recording in `dir`.
///
/// Blob content is read, not just referenced — that is the whole point, and it
/// is what makes an edited blob take effect.
pub fn load_request(dir: &Path, index: usize) -> Result<LoadedRequest, RerunError> {
    let recording = reader::load(dir)?;
    let call = recording.calls.get(index).ok_or(RerunError::NoSuchCall {
        index,
        len: recording.calls.len(),
    })?;
    request_of(&BlobStore::new(dir), index, call)
}

/// Every call of the recording, in order.
pub fn load_all(dir: &Path) -> Result<Vec<LoadedRequest>, RerunError> {
    let recording = reader::load(dir)?;
    let blobs = BlobStore::new(dir);
    recording
        .calls
        .iter()
        .enumerate()
        .map(|(i, call)| request_of(&blobs, i, call))
        .collect()
}

fn request_of(
    blobs: &BlobStore,
    index: usize,
    call: &RecordedCall,
) -> Result<LoadedRequest, RerunError> {
    let request = &call.request;
    let prompt_blocks = request
        .system
        .iter()
        .map(|id| fetch(blobs, index, id, "a prompt block"))
        .collect::<Result<Vec<PromptBlock>, _>>()?;
    let tools: Vec<ToolDef> = fetch(blobs, index, &request.tools, "a tool table")?;
    let messages = request
        .messages
        .iter()
        .map(|id| fetch(blobs, index, id, "a message"))
        .collect::<Result<Vec<ModelMessage>, _>>()?;

    Ok(LoadedRequest {
        prompt_blocks,
        tools,
        messages,
        params: params_of(request),
        recorded_response: call.response.clone(),
        record: request.clone(),
    })
}

fn fetch<T: serde::de::DeserializeOwned>(
    blobs: &BlobStore,
    index: usize,
    id: &BlobId,
    what: &'static str,
) -> Result<T, RerunError> {
    blobs
        .get::<T>(id)
        .map_err(|source| RerunError::UnreadableBlob {
            index,
            id: id.clone(),
            what,
            source,
        })?
        .ok_or_else(|| RerunError::MissingBlob {
            index,
            id: id.clone(),
        })
}

fn params_of(request: &CallRecord) -> StreamParams {
    StreamParams {
        model: request.params.model.clone(),
        max_tokens: request.params.max_tokens,
        thinking_mode: request.params.thinking_mode.clone(),
        effort: request.params.effort,
        fallback_model: request.params.fallback_model.clone(),
        cache_edits: request.params.cache_edits.clone(),
        // Attributed to the session that made it originally, and marked as a
        // rerun so a recorder downstream does not mistake it for turn work.
        origin: request
            .session_id
            .as_ref()
            .map(|id| CallOrigin::auxiliary(id, "rerun")),
        input_map: request.input_map.clone(),
        provider_search: request
            .params
            .search_max_uses
            .map(|max_uses| base::interface::model::ProviderSearch { max_uses }),
    }
}

/// One tool call, as the comparison sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    pub name: String,
    /// Argument keys and their canonical JSON, sorted so two calls that differ
    /// only in key order compare equal.
    pub args: Vec<(String, String)>,
}

/// How one argument key compared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArgDiff {
    pub key: String,
    pub recorded: Option<String>,
    pub live: Option<String>,
}

impl ArgDiff {
    pub fn matches(&self) -> bool {
        self.recorded == self.live
    }
}

/// One position in the tool-call sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolStep {
    /// Same tool, same arguments.
    Same { name: String },
    /// Same tool, at least one argument differs.
    ArgsDiffer { name: String, args: Vec<ArgDiff> },
    /// A different tool was called here, or one side had no call at all.
    NameDiffers {
        recorded: Option<String>,
        live: Option<String>,
    },
}

impl ToolStep {
    pub fn matches(&self) -> bool {
        matches!(self, ToolStep::Same { .. })
    }
}

/// The deterministic half of the comparison.
///
/// Tool calls are *acts*, not prose. `Write(path="src/main.c")` and
/// `Write(path="src/Main.c")` are not "roughly the same thing", and a semantic
/// judge asked to compare them will say they are — which is exactly the class
/// of regression worth catching. So nothing here is judged: names, arguments
/// and `stop_reason` compare exactly, and a mismatch is a failure.
///
/// The consequence is deliberate and known: a `Write` whose `content` is a
/// generated source file will differ on every run, and so will a free-text
/// `description`. That is reported as a failure with the offending key named,
/// leaving a human to tell benign drift from a real regression — rather than a
/// judge quietly deciding for them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolDiff {
    pub steps: Vec<ToolStep>,
    pub recorded_stop_reason: Option<String>,
    pub live_stop_reason: Option<String>,
}

impl ToolDiff {
    pub fn matches(&self) -> bool {
        self.steps.iter().all(ToolStep::matches)
            && self.recorded_stop_reason == self.live_stop_reason
    }

    /// Keys that differed, as `Tool.key` — the summary a reader triages from.
    pub fn offending_keys(&self) -> Vec<String> {
        let mut out = Vec::new();
        for step in &self.steps {
            match step {
                ToolStep::Same { .. } => {}
                ToolStep::ArgsDiffer { name, args } => out.extend(
                    args.iter()
                        .filter(|a| !a.matches())
                        .map(|a| format!("{name}.{}", a.key)),
                ),
                ToolStep::NameDiffers { recorded, live } => out.push(format!(
                    "<tool {} → {}>",
                    recorded.as_deref().unwrap_or("none"),
                    live.as_deref().unwrap_or("none")
                )),
            }
        }
        out
    }
}

/// What changed between the recorded response and a fresh one.
///
/// Split in two because the two halves want opposite treatment: the tool half
/// is decided here and exactly, the text half is left for a judge that
/// understands wording drift (see the `judge` module in the test runner).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponseDiff {
    pub recorded_text: String,
    pub live_text: String,
    pub recorded_events: usize,
    pub live_events: usize,
    pub tools: ToolDiff,
}

impl ResponseDiff {
    /// Whether the text is byte-identical — the cheap filter that decides
    /// whether a judge needs to be asked at all.
    pub fn text_identical(&self) -> bool {
        self.recorded_text == self.live_text
    }

    /// Nothing moved on either half. No judge needed, no report needed.
    pub fn identical(&self) -> bool {
        self.text_identical() && self.tools.matches()
    }

    /// A short human-readable summary, empty when nothing moved.
    pub fn report(&self) -> String {
        let mut out = String::new();
        if !self.tools.matches() {
            if self.recorded_stop_reason_differs() {
                out.push_str(&format!(
                    "stop_reason: recorded {:?}, live {:?}\n",
                    self.tools.recorded_stop_reason, self.tools.live_stop_reason
                ));
            }
            for step in &self.tools.steps {
                match step {
                    ToolStep::Same { .. } => {}
                    ToolStep::NameDiffers { recorded, live } => {
                        out.push_str(&format!("tool: recorded {:?}, live {:?}\n", recorded, live))
                    }
                    ToolStep::ArgsDiffer { name, args } => {
                        out.push_str(&format!("{name}: arguments differ\n"));
                        for a in args {
                            let mark = if a.matches() { "✓" } else { "✗" };
                            out.push_str(&format!(
                                "    {mark} {:<12} recorded {}, live {}\n",
                                a.key,
                                describe(a.recorded.as_deref()),
                                describe(a.live.as_deref()),
                            ));
                        }
                    }
                }
            }
        }
        if !self.text_identical() {
            out.push_str(&format!(
                "text: recorded {} chars, live {} chars\n  recorded: {}\n  live:     {}\n",
                self.recorded_text.len(),
                self.live_text.len(),
                elide(&self.recorded_text),
                elide(&self.live_text),
            ));
        }
        out
    }

    fn recorded_stop_reason_differs(&self) -> bool {
        self.tools.recorded_stop_reason != self.tools.live_stop_reason
    }
}

/// A value's size rather than the value: an argument can be a whole source
/// file, and a summary line is not the place to paste one.
fn describe(value: Option<&str>) -> String {
    match value {
        None => "(absent)".into(),
        Some(v) if v.len() <= 40 => v.to_string(),
        Some(v) => format!("{} chars", v.len()),
    }
}

fn elide(s: &str) -> String {
    const MAX: usize = 160;
    let flat = s.replace('\n', "\\n");
    match flat.char_indices().nth(MAX) {
        Some((cut, _)) => format!("{}…", &flat[..cut]),
        None => flat,
    }
}

fn text_of(events: &[ModelEvent]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            ModelEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn tools_of(events: &[ModelEvent]) -> Vec<ToolCall> {
    events
        .iter()
        .filter_map(|e| match e {
            ModelEvent::ToolUse { name, input, .. } => Some(ToolCall {
                name: name.clone(),
                args: canonical_args(input),
            }),
            _ => None,
        })
        .collect()
}

/// A tool's arguments as sorted `(key, canonical JSON)` pairs.
///
/// Sorted because argument order is a serialization detail the model does not
/// control; canonical JSON because `1.0` and `1` are the same argument even
/// though the bytes differ.
fn canonical_args(input: &serde_json::Value) -> Vec<(String, String)> {
    let Some(object) = input.as_object() else {
        return vec![(String::new(), input.to_string())];
    };
    let mut out: Vec<(String, String)> = object
        .iter()
        .map(|(k, v)| (k.clone(), v.to_string()))
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn stop_reason_of(events: &[ModelEvent]) -> Option<String> {
    events.iter().rev().find_map(|e| match e {
        ModelEvent::EndTurn { stop_reason, .. } => Some(stop_reason.clone()),
        _ => None,
    })
}

fn compare_tools(recorded: &[ToolCall], live: &[ToolCall]) -> Vec<ToolStep> {
    let mut steps = Vec::new();
    for i in 0..recorded.len().max(live.len()) {
        match (recorded.get(i), live.get(i)) {
            (Some(r), Some(l)) if r.name == l.name => {
                let mut keys: Vec<&String> = r.args.iter().chain(&l.args).map(|(k, _)| k).collect();
                keys.sort();
                keys.dedup();
                let args: Vec<ArgDiff> = keys
                    .into_iter()
                    .map(|key| ArgDiff {
                        key: key.clone(),
                        recorded: lookup(&r.args, key),
                        live: lookup(&l.args, key),
                    })
                    .collect();
                if args.iter().all(ArgDiff::matches) {
                    steps.push(ToolStep::Same {
                        name: r.name.clone(),
                    });
                } else {
                    steps.push(ToolStep::ArgsDiffer {
                        name: r.name.clone(),
                        args,
                    });
                }
            }
            (r, l) => steps.push(ToolStep::NameDiffers {
                recorded: r.map(|c| c.name.clone()),
                live: l.map(|c| c.name.clone()),
            }),
        }
    }
    steps
}

fn lookup(args: &[(String, String)], key: &str) -> Option<String> {
    args.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
}

pub fn diff(recorded: &[ModelEvent], live: &[ModelEvent]) -> ResponseDiff {
    ResponseDiff {
        recorded_text: text_of(recorded),
        live_text: text_of(live),
        recorded_events: recorded.len(),
        live_events: live.len(),
        tools: ToolDiff {
            steps: compare_tools(&tools_of(recorded), &tools_of(live)),
            recorded_stop_reason: stop_reason_of(recorded),
            live_stop_reason: stop_reason_of(live),
        },
    }
}

/// Send `request` to `model` and diff the answer against the recorded one.
///
/// `model` must be a real provider, not a `RecorderModel` in replay mode —
/// replaying the request would just hand back the recorded response and make
/// every diff empty.
pub async fn rerun_one(
    model: &Arc<dyn Model>,
    request: LoadedRequest,
    cancel: CancellationToken,
) -> Result<(ResponseDiff, Vec<ModelEvent>), ModelError> {
    let mut stream = model
        .stream(
            request.prompt_blocks,
            request.tools,
            request.messages,
            request.params,
            cancel,
        )
        .await?;

    let mut live = Vec::new();
    while let Some(event) = stream.next().await {
        live.push(event?);
    }
    Ok((diff(&request.recorded_response, &live), live))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recorder::format::{now_ms, Header, Record, RecordedParams, FORMAT_VERSION};
    use crate::recorder::writer::RecordingWriter;
    use base::provider::ApiType;
    use base::settings::ThinkingMode;

    fn write_one_call(dir: &Path) {
        let writer = RecordingWriter::create(dir.parent().unwrap(), {
            Header {
                version: FORMAT_VERSION,
                name: dir.file_name().unwrap().to_string_lossy().into_owned(),
                session_id: "S1".into(),
                parent: None,
                agent_type: None,
                created_at: now_ms(),
                engine_version: "test".into(),
            }
        });
        let blobs = writer.blobs();
        let system = vec![blobs.put(&PromptBlock::system("you are a bot")).unwrap()];
        let tools = blobs.put(&Vec::<ToolDef>::new()).unwrap();
        let messages = vec![blobs
            .put(&ModelMessage {
                role: base::interface::model::MessageRole::User,
                content: vec![base::interface::model::ModelContentBlock::Text {
                    text: "hi".into(),
                }],
            })
            .unwrap()];

        writer
            .append(&Record::Call(Box::new(CallRecord {
                seq: 0,
                ts: now_ms(),
                session_id: Some("S1".into()),
                parent_session_id: None,
                agent_type: None,
                turn: 1,
                step: 0,
                purpose: None,
                provider: "anthropic".into(),
                api_type: ApiType::Anthropic,
                params: RecordedParams {
                    search_max_uses: None,
                    model: "claude-opus-5".into(),
                    max_tokens: 4096,
                    thinking_mode: ThinkingMode::Off,
                    effort: None,
                    fallback_model: None,
                    cache_edits: vec![],
                },
                system,
                tools,
                messages,
                input_map: None,
            })))
            .unwrap();
        writer
            .append(&Record::Chunk(crate::recorder::format::ChunkRecord {
                seq: 1,
                ts: now_ms(),
                call: 0,
                chunk: ModelEvent::TextDelta {
                    text: "recorded answer".into(),
                },
            }))
            .unwrap();
        writer.flush().unwrap();
    }

    #[test]
    fn a_request_comes_back_out_of_the_blobs() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("run");
        write_one_call(&dir);

        let loaded = load_request(&dir, 0).unwrap();
        assert_eq!(loaded.prompt_blocks.len(), 1);
        assert_eq!(loaded.prompt_blocks[0].content, "you are a bot");
        assert_eq!(loaded.messages.len(), 1);
        assert_eq!(loaded.params.model, "claude-opus-5");
        assert_eq!(text_of(&loaded.recorded_response), "recorded answer");
    }

    /// The point of the mode: an edited blob changes what gets sent. Replay
    /// cannot do this, because it never reads a blob's content.
    #[test]
    fn editing_a_blob_changes_the_request_that_comes_back() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("run");
        write_one_call(&dir);

        let id = &reader::load(&dir).unwrap().calls[0].request.system[0].clone();
        let path = dir.join("blobs").join(&id.0);
        let edited = std::fs::read_to_string(&path)
            .unwrap()
            .replace("you are a bot", "you are a poet");
        std::fs::write(&path, edited).unwrap();

        let loaded = load_request(&dir, 0).unwrap();
        assert_eq!(loaded.prompt_blocks[0].content, "you are a poet");
    }

    #[test]
    fn a_missing_blob_names_the_call_and_the_blob() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("run");
        write_one_call(&dir);
        let id = reader::load(&dir).unwrap().calls[0].request.system[0].clone();
        std::fs::remove_file(dir.join("blobs").join(&id.0)).unwrap();

        assert!(matches!(
            load_request(&dir, 0),
            Err(RerunError::MissingBlob { index: 0, .. })
        ));
    }

    #[test]
    fn asking_past_the_end_says_how_many_there_are() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("run");
        write_one_call(&dir);
        assert!(matches!(
            load_request(&dir, 5),
            Err(RerunError::NoSuchCall { index: 5, len: 1 })
        ));
    }

    /// The whole workflow, end to end: record, edit the prompt, rerun, read the
    /// diff. The stub model answers from whatever system prompt it is given, so
    /// a different answer proves the edit reached the provider.
    #[tokio::test]
    async fn editing_the_prompt_and_rerunning_shows_a_different_answer() {
        struct EchoSystem;
        #[async_trait::async_trait]
        impl Model for EchoSystem {
            fn api_type(&self) -> ApiType {
                ApiType::Anthropic
            }
            async fn stream(
                &self,
                prompt_blocks: Vec<PromptBlock>,
                _: Vec<ToolDef>,
                _: Vec<ModelMessage>,
                _: StreamParams,
                _: CancellationToken,
            ) -> Result<base::interface::model::ModelStream, ModelError> {
                let text = prompt_blocks
                    .first()
                    .map(|b| b.content.clone())
                    .unwrap_or_default();
                Ok(Box::new(futures::stream::iter(vec![Ok(
                    ModelEvent::TextDelta { text },
                )])))
            }
        }

        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("run");
        write_one_call(&dir);

        let id = reader::load(&dir).unwrap().calls[0].request.system[0].clone();
        let path = dir.join("blobs").join(&id.0);
        let edited = std::fs::read_to_string(&path)
            .unwrap()
            .replace("you are a bot", "you are a poet");
        std::fs::write(&path, edited).unwrap();

        let model: Arc<dyn Model> = Arc::new(EchoSystem);
        let request = load_request(&dir, 0).unwrap();
        let (diff, live) = rerun_one(&model, request, CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(text_of(&live), "you are a poet");
        assert!(!diff.identical());
        assert_eq!(diff.recorded_text, "recorded answer");
        assert_eq!(diff.live_text, "you are a poet");

        // And the recording is still the recording.
        assert_eq!(
            text_of(&reader::load(&dir).unwrap().calls[0].response),
            "recorded answer"
        );
    }

    #[test]
    fn an_unchanged_answer_reports_nothing() {
        let events = vec![ModelEvent::TextDelta {
            text: "same".into(),
        }];
        let d = diff(&events, &events);
        assert!(d.identical());
        assert_eq!(d.report(), "");
    }

    #[test]
    fn a_changed_answer_reports_both_sides() {
        let recorded = vec![ModelEvent::TextDelta {
            text: "the old answer".into(),
        }];
        let live = vec![
            ModelEvent::TextDelta {
                text: "a new answer".into(),
            },
            ModelEvent::ToolUse {
                id: "t1".into(),
                name: "Read".into(),
                input: serde_json::json!({}),
            },
        ];
        let d = diff(&recorded, &live);
        assert!(!d.identical());
        let report = d.report();
        assert!(report.contains("the old answer"), "{report}");
        assert!(report.contains("a new answer"), "{report}");
        assert!(report.contains("Read"), "{report}");
    }

    fn tool(name: &str, input: serde_json::Value) -> ModelEvent {
        ModelEvent::ToolUse {
            id: "t".into(),
            name: name.into(),
            input,
        }
    }

    /// Argument order is a serialization detail the model does not control.
    #[test]
    fn argument_order_is_not_a_difference() {
        let a = vec![tool(
            "Write",
            serde_json::json!({"path":"a.c","content":"x"}),
        )];
        let b = vec![tool(
            "Write",
            serde_json::json!({"content":"x","path":"a.c"}),
        )];
        assert!(diff(&a, &b).tools.matches());
    }

    /// The case this whole tier exists for: a judge would call these "the same
    /// thing". They are not — one of them builds, the other does not.
    #[test]
    fn a_one_character_path_change_is_a_failure_not_a_nuance() {
        let recorded = vec![tool("Write", serde_json::json!({"path":"src/main.c"}))];
        let live = vec![tool("Write", serde_json::json!({"path":"src/Main.c"}))];
        let d = diff(&recorded, &live);
        assert!(!d.tools.matches());
        assert_eq!(d.tools.offending_keys(), vec!["Write.path"]);
    }

    /// The known cost of judging every argument exactly: generated file content
    /// never reproduces. It fails, and the report names the key so a human can
    /// see at a glance that it is `content` and not `path`.
    #[test]
    fn drifting_file_content_fails_but_names_itself() {
        let recorded = vec![tool(
            "Write",
            serde_json::json!({"path":"a.c","content":"int main(){return 0;}"}),
        )];
        let live = vec![tool(
            "Write",
            serde_json::json!({"path":"a.c","content":"int main(void){return 0;}"}),
        )];
        let d = diff(&recorded, &live);
        assert!(!d.tools.matches());
        assert_eq!(d.tools.offending_keys(), vec!["Write.content"]);
        let report = d.report();
        assert!(
            report.contains("✓ path"),
            "path should show as matching: {report}"
        );
        assert!(report.contains("✗ content"), "{report}");
    }

    #[test]
    fn calling_a_different_tool_is_reported_as_such() {
        let recorded = vec![tool("Write", serde_json::json!({}))];
        let live = vec![tool("Bash", serde_json::json!({}))];
        let d = diff(&recorded, &live);
        assert!(!d.tools.matches());
        assert!(d.tools.offending_keys()[0].contains("Write"));
        assert!(d.tools.offending_keys()[0].contains("Bash"));
    }

    #[test]
    fn an_extra_tool_call_is_a_difference() {
        let recorded = vec![tool("Read", serde_json::json!({}))];
        let live = vec![
            tool("Read", serde_json::json!({})),
            tool("Bash", serde_json::json!({})),
        ];
        assert!(!diff(&recorded, &live).tools.matches());
    }

    #[test]
    fn a_changed_stop_reason_is_a_difference() {
        let end = |r: &str| ModelEvent::EndTurn {
            stop_reason: r.into(),
            usage: Default::default(),
        };
        let d = diff(&[end("tool_use")], &[end("end_turn")]);
        assert!(!d.tools.matches());
        assert!(d.report().contains("stop_reason"), "{}", d.report());
    }

    /// Identical text needs no judge — this is the filter that keeps the judge
    /// (and its cost, and its own non-determinism) off the common path.
    #[test]
    fn identical_text_is_detected_without_a_judge() {
        let same = vec![ModelEvent::TextDelta { text: "hi".into() }];
        assert!(diff(&same, &same).text_identical());
        let other = vec![ModelEvent::TextDelta {
            text: "hello".into(),
        }];
        assert!(!diff(&same, &other).text_identical());
    }
}
