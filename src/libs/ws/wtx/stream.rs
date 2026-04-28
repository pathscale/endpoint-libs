use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::AsyncReadExt;

use crate::libs::ws::BoxedStream;

pub(crate) struct TokioStreamAdapter {
    pub inner: BoxedStream,
    prefix: Vec<u8>,
}

impl TokioStreamAdapter {
    pub fn new(inner: BoxedStream) -> Self {
        Self {
            inner,
            prefix: Vec::new(),
        }
    }

    pub fn with_prefix(inner: BoxedStream, prefix: &[u8]) -> Self {
        Self {
            inner,
            prefix: prefix.to_vec(),
        }
    }

    pub fn prepend_bytes(&mut self, mut extra: Vec<u8>) {
        if self.prefix.is_empty() {
            self.prefix = extra;
        } else {
            extra.extend_from_slice(&self.prefix);
            self.prefix = extra;
        }
    }
}

impl tokio::io::AsyncRead for TokioStreamAdapter {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if !this.prefix.is_empty() {
            let to_copy = this.prefix.len().min(buf.remaining());
            buf.put_slice(&this.prefix[..to_copy]);
            this.prefix.drain(..to_copy);
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl tokio::io::AsyncWrite for TokioStreamAdapter {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

impl Unpin for TokioStreamAdapter {}

impl wtx::stream::StreamReader for TokioStreamAdapter {
    async fn read(&mut self, bytes: &mut [u8]) -> wtx::Result<usize> {
        if !self.prefix.is_empty() {
            let to_copy = self.prefix.len().min(bytes.len());
            bytes[..to_copy].copy_from_slice(&self.prefix[..to_copy]);
            self.prefix.drain(..to_copy);
            return Ok(to_copy);
        }
        Ok(AsyncReadExt::read(&mut self.inner, bytes).await?)
    }

    async fn read_skip(&mut self, len: usize) -> wtx::Result<()> {
        let from_prefix = self.prefix.len().min(len);
        self.prefix.drain(..from_prefix);
        let mut remaining = len - from_prefix;
        if remaining > 0 {
            let mut buf = [0u8; 512];
            while remaining > 0 {
                let to_read = remaining.min(buf.len());
                let n = AsyncReadExt::read(&mut self.inner, &mut buf[..to_read]).await?;
                if n == 0 {
                    return Err(wtx::Error::UnexpectedStreamReadEOF);
                }
                remaining -= n;
            }
        }
        Ok(())
    }
}

impl wtx::stream::StreamWriter for TokioStreamAdapter {
    async fn write_all(&mut self, bytes: &[u8]) -> wtx::Result<()> {
        use tokio::io::AsyncWriteExt;
        Ok(AsyncWriteExt::write_all(&mut self.inner, bytes).await?)
    }

    async fn write_all_vectored(&mut self, bytes: &[&[u8]]) -> wtx::Result<()> {
        for b in bytes {
            self.write_all(b).await?;
        }
        Ok(())
    }
}
