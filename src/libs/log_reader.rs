use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::str::FromStr;

use crate::eyre::{self, ContextCompat};
use tracing::warn;

#[derive(Debug, Clone)]
pub struct LogEntry {
    pub datetime: i64,
    pub level: String,
    pub thread: String,
    pub path: String,
    pub line_number: usize,
    pub message: String,
}

/// Strip ANSI CSI SGR escape sequences (`\x1b\[[0-9;]*m`) without `regex`.
fn strip_ansi_csi(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'[') {
            let mut j = i + 2;
            while j < bytes.len() && (bytes[j].is_ascii_digit() || bytes[j] == b';') {
                j += 1;
            }
            if bytes.get(j) == Some(&b'm') {
                i = j + 1;
                continue;
            }
        }
        let ch = s[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

impl FromStr for LogEntry {
    type Err = eyre::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let no_control_sequence = strip_ansi_csi(s);
        let mut split = no_control_sequence.split_whitespace();
        let datetime = split
            .next()
            .and_then(|x| chrono::DateTime::parse_from_rfc3339(x).ok())
            .context("no datetime")?;
        let level: tracing::Level = split
            .next()
            .and_then(|x| x.parse().ok())
            .context("no level")?;
        let thread = split.next().context("no thread")?;
        let path = split
            .next()
            .and_then(|x| x.strip_suffix(':'))
            .map(str::to_string)
            .context("no path")?;

        let line_number = split
            .next()
            .and_then(|x| x.strip_suffix(':').and_then(|line| line.parse().ok()))
            .unwrap_or(0);
        let message = split.collect::<Vec<&str>>().join(" ");
        Ok(LogEntry {
            datetime: datetime.timestamp_millis(),
            level: level.to_string(),
            thread: thread.to_string(),
            path,
            line_number,
            message,
        })
    }
}

