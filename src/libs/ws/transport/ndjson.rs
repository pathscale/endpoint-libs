//! Newline-delimited JSON (`NdjsonStream`) implementing `MessageStream` over `futures::io::{AsyncRead, AsyncWrite}`.

use std::pin::Pin;

use eyre::eyre;
use futures::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::super::message::WireMessage;
use super::super::traits::{MessageStream, StreamError};

/// Maximum single NDJSON line length (16 MiB).
pub const MAX_NDJSON_LINE_BYTES: usize = 16 * 1024 * 1024;

pub struct NdjsonStream<IO> {
    io: IO,
    buf: Vec<u8>,
    max_line_bytes: usize,
}

impl<IO> NdjsonStream<IO> {
    pub fn new(io: IO) -> Self {
        Self {
            io,
            buf: Vec::with_capacity(4096),
            max_line_bytes: MAX_NDJSON_LINE_BYTES,
        }
    }

    pub fn with_max_line_bytes(io: IO, max_line_bytes: usize) -> Self {
        Self {
            io,
            buf: Vec::with_capacity(4096),
            max_line_bytes,
        }
    }

    pub fn into_inner(self) -> IO {
        self.io
    }
}

#[async_trait::async_trait(?Send)]
impl<IO> MessageStream for NdjsonStream<IO>
where
    IO: AsyncRead + AsyncWrite + Unpin + Send,
{
    async fn send(&mut self, msg: WireMessage) -> Result<(), StreamError> {
        let WireMessage::Text(text) = msg else {
            return Err(StreamError::Other(eyre!(
                "NDJSON transport supports only WireMessage::Text"
            )));
        };
        if text.contains('\n') || text.contains('\r') {
            return Err(StreamError::Other(eyre!(
                "NDJSON outbound payload must not contain raw newlines"
            )));
        }
        if text.len() > self.max_line_bytes {
            return Err(StreamError::Other(eyre!(
                "NDJSON outbound payload ({} bytes) exceeds max ({} bytes)",
                text.len(),
                self.max_line_bytes
            )));
        }
        let mut frame = Vec::with_capacity(text.len() + 1);
        frame.extend_from_slice(text.as_bytes());
        frame.push(b'\n');
        Pin::new(&mut self.io)
            .write_all(&frame)
            .await
            .map_err(|e| StreamError::Other(eyre!(e)))?;
        Pin::new(&mut self.io)
            .flush()
            .await
            .map_err(|e| StreamError::Other(eyre!(e)))?;
        Ok(())
    }

    async fn recv(&mut self) -> Option<Result<WireMessage, StreamError>> {
        let mut byte = [0u8; 1];
        loop {
            if let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
                let mut line: Vec<u8> = self.buf.drain(..=pos).collect();
                if line.last() == Some(&b'\n') {
                    line.pop();
                }
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                if line.is_empty() {
                    continue;
                }
                return match String::from_utf8(line) {
                    Ok(s) => Some(Ok(WireMessage::Text(s.into()))),
                    Err(e) => Some(Err(StreamError::Other(eyre!(
                        "invalid UTF-8 in NDJSON line: {e}"
                    )))),
                };
            }

            if self.buf.len() >= self.max_line_bytes {
                self.buf.clear();
                return Some(Err(StreamError::Other(eyre!(
                    "NDJSON line exceeded maximum length of {} bytes",
                    self.max_line_bytes
                ))));
            }

            match Pin::new(&mut self.io).read(&mut byte).await {
                Ok(0) => {
                    if self.buf.is_empty() {
                        return None;
                    }
                    let tail = std::mem::take(&mut self.buf);
                    if tail.iter().all(|b| *b == b'\r' || *b == b' ') {
                        return None;
                    }
                    return match String::from_utf8(tail) {
                        Ok(s) => Some(Ok(WireMessage::Text(s.into()))),
                        Err(e) => Some(Err(StreamError::Other(eyre!(
                            "invalid UTF-8 in trailing NDJSON line: {e}"
                        )))),
                    };
                }
                Ok(_) => {
                    self.buf.push(byte[0]);
                }
                Err(e) => return Some(Err(StreamError::Other(eyre!(e)))),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::block_on;
    use futures::io::Cursor;

    #[test]
    fn ndjson_round_trip_and_limits() {
        block_on(async {
            let input = b"\n{\"jsonrpc\":\"2.0\",\"id\":1}\r\n{\"jsonrpc\":\"2.0\",\"id\":2}\n";
            let mut stream = NdjsonStream::new(Cursor::new(input.to_vec()));
            let first = stream.recv().await.unwrap().unwrap();
            assert_eq!(first, WireMessage::Text("{\"jsonrpc\":\"2.0\",\"id\":1}".into()));
            let second = stream.recv().await.unwrap().unwrap();
            assert_eq!(second, WireMessage::Text("{\"jsonrpc\":\"2.0\",\"id\":2}".into()));
            assert!(stream.recv().await.is_none());

            let mut out_stream = NdjsonStream::with_max_line_bytes(Cursor::new(Vec::new()), 16);
            assert!(
                out_stream
                    .send(WireMessage::Text("line\nwith_newline".into()))
                    .await
                    .is_err()
            );
            assert!(
                out_stream
                    .send(WireMessage::Text("0123456789abcdef_too_long".into()))
                    .await
                    .is_err()
            );
        });
    }
}
