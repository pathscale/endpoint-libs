//! Shared plumbing for emitting API description documents (OpenAPI 3.1,
//! AsyncAPI 3.0) from the endpoint model.
//!
//! This module deliberately contains no OpenAPI- or AsyncAPI-specific shapes.
//! It provides the three things both emitters need and neither should
//! reimplement:
//!
//! 1. [`SchemaComponents`] — every referenced struct/enum collected once, at
//!    document scope rather than per-endpoint.
//! 2. [`relocate_refs`] — moving `#/$defs/X` to wherever a given document format
//!    wants its shared definitions.
//! 3. [`apply_meta`] — the `meta` passthrough, which is the sanctioned way to
//!    get per-field and per-endpoint annotations into a document without
//!    growing the model.
//!
//! It lives in endpoint-libs rather than endpointgen because a server needs the
//! same registry walk at startup to answer MCP requests, and a future OpenRPC
//! emitter would need it a third time.
//!
//! **The RON definitions are the source of truth.** Nothing here parses a
//! document; these are outputs only.

use std::collections::BTreeMap;

use eyre::{Result, bail};
use serde_json::{Map, Value};

use crate::model::endpoint::EndpointSchema;
use crate::model::json_schema::{TypeRegistry, fields_to_object_schema};
use crate::model::types::MetaMap;

/// Where a document format keeps its shared schema definitions.
///
/// JSON Schema puts them in `#/$defs`; both OpenAPI 3.1 and AsyncAPI 3.0 want
/// `#/components/schemas`.
pub const COMPONENTS_SCHEMAS_PREFIX: &str = "#/components/schemas/";

/// The `$defs` prefix that [`crate::model::Type::to_json_schema`] emits.
const DEFS_PREFIX: &str = "#/$defs/";

/// Every struct and enum referenced by a set of endpoints, emitted once.
///
/// [`crate::model::Type::to_json_schema`] already produces JSON Schema 2020-12,
/// and OpenAPI 3.1 is a superset of it, so these values drop into a document
/// essentially verbatim. The only work is hoisting them to document scope:
/// `to_mcp_input_schema`/`to_mcp_output_schema` deliberately build a *fresh*
/// `defs` map per endpoint so each MCP tool schema is self-contained, which is
/// exactly wrong for a document where operations should share `$ref`s.
///
/// Refs in [`SchemaComponents::schemas`] and in the per-endpoint schemas
/// returned by [`SchemaComponents::request_schema`] /
/// [`SchemaComponents::response_schema`] are already relocated to
/// [`COMPONENTS_SCHEMAS_PREFIX`] — callers do not need to call
/// [`relocate_refs`] themselves.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SchemaComponents {
    /// Definition name → schema, e.g. `"User"` → `{ "type": "object", ... }`.
    pub schemas: BTreeMap<String, Value>,
}

impl SchemaComponents {
    /// Walks `endpoints`, collecting every referenced definition exactly once.
    ///
    /// Fails if any `StructRef`/`EnumRef`/`StructTable` is missing from
    /// `registry` — a document with dangling `$ref`s is worse than no document,
    /// because validators accept it and generators emit broken clients.
    pub fn collect(endpoints: &[EndpointSchema], registry: &TypeRegistry) -> Result<Self> {
        let mut defs = BTreeMap::new();

        for endpoint in endpoints {
            // Build and discard the object schemas; the point is the side
            // effect on the shared `defs` map.
            fields_to_object_schema(&endpoint.parameters, registry, &mut defs)
                .map_err(|e| eyre::eyre!("endpoint {}: parameters: {e}", endpoint.name))?;
            fields_to_object_schema(&endpoint.returns, registry, &mut defs)
                .map_err(|e| eyre::eyre!("endpoint {}: returns: {e}", endpoint.name))?;

            if let Some(stream) = &endpoint.stream_response {
                stream
                    .to_json_schema(registry, &mut defs)
                    .map_err(|e| eyre::eyre!("endpoint {}: stream_response: {e}", endpoint.name))?;
            }

            for error in &endpoint.errors {
                fields_to_object_schema(&error.fields, registry, &mut defs).map_err(|e| {
                    eyre::eyre!("endpoint {}: error {}: {e}", endpoint.name, error.name)
                })?;
            }
        }

        for schema in defs.values_mut() {
            relocate_refs(schema, COMPONENTS_SCHEMAS_PREFIX);
        }

        Ok(Self { schemas: defs })
    }

