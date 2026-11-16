//! MCP discovery and dispatch without application request strings.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use crate::libs::door::json::Json;
use crate::libs::jsonrpc::{JSONRPC_VERSION, METHOD_NOT_FOUND};
use crate::libs::door::registry::{ArgumentType, Gate, Registry, ToolCompatibility};
use crate::libs::door::typed::{Asked, Shape};

pub enum Outcome<Q> {
    Reply(String),
    Ask {
        asked: Asked<Q>,
        id: Json,
    },
    Silent,
}

pub fn dispatch<R: Registry>(
    line: &str,
    registry: &R,
    server_name: &str,
    version: &str,
) -> Outcome<R::Request> {
    if line.trim().is_empty() {
        return Outcome::Silent;
    }
    let Ok(frame) = crate::libs::door::json::parse_rpc(line) else {
        return Outcome::Silent;
    };
    let Some(id) = frame.get("id").cloned() else {
        return Outcome::Silent;
    };
    let method = frame.get("method").and_then(Json::text).unwrap_or_default();
    match method {
        m if m.starts_with("notifications/") => Outcome::Silent,
        "initialize" => Outcome::Reply(rpc_result(id, initialize_result(server_name, version))),
        "ping" => Outcome::Reply(rpc_result(id, Json::Obj(Vec::new()))),
        "tools/list" => Outcome::Reply(rpc_result(id, tools_list(registry))),
        "tools/call" => {
            let name = frame.at(["params", "name"]).and_then(Json::text).unwrap_or_default();
            let Some(tool) = registry.verbs().iter()
                .filter(|verb| verb.gate != Gate::Closed)
                .filter_map(|verb| verb.tool.as_ref())
                .find(|tool| tool.name.as_ref() == name)
            else {
                return Outcome::Reply(rpc_error(id, format!(
                    "`{name}` is not a tool this bridge offers. it is either not a verb, or \
                     one the typed door closes because it changes what other agents see"
                )));
            };
            let checked;
            let frame = match tool.compatibility {
                ToolCompatibility::LegacyArguments => &frame,
                ToolCompatibility::Structured => {
                    checked = match crate::libs::door::json::parse_valid(line) {
                        Ok(value) => value,
                        Err(why) => return Outcome::Reply(rpc_error(id, why)),
                    };
                    if let Err(why) = crate::libs::door::json::unique(&checked) {
                        return Outcome::Reply(rpc_error(id, why));
                    }
                    &checked
                }
            };
            let id = frame.get("id").cloned().unwrap_or(id);
            let empty = Json::Obj(Vec::new());
            let arguments = frame.at(["params", "arguments"]).unwrap_or(&empty);
            if tool.compatibility == ToolCompatibility::Structured {
                if let Err(why) = crate::libs::door::registry::validate_arguments(&tool.arguments, arguments) {
                    return Outcome::Reply(rpc_error(id, why));
                }
            }
            let request = match registry.parse_tool(&tool.name, arguments) {
                Ok(request) => request,
                Err(why) => return Outcome::Reply(rpc_error(id, why)),
            };
            if let Err(why) = crate::libs::door::registry::admitted(registry, &request) {
                return Outcome::Reply(rpc_error(id, why));
            }
            if registry.verb(&request).tool.as_ref().map(|t| t.name.as_ref())
                != Some(tool.name.as_ref())
            {
                return Outcome::Reply(rpc_error(
                    id, "tool parser returned a request for a different tool".into(),
                ));
            }
            Outcome::Ask { asked: Asked { shape: Shape::Json, request }, id }
        }
        other => Outcome::Reply(rpc_error(id, format!("no method `{other}` on this bridge"))),
    }
}

pub fn tools_list<R: Registry>(registry: &R) -> Json {
    let tools = registry.verbs().iter()
        .filter(|verb| verb.gate != Gate::Closed)
        .filter_map(|verb| verb.tool.as_ref())
        .map(|tool| {
            let mut input = schema(&tool.arguments);
            // The driver's original schema omitted this key. Its argument parser remains
            // permissive, while structured tools publish the explicit setting.
            if tool.compatibility == ToolCompatibility::LegacyArguments {
                if let Json::Obj(fields) = &mut input {
                    fields.retain(|(name, _)| name != "additionalProperties");
                }
            }
            Json::Obj(vec![
                ("name".into(), Json::str(tool.name.as_ref())),
                ("description".into(), Json::str(tool.description.as_ref())),
                ("inputSchema".into(), input),
            ])
        }).collect();
    Json::Obj(vec![("tools".into(), Json::Arr(tools))])
}

