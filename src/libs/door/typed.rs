//! The `json` modifier and the driver compatibility envelope.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::libs::door::json::Json;
use crate::libs::door::protocol::Reply;
use crate::libs::door::registry::Registry;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    Text,
    Json,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Asked<Q> {
    pub shape: Shape,
    pub request: Q,
}

pub fn parse<R: Registry>(registry: &R, line: &str) -> Result<Asked<R::Request>, String> {
    let line = line.trim();
    if line == "json" {
        return Err("json needs a verb: `json report refusals dot tree-reduce`".into());
    }
    match line.strip_prefix("json ") {
        None => Ok(Asked { shape: Shape::Text, request: registry.parse_line(line)? }),
        Some(rest) => {
            let request = registry.parse_line(rest.trim())?;
            crate::libs::door::registry::admitted(registry, &request)?;
            Ok(Asked { shape: Shape::Json, request })
        }
    }
}

pub(crate) fn requested_shape(line: &str) -> Shape {
    let line = line.trim();
    if line == "json" || line.starts_with("json ") { Shape::Json } else { Shape::Text }
}

pub fn envelope(request: &str, reply: Reply, about: &[(String, String)]) -> Reply {
    let decoded = crate::libs::door::legacy::decode(&reply.body);
    let mut outcome_ok = true;
    let tables = decoded.tables.iter().map(|t| {
        let outcome_field = match t.name.as_str() {
            "diagnostics" => Some("status"),
            "predictions" => Some("verdict"),
            _ => None,
        };
        if let Some(field) = outcome_field {
            if let Some(status) = t.fields.iter().position(|candidate| candidate == field) {
                for row in &t.rows {
                    if matches!(
                        row.get(status).map(String::as_str),
                        Some("refused" | "failed" | "violated")
                    ) {
                        outcome_ok = false;
                    }
                }
            }
        }
        Json::Obj(vec![
            ("name".into(), Json::str(&t.name)),
            ("fields".into(), Json::strings(t.fields.clone())),
            ("rows".into(), Json::Arr(t.rows.iter().map(|r| Json::strings(r.clone())).collect())),
        ])
    }).collect();
    let mut problems = decoded.problems;
    if reply.body.is_empty() || (decoded.tables.is_empty() && decoded.prose.is_empty()) {
        problems.push("typed report is empty; no outcome was returned".into());
    }
    let ok = reply.ok && problems.is_empty() && outcome_ok;
    let errors = problems.iter().map(|problem| Json::Obj(vec![
        ("code".into(), Json::str(if problem.starts_with("table ") { "malformed_table" } else { "empty_or_malformed_report" })),
        ("message".into(), Json::str(problem.clone())),
    ])).collect();
    let document = Json::Obj(vec![
        ("ok".into(), Json::Bool(ok)),
        ("request".into(), Json::str(request)),
        ("about".into(), Json::Obj(about.iter().map(|(k, v)| (k.clone(), Json::str(v.clone()))).collect())),
        ("tables".into(), Json::Arr(tables)),
        ("prose".into(), Json::strings(decoded.prose)),
        ("problems".into(), Json::strings(problems)),
        ("errors".into(), Json::Arr(errors)),
        ("outcome_ok".into(), Json::Bool(outcome_ok)),
    ]);
    Reply { ok, body: vec![document.to_text()] }
}

pub fn envelope_err(request: &str, why: Vec<String>) -> Reply {
    let errors = why.iter().map(|message| Json::Obj(vec![
        ("code".into(), Json::str("request_error")),
        ("message".into(), Json::str(message.clone())),
    ])).collect();
    let document = Json::Obj(vec![
        ("ok".into(), Json::Bool(false)),
        ("request".into(), Json::str(request)),
        ("about".into(), Json::Obj(Vec::new())),
        ("tables".into(), Json::Arr(Vec::new())),
        ("prose".into(), Json::strings(why.clone())),
        ("problems".into(), Json::strings(why)),
        ("errors".into(), Json::Arr(errors)),
        ("outcome_ok".into(), Json::Bool(false)),
    ]);
    Reply { ok: false, body: vec![document.to_text()] }
}
