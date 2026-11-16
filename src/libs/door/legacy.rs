//! Driver compatibility: permissive decoding and the existing quoting decisions.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt::Write;

#[derive(Debug, Clone, PartialEq)]
pub struct Table {
    pub name: String,
    pub fields: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Decoded {
    pub tables: Vec<Table>,
    pub prose: Vec<String>,
    pub problems: Vec<String>,
}

pub fn decode(body: &[String]) -> Decoded {
    let mut out = Decoded::default();
    let mut at = 0usize;
    while at < body.len() {
        match header(&body[at]) {
            None => {
                out.prose.push(body[at].clone());
                at += 1;
            }
            Some((name, fields, count)) => {
                at += 1;
                let mut rows = Vec::with_capacity(count.min(body.len().saturating_sub(at)));
                let mut wrong: Vec<String> = Vec::new();
                for index in 0..count {
                    match body.get(at) {
                        Some(line) => {
                            let row = row_cells(line.trim_start(), fields.len());
                            if row.len() != fields.len() {
                                wrong.push(format!(
                                    "table `{name}` row {index} has {} cells and its header \
                                     declares {}: {line:?}",
                                    row.len(),
                                    fields.len()
                                ));
                            }
                            rows.push(row);
                            at += 1;
                        }
                        None => {
                            wrong.push(format!(
                                "table `{name}` declares {count} rows and the body ends after \
                                 {index}"
                            ));
                            break;
                        }
                    }
                }
                if wrong.is_empty() {
                    out.tables.push(Table { name, fields, rows });
                } else {
                    out.problems.append(&mut wrong);
                }
            }
        }
    }
    out
}

fn row_cells(line: &str, width: usize) -> Vec<String> {
    if width == 0 && line.is_empty() {
        return Vec::new();
    }
    cells(line)
}

fn header(line: &str) -> Option<(String, Vec<String>, usize)> {
    let line = line.strip_suffix(':')?;
    let open = line.find('[')?;
    let close = line.find(']')?;
    if close < open {
        return None;
    }
    let name = &line[..open];
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    let count: usize = line[open + 1..close].parse().ok()?;
    let braces = &line[close + 1..];
    let inner = braces.strip_prefix('{')?.strip_suffix('}')?;
    let fields: Vec<String> = if inner.is_empty() {
        Vec::new()
    } else {
        inner.split(',').map(str::to_string).collect()
    };
    Some((name.to_string(), fields, count))
}

fn cells(line: &str) -> Vec<String> {
    let chars: Vec<char> = line.chars().collect();
    let mut out = Vec::new();
    let mut at = 0usize;
    loop {
        let mut cell = String::new();
        if chars.get(at) == Some(&'"') {
            at += 1;
            while at < chars.len() {
                match chars[at] {
                    '"' => {
                        at += 1;
                        break;
                    }
                    '\\' => {
                        match chars.get(at + 1) {
                            Some('\\') => {
                                cell.push('\\');
                                at += 2;
                            }
                            Some('"') => {
                                cell.push('"');
                                at += 2;
                            }
                            Some('n') => {
                                cell.push('\n');
                                at += 2;
                            }
                            _ => {
                                cell.push('\\');
                                at += 1;
                            }
                        }
                    }
                    c => {
                        cell.push(c);
                        at += 1;
                    }
                }
            }
        } else {
            while at < chars.len() && chars[at] != ',' {
                cell.push(chars[at]);
                at += 1;
            }
        }
        out.push(cell);
        match chars.get(at) {
            Some(',') => at += 1,
            _ => return out,
        }
    }
}

pub fn write_table(name: &str, fields: &[&str], rows: &[Vec<String>], out: &mut String) {
    let mut additional = name.len().saturating_add(16);
    for field in fields {
        additional = additional.saturating_add(field.len()).saturating_add(1);
    }
    for row in rows {
        additional = additional.saturating_add(3);
        for cell in row {
            additional = additional.saturating_add(cell.len()).saturating_add(1);
        }
    }
    out.reserve(additional);
    let _ = writeln!(out, "{}[{}]{{{}}}:", name, rows.len(), fields.join(","));
    for row in rows {
        out.push_str("  ");
        for (index, cell) in row.iter().enumerate() {
            if index != 0 {
                out.push(',');
            }
            quote(cell, out);
        }
        out.push('\n');
    }
}

fn quote(cell: &str, out: &mut String) {
    let bytes = cell.as_bytes();
    // Edge whitespace of any kind, not only a space: `decode` reads a row as
    // `line.trim_start()`, so an unquoted first cell that started with a tab, CR or NBSP came
    // back without it. Inside quotes such characters are kept as written.
    let needs = bytes.is_empty()
        || cell.starts_with(char::is_whitespace)
        || cell.ends_with(char::is_whitespace)
        || bytes.iter().any(|byte| matches!(byte, b',' | b'"' | b'\n' | b':'));
    if !needs {
        out.push_str(cell);
        return;
    }
    out.push('"');
    let mut start = 0;
    for (index, byte) in bytes.iter().enumerate() {
        let escaped = match byte {
            b'\\' => "\\\\",
            b'"' => "\\\"",
            b'\n' => "\\n",
            _ => continue,
        };
        out.push_str(&cell[start..index]);
        out.push_str(escaped);
        start = index + 1;
    }
    out.push_str(&cell[start..]);
    out.push('"');
}
