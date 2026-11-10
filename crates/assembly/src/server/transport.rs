//! Framing for byte-stream transports: newline-delimited frames with a size cap,
//! the in-flight bound, and the sink that writes frames back.

use super::*;

pub(super) type Reader = Box<dyn AsyncRead + Send + Unpin + 'static>;

/// How many requests one connection may have in flight at once.
///
/// A bound, not a queue: at the limit further requests are refused with
/// [`codes::TOO_MANY_IN_FLIGHT`] rather than made to wait, because waiting
/// would have to happen in the read loop and a read loop that stops reading
/// is precisely what this whole arrangement exists to avoid.
///
/// Generous next to any real client — a browser tab has a handful of calls
/// outstanding, and sessions are capped well below this.
pub(crate) const MAX_IN_FLIGHT_PER_CONNECTION: usize = 64;
pub(super) type ByteWriter = Arc<AsyncMutex<Box<dyn AsyncWrite + Send + Unpin + 'static>>>;

/// Largest request a client may send on one connection.
///
/// Generous — a turn's worth of context or a pasted file is legitimately
/// large — but finite, which is the point: without a cap a peer that never
/// sends a newline makes the daemon buffer until it dies, and on the
/// WebSocket transport that peer can be any page the user's browser has open.
pub(crate) const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// Newline-delimited frames, with the cap enforced as the bytes arrive.
///
/// `tokio::io::Lines` would be the obvious choice, but it grows its buffer
/// until it finds a newline, so the limit has to live where the reading
/// happens rather than in a check after the fact.
pub(super) struct FrameReader {
    inner: BufReader<Reader>,
    buf: Vec<u8>,
}

impl FrameReader {
    pub(super) fn new(reader: Reader) -> Self {
        Self {
            inner: BufReader::new(reader),
            buf: Vec::new(),
        }
    }

    /// The next frame, or `None` at end of stream, on an I/O error, or when
    /// the peer runs past [`MAX_FRAME_BYTES`].
    ///
    /// Over-long frames end the connection rather than being skipped: the
    /// daemon has no idea where the frame boundary is anymore, so the honest
    /// options are close it or keep buffering, and the second one is the
    /// problem being fixed.
    pub(super) async fn next_frame(&mut self) -> Option<String> {
        self.buf.clear();
        loop {
            let (complete, consumed) = {
                let available = match self.inner.fill_buf().await {
                    Ok(b) => b,
                    Err(_) => return None,
                };
                if available.is_empty() {
                    return (!self.buf.is_empty())
                        .then(|| String::from_utf8_lossy(&self.buf).into_owned());
                }
                match available.iter().position(|&b| b == b'\n') {
                    Some(i) => {
                        self.buf.extend_from_slice(&available[..i]);
                        (true, i + 1)
                    }
                    None => {
                        self.buf.extend_from_slice(available);
                        (false, available.len())
                    }
                }
            };
            self.inner.consume(consumed);

            if self.buf.len() > MAX_FRAME_BYTES {
                warn!(
                    limit = MAX_FRAME_BYTES,
                    "closing a connection that sent a frame over the size limit"
                );
                return None;
            }
            if complete {
                return Some(String::from_utf8_lossy(&self.buf).into_owned());
            }
        }
    }
}

/// Newline-delimited JSON over a byte stream — Unix socket and TCP.
pub(super) struct LineSink(pub(super) ByteWriter);

#[async_trait::async_trait]
impl FrameSink for LineSink {
    async fn send_json(&self, json: String) -> bool {
        let mut buf = json.into_bytes();
        buf.push(b'\n');
        let mut w = self.0.lock().await;
        if w.write_all(&buf).await.is_err() {
            return false;
        }
        // Every frame, on every path. A response the caller is blocked on and
        // a streamed token are equally useless sitting in a buffer.
        w.flush().await.is_ok()
    }
}

#[cfg(test)]
mod frame_reader_tests {
    use super::*;

    pub(super) fn reader(bytes: &'static [u8]) -> FrameReader {
        FrameReader::new(Box::new(std::io::Cursor::new(bytes)))
    }

    #[tokio::test]
    pub(super) async fn frames_are_split_on_newlines() {
        let mut r = reader(b"{\"a\":1}\n{\"b\":2}\n");
        assert_eq!(r.next_frame().await.as_deref(), Some("{\"a\":1}"));
        assert_eq!(r.next_frame().await.as_deref(), Some("{\"b\":2}"));
        assert_eq!(r.next_frame().await, None);
    }

    /// A client that closes without a trailing newline still said something.
    #[tokio::test]
    pub(super) async fn a_final_frame_without_a_newline_is_still_delivered() {
        let mut r = reader(b"{\"a\":1}");
        assert_eq!(r.next_frame().await.as_deref(), Some("{\"a\":1}"));
        assert_eq!(r.next_frame().await, None);
    }

    /// The reason this type exists: without a cap, a peer that never sends a
    /// newline makes the daemon buffer until it dies.
    #[tokio::test]
    pub(super) async fn a_frame_over_the_limit_ends_the_connection() {
        let oversized: &'static [u8] = Box::leak(
            std::iter::repeat_n(b'x', MAX_FRAME_BYTES + 1)
                .collect::<Vec<u8>>()
                .into_boxed_slice(),
        );
        assert_eq!(reader(oversized).next_frame().await, None);
    }

    #[tokio::test]
    pub(super) async fn a_frame_at_the_limit_is_delivered() {
        let at_limit: &'static [u8] = Box::leak(
            std::iter::repeat_n(b'x', MAX_FRAME_BYTES)
                .chain(std::iter::once(b'\n'))
                .collect::<Vec<u8>>()
                .into_boxed_slice(),
        );
        assert_eq!(
            reader(at_limit).next_frame().await.map(|s| s.len()),
            Some(MAX_FRAME_BYTES)
        );
    }
}
