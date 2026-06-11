use crate::model::{Field, Type};
use convert_case::{Case, Casing};
use eyre::{ContextCompat, Result};
use serde::de::{Error, Unexpected};
use serde::ser::SerializeStruct;
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

    /// Public error variants that handlers may return for this endpoint.
    #[serde(default)]
    pub errors: Vec<EndpointErrorSchema>,
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
            errors: Vec::new(),
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

    /// Adds public error variants to the endpoint.
    pub fn with_errors(mut self, errors: Vec<EndpointErrorSchema>) -> Self {
        self.errors = errors;
        self
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Hash, PartialEq, PartialOrd, Eq, Ord)]
pub struct EndpointErrorSchema {
    pub name: String,
    pub code: EndpointErrorCodeRef,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub fields: Vec<Field>,
}

#[derive(Clone, Debug, Hash, PartialEq, PartialOrd, Eq, Ord)]
pub struct EndpointErrorCodeRef {
    pub ty: Type,
    pub variant: String,
}

impl EndpointErrorCodeRef {
    pub const ENUM_NAME: &'static str = "ErrorCode";

    pub fn new(variant: impl Into<String>) -> Self {
        Self {
            ty: Type::enum_ref(Self::ENUM_NAME, true),
            variant: variant.into(),
        }
    }

    pub fn variant(&self) -> &str {
        &self.variant
    }

    pub fn path(&self) -> String {
        format!("{}::{}", Self::ENUM_NAME, self.variant)
    }

    fn validate_ty(ty: Type) -> std::result::Result<Type, String> {
        match &ty {
            Type::EnumRef { name, .. } if name == Self::ENUM_NAME => Ok(ty),
            Type::EnumRef { name, .. } => {
                Err(format!("expected {} enum ref, got {name}", Self::ENUM_NAME))
            }
            _ => Err(format!("expected {} enum ref", Self::ENUM_NAME)),
        }
    }

    fn parse_path(path: &str) -> std::result::Result<Self, String> {
        let (enum_name, variant) = path
            .split_once("::")
            .ok_or_else(|| format!("expected {}::Variant", Self::ENUM_NAME))?;

        if enum_name != Self::ENUM_NAME {
            return Err(format!(
                "expected {} enum path, got {enum_name}",
                Self::ENUM_NAME
            ));
        }

        if variant.is_empty() || variant.contains("::") {
            return Err(format!("expected {}::Variant", Self::ENUM_NAME));
        }

        Ok(Self::new(variant))
    }
}

impl std::fmt::Display for EndpointErrorCodeRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.path())
    }
}

impl Serialize for EndpointErrorCodeRef {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("EndpointErrorCodeRef", 2)?;
        state.serialize_field("ty", &self.ty)?;
        state.serialize_field("variant", &self.variant)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for EndpointErrorCodeRef {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Helper {
            Path(String),
            Structured { ty: Type, variant: String },
        }

        match Helper::deserialize(deserializer)? {
            Helper::Path(path) => Self::parse_path(&path)
                .map_err(|err| D::Error::invalid_value(Unexpected::Str(&path), &err.as_str())),
            Helper::Structured { ty, variant } => {
                let ty = Self::validate_ty(ty).map_err(D::Error::custom)?;
                Ok(Self { ty, variant })
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libs::error_code::ErrorCode;
    use std::str::FromStr;

    #[test]
    fn endpoint_schema_defaults_errors() {
        let schema: EndpointSchema = serde_json::from_value(serde_json::json!({
            "name": "Login",
            "code": 10001,
            "parameters": [],
            "returns": [],
            "roles": []
        }))
        .unwrap();

        assert!(schema.errors.is_empty());
    }

    #[test]
    fn endpoint_schema_reads_error_definitions() {
        let schema: EndpointSchema = serde_json::from_value(serde_json::json!({
            "name": "Login",
            "code": 10001,
            "parameters": [],
            "returns": [],
            "roles": [],
            "errors": [
                {
                    "name": "WrongPassword",
                    "code": "ErrorCode::Unauthorized",
                    "message": "Wrong password",
                    "fields": []
                },
                {
                    "name": "PasswordTooShort",
                    "code": "ErrorCode::BadRequest",
                    "message": "Password too short",
                    "fields": [
                        { "name": "min_length", "ty": "Int32" }
                    ]
                }
            ]
        }))
        .unwrap();

        assert_eq!(schema.errors.len(), 2);
        assert_eq!(schema.errors[0].code.variant(), "Unauthorized");
        assert_eq!(schema.errors[1].fields[0].name, "min_length");
    }

    #[test]
    fn endpoint_error_code_ref_serializes_as_typed_ref() {
        let value = serde_json::to_value(EndpointErrorCodeRef::new("BadRequest")).unwrap();

        assert_eq!(
            value,
            serde_json::json!({
                "ty": {
                    "EnumRef": {
                        "name": "ErrorCode"
                    }
                },
                "variant": "BadRequest"
            })
        );

        let parsed: EndpointErrorCodeRef = serde_json::from_value(value).unwrap();
        assert_eq!(parsed.variant(), "BadRequest");
    }

    #[test]
    fn endpoint_error_code_ref_rejects_wrong_paths() {
        assert!(
            serde_json::from_value::<EndpointErrorCodeRef>(serde_json::json!("BadRequest"))
                .is_err()
        );
        assert!(
            serde_json::from_value::<EndpointErrorCodeRef>(serde_json::json!("UserRole::Admin"))
                .is_err()
        );
    }

    #[test]
    fn error_code_parses_names_and_numeric_codes() {
        assert_eq!(
            ErrorCode::from_str("BadRequest").unwrap(),
            ErrorCode::BAD_REQUEST
        );
        assert_eq!(
            ErrorCode::from_str("BAD_REQUEST").unwrap(),
            ErrorCode::BAD_REQUEST
        );
        assert_eq!(
            ErrorCode::from_str("100777").unwrap(),
            ErrorCode::new(100777)
        );
        assert!(ErrorCode::from_str("not a code").is_err());
    }
}