fn schema(ty: &ArgumentType) -> Json {
    match ty {
        ArgumentType::String { values, pattern, .. } => {
            let mut fields = vec![("type".into(), Json::str("string"))];
            if !values.is_empty() {
                fields.push(("enum".into(), Json::strings(values.iter().map(|v| v.as_ref()))));
            }
            if let Some(pattern) = pattern {
                fields.push(("pattern".into(), Json::str(pattern.as_ref())));
            }
            Json::Obj(fields)
        }
        ArgumentType::Boolean => Json::Obj(vec![("type".into(), Json::str("boolean"))]),
        ArgumentType::UnsignedInteger { maximum } => Json::Obj(vec![
            ("type".into(), Json::str("integer")),
            ("minimum".into(), Json::num(0)),
            ("maximum".into(), Json::num(maximum)),
        ]),
        ArgumentType::Array { items } => Json::Obj(vec![
            ("type".into(), Json::str("array")),
            ("items".into(), schema(items)),
        ]),
        ArgumentType::Object { fields, additional_properties } => {
            let properties = fields.iter().map(|field| {
                let mut value = schema(&field.ty);
                if let Json::Obj(entries) = &mut value {
                    entries.insert(1, (
                        "description".into(), Json::str(field.description.as_ref()),
                    ));
                }
                (field.name.to_string(), value)
            }).collect();
            Json::Obj(vec![
                ("type".into(), Json::str("object")),
                ("properties".into(), Json::Obj(properties)),
                ("required".into(), Json::strings(
                    fields.iter().filter(|f| f.required).map(|f| f.name.as_ref()),
                )),
                ("additionalProperties".into(), Json::Bool(*additional_properties)),
            ])
        }
        ArgumentType::OneOf(alternatives) => Json::Obj(vec![
            ("oneOf".into(), Json::Arr(alternatives.iter().map(schema).collect())),
        ]),
    }
}

pub fn answered(id: Json, ok: bool, body: &[String]) -> String {
    let mut out = String::new();
    out.push_str("{\"jsonrpc\":\"");
    out.push_str(JSONRPC_VERSION);
    out.push_str("\",\"id\":");
    id.render(&mut out);
    out.push_str(",\"result\":{\"isError\":");
    out.push_str(if ok { "false" } else { "true" });
    out.push_str(",\"content\":[{\"type\":\"text\",\"text\":\"");
    for (index, line) in body.iter().enumerate() {
        if index > 0 {
            out.push_str("\\n");
        }
        crate::libs::door::json::escape_contents(line, &mut out);
    }
    out.push_str("\"}]}}");
    out
}

pub fn unanswered(id: Json, why: &str) -> String {
    rpc_error(id, format!("the compiler did not answer: {why}"))
}

fn initialize_result(server_name: &str, version: &str) -> Json {
    Json::Obj(vec![
        ("protocolVersion".into(), Json::str("2024-11-05")),
        ("capabilities".into(), Json::Obj(vec![("tools".into(), Json::Obj(Vec::new()))])),
        (
            "serverInfo".into(),
            Json::Obj(vec![
                ("name".into(), Json::str(server_name)),
                ("version".into(), Json::str(version)),
            ]),
        ),
    ])
}

pub fn rpc_result(id: Json, result: Json) -> String {
    Json::Obj(vec![
        ("jsonrpc".into(), Json::str(JSONRPC_VERSION)),
        ("id".into(), id),
        ("result".into(), result),
    ]).to_text()
}

pub fn rpc_error(id: Json, message: String) -> String {
    Json::Obj(vec![
        ("jsonrpc".into(), Json::str(JSONRPC_VERSION)),
        ("id".into(), id),
        (
            "error".into(),
            Json::Obj(vec![
                ("code".into(), Json::num(METHOD_NOT_FOUND)),
                ("message".into(), Json::str(message)),
            ]),
        ),
    ]).to_text()
}
