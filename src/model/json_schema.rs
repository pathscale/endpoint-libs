//! Conversion of the endpoint [`Type`] model into JSON Schema, as required by
//! MCP (Model Context Protocol) tool definitions (`inputSchema` / `outputSchema`).
//!
//! The conversion is a pure function over [`Type`]; `StructRef`/`EnumRef`/
//! `StructTable` references are resolved through a caller-provided
//! [`TypeRegistry`] and emitted as `$ref` entries under `$defs`.

use std::collections::BTreeMap;

use convert_case::{Case, Casing};
use eyre::{Result, bail};
use serde_json::{Value, json};

use crate::model::endpoint::EndpointSchema;
use crate::model::types::{Field, Type};

/// Name → [`Type`] map used to resolve `StructRef`, `EnumRef` and
/// `StructTable` while converting to JSON Schema.
///
/// Built by the caller (a server at startup, or `endpoint-gen`) from the known
/// struct and enum definitions.
#[derive(Debug, Default, Clone)]
pub struct TypeRegistry {
    structs: BTreeMap<String, Type>,
    enums: BTreeMap<String, Type>,
}

impl TypeRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Indexes `Struct` and `Enum` definitions found in `ty`, recursing into
    /// struct fields and container types so nested definitions are captured.
    pub fn add(&mut self, ty: &Type) {
        match ty {
            Type::Struct { name, fields } => {
                self.structs.insert(name.clone(), ty.clone());
                for field in fields {
                    self.add(&field.ty);
                }
            }
            Type::Enum { name, .. } => {
                self.enums.insert(name.clone(), ty.clone());
            }
            Type::Vec(inner) | Type::Optional(inner) => self.add(inner),
            _ => {}
        }
    }

    /// Indexes every type in `tys`. See [`TypeRegistry::add`].
    pub fn add_all<'a>(&mut self, tys: impl IntoIterator<Item = &'a Type>) {
        for ty in tys {
            self.add(ty);
        }
    }

    /// Indexes all types referenced by an endpoint's parameters, returns and
    /// stream response.
    pub fn add_endpoint(&mut self, schema: &EndpointSchema) {
        self.add_all(schema.parameters.iter().map(|f| &f.ty));
        self.add_all(schema.returns.iter().map(|f| &f.ty));
        if let Some(stream) = &schema.stream_response {
            self.add(stream);
        }
    }

    pub fn get_struct(&self, name: &str) -> Option<&Type> {
        self.structs.get(name)
    }

    pub fn get_enum(&self, name: &str) -> Option<&Type> {
        self.enums.get(name)
    }
}

/// Definition name used for `$defs`/`$ref`. Enums keep their optional `Enum`
/// prefix (mirrors `prefixed_name` handling in generated Rust code).
fn enum_def_name(name: &str, prefixed_name: bool) -> String {
    if prefixed_name {
        format!("Enum{}", name.to_case(Case::Pascal))
    } else {
        name.to_case(Case::Pascal)
    }
}

fn struct_def_name(name: &str) -> String {
    name.to_case(Case::Pascal)
}

fn ref_to(def_name: &str) -> Value {
    json!({ "$ref": format!("#/$defs/{def_name}") })
}

/// Builds an object schema over `fields`, in camelCase (matching the
/// `#[serde(rename_all = "camelCase")]` on generated structs). Fields whose
/// type is `Optional` are excluded from `required`.
fn fields_to_object_schema(
    fields: &[Field],
    registry: &TypeRegistry,
    defs: &mut BTreeMap<String, Value>,
) -> Result<Value> {
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();

    for field in fields {
        let key = field.name.to_case(Case::Camel);
        let mut schema = match &field.ty {
            // Unwrap Optional at the field level: the field is simply not required.
            Type::Optional(inner) => inner.to_json_schema(registry, defs)?,
            other => {
                required.push(Value::String(key.clone()));
                other.to_json_schema(registry, defs)?
            }
        };
        if !field.description.is_empty()
            && let Value::Object(map) = &mut schema
        {
            map.entry("description")
                .or_insert_with(|| Value::String(field.description.clone()));
        }
        properties.insert(key, schema);
    }

    let mut obj = serde_json::Map::new();
    obj.insert("type".into(), "object".into());
    obj.insert("properties".into(), Value::Object(properties));
    obj.insert("required".into(), Value::Array(required));
    obj.insert("additionalProperties".into(), Value::Bool(false));
    Ok(Value::Object(obj))
}

