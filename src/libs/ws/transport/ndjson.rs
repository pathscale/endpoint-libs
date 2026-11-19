//! Newline-delimited JSON (`NdjsonStream`) implementing `MessageStream` over `PollRead + PollWrite`.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::future::poll_fn;
use core::pin::Pin;

use crate::eyre::eyre;

use super::super::message::WireMessage;
use super::super::traits::{MessageStream, StreamError};
use super::{PollRead, PollWrite};

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
    IO: PollRead + PollWrite + Unpin + Send,
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
        };
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

        let mut written = 0;
        while written < frame.len() {
            let n = poll_fn(|cx| Pin::new(&mut self.io).poll_write(cx, &frame[written..]))
                .await
                .map_err(|e| StreamError::Other(eyre!(e)))?;
            if n == 0 {
                return Err(StreamError::Other(eyre!("NDJSON write returned 0 bytes")));
            }
            written += n;
        }
        poll_fn(|cx| Pin::new(&mut self.io).poll_flush(cx))
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

            let read_res = poll_fn(|cx| Pin::new(&mut self.io).poll_read(cx, &mut byte)).await;

            match read_res {
                Ok(0) => {
                    if self.buf.is_empty() {
                        return None;
                    }
                    let tail = core::mem::take(&mut self.buf);
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
    use core::task::{Context, Poll};
    use eko::compat::io;
    use futures::executor::block_on;

    struct MemBuf {
        data: Vec<u8>,
        pos: usize,
    }

    impl MemBuf {
        fn new(data: Vec<u8>) -> Self {
            Self { data, pos: 0 }
        }
    }

    impl PollRead for MemBuf {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut [u8],
        ) -> Poll<io::Result<usize>> {
            let rem = &self.data[self.pos..];
            let n = rem.len().min(buf.len());
            buf[..n].copy_from_slice(&rem[..n]);
            self.pos += n;
            Poll::Ready(Ok(n))
        }
    }

    impl PollWrite for MemBuf {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            self.data.extend_from_slice(buf);
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[test]
    fn ndjson_round_trip_and_limits() {
        block_on(async {
            let input = b"\n{\"jsonrpc\":\"2.0\",\"id\":1}\r\n{\"jsonrpc\":\"2.0\",\"id\":2}\n";
            let mut stream = NdjsonStream::new(MemBuf::new(input.to_vec()));
            let first = stream.recv().await.unwrap().unwrap();
            assert_eq!(first, WireMessage::Text("{\"jsonrpc\":\"2.0\",\"id\":1}".into()));
            let second = stream.recv().await.unwrap().unwrap();
            assert_eq!(second, WireMessage::Text("{\"jsonrpc\":\"2.0\",\"id\":2}".into()));
            assert!(stream.recv().await.is_none());

            let mut out_stream = NdjsonStream::with_max_line_bytes(MemBuf::new(Vec::new()), 16);
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
