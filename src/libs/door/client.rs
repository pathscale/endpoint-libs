//! Counted exchanges with explicit framing policy and retained failure evidence.

use alloc::borrow::Cow;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::time::Duration;
use eko::libc_lite as libc;

use crate::libs::door::protocol::Reply;
use crate::libs::door::sys::Errno;

#[derive(Debug)]
pub enum WireError {
    Io(Errno),
    NoReply,
    Header(String),
    Truncated { promised: usize, delivered: usize },
}

impl core::fmt::Display for WireError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            WireError::Io(e) => write!(f, "{e}"),
            WireError::NoReply => write!(f, "the server closed the connection without replying"),
            WireError::Header(line) => write!(f, "reply header is not `ok <n>`/`err <n>`: {line:?}"),
            WireError::Truncated { promised, delivered } => {
                write!(f, "reply promised {promised} lines and delivered {delivered}")
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FramingPolicy {
    Legacy,
    Strict,
}

#[derive(Debug, Clone, Copy)]
pub struct ClientOptions {
    pub timeout: Option<Duration>,
    pub framing: FramingPolicy,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self { timeout: None, framing: FramingPolicy::Legacy }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExchangeOperation {
    Connect,
    Configure,
    WriteRequest,
    ReadHeader,
    ReadBody,
}

#[derive(Debug, Clone)]
pub enum ExchangeKind {
    Io(Errno),
    InvalidRequest,
    NoReply,
    InvalidHeader,
    InvalidUtf8,
    Truncated,
    Poisoned,
}

#[derive(Debug, Clone, Copy)]
pub struct ReplyLimits {
    pub max_lines: u64,
    /// Maximum counted body bytes as transmitted, including each line terminator.
    pub max_bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CountedReply {
    pub ok: bool,
    pub lines: u64,
    /// Counted body bytes as transmitted, including each line terminator.
    pub bytes: usize,
}

#[derive(Debug)]
pub enum LineRequestError<E> {
    Exchange(ExchangeError),
    LimitExceeded {
        promised: u64,
        delivered: u64,
        bytes: usize,
        limits: ReplyLimits,
    },
    Consumer {
        error: E,
        delivered: u64,
        bytes: usize,
    },
}

#[derive(Debug)]
enum ReadLineError {
    Io(Errno),
    TooLong(usize),
}

#[derive(Debug, Clone)]
pub struct ExchangeError {
    pub kind: ExchangeKind,
    pub operation: ExchangeOperation,
    pub request: Arc<[u8]>,
    pub header: Arc<[u8]>,
    pub partial: Arc<[u8]>,
    pub body: Vec<String>,
    pub promised: Option<u64>,
    pub delivered: u64,
    pub reason: Arc<str>,
}

impl core::fmt::Display for ExchangeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:?}: {}", self.operation, self.reason)
    }
}

const MAX_REPLY_HEADER_BYTES: usize = 4096;

pub struct Client {
    fd: libc::c_int,
    pending: Vec<u8>,
    options: ClientOptions,
    broken: Option<ExchangeError>,
    /// `Some` once [`Client::use_framed_wire`] has been called: requests go as endpoint-libs frames.
    framed: Option<crate::libs::door::frame::FrameDecoder>,
}

pub(crate) fn c_path(path: &str) -> Vec<u8> {
    let mut bytes = path.to_string().into_bytes();
    bytes.push(0);
    bytes
}

fn evidence(
    kind: ExchangeKind,
    operation: ExchangeOperation,
    request: &[u8],
    header: &[u8],
    partial: &[u8],
    body: &[String],
    promised: Option<u64>,
    reason: impl Into<Arc<str>>,
) -> ExchangeError {
    ExchangeError {
        kind,
        operation,
        request: Arc::from(request),
        header: Arc::from(header),
        partial: Arc::from(partial),
        body: body.to_vec(),
        promised,
        delivered: body.len() as u64,
        reason: reason.into(),
    }
}

fn errno_code() -> libc::c_int {
    Errno::current().0
}

pub(crate) fn last_error() -> Errno {
    Errno::current()
}

