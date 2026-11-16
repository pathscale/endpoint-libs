//! Ordered JSON values and the legacy reader.
//!
//! Canonical documents and structured tools use `parse_valid`, which validates JSON syntax
//! without converting numbers through a floating-point representation.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt::Write;

#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Str(String),
    Num(String),
    Bool(bool),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    pub fn str(text: impl Into<String>) -> Json {
        Json::Str(text.into())
    }

    pub fn num(value: impl core::fmt::Display) -> Json {
        Json::Num(value.to_string())
    }

    pub fn strings<I, S>(items: I) -> Json
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Json::Arr(items.into_iter().map(Json::str).collect())
    }

    pub fn render(&self, out: &mut String) {
        match self {
            Json::Str(text) => escape(text, out),
            Json::Num(text) => out.push_str(text),
            Json::Bool(true) => out.push_str("true"),
            Json::Bool(false) => out.push_str("false"),
            Json::Arr(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.render(out);
                }
                out.push(']');
            }
            Json::Obj(fields) => {
                out.push('{');
                for (i, (key, value)) in fields.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    escape(key, out);
                    out.push(':');
                    value.render(out);
                }
                out.push('}');
            }
        }
    }

    pub fn to_text(&self) -> String {
        let mut out = String::new();
        self.render(&mut out);
        out
    }

    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn at<'a, I>(&self, path: I) -> Option<&Json>
    where
        I: IntoIterator<Item = &'a str>,
    {
        let mut here = self;
        for key in path {
            here = here.get(key)?;
        }
        Some(here)
    }

    pub fn text(&self) -> Option<&str> {
        match self {
            Json::Str(s) | Json::Num(s) => Some(s),
            _ => None,
        }
    }
}

pub fn escape(text: &str, out: &mut String) {
    out.push('"');
    escape_contents(text, out);
    out.push('"');
}

