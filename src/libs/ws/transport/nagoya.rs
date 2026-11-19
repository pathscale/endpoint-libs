//! A Nagoya socket as a [`PollRead`]/[`PollWrite`] byte stream.
//!
//! [`framed_json_neutral`](super::framed::framed_json_neutral) asks for
//! [`PollRead`]/[`PollWrite`], which is the poll-based byte stream trait pair over
//! `ekostd` that names no runtime and no `std`. `TcpStream` exposes `poll_read`,
//! `poll_write`, and `poll_flush` directly, so this adapter delegates without a boxed
//! future or extra buffer.

use core::pin::Pin;
use core::task::{Context, Poll};

use eko::compat::io;
use eko::Errno;
use nagoya::io::StreamError;
use nagoya::net::TcpStream;

use super::{PollRead, PollWrite};

/// A Nagoya [`TcpStream`] presented as a [`PollRead`]/[`PollWrite`] byte stream.
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

fn io_error(error: StreamError) -> io::Error {
    io::Error::from_errno(Errno(error.0), "socket")
}

impl PollRead for NagoyaStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        self.get_mut().0.poll_read(cx, buffer).map_err(io_error)
    }
}

impl PollWrite for NagoyaStream {
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

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_flush(cx)
    }
}