fn socket_option<T>(
    fd: libc::c_int,
    option: libc::c_int,
    value: &T,
) -> Result<(), Errno> {
    let result = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            option,
            (value as *const T).cast(),
            core::mem::size_of::<T>() as libc::socklen_t,
        )
    };
    if result < 0 { Err(last_error()) } else { Ok(()) }
}

fn configure(fd: libc::c_int, options: ClientOptions) -> Result<(), Errno> {
    if unsafe { libc::fcntl3(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(last_error());
    }
    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd",
              target_os = "openbsd", target_os = "netbsd", target_os = "dragonfly"))]
    socket_option(fd, libc::SO_NOSIGPIPE, &1i32)?;
    if let Some(timeout) = options.timeout {
        let micros = timeout.as_micros()
            + u128::from(timeout.subsec_nanos() % 1_000 != 0);
        if micros == 0 {
            return Err(Errno(libc::EINVAL));
        }
        let seconds = (micros / 1_000_000).try_into().map_err(|_| Errno(libc::EINVAL))?;
        let timeval = libc::timeval {
            tv_sec: seconds,
            tv_usec: (micros % 1_000_000) as _,
        };
        socket_option(fd, libc::SO_RCVTIMEO, &timeval)?;
        socket_option(fd, libc::SO_SNDTIMEO, &timeval)?;
    }
    Ok(())
}

impl Client {
    pub fn connect(socket: &str) -> Result<Client, Errno> {
        Self::connect_with(socket.as_bytes(), ClientOptions::default()).map_err(|error| {
            match error.kind {
                ExchangeKind::Io(error) => error,
                ExchangeKind::InvalidRequest => Errno(libc::EINVAL),
                _ => Errno(libc::EIO),
            }
        })
    }

    pub fn connect_waiting(socket: &str, within: Duration) -> Result<Client, Errno> {
        let deadline = eko::clock::now_ns() + within.as_nanos() as u64;
        loop {
            match Self::connect(socket) {
                Ok(client) => return Ok(client),
                Err(error) if eko::clock::now_ns() >= deadline => return Err(error),
                Err(_) => crate::libs::door::sys::sleep_ms(10),
            }
        }
    }