pub fn escape_contents(text: &str, out: &mut String) {
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Scalar {
    Str(String),
    Num(String),
    Bool(bool),
}

impl Scalar {
    pub fn text(&self) -> &str {
        match self {
            Scalar::Str(s) | Scalar::Num(s) => s,
            Scalar::Bool(true) => "true",
            Scalar::Bool(false) => "false",
        }
    }
}

const MAX_JSON_DEPTH: usize = 128;
pub const MAX_JSON_INPUT_BYTES: usize = 4 * 1024 * 1024;

pub fn parse(text: &str) -> Result<Json, String> {
    parse_with_null(text, false)
}

/// The RPC bridge retains legacy syntax while preserving the raw null ID token.
pub(crate) fn parse_rpc(text: &str) -> Result<Json, String> {
    parse_with_null(text, true)
}

fn parse_with_null(text: &str, null_as_number: bool) -> Result<Json, String> {
    if text.len() > MAX_JSON_INPUT_BYTES {
        return Err(format!(
            "crates/door/src/json.rs: JSON input exceeds the {MAX_JSON_INPUT_BYTES} byte limit"
        ));
    }
    let chars: Vec<char> = text.chars().collect();
    let mut at = 0usize;
    let value = read_value(&chars, &mut at, 0, null_as_number)?;
    skip_space(&chars, &mut at);
    if at != chars.len() {
        return Err(format!("trailing text after the value, at character {at}"));
    }
    Ok(value)
}

fn read_value(
    bytes: &[char],
    at: &mut usize,
    depth: usize,
    null_as_number: bool,
) -> Result<Json, String> {
    skip_space(bytes, at);
    if depth >= MAX_JSON_DEPTH && matches!(peek(bytes, *at), Some('{') | Some('[')) {
        return Err(format!(
            "maximum JSON nesting depth of {MAX_JSON_DEPTH} exceeded at character {}",
            *at
        ));
    }
    match peek(bytes, *at) {
        Some('{') => {
            *at += 1;
            let mut fields = Vec::new();
            skip_space(bytes, at);
            if peek(bytes, *at) == Some('}') {
                *at += 1;
                return Ok(Json::Obj(fields));
            }
            loop {
                skip_space(bytes, at);
                let key = read_string(bytes, at)?;
                skip_space(bytes, at);
                expect(bytes, at, ':')?;
                fields.push((key, read_value(bytes, at, depth + 1, null_as_number)?));
                skip_space(bytes, at);
                match peek(bytes, *at) {
                    Some(',') => *at += 1,
                    Some('}') => {
                        *at += 1;
                        return Ok(Json::Obj(fields));
                    }
                    other => {
                        return Err(format!(
                            "expected `,` or `}}` after a field, found {} at character {at}",
                            describe(other)
                        ))
                    }
                }
            }
        }
        Some('[') => {
            *at += 1;
            let mut items = Vec::new();
            skip_space(bytes, at);
            if peek(bytes, *at) == Some(']') {
                *at += 1;
                return Ok(Json::Arr(items));
            }
            loop {
                items.push(read_value(bytes, at, depth + 1, null_as_number)?);
                skip_space(bytes, at);
                match peek(bytes, *at) {
                    Some(',') => *at += 1,
                    Some(']') => {
                        *at += 1;
                        return Ok(Json::Arr(items));
                    }
                    other => {
                        return Err(format!(
                            "expected `,` or `]` in an array, found {} at character {at}",
                            describe(other)
                        ))
                    }
                }
            }
        }
        Some('n') => word(bytes, at, "null").map(|_| {
            if null_as_number { Json::Num("null".into()) } else { Json::Str("null".into()) }
        }),
        _ => read_scalar(bytes, at).map(|s| match s {
            Scalar::Str(text) => Json::Str(text),
            Scalar::Num(text) => Json::Num(text),
            Scalar::Bool(b) => Json::Bool(b),
        }),
    }
}

/// Read legacy flat scalar objects; this separate reader does not pass through the
/// byte-admitted `parse_with_null` entry point.
pub fn read_flat_object(text: &str) -> Result<Vec<(String, Scalar)>, String> {
    let bytes: Vec<char> = text.chars().collect();
    let mut at = 0usize;
    skip_space(&bytes, &mut at);
    expect(&bytes, &mut at, '{')?;
    let mut fields = Vec::new();
    skip_space(&bytes, &mut at);
    if peek(&bytes, at) == Some('}') {
        at += 1;
        return finish(&bytes, at, fields);
    }
    loop {
        skip_space(&bytes, &mut at);
        let key = read_string(&bytes, &mut at)?;
        skip_space(&bytes, &mut at);
        expect(&bytes, &mut at, ':')?;
        skip_space(&bytes, &mut at);
        let value = read_scalar(&bytes, &mut at)?;
        fields.push((key, value));
        skip_space(&bytes, &mut at);
        match peek(&bytes, at) {
            Some(',') => at += 1,
            Some('}') => {
                at += 1;
                return finish(&bytes, at, fields);
            }
            other => {
                return Err(format!(
                    "expected `,` or `}}` after a field, found {} at character {at}",
                    describe(other)
                ))
            }
        }
    }
}

fn finish(
    bytes: &[char],
    mut at: usize,
    fields: Vec<(String, Scalar)>,
) -> Result<Vec<(String, Scalar)>, String> {
    skip_space(bytes, &mut at);
    if at != bytes.len() {
        return Err(format!("trailing text after the object, at character {at}"));
    }
    Ok(fields)
}

fn peek(bytes: &[char], at: usize) -> Option<char> {
    bytes.get(at).copied()
}

fn describe(ch: Option<char>) -> String {
    match ch {
        Some(c) => format!("`{c}`"),
        None => "the end of the text".to_string(),
    }
}

fn skip_space(bytes: &[char], at: &mut usize) {
    while matches!(peek(bytes, *at), Some(' ') | Some('\t') | Some('\n') | Some('\r')) {
        *at += 1;
    }
}

fn expect(bytes: &[char], at: &mut usize, want: char) -> Result<(), String> {
    match peek(bytes, *at) {
        Some(c) if c == want => {
            *at += 1;
            Ok(())
        }
        other => Err(format!(
            "expected `{want}`, found {} at character {at}",
            describe(other)
        )),
    }
}

fn read_string(bytes: &[char], at: &mut usize) -> Result<String, String> {
    expect(bytes, at, '"')?;
    let mut out = String::new();
    loop {
        match peek(bytes, *at) {
            None => return Err("a string ran to the end of the text without closing".into()),
            Some('"') => {
                *at += 1;
                return Ok(out);
            }
            Some('\\') => {
                *at += 1;
                match peek(bytes, *at) {
                    Some('"') => out.push('"'),
                    Some('\\') => out.push('\\'),
                    Some('/') => out.push('/'),
                    Some('n') => out.push('\n'),
                    Some('r') => out.push('\r'),
                    Some('t') => out.push('\t'),
                    Some('b') => out.push('\u{8}'),
                    Some('f') => out.push('\u{c}'),
                    Some('u') => {
                        let first = read_hex4(bytes, at)?;
                        let code = match first {
                            0xd800..=0xdbff => {
                                if peek(bytes, *at + 1) != Some('\\')
                                    || peek(bytes, *at + 2) != Some('u')
                                {
                                    return Err("a high surrogate needs a low surrogate".into());
                                }
                                *at += 2;
                                let second = read_hex4(bytes, at)?;
                                if !(0xdc00..=0xdfff).contains(&second) {
                                    return Err("a high surrogate needs a low surrogate".into());
                                }
                                0x10000 + (((first as u32 - 0xd800) << 10)
                                    | (second as u32 - 0xdc00))
                            }
                            0xdc00..=0xdfff => {
                                return Err("a low surrogate needs a preceding high surrogate".into());
                            }
                            _ => first as u32,
                        };
                        out.push(char::from_u32(code).ok_or_else(|| {
                            format!("\\u{code:04x} is not a character")
                        })?);
                    }
                    other => {
                        return Err(format!("unknown escape `\\{}`", describe(other)));
                    }
                }
                *at += 1;
            }
            Some(c) if (c as u32) < 0x20 => {
                return Err("a JSON string contains an unescaped control character".into());
            }
            Some(c) => {
                out.push(c);
                *at += 1;
            }
        }
    }
}

fn read_hex4(bytes: &[char], at: &mut usize) -> Result<u16, String> {
    let mut code = 0u16;
    for _ in 0..4 {
        *at += 1;
        let digit = peek(bytes, *at)
            .and_then(|c| c.to_digit(16))
            .ok_or_else(|| "a \\u escape needs four hex digits".to_string())?;
        code = code * 16 + digit as u16;
    }
    Ok(code)
}

fn read_number(bytes: &[char], at: &mut usize) -> Result<Scalar, String> {
    let start = *at;
    if peek(bytes, *at) == Some('-') {
        *at += 1;
    }
    match peek(bytes, *at) {
        Some('0') => {
            *at += 1;
            if peek(bytes, *at).is_some_and(|c| c.is_ascii_digit()) {
                return Err(format!("a JSON number has a leading zero at character {}", *at));
            }
        }
        Some('1'..='9') => {
            *at += 1;
            while peek(bytes, *at).is_some_and(|c| c.is_ascii_digit()) {
                *at += 1;
            }
        }
        _ => return Err(format!("expected a digit in JSON number at character {}", *at)),
    }
    if peek(bytes, *at) == Some('.') {
        *at += 1;
        let fraction = *at;
        while peek(bytes, *at).is_some_and(|c| c.is_ascii_digit()) {
            *at += 1;
        }
        if *at == fraction {
            return Err(format!("expected a fraction digit at character {}", *at));
        }
    }
    if matches!(peek(bytes, *at), Some('e') | Some('E')) {
        *at += 1;
        if matches!(peek(bytes, *at), Some('+') | Some('-')) {
            *at += 1;
        }
        let exponent = *at;
        while peek(bytes, *at).is_some_and(|c| c.is_ascii_digit()) {
            *at += 1;
        }
        if *at == exponent {
            return Err(format!("expected an exponent digit at character {}", *at));
        }
    }
    Ok(Scalar::Num(bytes[start..*at].iter().collect()))
}

fn read_scalar(bytes: &[char], at: &mut usize) -> Result<Scalar, String> {
    match peek(bytes, *at) {
        Some('"') => read_string(bytes, at).map(Scalar::Str),
        Some('t') => word(bytes, at, "true").map(|_| Scalar::Bool(true)),
        Some('f') => word(bytes, at, "false").map(|_| Scalar::Bool(false)),
        Some(c) if c == '-' || c.is_ascii_digit() => read_number(bytes, at),
        Some('{') | Some('[') => {
            Err(format!("nested values are not read here, at character {at}"))
        }
        other => Err(format!(
            "expected a string, number or boolean, found {} at character {at}",
            describe(other)
        )),
    }
}

fn word(bytes: &[char], at: &mut usize, want: &str) -> Result<(), String> {
    for ch in want.chars() {
        expect(bytes, at, ch)?;
    }
    Ok(())
}

pub(crate) fn parse_string(text: &str) -> Result<String, String> {
    match parse_valid(text)? {
        Json::Str(value) => Ok(value),
        _ => Err("quoted cell is not a JSON string".to_string()),
    }
}

/// Preserve duplicate keys so their owner can report a precise location.
/// The raw null token uses the existing Num("null") RPC convention and never passes
/// string, boolean or unsigned-integer argument validation.
/// Parse strict JSON while preserving numeric lexemes and rejecting duplicate object keys.
///
/// Raw `null` is represented as `Json::Num("null")`, matching the existing RPC convention.
/// Numbers remain their original lexical text, including values beyond integer machine widths.
/// Duplicate keys are rejected recursively, including inside arrays. The parser enforces the
/// existing maximum nesting depth of 128.
pub fn parse_strict(text: &str) -> Result<Json, String> {
    let value = parse_valid(text)?;
    unique(&value)?;
    Ok(value)
}

pub(crate) fn parse_valid(text: &str) -> Result<Json, String> {
    parse_with_null(text, true)
}

pub(crate) fn unique(value: &Json) -> Result<(), String> {
    unique_at(value, "$")
}

fn unique_at(value: &Json, at: &str) -> Result<(), String> {
    match value {
        Json::Obj(fields) => {
            let mut names = alloc::collections::BTreeSet::new();
            for (name, value) in fields {
                if !names.insert(name.as_str()) {
                    return Err(format!("{at}.{name}: duplicate field"));
                }
                unique_at(value, &format!("{at}.{name}"))?;
            }
        }
        Json::Arr(values) => {
            for (index, value) in values.iter().enumerate() {
                unique_at(value, &format!("{at}[{index}]"))?;
            }
        }
        _ => {}
    }
    Ok(())
}