    /// Object schema over an endpoint's `parameters`, with refs pointing at
    /// [`Self::schemas`]. Non-`Optional` parameters land in `required`.
    pub fn request_schema(
        &self,
        endpoint: &EndpointSchema,
        registry: &TypeRegistry,
    ) -> Result<Value> {
        self.object_schema(
            &endpoint.parameters,
            registry,
            &format!("endpoint {} parameters", endpoint.name),
        )
    }

    /// Object schema over an endpoint's `returns`. See [`Self::request_schema`].
    pub fn response_schema(
        &self,
        endpoint: &EndpointSchema,
        registry: &TypeRegistry,
    ) -> Result<Value> {
        self.object_schema(
            &endpoint.returns,
            registry,
            &format!("endpoint {} returns", endpoint.name),
        )
    }

    /// Builds an object schema whose `$ref`s point into the shared components.
    ///
    /// The throwaway `defs` map here is not a leak: every definition it would
    /// collect is already in [`Self::schemas`], because `collect` walked the
    /// same fields. Only the returned root object is used.
    fn object_schema(
        &self,
        fields: &[crate::model::types::Field],
        registry: &TypeRegistry,
        context: &str,
    ) -> Result<Value> {
        let mut throwaway = BTreeMap::new();
        let mut schema = fields_to_object_schema(fields, registry, &mut throwaway)?;
        relocate_refs(&mut schema, COMPONENTS_SCHEMAS_PREFIX);
        apply_field_meta(&mut schema, fields, context)?;
        Ok(schema)
    }
}

/// Applies each field's `meta` to its property in an object schema.
///
/// Deliberately done here rather than inside `fields_to_object_schema`: that
/// helper also builds MCP tool schemas, and those must stay byte-identical.
/// Field annotations are a document concern.
///
/// A field whose schema is a bare `$ref` is a hard error rather than a silent
/// no-op: sibling keys next to a `$ref` are ignored by most JSON Schema 2020-12
/// tooling, so emitting them would produce a document that looks annotated and
/// is not. Annotate the referenced definition instead.
fn apply_field_meta(
    schema: &mut Value,
    fields: &[crate::model::types::Field],
    context: &str,
) -> Result<()> {
    use convert_case::{Case, Casing};

    for field in fields {
        if field.meta.is_empty() {
            continue;
        }
        let key = field.name.to_case(Case::Camel);
        let Some(property) = schema.get_mut("properties").and_then(|p| p.get_mut(&key)) else {
            continue;
        };
        if property.get("$ref").is_some() {
            bail!(
                "{context}: field `{}` carries meta but its schema is a bare $ref; \
                 annotate the referenced definition instead — sibling keys next to \
                 $ref are ignored by JSON Schema 2020-12 tooling",
                field.name
            );
        }
        apply_meta(
            property,
            &field.meta,
            &format!("{context}: field `{}`", field.name),
        )?;
    }
    Ok(())
}

/// Rewrites `#/$defs/X` to `{prefix}X` throughout `value`, in place.
///
/// Recurses through objects and arrays, and only touches string values under a
/// `$ref` key — a `description` that happens to mention `#/$defs/` is left
/// alone. Refs that do not start with `#/$defs/` (absolute URLs, refs already
/// relocated) are untouched, which makes this idempotent.
///
/// Termination: this walks the finished `Value` tree, not the type graph.
/// `to_json_schema` reserves a `$defs` slot before recursing into struct
/// fields, so recursive types produce a finite tree containing a `$ref` back to
/// themselves, not an infinite one.
pub fn relocate_refs(value: &mut Value, prefix: &str) {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(target)) = map.get_mut("$ref")
                && let Some(name) = target.strip_prefix(DEFS_PREFIX)
            {
                *target = format!("{prefix}{name}");
            }
            for child in map.values_mut() {
                relocate_refs(child, prefix);
            }
        }
        Value::Array(items) => {
            for item in items {
                relocate_refs(item, prefix);
            }
        }
        _ => {}
    }
}

/// Non-`x-` `meta` keys that map onto a schema or operation object verbatim.
///
/// Everything else without an `x-` prefix is rejected: a typo'd `exmaple` that
/// silently vanishes is how these documents rot, and the failure is invisible
/// until someone reads the generated spec and believes it.
const RECOGNISED_META_KEYS: &[&str] = &[
    // Annotation keywords
    "example",
    "examples",
    "deprecated",
    "tags",
    // JSON Schema constraint keywords
    "minimum",
    "maximum",
    "minLength",
    "maxLength",
    "pattern",
    "enum",
];