    pub fn connect_with(socket: &[u8], options: ClientOptions) -> Result<Client, ExchangeError> {
        let mut address: libc::sockaddr_un = unsafe { core::mem::zeroed() };
        if socket.is_empty() || socket.contains(&0) || socket.len() >= address.sun_path.len() {
            return Err(evidence(
                ExchangeKind::InvalidRequest, ExchangeOperation::Connect,
                socket, &[], &[], &[], None,
                "socket path is empty, contains NUL, or exceeds sockaddr_un",
            ));
        }
        address.sun_family = libc::AF_UNIX as _;
        for (out, byte) in address.sun_path.iter_mut().zip(socket) {
            *out = *byte as _;
        }
        let length = core::mem::size_of::<libc::sockaddr_un>()
            - core::mem::size_of_val(&address.sun_path) + socket.len() + 1;
        #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd",
                  target_os = "openbsd", target_os = "netbsd", target_os = "dragonfly"))]
        {
            address.sun_len = length as _;
        }
        let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
        if fd < 0 {
            let error = last_error();
            return Err(evidence(
                ExchangeKind::Io(error), ExchangeOperation::Connect,
                socket, &[], &[], &[], None, format!("socket creation failed: {error}"),
            ));
        }
        let client = Client { fd, pending: Vec::new(), options, broken: None, framed: None };
        if let Err(error) = configure(fd, options) {
            return Err(evidence(
                ExchangeKind::Io(error), ExchangeOperation::Configure,
                socket, &[], &[], &[], None, format!("socket configuration failed: {error}"),
            ));
        }
        let result = unsafe {
            libc::connect(
                fd,
                (&address as *const libc::sockaddr_un).cast(),
                length as libc::socklen_t,
            )
        };
        if result < 0 {
            let error = last_error();
            return Err(evidence(
                ExchangeKind::Io(error), ExchangeOperation::Connect,
                socket, &[], &[], &[], None, format!("socket connection failed: {error}"),
            ));
        }
        Ok(client)
    }

    /// Speak the framed wire (`crate::libs::door::frame`) on this connection from the next request on. The
    /// server must be one that lists `framed-v1` in `help`; the counted wire stays the default.
    pub fn use_framed_wire(&mut self) {
        self.framed = Some(crate::libs::door::frame::FrameDecoder::new());
    }

    pub fn request(&mut self, line: &str) -> Result<Reply, WireError> {
        self.request_checked(line).map_err(|error| {
            match error.kind {
                ExchangeKind::Io(error) => WireError::Io(error),
                ExchangeKind::NoReply => WireError::NoReply,
                ExchangeKind::Truncated => WireError::Truncated {
                    promised: error.promised.unwrap_or(0) as usize,
                    delivered: error.delivered as usize,
                },
                ExchangeKind::InvalidHeader | ExchangeKind::InvalidUtf8 => {
                    let header = String::from_utf8_lossy(&error.header);
                    WireError::Header(header.strip_suffix('\n').unwrap_or(&header).to_string())
                }
                ExchangeKind::InvalidRequest => WireError::Io(Errno(libc::EINVAL)),
                ExchangeKind::Poisoned => WireError::Io(Errno(libc::EPROTO)),
            }
        })
    }

    pub fn request_checked(&mut self, line: &str) -> Result<Reply, ExchangeError> {
        let mut body = Vec::new();
        let limits = ReplyLimits { max_lines: u64::MAX, max_bytes: usize::MAX };
        match self.request_checked_lines(line, limits, |line| {
            body.push(line.to_string());
            Ok::<(), core::convert::Infallible>(())
        }) {
            Ok(counted) => Ok(Reply { ok: counted.ok, body }),
            Err(LineRequestError::Exchange(mut error)) => {
                error.body = body;
                error.delivered = error.delivered.max(error.body.len() as u64);
                Err(error)
            }
            Err(LineRequestError::LimitExceeded { promised, delivered, bytes, limits }) => {
                let mut error = evidence(
                    ExchangeKind::InvalidHeader, ExchangeOperation::ReadBody,
                    line.as_bytes(), &[], &[], &body, Some(promised),
                    format!("reply delivered {delivered} lines and {bytes} bytes beyond limits {} lines and {} bytes",
                        limits.max_lines, limits.max_bytes),
                );
                error.delivered = delivered;
                Err(error)
            }
            Err(LineRequestError::Consumer { error: never, .. }) => match never {},
        }
    }

    /// Consume a counted reply one line at a time with explicit caller-owned limits.
    /// Body byte accounting includes line terminators. A consumer refusal is returned only
    /// after the declared frame has been drained, so the next request starts at its header.
    pub fn request_checked_lines<F, E>(
        &mut self,
        line: &str,
        limits: ReplyLimits,
        mut consume: F,
    ) -> Result<CountedReply, LineRequestError<E>>
    where
        F: FnMut(&str) -> Result<(), E>,
    {
        if let Some(previous) = &self.broken {
            let mut error = previous.clone();
            error.kind = ExchangeKind::Poisoned;
            error.reason = Arc::from(format!(
                "request was not sent: connection is poisoned by {:?}: {}",
                previous.operation, previous.reason,
            ));
            return Err(LineRequestError::Exchange(error));
        }
        let result = self.exchange_lines(line, limits, &mut consume);
        if let Err(LineRequestError::Exchange(error)) = &result {
            self.broken = Some(error.clone());
        }
        result
    }

    fn exchange_lines<F, E>(
        &mut self,
        line: &str,
        limits: ReplyLimits,
        consume: &mut F,
    ) -> Result<CountedReply, LineRequestError<E>>
    where
        F: FnMut(&str) -> Result<(), E>,
    {
        if self.framed.is_some() {
            return self.exchange_framed(line, limits, consume);
        }
        let request = line.as_bytes();
        if self.options.framing == FramingPolicy::Strict
            && (line.is_empty() || request.iter().any(|b| matches!(b, b'\r' | b'\n' | 0)))
        {
            return Err(LineRequestError::Exchange(evidence(
                ExchangeKind::InvalidRequest, ExchangeOperation::WriteRequest,
                request, &[], &[], &[], None,
                "strict requests must be nonempty and contain no CR, LF or NUL",
            )));
        }
        let mut wire = request.to_vec();
        wire.push(b'\n');
        let mut written = 0usize;
        while written < wire.len() {
            #[cfg(any(target_os = "linux", target_os = "android"))]
            let flags = libc::MSG_NOSIGNAL;
            #[cfg(not(any(target_os = "linux", target_os = "android")))]
            let flags = 0;
            let count = unsafe {
                libc::send(
                    self.fd, wire[written..].as_ptr().cast(), wire.len() - written, flags,
                )
            };
            if count < 0 {
                let code = errno_code();
                if code == libc::EINTR {
                    continue;
                }
                let error = Errno(code);
                return Err(LineRequestError::Exchange(evidence(
                    ExchangeKind::Io(error), ExchangeOperation::WriteRequest,
                    request, &[], &wire[..written], &[], None,
                    format!("request write failed: {error}"),
                )));
            }
            if count == 0 {
                return Err(LineRequestError::Exchange(evidence(
                    ExchangeKind::Io(Errno(libc::EPIPE)), ExchangeOperation::WriteRequest,
                    request, &[], &wire[..written], &[], None,
                    "request write made no progress",
                )));
            }
            written += count as usize;
        }

        let header = match self.read_line_bounded(MAX_REPLY_HEADER_BYTES) {
            Ok(Some(header)) => header,
            Ok(None) => {
                let incomplete = !self.pending.is_empty();
                return Err(LineRequestError::Exchange(evidence(
                    if incomplete { ExchangeKind::Truncated } else { ExchangeKind::NoReply },
                    ExchangeOperation::ReadHeader,
                    request, &[], &self.pending, &[], None,
                    if incomplete {
                        "reply header was not newline terminated"
                    } else {
                        "the server closed the connection without replying"
                    },
                )));
            }
            Err(ReadLineError::Io(error)) => return Err(LineRequestError::Exchange(evidence(
                ExchangeKind::Io(error), ExchangeOperation::ReadHeader,
                request, &[], &self.pending, &[], None,
                format!("reply header read failed: {error}"),
            ))),
            Err(ReadLineError::TooLong(_)) => return Err(LineRequestError::Exchange(evidence(
                ExchangeKind::InvalidHeader, ExchangeOperation::ReadHeader,
                request, &[], &self.pending, &[], None,
                "reply header exceeds the 4096-byte framing limit",
            ))),
        };
        let header_text = self.line_text(&header).map_err(|why| {
            LineRequestError::Exchange(evidence(
                ExchangeKind::InvalidUtf8, ExchangeOperation::ReadHeader,
                request, &header, &header, &[], None, why,
            ))
        })?;
        let (ok, promised) = self.header(&header_text).ok_or_else(|| {
            LineRequestError::Exchange(evidence(
                ExchangeKind::InvalidHeader, ExchangeOperation::ReadHeader,
                request, &header, &[], &[], None,
                "reply header is not a counted ok/err header",
            ))
        })?;

        let mut delivered = 0u64;
        let mut bytes_total = 0usize;
        let mut limit_exceeded = promised > limits.max_lines;
        let mut exchange_error = None;
        let mut consumer_error = None;
        for _ in 0..promised {
            let remaining_bytes = limits.max_bytes.saturating_sub(bytes_total);
            let bytes = match self.read_line_bounded(remaining_bytes) {
                Ok(Some(bytes)) => bytes,
                Ok(None) => {
                    let mut error = evidence(
                        ExchangeKind::Truncated, ExchangeOperation::ReadBody,
                        request, &header, &self.pending, &[], Some(promised),
                        format!("reply promised {promised} lines and delivered {delivered}"),
                    );
                    error.delivered = delivered;
                    return Err(LineRequestError::Exchange(error));
                }
                Err(ReadLineError::Io(error)) => {
                    let mut error = evidence(
                        ExchangeKind::Io(error), ExchangeOperation::ReadBody,
                        request, &header, &self.pending, &[], Some(promised),
                        format!("reply body read failed: {error}"),
                    );
                    error.delivered = delivered;
                    return Err(LineRequestError::Exchange(error));
                }
                Err(ReadLineError::TooLong(line_bytes)) => {
                    delivered = delivered.saturating_add(1);
                    bytes_total = bytes_total.saturating_add(line_bytes);
                    limit_exceeded = true;
                    continue;
                }
            };
            delivered = delivered.saturating_add(1);
            // `bytes` is still the wire line here, so its length includes the LF.
            let line_wire_bytes = bytes.len();
            bytes_total = match bytes_total.checked_add(line_wire_bytes) {
                Some(total) => total,
                None => {
                    limit_exceeded = true;
                    usize::MAX
                }
            };
            if bytes_total > limits.max_bytes {
                limit_exceeded = true;
            }
            let text = match self.line_view(&bytes) {
                Ok(text) => text,
                Err(why) => {
                    if exchange_error.is_none() {
                        exchange_error = Some(evidence(
                            ExchangeKind::InvalidUtf8, ExchangeOperation::ReadBody,
                            request, &header, &bytes, &[], Some(promised), why,
                        ));
                    }
                    continue;
                }
            };
            if exchange_error.is_none()
                && consumer_error.is_none()
                && !limit_exceeded
                && delivered <= limits.max_lines
            {
                if let Err(error) = consume(text.as_ref()) {
                    consumer_error = Some(error);
                }
            }
        }

        if let Some(mut error) = exchange_error {
            error.delivered = delivered;
            return Err(LineRequestError::Exchange(error));
        }
        if limit_exceeded || delivered != promised || bytes_total > limits.max_bytes {
            return Err(LineRequestError::LimitExceeded {
                promised, delivered, bytes: bytes_total, limits,
            });
        }
        if let Some(error) = consumer_error {
            return Err(LineRequestError::Consumer {
                error, delivered, bytes: bytes_total,
            });
        }
        Ok(CountedReply { ok, lines: delivered, bytes: bytes_total })
    }

    /// One request and its reply on the framed wire. The reply is one Text frame: `ok` or `err`,
    /// then the body lines. Limits and the consumer apply to those lines as they do to a counted
    /// reply; the whole frame is already in memory, so a refusal leaves the stream on a frame boundary.
    fn exchange_framed<F, E>(
        &mut self,
        line: &str,
        limits: ReplyLimits,
        consume: &mut F,
    ) -> Result<CountedReply, LineRequestError<E>>
    where
        F: FnMut(&str) -> Result<(), E>,
    {
        use crate::libs::door::frame::{Frame, FrameDecoder};
        let request = line.as_bytes();
        let fail = |kind: ExchangeKind, operation: ExchangeOperation, header: &[u8], reason: String| {
            LineRequestError::Exchange(evidence(
                kind, operation, request, header, &[], &[], None, reason,
            ))
        };
        if line.is_empty() || request.iter().any(|b| matches!(b, b'\r' | b'\n' | 0)) {
            return Err(fail(
                ExchangeKind::InvalidRequest, ExchangeOperation::WriteRequest, &[],
                "framed requests must be nonempty and contain no CR, LF or NUL".to_string(),
            ));
        }
        let mut wire = Vec::new();
        if let Err(why) = crate::libs::door::frame::encode_text(line, &mut wire) {
            return Err(fail(
                ExchangeKind::InvalidRequest, ExchangeOperation::WriteRequest, &[], format!("{why}"),
            ));
        }
        let mut written = 0usize;
        while written < wire.len() {
            #[cfg(any(target_os = "linux", target_os = "android"))]
            let flags = libc::MSG_NOSIGNAL;
            #[cfg(not(any(target_os = "linux", target_os = "android")))]
            let flags = 0;
            let count = unsafe {
                libc::send(self.fd, wire[written..].as_ptr().cast(), wire.len() - written, flags)
            };
            if count < 0 {
                let code = errno_code();
                if code == libc::EINTR {
                    continue;
                }
                let error = Errno(code);
                return Err(fail(
                    ExchangeKind::Io(error), ExchangeOperation::WriteRequest, &[],
                    format!("request write failed: {error}"),
                ));
            }
            if count == 0 {
                return Err(fail(
                    ExchangeKind::Io(Errno(libc::EPIPE)), ExchangeOperation::WriteRequest, &[],
                    "request write made no progress".to_string(),
                ));
            }
            written += count as usize;
        }
        let mut decoder = self.framed.take().unwrap_or_else(FrameDecoder::new);
        let mut buf = [0u8; 8192];
        let payload = loop {
            match decoder.next_frame() {
                Ok(Some(Frame::Text(bytes))) => break bytes,
                Ok(Some(Frame::Close(_))) => {
                    self.framed = Some(decoder);
                    return Err(fail(
                        ExchangeKind::NoReply, ExchangeOperation::ReadHeader, &[],
                        "the server closed the connection without replying".to_string(),
                    ));
                }
                Ok(Some(_)) => continue,
                Err(why) => {
                    self.framed = Some(decoder);
                    return Err(fail(
                        ExchangeKind::InvalidHeader, ExchangeOperation::ReadHeader, &[],
                        format!("reply frame refused: {why}"),
                    ));
                }
                Ok(None) => {}
            }
            match crate::libs::door::frame::read_some(self.fd, &mut buf) {
                Ok(0) => {
                    let incomplete = decoder.buffered() != 0;
                    self.framed = Some(decoder);
                    return Err(fail(
                        if incomplete { ExchangeKind::Truncated } else { ExchangeKind::NoReply },
                        ExchangeOperation::ReadHeader, &[],
                        if incomplete { "reply frame was cut off" } else {
                            "the server closed the connection without replying"
                        }.to_string(),
                    ));
                }
                Ok(got) => decoder.push(&buf[..got]),
                Err(error) => {
                    self.framed = Some(decoder);
                    return Err(fail(
                        ExchangeKind::Io(error), ExchangeOperation::ReadHeader, &[],
                        format!("reply read failed: {error}"),
                    ));
                }
            }
        };
        self.framed = Some(decoder);
        let Some(reply) = core::str::from_utf8(&payload).ok().and_then(crate::libs::door::frame::parse_reply_payload)
        else {
            return Err(fail(
                ExchangeKind::InvalidHeader, ExchangeOperation::ReadHeader, &payload,
                "reply frame does not start with an ok/err line".to_string(),
            ));
        };
        let promised = reply.body.len() as u64;
        let bytes: usize = reply.body.iter().map(|line| line.len() + 1).sum();
        if promised > limits.max_lines || bytes > limits.max_bytes {
            return Err(LineRequestError::LimitExceeded {
                promised, delivered: promised, bytes, limits,
            });
        }
        let mut delivered = 0u64;
        for body in &reply.body {
            delivered += 1;
            if let Err(error) = consume(body) {
                return Err(LineRequestError::Consumer { error, delivered, bytes });
            }
        }
        Ok(CountedReply { ok: reply.ok, lines: delivered, bytes })
    }

    fn header(&self, text: &str) -> Option<(bool, u64)> {
        let text = if self.options.framing == FramingPolicy::Legacy {
            text.trim_end()
        } else {
            text
        };
        let mut parts = text.split(' ');
        let ok = match parts.next()? {
            "ok" => true,
            "err" => false,
            _ => return None,
        };
        let count = parts.next()?;
        if self.options.framing == FramingPolicy::Strict {
            if parts.next().is_some()
                || count.is_empty()
                || !count.bytes().all(|b| b.is_ascii_digit())
                || (count.len() > 1 && count.starts_with('0'))
            {
                return None;
            }
            Some((ok, count.parse::<u64>().ok()?))
        } else {
            Some((ok, count.parse::<usize>().ok()? as u64))
        }
    }

    fn line_view<'a>(&self, bytes: &'a [u8]) -> Result<Cow<'a, str>, String> {
        let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
        match self.options.framing {
            FramingPolicy::Strict => core::str::from_utf8(bytes)
                .map(Cow::Borrowed)
                .map_err(|e| format!("reply line is not UTF-8: {e}")),
            FramingPolicy::Legacy => {
                let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
                Ok(String::from_utf8_lossy(bytes))
            }
        }
    }

    fn line_text(&self, bytes: &[u8]) -> Result<String, String> {
        let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
        match self.options.framing {
            FramingPolicy::Strict => core::str::from_utf8(bytes)
                .map(str::to_string)
                .map_err(|e| format!("reply line is not UTF-8: {e}")),
            FramingPolicy::Legacy => {
                let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
                Ok(String::from_utf8_lossy(bytes).into_owned())
            }
        }
    }

    fn read_line_bounded(
        &mut self,
        limit: usize,
    ) -> Result<Option<Vec<u8>>, ReadLineError> {
        if limit == usize::MAX {
            return self.read_line().map_err(ReadLineError::Io);
        }
        let mut too_long = false;
        let mut line_bytes = 0usize;
        let mut scanned = 0usize;
        loop {
            if let Some(offset) = self.pending[scanned..].iter().position(|byte| *byte == b'\n') {
                let at = scanned + offset;
                let length = at + 1;
                let rest = self.pending.split_off(length);
                let line = core::mem::replace(&mut self.pending, rest);
                if too_long || length > limit {
                    return Err(ReadLineError::TooLong(line_bytes.saturating_add(length)));
                }
                return Ok(Some(line));
            }
            if !too_long && self.pending.len() > limit {
                too_long = true;
                line_bytes = self.pending.len();
                self.pending.clear();
            }
            scanned = self.pending.len();
            let mut buffer = [0u8; 4096];
            let count = unsafe {
                libc::recv(self.fd, buffer.as_mut_ptr().cast(), buffer.len(), 0)
            };
            if count < 0 {
                let code = errno_code();
                if code == libc::EINTR {
                    continue;
                }
                return Err(ReadLineError::Io(Errno(code)));
            }
            if count == 0 {
                if self.options.framing == FramingPolicy::Legacy && !self.pending.is_empty() {
                    if too_long {
                        return Err(ReadLineError::TooLong(line_bytes));
                    }
                    return Ok(Some(core::mem::take(&mut self.pending)));
                }
                return Ok(None);
            }
            let count = count as usize;
            if too_long {
                let received = &buffer[..count];
                if let Some(at) = received.iter().position(|byte| *byte == b'\n') {
                    line_bytes = line_bytes.saturating_add(at + 1);
                    self.pending.clear();
                    self.pending.extend_from_slice(&received[at + 1..]);
                    return Err(ReadLineError::TooLong(line_bytes));
                }
                line_bytes = line_bytes.saturating_add(count);
            } else {
                self.pending.extend_from_slice(&buffer[..count]);
            }
        }
    }

    fn read_line(&mut self) -> Result<Option<Vec<u8>>, Errno> {
        loop {
            if let Some(at) = self.pending.iter().position(|byte| *byte == b'\n') {
                let rest = self.pending.split_off(at + 1);
                return Ok(Some(core::mem::replace(&mut self.pending, rest)));
            }
            let mut buffer = [0u8; 4096];
            let count = unsafe {
                libc::recv(self.fd, buffer.as_mut_ptr().cast(), buffer.len(), 0)
            };
            if count < 0 {
                let code = errno_code();
                if code == libc::EINTR {
                    continue;
                }
                return Err(Errno(code));
            }
            if count == 0 {
                return if self.options.framing == FramingPolicy::Legacy && !self.pending.is_empty() {
                    Ok(Some(core::mem::take(&mut self.pending)))
                } else {
                    Ok(None)
                };
            }
            self.pending.extend_from_slice(&buffer[..count as usize]);
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd); }
    }
}
