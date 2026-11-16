//! Strict ordered documents. The permissive driver profile lives in `legacy`.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;

use crate::libs::door::json::Json;

#[derive(Debug, Clone)]
pub struct Document {
    pub tables: Vec<Table>,
}

#[derive(Debug, Clone)]
pub struct Table {
    pub name: Arc<str>,
    pub fields: Vec<Arc<str>>,
    pub rows: Vec<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Toon,
    Json,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodecError {
    pub table: Option<Arc<str>>,
    pub row: Option<u64>,
    pub field: Option<Arc<str>>,
    pub reason: Arc<str>,
}

impl core::fmt::Display for CodecError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if let Some(table) = &self.table {
            write!(f, "table `{table}`: ")?;
        }
        if let Some(row) = self.row {
            write!(f, "row {row}: ")?;
        }
        if let Some(field) = &self.field {
            write!(f, "field `{field}`: ")?;
        }
        f.write_str(&self.reason)
    }
}

fn error(reason: impl Into<Arc<str>>) -> CodecError {
    CodecError { table: None, row: None, field: None, reason: reason.into() }
}

fn located(
    mut error: CodecError,
    table: &Arc<str>,
    row: Option<u64>,
    field: Option<&Arc<str>>,
) -> CodecError {
    error.table = Some(Arc::clone(table));
    error.row = row;
    if let Some(field) = field {
        error.field = Some(Arc::clone(field));
    }
    error
}

fn check(document: &Document) -> Result<(), CodecError> {
    for (index, table) in document.tables.iter().enumerate() {
        if !header_name(&table.name) {
            return Err(located(
                error("name cannot be represented in a TOON header"),
                &table.name,
                None,
                None,
            ));
        }
        if document.tables[..index].iter().any(|t| t.name == table.name) {
            return Err(located(error("duplicate table name"), &table.name, None, None));
        }
        for (index, field) in table.fields.iter().enumerate() {
            if !header_name(field) {
                return Err(located(
                    error("name cannot be represented in a TOON header"),
                    &table.name,
                    None,
                    Some(field),
                ));
            }
            if table.fields[..index].contains(field) {
                return Err(located(
                    error("duplicate field name"), &table.name, None, Some(field),
                ));
            }
        }
        for (index, row) in table.rows.iter().enumerate() {
            if row.len() != table.fields.len() {
                return Err(located(
                    error(format!(
                        "row has {} cells; header declares {}", row.len(), table.fields.len()
                    )),
                    &table.name,
                    Some(index as u64),
                    None,
                ));
            }
        }
    }
    Ok(())
}

fn header_name(name: &str) -> bool {
    !name.is_empty()
        && !name.chars().any(|c| {
            c.is_whitespace() || c.is_control() || matches!(c, '[' | ']' | '{' | '}' | ',' | ':' | '"')
        })
}

pub fn write_document(document: &Document, format: Format) -> Result<String, CodecError> {
    check(document)?;
    match format {
        Format::Json => {
            let mut out = String::new();
            out.push('{');
            for (table_index, table) in document.tables.iter().enumerate() {
                if table_index > 0 {
                    out.push(',');
                }
                crate::libs::door::json::escape(&table.name, &mut out);
                out.push_str(":[");
                for (row_index, row) in table.rows.iter().enumerate() {
                    if row_index > 0 {
                        out.push(',');
                    }
                    out.push('{');
                    for (cell_index, (field, value)) in table.fields.iter().zip(row).enumerate() {
                        if cell_index > 0 {
                            out.push(',');
                        }
                        crate::libs::door::json::escape(field, &mut out);
                        out.push(':');
                        crate::libs::door::json::escape(value, &mut out);
                    }
                    out.push('}');
                }
                out.push(']');
            }
            out.push('}');
            Ok(out)
        }
        Format::Toon => {
            let mut out = String::new();
            for table in &document.tables {
                let fields = table.fields.iter().map(|f| f.as_ref()).collect::<Vec<_>>();
                let _ = writeln!(out, "{}[{}]{{{}}}:", table.name, table.rows.len(), fields.join(","));
                for (index, row) in table.rows.iter().enumerate() {
                    let mut cells = Vec::new();
                    for (field, value) in table.fields.iter().zip(row) {
                        cells.push(encode_toon_cell(value).map_err(|e| {
                            located(e, &table.name, Some(index as u64), Some(field))
                        })?);
                    }
                    let _ = writeln!(out, "  {}", cells.join(","));
                }
            }
            Ok(out)
        }
    }
}