/// Copies `meta` onto `target`, which must be a JSON object.
///
/// - Keys starting with `x-` are copied verbatim (both formats allow arbitrary
///   specification extensions).
/// - Keys in [`RECOGNISED_META_KEYS`] are copied verbatim.
/// - Anything else is a hard error naming `context`, so a mistake surfaces at
///   generation time rather than as a quietly missing field.
///
/// Existing keys on `target` are overwritten: `meta` is an explicit author
/// annotation and should win over an inferred default.
pub fn apply_meta(target: &mut Value, meta: &MetaMap, context: &str) -> Result<()> {
    if meta.is_empty() {
        return Ok(());
    }

    let Value::Object(map) = target else {
        bail!("{context}: cannot apply meta to a non-object JSON value");
    };

    for (key, value) in &meta.0 {
        if key.starts_with("x-") || RECOGNISED_META_KEYS.contains(&key.as_str()) {
            map.insert(key.clone(), value.clone());
            continue;
        }
        bail!(
            "{context}: unrecognised meta key `{key}`. Prefix it with `x-` to emit it as a \
             specification extension, or use one of the mapped keys: {}",
            RECOGNISED_META_KEYS.join(", ")
        );
    }

    Ok(())
}

/// Collects every `$ref` target string in `value`.
///
/// Used by emitter tests to prove that a finished document has no dangling
/// references and no surviving `$defs`.
pub fn collect_refs(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(target)) = map.get("$ref") {
                out.push(target.clone());
            }
            for child in map.values() {
                collect_refs(child, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_refs(item, out);
            }
        }
        _ => {}
    }
}

/// Object schema over a `Vec<Field>` with refs already relocated.
///
/// Convenience for emitters building envelope payloads (error objects, message
/// wrappers) that are not an endpoint's own parameters or returns.
pub fn object_schema_for_fields(
    fields: &[crate::model::types::Field],
    registry: &TypeRegistry,
) -> Result<Value> {
    let mut defs = BTreeMap::new();
    let mut schema = fields_to_object_schema(fields, registry, &mut defs)?;
    relocate_refs(&mut schema, COMPONENTS_SCHEMAS_PREFIX);
    Ok(schema)
}

