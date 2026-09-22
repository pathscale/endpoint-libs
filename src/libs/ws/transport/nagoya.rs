//! A Nagoya socket as a `futures-io` byte stream.
//!
//! [`framed_json_neutral`](super::framed::framed_json_neutral) asks for
//! [`futures::io::AsyncRead`]/[`AsyncWrite`], which is the trait pair that names no
//! runtime. Nagoya's socket does not implement it: it implements Nagoya's own
//! [`Stream`](nagoya::io::Stream), whose `read` and `write_all` are `async fn` rather
//! than `poll_` methods. Nagoya's `io::Compat` does not close that gap either, since it
//! runs the other way, presenting a `futures-io` stream to a Nagoya consumer.
//!
//! The adapter is thin because `TcpStream` already exposes the poll-shaped methods
//! underneath its futures: `poll_read`, `poll_write`, and `poll_flush` are public and have
//! exactly the `futures-io` signature. So this is delegation, not a bridge — no boxed
//! future, no buffer, and no second copy of the readiness logic. Had those methods been
//! private, this file would have had to poll an `async fn` borrowing `&mut self` from
//! inside `poll_read`, which is not something a wrapper can do soundly.
//!
//! `TcpStream` is the type for a Unix-domain connection too, so there is one adapter
//! rather than one per address family.
//!
//! ```no_run
//! # use endpoint_libs::libs::ws::transport::{framed::framed_json_neutral, nagoya::NagoyaStream};
//! # fn example(socket: nagoya::net::TcpStream) {
//! let transport = framed_json_neutral(NagoyaStream::new(socket));
//! # let _ = transport;
//! # }
//! ```

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures::io::{AsyncRead, AsyncWrite};
use nagoya::io::StreamError;
use nagoya::net::TcpStream;

/// A Nagoya [`TcpStream`] presented as a `futures-io` byte stream.
#[derive(Debug)]
pub struct NagoyaStream(TcpStream);

impl NagoyaStream {
    pub const fn new(stream: TcpStream) -> Self {
        Self(stream)
    }

    /// The socket back, for a caller that wants the Nagoya API again.
    pub fn into_inner(self) -> TcpStream {
        self.0
    }

    pub const fn get_ref(&self) -> &TcpStream {
        &self.0
    }

    pub const fn get_mut(&mut self) -> &mut TcpStream {
        &mut self.0
    }
}

impl From<TcpStream> for NagoyaStream {
    fn from(stream: TcpStream) -> Self {
        Self::new(stream)
    }
}

/// The platform's own number, which is what Nagoya carries and what `std` wants.
fn io_error(error: StreamError) -> io::Error {
    io::Error::from_raw_os_error(error.0)
}

impl AsyncRead for NagoyaStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        self.get_mut().0.poll_read(cx, buffer).map_err(io_error)
    }
}

impl AsyncWrite for NagoyaStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.get_mut().0.poll_write(cx, buffer).map_err(io_error)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.get_mut().0.poll_flush(cx).map_err(io_error)
    }

    /// Flush, and leave the close to the drop.
    ///
    /// Nagoya exposes no half-close, so this cannot shut the write side down while
    /// leaving the read side open. Reporting success after a flush is the honest answer
    /// available: everything written has been handed to the kernel, and the descriptor is
    /// closed when the stream is dropped. A peer that needs to see end-of-stream before
    /// the drop needs a half-close on the Nagoya side first.
    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_flush(cx)
    }
}
