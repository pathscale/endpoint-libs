use crate::model::{Field, MetaMap, Type};
use convert_case::{Case, Casing};
use eyre::{ContextCompat, Result};
use serde::de::{Error, Unexpected};
use serde::ser::SerializeStruct;
use serde::*;
use std::fmt::Write;

/// `EndpointSchema` is a struct that represents a single endpoint in the API.
#[derive(Debug, Serialize, Deserialize, Default, Clone)]
#[non_exhaustive]
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

    /// Emitter annotations — see [`MetaMap`](crate::model::MetaMap). Empty in 2.0;
    /// consumed by the OpenAPI/AsyncAPI emitters in 2.1.
    #[serde(default, skip_serializing_if = "MetaMap::is_empty")]
    pub meta: MetaMap,
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
            meta: MetaMap::default(),
        }
    }

    /// Attach emitter annotations. See [`MetaMap`](crate::model::MetaMap).
    #[must_use]
    pub fn with_meta(mut self, meta: MetaMap) -> Self {
        self.meta = meta;
        self
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
#[non_exhaustive]
pub struct EndpointErrorSchema {
    pub name: String,
    pub code: EndpointErrorCodeRef,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub fields: Vec<Field>,
}

impl EndpointErrorSchema {
    /// Creates an error schema with no message and no fields.
    ///
    /// This type is `#[non_exhaustive]`, so out-of-crate callers must build it here
    /// rather than with a struct literal.
    pub fn new(name: impl Into<String>, code: EndpointErrorCodeRef) -> Self {
        Self {
            name: name.into(),
            code,
            message: String::new(),
            fields: Vec::new(),
        }
    }

    /// Sets the human-readable message.
    #[must_use]
    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = message.into();
        self
    }

    /// Sets the structured fields carried by this error.
    #[must_use]
    pub fn with_fields(mut self, fields: Vec<Field>) -> Self {
        self.fields = fields;
        self
    }
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
mod forward_compat_tests {
    use super::*;
    use crate::model::MetaMap;

    /// The contract 2.1 depends on: generated code deserializes committed schema JSON
    /// (`serde_json::from_str`, see endpointgen's rust.rs emitter), so a *newer*
    /// endpointgen emitting fields this version does not know about must not break an
    /// older endpoint-libs. If this test ever fails, adding anything to the schema
    /// model has become a breaking change and OpenAPI support becomes a 3.0.
    #[test]
    fn unknown_future_fields_are_ignored() {
        let json = r#"{
            "name": "UserListSymbols",
            "code": 10020,
            "parameters": [],
            "returns": [],
            "roles": ["Admin"],
            "x_future_openapi_binding": {"path": "/rpc/UserListSymbols"},
            "some_field_from_2_5": 42
        }"#;
        let schema: EndpointSchema = serde_json::from_str(json).expect("unknown fields must not break deserialization");
        assert_eq!(schema.name, "UserListSymbols");
        assert_eq!(schema.code, 10020);
        assert!(schema.meta.is_empty());
    }

    /// Old committed schema JSON (no `meta` key at all) must still load.
    #[test]
    fn absent_meta_defaults_empty() {
        let json = r#"{"name":"A","code":1,"parameters":[],"returns":[],"roles":[]}"#;
        let schema: EndpointSchema = serde_json::from_str(json).unwrap();
        assert!(schema.meta.is_empty());
    }

    /// `meta` must survive a serialize → deserialize cycle byte-for-byte, including
    /// keys this version assigns no meaning to. The 2.1 emitters read these.
    #[test]
    fn meta_round_trips_including_unknown_keys() {
        let mut meta = MetaMap::default();
        meta.insert("example", serde_json::json!({"symbol": "BTC"}));
        meta.insert("x-openapi-tags", serde_json::json!(["trading"]));
        meta.insert("deprecated", serde_json::json!(true));

        let schema = EndpointSchema::new("A", 1, vec![], vec![]).with_meta(meta.clone());
        let text = serde_json::to_string(&schema).unwrap();
        let back: EndpointSchema = serde_json::from_str(&text).unwrap();

        assert_eq!(back.meta, meta);
        assert_eq!(
            back.meta.get("x-openapi-tags"),
            Some(&serde_json::json!(["trading"]))
        );
    }

    /// Empty `meta` must not appear in the output at all, so 2.0 artifacts stay
    /// byte-identical to 1.9 ones and `endpointgen --check` sees no spurious drift.
    #[test]
    fn empty_meta_is_not_serialized() {
        let schema = EndpointSchema::new("A", 1, vec![], vec![]);
        let text = serde_json::to_string(&schema).unwrap();
        assert!(!text.contains("meta"), "empty meta leaked into output: {text}");

        let field = Field::new("x", Type::String);
        let text = serde_json::to_string(&field).unwrap();
        assert!(!text.contains("meta"), "empty field meta leaked: {text}");
    }

    /// Field-level meta is what carries per-parameter OpenAPI enrichment.
    #[test]
    fn field_meta_round_trips_and_preserves_derives() {
        let mut meta = MetaMap::default();
        meta.insert("minimum", serde_json::json!(0));
        let field = Field::new("amount", Type::Int64).with_meta(meta);

        let back: Field = serde_json::from_str(&serde_json::to_string(&field).unwrap()).unwrap();
        assert_eq!(back.meta.get("minimum"), Some(&serde_json::json!(0)));

        // Field derives Hash/Ord/Eq; MetaMap's manual impls must keep that working.
        let mut set = std::collections::BTreeSet::new();
        set.insert(field.clone());
        assert!(set.contains(&field));
        assert_ne!(field, Field::new("amount", Type::Int64));
    }
}
