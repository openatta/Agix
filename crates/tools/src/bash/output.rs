//! Turns a command's tagged output stream into per-pipe text: line splitting, caps and progress.

use base::tool::ProgressSender;
use futures::StreamExt;

/// 输出文本上限（防止一个 yes / cat /dev/random 把内存吃光）
const MAX_OUTPUT_BYTES: usize = 4 * 1024 * 1024;

/// Accumulate a byte stream into lines the way `LinesCodec` did.
///
/// The output of a command is bytes; where the lines are is the reader's
/// question, which is why the contract does not answer it. This is that
/// answer for a shell command: split on `\n`, drop a `\r` before it. A line
/// longer than the cap is cut at the cap, marked `…[line truncated]`, and the
/// rest of it discarded — reading goes on with the next line rather than
/// stopping, so output after one runaway line is not lost.
#[derive(Default)]
pub(super) struct Lines {
    pending: Vec<u8>,
    buf: String,
    /// Past the cap on the current, still-unterminated line: its truncation
    /// marker already went out, so bytes are dropped until the next newline.
    skipping_rest_of_line: bool,
}

const MAX_LINE_BYTES: usize = 64 * 1024;

/// The largest index `<= i` that does not fall inside a UTF-8 character, so a
/// cut there never splits one.
fn floor_char_boundary(bytes: &[u8], i: usize) -> usize {
    let mut i = i.min(bytes.len());
    while i > 0 && (bytes[i] & 0b1100_0000) == 0b1000_0000 {
        i -= 1;
    }
    i
}

impl Lines {
    pub(super) fn feed(&mut self, bytes: &[u8], sink: &Sink) {
        self.pending.extend_from_slice(bytes);
        loop {
            match self.pending.iter().position(|b| *b == b'\n') {
                Some(nl) => {
                    let mut line = self.pending.drain(..=nl).collect::<Vec<u8>>();
                    line.pop();
                    if line.last() == Some(&b'\r') {
                        line.pop();
                    }
                    if self.skipping_rest_of_line {
                        self.skipping_rest_of_line = false;
                    } else {
                        self.push(String::from_utf8_lossy(&line).into_owned(), sink);
                    }
                }
                None => {
                    if self.skipping_rest_of_line {
                        self.pending.clear();
                    } else if self.pending.len() > MAX_LINE_BYTES {
                        let cut = floor_char_boundary(&self.pending, MAX_LINE_BYTES);
                        let head = String::from_utf8_lossy(&self.pending[..cut]).into_owned();
                        self.push(format!("{head}…[line truncated]"), sink);
                        self.pending.clear();
                        self.skipping_rest_of_line = true;
                    }
                    break;
                }
            }
        }
    }

    pub(super) fn finish(mut self, sink: &Sink) -> String {
        if !self.skipping_rest_of_line && !self.pending.is_empty() {
            let line = String::from_utf8_lossy(&std::mem::take(&mut self.pending)).into_owned();
            self.push(line, sink);
        }
        if self.buf.len() >= MAX_OUTPUT_BYTES {
            self.buf.push_str("\n[output truncated]\n");
        }
        self.buf
    }

    fn push(&mut self, line: String, sink: &Sink) {
        if self.buf.len() < MAX_OUTPUT_BYTES {
            self.buf.push_str(&line);
            self.buf.push('\n');
        }
        sink.line(&line);
    }
}

/// Where each line goes as it arrives: the tool call's progress, and the
/// task's own output (`TaskOutput`, a host's `session.taskOutput`).
pub(super) struct Sink {
    pub(super) progress: ProgressSender,
    pub(super) task: std::sync::Arc<base::context::RunningTask>,
}

impl Sink {
    fn line(&self, line: &str) {
        let line = format!("{line}\n");
        self.progress.send(&line);
        self.task.append_output(&line);
    }
}

/// Drain the tagged output stream into one buffer per pipe, until it ends or
/// `stop` fires.
///
/// Two buffers rather than one, because the result reports all of stdout and
/// then all of stderr — the interleaving on the wire is what keeps a quiet
/// stdout from holding stderr up, not what the model is shown.
pub(super) async fn drain_output(
    mut output: futures::stream::BoxStream<
        'static,
        Result<base::interface::exec::OutputChunk, base::interface::exec::ExecError>,
    >,
    sink: Sink,
    stop: tokio_util::sync::CancellationToken,
) -> (String, String) {
    use base::interface::exec::OutputStream;
    let (mut out, mut err) = (Lines::default(), Lines::default());
    loop {
        let next = tokio::select! {
            biased;
            _ = stop.cancelled() => None,
            n = output.next() => n,
        };
        let Some(chunk) = next else { break };
        let Ok(chunk) = chunk else { break };
        match chunk.stream {
            OutputStream::Stdout => out.feed(&chunk.bytes, &sink),
            OutputStream::Stderr => err.feed(&chunk.bytes, &sink),
        }
    }
    (out.finish(&sink), err.finish(&sink))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cut_line_ends_on_a_character_boundary() {
        let sink = Sink {
            progress: ProgressSender::noop("t"),
            task: std::sync::Arc::new(base::context::RunningTask::new(
                "t".into(),
                base::context::TaskType::Shell,
            )),
        };
        let mut lines = Lines::default();
        lines.feed("é".repeat(MAX_LINE_BYTES).as_bytes(), &sink);
        lines.feed(b"\nnext\n", &sink);
        let out = lines.finish(&sink);
        let first = out.lines().next().unwrap();
        assert!(first.ends_with("…[line truncated]"));
        assert!(!first.contains('\u{fffd}'), "a character was split");
        assert!(out.contains("\nnext\n"));
    }
}
