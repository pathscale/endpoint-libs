use crate::model::{Field, Type};
use convert_case::{Case, Casing};
use eyre::{ContextCompat, Result};
use serde::*;
use std::fmt::Write;

/// `EndpointSchema` is a struct that represents a single endpoint in the API.
#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct EndpointSchema {
    /// The name of the endpoint (e.g. `UserListSymbols`)
    pub name: String,

    /// The method code of the endpoint (e.g. `10020`)
    pub code: u32,

    /// A list of parameters that the endpoint accepts (e.g. "symbol" of type `String`)
    pub parameters: Vec<Field>,

    /// A list of fields that the endpoint returns
    pub returns: Vec<Field>,

    /// The type of the stream response (if any)
    #[serde(default)]
    pub stream_response: Option<Type>,

    /// A description of the endpoint added by `with_description` method
    #[serde(default)]
    pub description: String,

    /// The JSON schema of the endpoint (`Default::default()`)
    #[serde(default)]
    pub json_schema: serde_json::Value,

    // Allowed roles for this endpoint ["EnumRole::EnumVariant"]
    pub roles: Vec<String>,
}

impl EndpointSchema {
    /// Creates a new `EndpointSchema` with the given name, method code, parameters and returns.
    pub fn new(
        name: impl Into<String>,
        code: u32,
        parameters: Vec<Field>,
        returns: Vec<Field>,
    ) -> Self {
        Self {
            name: name.into(),
            code,
            parameters,
            returns,
            stream_response: None,
            description: "".to_string(),
            json_schema: Default::default(),
            roles: Vec::new(),
        }
    }

    /// Adds a stream response type field to the endpoint.
    pub fn with_stream_response_type(mut self, stream_response: Type) -> Self {
        self.stream_response = Some(stream_response);
        self
    }

    /// Adds a description field to the endpoint.
    pub fn with_description(mut self, desc: impl Into<String>) -> Self {
        self.description = desc.into();
        self
    }

    /// Adds allowed roles to the endpoint.
    pub fn with_roles(mut self, roles: Vec<String>) -> Self {
        self.roles = roles;
        self
    }
}

pub fn encode_header<T: Serialize>(v: T, schema: EndpointSchema) -> Result<String> {
    let mut s = String::new();
    write!(s, "0{}", schema.name.to_ascii_lowercase())?;
    let v = serde_json::to_value(&v)?;

    for (i, f) in schema.parameters.iter().enumerate() {
        let key = f.name.to_case(Case::Camel);
        let value = v.get(&key).with_context(|| format!("key: {key}"))?;
        if value.is_null() {
            continue;
        }
        write!(
            s,
            ", {}{}",
            i + 1,
            urlencoding::encode(&value.to_string().replace('\"', ""))
        )?;
    }
    Ok(s)
}
