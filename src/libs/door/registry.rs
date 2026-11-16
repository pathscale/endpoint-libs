//! Application-owned vocabulary and transport metadata.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::libs::door::json::Json;
use crate::libs::door::protocol::Reply;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    ReadOnly,
    /// Work on a clone whose cost the application requires the caller to declare.
    DeclaredCost,
    ScopedCancel,
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnswerMode {
    LegacyEnvelope,
    CanonicalDocument,
}

#[derive(Debug, Clone)]
pub enum ArgumentType {
    String {
        logical_type: Arc<str>,
        values: Vec<Arc<str>>,
        pattern: Option<Arc<str>>,
    },
    Boolean,
    UnsignedInteger {
        maximum: u64,
    },
    Array {
        items: Box<ArgumentType>,
    },
    Object {
        fields: Vec<Argument>,
        additional_properties: bool,
    },
    OneOf(Vec<ArgumentType>),
}

#[derive(Debug, Clone)]
pub struct Argument {
    pub name: Arc<str>,
    pub description: Arc<str>,
    pub required: bool,
    pub ty: ArgumentType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCompatibility {
    LegacyArguments,
    Structured,
}

#[derive(Debug, Clone)]
pub struct Tool {
    pub name: Arc<str>,
    pub description: Arc<str>,
    pub arguments: ArgumentType,
    pub compatibility: ToolCompatibility,
}

#[derive(Debug, Clone)]
pub struct Verb {
    pub name: Arc<str>,
    pub spelling: Arc<str>,
    pub description: Arc<str>,
    pub arguments: Vec<Argument>,
    pub gate: Gate,
    pub answer_mode: AnswerMode,
    pub tool: Option<Tool>,
}

#[derive(Debug, Clone)]
pub struct AnswerField {
    pub name: Arc<str>,
    pub logical_type: Arc<str>,
}

#[derive(Debug, Clone)]
pub struct AnswerTable {
    pub name: Arc<str>,
    pub fields: Vec<AnswerField>,
    pub minimum: u64,
    pub maximum: Option<u64>,
    pub ordering: Arc<str>,
}

#[derive(Debug, Clone, Default)]
pub struct Submission {
    pub owner: Option<String>,
    pub priority: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Handler,
    Help,
    Ping,
    List,
    Cancel { id: u64 },
    Shutdown,
}

pub trait Registry: Send + Sync + 'static {
    type Request: Clone + core::fmt::Display + Send + 'static;

    fn verbs(&self) -> &[Verb];

    fn parse_line(&self, line: &str) -> Result<Self::Request, String>;

    fn parse_tool(
        &self,
        name: &str,
        arguments: &Json,
    ) -> Result<Self::Request, String>;

    fn verb(&self, request: &Self::Request) -> &Verb;

    fn answer_tables(&self, request: &Self::Request) -> Vec<AnswerTable>;

    /// The application checks that the declaration matches the requested work.
    /// Registries without cost-bearing requests admit none through DeclaredCost.
    fn has_declared_cost(&self, _request: &Self::Request) -> bool {
        false
    }

    fn closed_reason(&self, request: &Self::Request) -> String;

    fn submission(&self, request: &Self::Request) -> Submission;

    fn route(&self, request: &Self::Request) -> Route;

    fn help(&self) -> Vec<String>;

    fn rejected(
        &self,
        line: &str,
        shape: crate::libs::door::typed::Shape,
        reason: &str,
    ) -> Reply;
}

pub fn admitted<R: Registry>(
    registry: &R,
    request: &R::Request,
) -> Result<(), String> {
    match registry.verb(request).gate {
        Gate::ReadOnly => Ok(()),
        Gate::DeclaredCost if registry.has_declared_cost(request) => Ok(()),
        Gate::ScopedCancel if matches!(registry.route(request), Route::Cancel { .. }) => Ok(()),
        Gate::DeclaredCost | Gate::ScopedCancel | Gate::Closed => Err(registry.closed_reason(request)),
    }
}

/// Structural validation and declared domains precede application parsing.
/// Logical types are interpreted by `Registry::parse_tool`; patterns are discovery metadata.
pub fn validate_arguments(ty: &ArgumentType, value: &Json) -> Result<(), String> {
    preflight_json(value)?;
    crate::libs::door::json::unique(value)?;
    let mut budget = ValidationBudget { visits: 0 };
    validate_at(ty, value, "arguments", 0, &mut budget)
}

const MAX_ARGUMENT_DEPTH: usize = 64;
const MAX_ARGUMENT_VISITS: usize = 65_536;

