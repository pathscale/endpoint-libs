//! A verdict followed by exactly the declared number of body lines.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

pub const PROTOCOL_VERSION: u32 = 1;

// The framed wire (`crate::libs::door::frame`) is an addition beside the counted one; it does not change
// `PROTOCOL_VERSION`. Its own version is `crate::libs::door::frame::WIRE_VERSION`.

#[derive(Debug, Clone, PartialEq)]
pub struct Reply {
    pub ok: bool,
    pub body: Vec<String>,
}

impl Reply {
    pub fn ok(body: impl Into<Body>) -> Reply {
        Reply { ok: true, body: body.into().0 }
    }

    pub fn err(body: impl Into<Body>) -> Reply {
        Reply { ok: false, body: body.into().0 }
    }

    /// The count is of **lines on the wire**, not of body elements. An element that carries a
    /// newline (a `help` entry whose description wraps onto a second line) used to be counted
    /// once and written as two, so the reader stopped short and left the rest in the stream to
    /// be read as the start of the next reply: `help` promised 41 lines and wrote 46.
    pub fn wire(&self) -> String {
        use core::fmt::Write as _;
        let lines: Vec<&str> = self.body.iter().flat_map(|element| element.split('\n')).collect();
        let mut out = String::new();
        let _ = writeln!(out, "{} {}", if self.ok { "ok" } else { "err" }, lines.len());
        for line in lines {
            let _ = writeln!(out, "{line}");
        }
        out
    }
    /// Serialize this reply only when its physical line count and total wire size fit the
    /// caller's limits. The byte limit includes the status, count, and every newline.
    pub fn wire_bounded(&self, max_lines: usize, max_bytes: usize) -> Result<String, String> {
        let prefix = if self.ok { "ok " } else { "err " };
        let mut line_count = 0usize;
        let mut body_bytes = 0usize;
        for element in &self.body {
            for line in element.split('\n') {
                line_count = line_count
                    .checked_add(1)
                    .ok_or_else(|| "reply line count overflow".to_string())?;
                body_bytes = body_bytes
                    .checked_add(line.len())
                    .and_then(|bytes| bytes.checked_add(1))
                    .ok_or_else(|| "reply byte count overflow".to_string())?;
            }
        }

        let header_bytes = prefix
            .len()
            .checked_add(private_decimal_digits(line_count))
            .and_then(|bytes| bytes.checked_add(1))
            .ok_or_else(|| "reply byte count overflow".to_string())?;
        let total_bytes = header_bytes
            .checked_add(body_bytes)
            .ok_or_else(|| "reply byte count overflow".to_string())?;
        if line_count > max_lines {
            return Err("reply line limit exceeded".to_string());
        }
        if total_bytes > max_bytes {
            return Err("reply byte limit exceeded".to_string());
        }

        let mut out = String::new();
        out.try_reserve_exact(total_bytes)
            .map_err(|_| "reply allocation failed".to_string())?;
        out.push_str(prefix);
        private_push_decimal(&mut out, line_count);
        out.push('\n');
        for element in &self.body {
            for line in element.split('\n') {
                out.push_str(line);
                out.push('\n');
            }
        }
        Ok(out)
    }
}

fn private_decimal_digits(mut value: usize) -> usize {
    let mut digits = 1;
    while value >= 10 {
        value /= 10;
        digits += 1;
    }
    digits
}

fn private_push_decimal(out: &mut String, mut value: usize) {
    let mut digits = [0u8; 20];
    let mut start = digits.len();
    loop {
        start -= 1;
        digits[start] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    for digit in &digits[start..] {
        out.push(char::from(*digit));
    }
}

pub struct Body(pub Vec<String>);

impl From<String> for Body {
    fn from(text: String) -> Body {
        Body(text.strip_suffix('\n').unwrap_or(&text).split('\n').map(str::to_string).collect())
    }
}

impl From<&str> for Body {
    fn from(text: &str) -> Body {
        Body::from(text.to_string())
    }
}

impl From<Vec<String>> for Body {
    fn from(lines: Vec<String>) -> Body {
        Body(lines)
    }
}