fn enum_to_schema(name: &str, variants: &[crate::model::types::EnumVariant]) -> Value {
    let one_of: Vec<Value> = variants
        .iter()
        .map(|v| {
            let mut entry = serde_json::Map::new();
            entry.insert("const".into(), json!(v.value));
            entry.insert("title".into(), Value::String(v.name.clone()));
            if !v.description.is_empty() {
                entry.insert("description".into(), Value::String(v.description.clone()));
            }
            Value::Object(entry)
        })
        .collect();
    json!({
        "type": "integer",
        "title": name,
        "oneOf": one_of,
    })
}

impl Type {
    /// Converts this type to a JSON Schema fragment.
    ///
    /// `Struct`/`Enum` definitions (inline or by reference) are recorded in
    /// `defs` and referenced via `$ref` so shared definitions are emitted once.
    /// Returns an error for `StructRef`/`EnumRef`/`StructTable` names that are
    /// not present in `registry` — callers should fail at startup rather than
    /// emit a schema with dangling references.
    pub fn to_json_schema(
        &self,
        registry: &TypeRegistry,
        defs: &mut BTreeMap<String, Value>,
    ) -> Result<Value> {
        Ok(match self {
            Type::UInt32 => json!({ "type": "integer", "minimum": 0, "maximum": u32::MAX }),
            Type::Int32 => json!({ "type": "integer", "minimum": i32::MIN, "maximum": i32::MAX }),
            Type::Int64 => json!({ "type": "integer" }),
            Type::TimeStampMs => {
                json!({ "type": "integer", "description": "Unix timestamp in milliseconds" })
            }
            Type::Float64 => json!({ "type": "number" }),
            Type::Boolean => json!({ "type": "boolean" }),
            Type::String => json!({ "type": "string" }),
            Type::Bytea => json!({ "type": "string", "contentEncoding": "base64" }),
            Type::UUID => json!({ "type": "string", "format": "uuid" }),
            Type::NanoId { len } => json!({
                "type": "string",
                "minLength": len,
                "maxLength": len,
                "pattern": "^[0-9A-Za-z]+$",
            }),
            // Both IPv4 and IPv6 are accepted on the wire, so no single JSON
            // Schema `format` applies.
            Type::IpAddr => json!({ "type": "string", "description": "IPv4 or IPv6 address" }),
            Type::Object => json!({ "type": "object" }),
            Type::Unit => json!({ "type": "null" }),
            Type::BlockchainDecimal => {
                json!({ "type": "string", "description": "Decimal number as string" })
            }
            Type::BlockchainAddress => {
                json!({ "type": "string", "pattern": "^0x[0-9a-fA-F]{40}$" })
            }
            Type::BlockchainTransactionHash => {
                json!({ "type": "string", "pattern": "^0x[0-9a-fA-F]{64}$" })
            }
            Type::Vec(inner) => {
                json!({ "type": "array", "items": inner.to_json_schema(registry, defs)? })
            }
            Type::Optional(inner) => {
                // Standalone Optional (not unwrapped by a parent object):
                // nullable variant of the inner schema.
                let inner = inner.to_json_schema(registry, defs)?;
                json!({ "anyOf": [inner, { "type": "null" }] })
            }
            Type::Struct { name, fields } => {
                let def_name = struct_def_name(name);
                if !defs.contains_key(&def_name) {
                    // Reserve the slot first so recursive references terminate.
                    defs.insert(def_name.clone(), Value::Null);
                    let schema = fields_to_object_schema(fields, registry, defs)?;
                    defs.insert(def_name.clone(), schema);
                }
                ref_to(&def_name)
            }
            Type::StructRef(name) => {
                let Some(ty) = registry.get_struct(name).cloned() else {
                    bail!("unresolved StructRef: {name} (not present in TypeRegistry)");
                };
                ty.to_json_schema(registry, defs)?
            }
            Type::StructTable { struct_ref } => {
                let Some(ty) = registry.get_struct(struct_ref).cloned() else {
                    bail!("unresolved StructTable ref: {struct_ref} (not present in TypeRegistry)");
                };
                let items = ty.to_json_schema(registry, defs)?;
                json!({ "type": "array", "items": items })
            }
            Type::Enum { name, variants } => {
                let def_name = enum_def_name(name, false);
                defs.entry(def_name.clone())
                    .or_insert_with(|| enum_to_schema(&def_name, variants));
                ref_to(&def_name)
            }
            Type::EnumRef {
                name,
                prefixed_name,
            } => {
                let Some(Type::Enum { variants, .. }) = registry.get_enum(name) else {
                    bail!("unresolved EnumRef: {name} (not present in TypeRegistry)");
                };
                let def_name = enum_def_name(name, *prefixed_name);
                if !defs.contains_key(&def_name) {
                    let schema = enum_to_schema(&def_name, variants);
                    defs.insert(def_name.clone(), schema);
                }
                ref_to(&def_name)
            }
        })
    }
}