enum JsonFrame<'a> {
    Array(core::slice::Iter<'a, Json>, usize),
    Object(core::slice::Iter<'a, (String, Json)>, usize),
}

fn preflight_json(value: &Json) -> Result<(), String> {
    let mut visits = 1usize;
    let mut frames = Vec::new();
    match value {
        Json::Arr(items) => frames.push(JsonFrame::Array(items.iter(), 0)),
        Json::Obj(fields) => frames.push(JsonFrame::Object(fields.iter(), 0)),
        _ => {}
    }
    while let Some(frame) = frames.last_mut() {
        let (child, parent_depth) = match frame {
            JsonFrame::Array(items, depth) => match items.next() {
                Some(child) => (child, *depth),
                None => {
                    frames.pop();
                    continue;
                }
            },
            JsonFrame::Object(fields, depth) => match fields.next() {
                Some((_, child)) => (child, *depth),
                None => {
                    frames.pop();
                    continue;
                }
            },
        };
        let depth = parent_depth + 1;
        if depth > MAX_ARGUMENT_DEPTH {
            return Err(format!("registry.rs: argument depth limit {MAX_ARGUMENT_DEPTH} exceeded"));
        }
        if visits == MAX_ARGUMENT_VISITS {
            return Err(format!("registry.rs: argument visit limit {MAX_ARGUMENT_VISITS} exceeded"));
        }
        visits += 1;
        match child {
            Json::Arr(items) => frames.push(JsonFrame::Array(items.iter(), depth)),
            Json::Obj(fields) => frames.push(JsonFrame::Object(fields.iter(), depth)),
            _ => {}
        }
    }
    Ok(())
}

struct ValidationBudget {
    visits: usize,
}

impl ValidationBudget {
    fn charge(&mut self) -> Result<(), String> {
        if self.visits == MAX_ARGUMENT_VISITS {
            return Err(format!("registry.rs: validation visit limit {MAX_ARGUMENT_VISITS} exceeded"));
        }
        self.visits += 1;
        Ok(())
    }
}

fn validate_at(
    ty: &ArgumentType,
    value: &Json,
    at: &str,
    depth: usize,
    budget: &mut ValidationBudget,
) -> Result<(), String> {
    budget.charge()?;
    if depth > MAX_ARGUMENT_DEPTH {
        return Err(format!("registry.rs: schema depth limit {MAX_ARGUMENT_DEPTH} exceeded"));
    }
    match (ty, value) {
        (ArgumentType::String { values, .. }, Json::Str(text)) => {
            if !values.is_empty() && !values.iter().any(|v| v.as_ref() == text) {
                return Err(format!(
                    "{at}: `{text}` is outside the declared domain [{}]",
                    values.iter().map(|v| v.as_ref()).collect::<Vec<_>>().join(", ")
                ));
            }
            Ok(())
        }
        (ArgumentType::Boolean, Json::Bool(_)) => Ok(()),
        (ArgumentType::UnsignedInteger { maximum }, Json::Num(text)) => {
            let number = if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) {
                text.parse::<u64>().ok()
            } else {
                None
            };
            match number {
                Some(n) if n <= *maximum => Ok(()),
                _ => Err(format!("{at}: expected an unsigned integer no greater than {maximum}")),
            }
        }
        (ArgumentType::Array { items }, Json::Arr(values)) => {
            for (index, value) in values.iter().enumerate() {
                budget.charge()?;
                validate_at(items, value, &format!("{at}[{index}]"), depth + 1, budget)?;
            }
            Ok(())
        }
        (ArgumentType::Object { fields, additional_properties }, Json::Obj(values)) => {
            for field in fields {
                budget.charge()?;
                let mut found = None;
                for entry in values {
                    budget.charge()?;
                    if entry.0 == field.name.as_ref() {
                        found = Some(entry);
                        break;
                    }
                }
                match found {
                    Some((_, value)) => {
                        validate_at(&field.ty, value, &format!("{at}.{}", field.name), depth + 1, budget)?;
                    }
                    None if field.required => {
                        return Err(format!("{at}.{}: required field is missing", field.name));
                    }
                    None => {}
                }
            }
            for (name, _) in values {
                budget.charge()?;
                if !*additional_properties {
                    let mut admitted = false;
                    for field in fields {
                        budget.charge()?;
                        if field.name.as_ref() == name {
                            admitted = true;
                            break;
                        }
                    }
                    if !admitted {
                        return Err(format!("{at}.{name}: extra field is not admitted"));
                    }
                }
            }
            Ok(())
        }
        (ArgumentType::OneOf(alternatives), _) => {
            let mut matches = 0usize;
            let mut reasons = Vec::new();
            for alternative in alternatives {
                match validate_at(alternative, value, at, depth + 1, budget) {
                    Ok(()) => matches += 1,
                    Err(reason) if reason.starts_with("registry.rs: ") => return Err(reason),
                    Err(reason) => reasons.push(reason),
                }
            }
            match matches {
                1 => Ok(()),
                0 => Err(format!("{at}: no alternative matched: {}", reasons.join("; "))),
                _ => Err(format!("{at}: value matches more than one alternative")),
            }
        }
        (ArgumentType::String { .. }, _) => Err(format!("{at}: expected a string")),
        (ArgumentType::Boolean, _) => Err(format!("{at}: expected a boolean")),
        (ArgumentType::UnsignedInteger { maximum }, _) => {
            Err(format!("{at}: expected an unsigned integer no greater than {maximum}"))
        }
        (ArgumentType::Array { .. }, _) => Err(format!("{at}: expected an array")),
        (ArgumentType::Object { .. }, _) => Err(format!("{at}: expected an object")),
    }
}