pub fn encode_toon_cell(value: &str) -> Result<String, CodecError> {
    let quoted = value.is_empty()
        || value.trim() != value
        || value.chars().any(|c| {
            c.is_control() || matches!(c, ',' | '"' | '\\' | ':')
        });
    Ok(if quoted { Json::str(value).to_text() } else { value.to_string() })
}

/// Decode the cell portion of a row, without its two-space document indentation.
pub fn decode_toon_row(line: &str) -> Result<Vec<String>, CodecError> {
    let bytes = line.as_bytes();
    let mut out = Vec::new();
    let mut at = 0usize;
    loop {
        let cell;
        if bytes.get(at) == Some(&b'"') {
            let start = at;
            at += 1;
            let mut escaped = false;
            let mut closed = false;
            while at < bytes.len() {
                let byte = bytes[at];
                at += 1;
                if byte == b'"' && !escaped {
                    closed = true;
                    break;
                }
                if byte == b'\\' && !escaped {
                    escaped = true;
                } else {
                    escaped = false;
                }
            }
            if !closed {
                return Err(error("unfinished quoted cell"));
            }
            cell = crate::libs::door::json::parse_string(&line[start..at])
                .map_err(|e| error(format!("invalid quoted cell: {e}")))?;
        } else {
            let start = at;
            at = line[at..].find(',').map(|n| at + n).unwrap_or(bytes.len());
            let raw = &line[start..at];
            if raw.contains('"') || raw.chars().any(char::is_control) {
                return Err(error("unquoted cell contains a quote or control character"));
            }
            if raw.trim() != raw {
                return Err(error("unquoted cell has leading or trailing whitespace"));
            }
            cell = raw.to_string();
        }
        out.push(cell);
        match bytes.get(at) {
            Some(b',') => at += 1,
            None => return Ok(out),
            _ => return Err(error("text follows a quoted cell without a delimiter")),
        }
    }
}

pub fn decode_toon_header(
    line: &str,
) -> Result<(Arc<str>, u64, Vec<Arc<str>>), CodecError> {
    let (name, rest) = line.split_once('[').ok_or_else(|| error("missing TOON count"))?;
    let name: Arc<str> = Arc::from(name);
    if !header_name(&name) {
        return Err(located(error("invalid TOON table name"), &name, None, None));
    }
    let (count, rest) = rest.split_once("]{").ok_or_else(|| {
        located(error("invalid TOON header"), &name, None, None)
    })?;
    if count.is_empty()
        || !count.bytes().all(|b| b.is_ascii_digit())
        || (count.len() > 1 && count.starts_with('0'))
    {
        return Err(located(error("noncanonical TOON count"), &name, None, None));
    }
    let count = count.parse::<u64>().map_err(|_| {
        located(error("TOON count exceeds u64"), &name, None, None)
    })?;
    let rest = rest.strip_suffix("}:").ok_or_else(|| {
        located(error("invalid TOON header terminator"), &name, None, None)
    })?;
    let mut fields = Vec::new();
    if !rest.is_empty() {
        for text in rest.split(',') {
            let field: Arc<str> = Arc::from(text);
            if !header_name(&field) {
                return Err(located(error("invalid TOON field name"), &name, None, Some(&field)));
            }
            if fields.contains(&field) {
                return Err(located(error("duplicate field name"), &name, None, Some(&field)));
            }
            fields.push(field);
        }
    }
    Ok((name, count, fields))
}

