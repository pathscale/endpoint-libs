//! The framed wire: the endpoint-libs length-delimited frame carrying today's request line.
//!
//! **Default taken (owner may overrule):** the payload is the smallest one. A request is one
//! Text frame holding the request line exactly as the counted wire carries it, without the
//! newline. A reply is one Text frame: a verdict line `ok` or `err`, then the body lines
//! unchanged. The frame supplies the length, so there is no count to get wrong. MCP lines
//! (starting with `{`) travel the same way, one JSON document per Text frame.
//!
//! Discovery: a connection whose first byte is `0x00` is framed (a frame starts with the high
//! byte of a length below 16 MiB). Every counted request starts with an ASCII verb or `{`, so the
//! two cannot be confused and the counted wire is untouched. `help` lists this mode.

use alloc::string::String;
use alloc::vec::Vec;
use eko::libc_lite as libc;

use crate::libs::door::protocol::Reply;
use crate::libs::door::sys::Errno;

pub use crate::libs::frame::{
    DEFAULT_MAX_FRAME_BYTES, Frame, FrameDecoder, FrameError, KIND_TEXT, encode_close,
    encode_frame, encode_text, KIND_PONG,
};

/// First byte of every framed connection.
pub const FIRST_BYTE: u8 = 0;
pub const WIRE_NAME: &str = "framed-v1";
pub const WIRE_VERSION: u32 = 1;

/// The line `help` appends so that a client can find this mode.
pub const HELP_LINE: &str = "wire: counted (default: reply `ok N`/`err N` then N lines) or framed-v1 (first byte 0x00; endpoint-libs frame: u32 BE length, kind byte, payload; request is a Text frame holding the request line, reply is a Text frame of `ok`/`err` then the body lines)";

/// `ok`/`err` then every body line, each newline terminated.
pub fn reply_payload(reply: &Reply) -> String {
    let mut out = String::new();
    out.push_str(if reply.ok { "ok\n" } else { "err\n" });
    for element in &reply.body {
        for line in element.split('\n') {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// The inverse of [`reply_payload`]; `None` when the verdict line is neither `ok` nor `err`.
pub fn parse_reply_payload(text: &str) -> Option<Reply> {
    let text = text.strip_suffix('\n').unwrap_or(text);
    let mut lines = text.split('\n');
    let ok = match lines.next()? {
        "ok" => true,
        "err" => false,
        _ => return None,
    };
    Some(Reply { ok, body: lines.map(String::from).collect() })
}

/// What a reply costs on the wire, or the refusal that replaces a reply too large for one frame.
pub fn reply_frame(reply: &Reply) -> Vec<u8> {
    let mut payload = reply_payload(reply);
    if payload.len() >= DEFAULT_MAX_FRAME_BYTES {
        payload = String::from("err\nreply exceeds the 16 MiB frame limit\n");
    }
    let mut out = Vec::new();
    let _ = encode_text(&payload, &mut out);
    out
}

const MSG_PEEK: libc::c_int = 0x2;

/// Look at the first byte of a connection without consuming it. `None` is end of stream.
pub fn peek_first(fd: libc::c_int) -> Result<Option<u8>, Errno> {
    let mut byte = [0u8; 1];
    loop {
        let got = unsafe { libc::recv(fd, byte.as_mut_ptr().cast(), 1, MSG_PEEK) };
        if got < 0 {
            let error = crate::libs::door::client::last_error();
            if error.0 == libc::EINTR {
                continue;
            }
            return Err(error);
        }
        return Ok(if got == 0 { None } else { Some(byte[0]) });
    }
}

/// Read once from a connection. Zero is end of stream.
pub fn read_some(fd: libc::c_int, buf: &mut [u8]) -> Result<usize, Errno> {
    loop {
        let got = unsafe { libc::recv(fd, buf.as_mut_ptr().cast(), buf.len(), 0) };
        if got < 0 {
            let error = crate::libs::door::client::last_error();
            if error.0 == libc::EINTR {
                continue;
            }
            return Err(error);
        }
        return Ok(got as usize);
    }
}