/// Builds a JSON object from `(key, value)` pairs, skipping `None` values.
///
/// Emitters assemble a lot of optional fields; this keeps that readable without
/// a builder type.
pub fn json_object(entries: impl IntoIterator<Item = (String, Option<Value>)>) -> Value {
    let mut map = Map::new();
    for (key, value) in entries {
        if let Some(value) = value {
            map.insert(key, value);
        }
    }
    Value::Object(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::types::{EnumVariant, Field, Type};
    use serde_json::json;

    fn registry_with(types: &[Type]) -> TypeRegistry {
        let mut registry = TypeRegistry::new();
        registry.add_all(types.iter());
        registry
    }

    fn user_struct() -> Type {
        Type::struct_(
            "User",
            vec![
                Field::new("id", Type::Int64),
                Field::new("name", Type::String),
            ],
        )
    }

    /// A struct that contains itself, to prove the rewrite terminates.
    fn recursive_node() -> Type {
        Type::struct_(
            "Node",
            vec![
                Field::new("value", Type::String),
                Field::new(
                    "children",
                    Type::Vec(Box::new(Type::StructRef("Node".into()))),
                ),
            ],
        )
    }

    #[test]
    fn relocate_refs_rewrites_nested_and_array_positions() {
        let mut value = json!({
            "type": "object",
            "properties": {
                "items": {
                    "type": "array",
                    "items": { "anyOf": [ { "$ref": "#/$defs/User" }, { "type": "null" } ] }
                }
            }
        });

        relocate_refs(&mut value, COMPONENTS_SCHEMAS_PREFIX);

        let target = &value["properties"]["items"]["items"]["anyOf"][0]["$ref"];
        assert_eq!(target, "#/components/schemas/User");
    }

    #[test]
    fn relocate_refs_leaves_non_defs_refs_and_is_idempotent() {
        let mut value = json!({
            "a": { "$ref": "#/components/schemas/Already" },
            "b": { "$ref": "https://example.com/schema.json" },
            "c": { "$ref": "#/$defs/Moves" },
        });

        relocate_refs(&mut value, COMPONENTS_SCHEMAS_PREFIX);
        let once = value.clone();
        relocate_refs(&mut value, COMPONENTS_SCHEMAS_PREFIX);

        assert_eq!(value, once, "relocation must be idempotent");
        assert_eq!(value["a"]["$ref"], "#/components/schemas/Already");
        assert_eq!(value["b"]["$ref"], "https://example.com/schema.json");
        assert_eq!(value["c"]["$ref"], "#/components/schemas/Moves");
    }

    #[test]
    fn relocate_refs_ignores_ref_like_strings_outside_ref_keys() {
        let mut value = json!({ "description": "see #/$defs/User for details" });
        relocate_refs(&mut value, COMPONENTS_SCHEMAS_PREFIX);
        assert_eq!(value["description"], "see #/$defs/User for details");
    }

    #[test]
    fn collect_shares_definitions_across_endpoints() {
        let registry = registry_with(&[user_struct()]);
        let endpoints = vec![
            EndpointSchema::new(
                "GetUser",
                1,
                vec![Field::new("id", Type::Int64)],
                vec![Field::new("user", Type::StructRef("User".into()))],
            ),
            EndpointSchema::new(
                "ListUsers",
                2,
                vec![],
                vec![Field::new(
                    "users",
                    Type::Vec(Box::new(Type::StructRef("User".into()))),
                )],
            ),
        ];

        let components = SchemaComponents::collect(&endpoints, &registry).unwrap();

        // One shared definition, not one per endpoint.
        assert_eq!(components.schemas.keys().collect::<Vec<_>>(), vec!["User"]);
    }

    #[test]
    fn collect_relocates_refs_inside_components() {
        let registry = registry_with(&[recursive_node()]);
        let endpoints = vec![EndpointSchema::new(
            "GetTree",
            1,
            vec![],
            vec![Field::new("root", Type::StructRef("Node".into()))],
        )];

        let components = SchemaComponents::collect(&endpoints, &registry).unwrap();

        let mut refs = vec![];
        for schema in components.schemas.values() {
            collect_refs(schema, &mut refs);
        }
        assert!(!refs.is_empty(), "recursive struct should self-reference");
        for target in &refs {
            assert!(
                target.starts_with(COMPONENTS_SCHEMAS_PREFIX),
                "ref {target} was not relocated"
            );
        }
    }

    #[test]
    fn collect_terminates_on_recursive_structs() {
        let registry = registry_with(&[recursive_node()]);
        let endpoints = vec![EndpointSchema::new(
            "GetTree",
            1,
            vec![],
            vec![Field::new("root", Type::StructRef("Node".into()))],
        )];

        // The assertion is that this returns at all.
        let components = SchemaComponents::collect(&endpoints, &registry).unwrap();
        assert!(components.schemas.contains_key("Node"));
    }

    #[test]
    fn collect_resolves_enum_refs() {
        let role = Type::Enum {
            name: "UserRole".into(),
            variants: vec![
                EnumVariant::new_with_description("Admin", "Platform admin".to_string(), 0),
                EnumVariant::new_with_description("User", "Regular user".to_string(), 1),
            ],
        };
        let registry = registry_with(&[role]);
        let endpoints = vec![EndpointSchema::new(
            "GetRole",
            1,
            vec![],
            vec![Field::new(
                "role",
                Type::EnumRef {
                    name: "UserRole".into(),
                    prefixed_name: false,
                },
            )],
        )];

        let components = SchemaComponents::collect(&endpoints, &registry).unwrap();
        assert!(
            components.schemas.contains_key("UserRole"),
            "got {:?}",
            components.schemas.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn collect_reports_the_endpoint_for_a_dangling_ref() {
        let registry = TypeRegistry::new();
        let endpoints = vec![EndpointSchema::new(
            "GetGhost",
            1,
            vec![],
            vec![Field::new("ghost", Type::StructRef("Missing".into()))],
        )];

        let err = SchemaComponents::collect(&endpoints, &registry)
            .unwrap_err()
            .to_string();
        assert!(err.contains("GetGhost"), "{err}");
        assert!(err.contains("Missing"), "{err}");
    }

    #[test]
    fn request_and_response_schemas_reference_shared_components() {
        let registry = registry_with(&[user_struct()]);
        let endpoint = EndpointSchema::new(
            "GetUser",
            1,
            vec![Field::new("id", Type::Int64)],
            vec![Field::new("user", Type::StructRef("User".into()))],
        );
        let components =
            SchemaComponents::collect(std::slice::from_ref(&endpoint), &registry).unwrap();

        let response = components.response_schema(&endpoint, &registry).unwrap();

        let mut refs = vec![];
        collect_refs(&response, &mut refs);
        assert_eq!(refs, vec!["#/components/schemas/User"]);

        let request = components.request_schema(&endpoint, &registry).unwrap();
        assert_eq!(request["required"], json!(["id"]));
    }

    #[test]
    fn optional_fields_are_not_required() {
        let registry = TypeRegistry::new();
        let endpoint = EndpointSchema::new(
            "Search",
            1,
            vec![
                Field::new("query", Type::String),
                Field::new("cursor", Type::Optional(Box::new(Type::String))),
            ],
            vec![],
        );
        let components =
            SchemaComponents::collect(std::slice::from_ref(&endpoint), &registry).unwrap();

        let request = components.request_schema(&endpoint, &registry).unwrap();
        assert_eq!(request["required"], json!(["query"]));
        assert!(request["properties"]["cursor"].is_object());
    }

    #[test]
    fn apply_meta_copies_extensions_and_recognised_keys() {
        let mut target = json!({ "type": "string" });
        let mut meta = MetaMap::default();
        meta.insert("x-internal-id", json!(42));
        meta.insert("example", json!("hello"));
        meta.insert("deprecated", json!(true));

        apply_meta(&mut target, &meta, "Endpoint Foo").unwrap();

        assert_eq!(target["x-internal-id"], 42);
        assert_eq!(target["example"], "hello");
        assert_eq!(target["deprecated"], true);
        assert_eq!(target["type"], "string");
    }

    #[test]
    fn apply_meta_rejects_unrecognised_keys_with_context() {
        let mut target = json!({ "type": "string" });
        let mut meta = MetaMap::default();
        // A plausible typo of `example`.
        meta.insert("exmaple", json!("hello"));

        let err = apply_meta(&mut target, &meta, "endpoint GetUser field id")
            .unwrap_err()
            .to_string();

        assert!(err.contains("endpoint GetUser field id"), "{err}");
        assert!(err.contains("exmaple"), "{err}");
        // The message should tell the author how to fix it.
        assert!(err.contains("x-"), "{err}");
    }

    #[test]
    fn field_meta_lands_on_the_property() {
        let registry = TypeRegistry::new();
        let mut field = Field::new("cursor", Type::String);
        field.meta.insert("example", json!("abc123"));
        field.meta.insert("x-opaque", json!(true));

        let endpoint = EndpointSchema::new("Search", 1, vec![field], vec![]);
        let components =
            SchemaComponents::collect(std::slice::from_ref(&endpoint), &registry).unwrap();

        let request = components.request_schema(&endpoint, &registry).unwrap();
        assert_eq!(request["properties"]["cursor"]["example"], "abc123");
        assert_eq!(request["properties"]["cursor"]["x-opaque"], true);
    }

    #[test]
    fn field_meta_on_a_bare_ref_is_rejected() {
        // Sibling keys next to $ref are ignored by tooling; failing loudly beats
        // emitting a document that looks annotated but isn't.
        let registry = registry_with(&[user_struct()]);
        let mut field = Field::new("user", Type::StructRef("User".into()));
        field.meta.insert("example", json!({ "id": 1 }));

        let endpoint = EndpointSchema::new("GetUser", 1, vec![], vec![field]);
        let components =
            SchemaComponents::collect(std::slice::from_ref(&endpoint), &registry).unwrap();

        let err = components
            .response_schema(&endpoint, &registry)
            .unwrap_err()
            .to_string();
        assert!(err.contains("GetUser"), "{err}");
        assert!(err.contains("user"), "{err}");
        assert!(err.contains("$ref"), "{err}");
    }

    #[test]
    fn apply_meta_is_a_noop_when_empty() {
        let mut target = json!({ "type": "string" });
        let before = target.clone();
        apply_meta(&mut target, &MetaMap::default(), "ctx").unwrap();
        assert_eq!(target, before);
    }

    #[test]
    fn mcp_schemas_stay_self_contained() {
        // The document emitters share definitions; MCP tool schemas must not.
        // Consumers rely on each tool schema standing alone, so this guards
        // against a future refactor "unifying" the two paths.
        let registry = registry_with(&[user_struct()]);
        let endpoint = EndpointSchema::new(
            "GetUser",
            1,
            vec![],
            vec![Field::new("user", Type::StructRef("User".into()))],
        );

        let mcp = endpoint.to_mcp_output_schema(&registry).unwrap();

        assert!(
            mcp.get("$defs").is_some(),
            "MCP output schema must carry its own $defs"
        );
        let mut refs = vec![];
        collect_refs(&mcp, &mut refs);
        assert!(
            refs.iter().all(|r| r.starts_with("#/$defs/")),
            "MCP schemas must keep $defs refs, got {refs:?}"
        );
    }
}