pub fn decode_document(input: &[u8], format: Format) -> Result<Document, CodecError> {
    let text = core::str::from_utf8(input).map_err(|e| error(format!("invalid UTF-8: {e}")))?;
    let document = match format {
        Format::Toon => decode_toon(text)?,
        Format::Json => decode_json(text)?,
    };
    check(&document)?;
    Ok(document)
}

fn decode_toon(text: &str) -> Result<Document, CodecError> {
    let mut lines = text.split_terminator('\n');
    let mut tables: Vec<Table> = Vec::new();
    while let Some(line) = lines.next() {
        let (name, count, fields) = decode_toon_header(line)?;
        if tables.iter().any(|t| t.name == name) {
            return Err(located(error("duplicate table name"), &name, None, None));
        }
        let mut rows = Vec::new();
        for index in 0..count {
            let line = lines.next().ok_or_else(|| {
                located(error(format!("declared {count} rows; body ends after {index}")),
                    &name, Some(index), None)
            })?;
            let line = line.strip_prefix("  ").ok_or_else(|| {
                located(error("TOON row is missing its two-space indentation"),
                    &name, Some(index), None)
            })?;
            let row = if fields.is_empty() && line.is_empty() {
                Vec::new()
            } else {
                decode_toon_row(line).map_err(|e| located(e, &name, Some(index), None))?
            };
            if row.len() != fields.len() {
                return Err(located(
                    error(format!("row has {} cells; header declares {}", row.len(), fields.len())),
                    &name, Some(index), None,
                ));
            }
            rows.push(row);
        }
        tables.push(Table { name, fields, rows });
    }
    Ok(Document { tables })
}

fn decode_json(text: &str) -> Result<Document, CodecError> {
    let value = crate::libs::door::json::parse_valid(text).map_err(error)?;
    let Json::Obj(entries) = value else {
        return Err(error("document JSON must be an object"));
    };
    let mut tables: Vec<Table> = Vec::new();
    for (name, value) in entries {
        let name: Arc<str> = Arc::from(name);
        if tables.iter().any(|t| t.name == name) {
            return Err(located(error("duplicate table name"), &name, None, None));
        }
        let Json::Arr(values) = value else {
            return Err(located(error("table JSON must be an array"), &name, None, None));
        };
        let mut fields: Vec<Arc<str>> = Vec::new();
        let mut rows = Vec::new();
        for (index, value) in values.into_iter().enumerate() {
            let index = index as u64;
            let Json::Obj(entries) = value else {
                return Err(located(error("row JSON must be an object"), &name, Some(index), None));
            };
            for (at, (field, value)) in entries.iter().enumerate() {
                let field: Arc<str> = Arc::from(field.as_str());
                if entries[..at].iter().any(|(previous, _)| previous == field.as_ref()) {
                    return Err(located(
                        error("duplicate field name"), &name, Some(index), Some(&field),
                    ));
                }
                if !matches!(value, Json::Str(_)) {
                    return Err(located(
                        error("every document cell must be a string"),
                        &name, Some(index), Some(&field),
                    ));
                }
            }
            if index == 0 {
                fields = entries.iter().map(|(field, _)| Arc::from(field.as_str())).collect();
            }
            if entries.len() != fields.len() {
                return Err(located(
                    error(format!("row has {} cells; first row declares {}", entries.len(), fields.len())),
                    &name, Some(index), None,
                ));
            }
            let mut row = Vec::new();
            for field in &fields {
                match entries.iter().find(|(key, _)| key == field.as_ref()) {
                    Some((_, Json::Str(value))) => row.push(value.clone()),
                    _ => return Err(located(
                        error("row does not carry the declared field"),
                        &name, Some(index), Some(field),
                    )),
                }
            }
            rows.push(row);
        }
        tables.push(Table { name, fields, rows });
    }
    Ok(Document { tables })
}