/// Attaches accumulated `$defs` to a root schema object, if any.
fn attach_defs(mut root: Value, defs: BTreeMap<String, Value>) -> Value {
    if !defs.is_empty()
        && let Value::Object(map) = &mut root
    {
        map.insert("$defs".into(), json!(defs));
    }
    root
}

impl EndpointSchema {
    /// MCP tool name for this endpoint: the endpoint name in snake_case
    /// (e.g. `UserListSymbols` → `user_list_symbols`).
    pub fn tool_name(&self) -> String {
        self.name.to_case(Case::Snake)
    }

    /// MCP `inputSchema`: an object schema over `parameters`. Non-`Optional`
    /// parameters are listed in `required`.
    pub fn to_mcp_input_schema(&self, registry: &TypeRegistry) -> Result<Value> {
        let mut defs = BTreeMap::new();
        let root = fields_to_object_schema(&self.parameters, registry, &mut defs)?;
        Ok(attach_defs(root, defs))
    }

    /// MCP `outputSchema`: an object schema over `returns`.
    pub fn to_mcp_output_schema(&self, registry: &TypeRegistry) -> Result<Value> {
        let mut defs = BTreeMap::new();
        let root = fields_to_object_schema(&self.returns, registry, &mut defs)?;
        Ok(attach_defs(root, defs))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::types::EnumVariant;

    fn schema_of(ty: &Type) -> Value {
        let registry = TypeRegistry::new();
        let mut defs = BTreeMap::new();
        let schema = ty.to_json_schema(&registry, &mut defs).unwrap();
        attach_defs(schema, defs)
    }

    #[test]
    fn primitives() {
        assert_eq!(
            schema_of(&Type::UInt32),
            json!({ "type": "integer", "minimum": 0, "maximum": u32::MAX })
        );
        assert_eq!(
            schema_of(&Type::Int32),
            json!({ "type": "integer", "minimum": i32::MIN, "maximum": i32::MAX })
        );
        assert_eq!(schema_of(&Type::Int64), json!({ "type": "integer" }));
        assert_eq!(schema_of(&Type::Float64), json!({ "type": "number" }));
        assert_eq!(schema_of(&Type::Boolean), json!({ "type": "boolean" }));
        assert_eq!(schema_of(&Type::String), json!({ "type": "string" }));
        assert_eq!(schema_of(&Type::Object), json!({ "type": "object" }));
        assert_eq!(schema_of(&Type::Unit), json!({ "type": "null" }));
    }

    #[test]
    fn string_formats() {
        assert_eq!(
            schema_of(&Type::Bytea),
            json!({ "type": "string", "contentEncoding": "base64" })
        );
        assert_eq!(
            schema_of(&Type::UUID),
            json!({ "type": "string", "format": "uuid" })
        );
        assert_eq!(
            schema_of(&Type::NanoId { len: 21 }),
            json!({
                "type": "string",
                "minLength": 21,
                "maxLength": 21,
                "pattern": "^[0-9A-Za-z]+$",
            })
        );
        assert_eq!(
            schema_of(&Type::BlockchainAddress),
            json!({ "type": "string", "pattern": "^0x[0-9a-fA-F]{40}$" })
        );
        assert_eq!(
            schema_of(&Type::BlockchainTransactionHash),
            json!({ "type": "string", "pattern": "^0x[0-9a-fA-F]{64}$" })
        );
    }

    #[test]
    fn containers() {
        assert_eq!(
            schema_of(&Type::vec(Type::String)),
            json!({ "type": "array", "items": { "type": "string" } })
        );
        assert_eq!(
            schema_of(&Type::optional(Type::Int64)),
            json!({ "anyOf": [{ "type": "integer" }, { "type": "null" }] })
        );
    }

    #[test]
    fn inline_struct_goes_to_defs() {
        let ty = Type::struct_(
            "UserInfo",
            vec![
                Field::new("user_id", Type::Int64),
                Field::new("email", Type::optional(Type::String)),
            ],
        );
        let schema = schema_of(&ty);
        assert_eq!(schema["$ref"], json!("#/$defs/UserInfo"));
        let def = &schema["$defs"]["UserInfo"];
        assert_eq!(def["type"], json!("object"));
        assert_eq!(def["properties"]["userId"]["type"], json!("integer"));
        assert_eq!(def["properties"]["email"]["type"], json!("string"));
        // Optional field is not required.
        assert_eq!(def["required"], json!(["userId"]));
        assert_eq!(def["additionalProperties"], json!(false));
    }

    #[test]
    fn enum_schema() {
        let ty = Type::enum_(
            "role",
            vec![
                EnumVariant::new_with_description("Admin", "administrator", 1),
                EnumVariant::new("User", 2),
            ],
        );
        let schema = schema_of(&ty);
        assert_eq!(schema["$ref"], json!("#/$defs/Role"));
        let def = &schema["$defs"]["Role"];
        assert_eq!(def["type"], json!("integer"));
        assert_eq!(def["oneOf"][0]["const"], json!(1));
        assert_eq!(def["oneOf"][0]["title"], json!("Admin"));
        assert_eq!(def["oneOf"][0]["description"], json!("administrator"));
        assert_eq!(def["oneOf"][1], json!({ "const": 2, "title": "User" }));
    }

    #[test]
    fn struct_ref_resolves_through_registry() {
        let user_info = Type::struct_("UserInfo", vec![Field::new("user_id", Type::Int64)]);
        let mut registry = TypeRegistry::new();
        registry.add(&user_info);

        let mut defs = BTreeMap::new();
        let schema = Type::struct_ref("UserInfo")
            .to_json_schema(&registry, &mut defs)
            .unwrap();
        assert_eq!(schema, json!({ "$ref": "#/$defs/UserInfo" }));
        assert!(defs.contains_key("UserInfo"));
    }

    #[test]
    fn struct_table_resolves_to_array() {
        let row = Type::struct_("Row", vec![Field::new("id", Type::Int64)]);
        let mut registry = TypeRegistry::new();
        registry.add(&row);

        let mut defs = BTreeMap::new();
        let schema = Type::struct_table("Row")
            .to_json_schema(&registry, &mut defs)
            .unwrap();
        assert_eq!(
            schema,
            json!({ "type": "array", "items": { "$ref": "#/$defs/Row" } })
        );
    }

    #[test]
    fn unresolved_refs_error() {
        let registry = TypeRegistry::new();
        let mut defs = BTreeMap::new();
        assert!(
            Type::struct_ref("Missing")
                .to_json_schema(&registry, &mut defs)
                .is_err()
        );
        assert!(
            Type::enum_ref("Missing", true)
                .to_json_schema(&registry, &mut defs)
                .is_err()
        );
        assert!(
            Type::struct_table("Missing")
                .to_json_schema(&registry, &mut defs)
                .is_err()
        );
    }

    #[test]
    fn enum_ref_prefixed_naming() {
        let role = Type::enum_("role", vec![EnumVariant::new("Admin", 1)]);
        let mut registry = TypeRegistry::new();
        registry.add(&role);

        let mut defs = BTreeMap::new();
        let schema = Type::enum_ref("role", true)
            .to_json_schema(&registry, &mut defs)
            .unwrap();
        assert_eq!(schema, json!({ "$ref": "#/$defs/EnumRole" }));
        assert!(defs.contains_key("EnumRole"));
    }

    #[test]
    fn shared_defs_emitted_once() {
        let user_info = Type::struct_("UserInfo", vec![Field::new("user_id", Type::Int64)]);
        let outer = Type::struct_(
            "Outer",
            vec![
                Field::new("a", user_info.clone()),
                Field::new("b", user_info),
            ],
        );
        let schema = schema_of(&outer);
        let def = &schema["$defs"]["Outer"];
        assert_eq!(
            def["properties"]["a"],
            json!({ "$ref": "#/$defs/UserInfo" })
        );
        assert_eq!(
            def["properties"]["b"],
            json!({ "$ref": "#/$defs/UserInfo" })
        );
        assert!(schema["$defs"]["UserInfo"].is_object());
    }

    #[test]
    fn tool_name_is_snake_case() {
        let schema = EndpointSchema::new("UserListSymbols", 10020, vec![], vec![]);
        assert_eq!(schema.tool_name(), "user_list_symbols");
    }

    #[test]
    fn endpoint_input_schema_golden() {
        let endpoint = EndpointSchema::new(
            "UserGetProfile",
            10010,
            vec![
                Field::new("user_id", Type::Int64),
                Field::new_with_description(
                    "nickname",
                    "Optional display name",
                    Type::optional(Type::String),
                ),
            ],
            vec![Field::new("profile", Type::struct_ref("UserProfile"))],
        );

        let profile = Type::struct_(
            "UserProfile",
            vec![
                Field::new("user_id", Type::Int64),
                Field::new("role", Type::enum_ref("role", true)),
            ],
        );
        let role = Type::enum_("role", vec![EnumVariant::new("Admin", 1)]);
        let mut registry = TypeRegistry::new();
        registry.add(&profile);
        registry.add(&role);

        let input = endpoint.to_mcp_input_schema(&registry).unwrap();
        assert_eq!(
            input,
            json!({
                "type": "object",
                "properties": {
                    "userId": { "type": "integer" },
                    "nickname": { "type": "string", "description": "Optional display name" },
                },
                "required": ["userId"],
                "additionalProperties": false,
            })
        );

        let output = endpoint.to_mcp_output_schema(&registry).unwrap();
        assert_eq!(
            output["properties"]["profile"],
            json!({ "$ref": "#/$defs/UserProfile" })
        );
        assert_eq!(output["required"], json!(["profile"]));
        assert_eq!(
            output["$defs"]["UserProfile"]["properties"]["role"],
            json!({ "$ref": "#/$defs/EnumRole" })
        );
        assert!(output["$defs"]["EnumRole"].is_object());
    }
}