fn read_exact_range(file: &mut eko::file::File, offset: u64, out: &mut [u8]) -> eko::file::Result<()> {
    file.seek(offset)?;
    let mut filled = 0;
    while filled < out.len() {
        let n = file.read(&mut out[filled..])?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    Ok(())
}

fn read_lines_reverse(mut file: eko::file::File, limit: usize) -> Vec<Result<String, eko::file::Error>> {
    const CHUNK_SIZE: usize = 8192;
    if limit == 0 {
        return Vec::new();
    }
    let mut pos = match file.len() {
        Ok(len) => len,
        Err(err) => return alloc::vec![Err(err)],
    };
    if pos == 0 {
        return Vec::new();
    }

    let mut results = Vec::new();
    let mut tail_rev: Vec<u8> = Vec::new();
    let mut skipped_trailing_newline = false;
    let mut buf = [0u8; CHUNK_SIZE];

    while pos > 0 && results.len() < limit {
        let read_len = (pos.min(CHUNK_SIZE as u64)) as usize;
        pos -= read_len as u64;
        if let Err(err) = read_exact_range(&mut file, pos, &mut buf[..read_len]) {
            results.push(Err(err));
            return results;
        }

        for idx in (0..read_len).rev() {
            let byte = buf[idx];
            if !skipped_trailing_newline {
                skipped_trailing_newline = true;
                if byte == b'\n' {
                    continue;
                }
            }
            if byte == b'\n' {
                results.push(decode_reversed_line(&mut tail_rev));
                if results.len() >= limit {
                    return results;
                }
            } else {
                tail_rev.push(byte);
            }
        }
    }

    if !tail_rev.is_empty() && results.len() < limit {
        results.push(decode_reversed_line(&mut tail_rev));
    }

    results
}

fn decode_reversed_line(tail_rev: &mut Vec<u8>) -> Result<String, eko::file::Error> {
    let mut bytes = core::mem::take(tail_rev);
    bytes.reverse();
    if bytes.last() == Some(&b'\r') {
        bytes.pop();
    }
    String::from_utf8(bytes).map_err(|_| eko::file::Error::invalid_data("invalid UTF-8 log line"))
}

pub async fn get_log_entries(
    path: impl AsRef<eko::path::Path>,
    limit: usize,
) -> eyre::Result<Vec<LogEntry>> {
    let path_ref = path.as_ref();
    let display_path = path_ref.display().to_string();
    let file = eko::file::File::open(path_ref)?;

    // Seek and read backwards on a dedicated `eko::thread` so the async caller never blocks
    // a reactor worker.
    let (result_tx, result_rx) = futures::channel::oneshot::channel();
    eko::thread::spawn(move || {
        let mut entries = Vec::new();
        for line in read_lines_reverse(file, limit) {
            let line = match line {
                Ok(line) => line,
                Err(error) => {
                    warn!("Error reading line: {:?}", error);
                    entries.push(LogEntry {
                        datetime: 0,
                        level: String::new(),
                        thread: String::new(),
                        path: String::new(),
                        line_number: 0,
                        message: error.to_string(),
                    });
                    break;
                }
            };
            match LogEntry::from_str(&line) {
                Ok(entry) => entries.push(entry),
                Err(_) => entries.push(LogEntry {
                    datetime: 0,
                    level: String::new(),
                    thread: String::new(),
                    path: String::new(),
                    line_number: 0,
                    message: line,
                }),
            }
        }
        let _ = result_tx.send(entries);
    })
    .map_err(|err| eyre::eyre!("failed to spawn log reader thread: {err}"))?;

    let mut entries = result_rx
        .await
        .map_err(|_| eyre::eyre!("log reader thread died before sending its result"))?;
    if entries.is_empty() {
        entries.push(LogEntry {
            datetime: 0,
            level: String::new(),
            thread: String::new(),
            path: String::new(),
            line_number: 0,
            message: format!("No entries found in {display_path}"),
        });
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn log_entry_from_line() {
        let line = "2024-05-18T14:26:36.709390Z  WARN                 main trading_be: 290: terminated diff 0_table_limit_1 initially";
        let entry = LogEntry::from_str(line).unwrap();
        assert_eq!(entry.level, "WARN");
        assert_eq!(entry.thread, "main");
        assert_eq!(entry.path, "trading_be");
        assert_eq!(entry.line_number, 290);
        assert_eq!(entry.message, "terminated diff 0_table_limit_1 initially");
    }
    #[test]
    fn log_entry_from_line_control_sequence() {
        let line = "\u{1b}[2m2024-06-07T12:25:06.735143Z\u{1b}[0m \u{1b}[32m INFO\u{1b}[0m main \u{1b}[2mtrading_be::strategy::data_factory\u{1b}[0m\u{1b}[2m:\u{1b}[0m \u{1b}[2m110:\u{1b}[0m terminated diff 0_table_limit_1 initially";
        let entry = LogEntry::from_str(line).unwrap();
        assert_eq!(entry.level, "INFO");
        assert_eq!(entry.thread, "main");
        assert_eq!(entry.path, "trading_be::strategy::data_factory");
        assert_eq!(entry.line_number, 110);
        assert_eq!(entry.message, "terminated diff 0_table_limit_1 initially");
    }

    #[test]
    fn test_get_log_entries() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut temp_file = NamedTempFile::new().unwrap();
        writeln!(
            temp_file,
            "2025-03-19T12:34:56Z INFO thread-1 src/main.rs: 42: Application started"
        )
        .unwrap();
        writeln!(
            temp_file,
            "2025-03-19T12:35:01Z ERROR thread-2 src/lib.rs: 128: Failed to connect"
        )
        .unwrap();

        let entries =
            nagoya::block_on(get_log_entries(temp_file.path().to_str().unwrap(), 10)).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].level, "ERROR");
        assert_eq!(entries[0].path, "src/lib.rs");
        assert_eq!(entries[0].line_number, 128);
        assert_eq!(entries[1].level, "INFO");
        assert_eq!(entries[1].path, "src/main.rs");
        assert_eq!(entries[1].line_number, 42);
    }
}
