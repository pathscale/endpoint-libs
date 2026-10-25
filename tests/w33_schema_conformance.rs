use endpoint_libs::model::{EndpointSchema, EnumVariant, Field, Type, TypeRegistry};
use serde_json::Value;
use std::collections::BTreeMap;

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/w33_schema_conformance.json"))
        .expect("schema conformance fixture must be valid JSON")
}

fn public_shapes() -> (EndpointSchema, TypeRegistry) {
    // These use public DTO field names and types only. Values are synthetic.
    let receipt = Type::struct_(
        "PaymentJobReceipt",
        vec![
            Field::new("outcome", Type::String),
            Field::new("amount_base_units", Type::String),
            Field::new("asset", Type::String),
            Field::new("recipient", Type::String),
            Field::new("transaction_signature", Type::String),
            Field::new("recorded_at_unix", Type::Int64),
        ],
    );
    let merchant = Type::struct_(
        "MerchantSummary",
        vec![
            Field::new("merchant_public_id", Type::String),
            Field::new("username", Type::String),
            Field::new("status", Type::String),
            Field::new("app_count", Type::Int32),
            Field::new("key_count", Type::Int32),
            Field::new("revision", Type::String),
        ],
    );
    let health_sample = Type::struct_(
        "HealthSample",
        vec![
            Field::new("sampled_at_unix_ms", Type::Int64),
            Field::new("latest_slot", Type::optional(Type::Int64)),
        ],
    );
    let action = Type::enum_(
        "AdminAuditAction",
        vec![EnumVariant::new("GetAdminState", 1), EnumVariant::new("SetWalletProvider", 2)],
    );
    let endpoint = EndpointSchema::new(
        "SchemaVector",
        1,
        vec![],
        vec![
            Field::new("receipt", Type::struct_ref("PaymentJobReceipt")),
            Field::new("merchant", Type::struct_ref("MerchantSummary")),
            Field::new("actions", Type::vec(Type::enum_ref("AdminAuditAction", false))),
            Field::new("samples", Type::struct_table("HealthSample")),
            Field::new("amount", Type::BlockchainDecimal),
            Field::new("unsigned_count", Type::UInt32),
            Field::new("signed_delta", Type::Int32),
            Field::new("optional_cursor", Type::optional(Type::Int64)),
        ],
    )
    .with_stream_response_type(Type::struct_ref("HealthSample"));
    let mut registry = TypeRegistry::new();
    registry.add(&receipt);
    registry.add(&merchant);
    registry.add(&health_sample);
    registry.add(&action);
    (endpoint, registry)
}

#[test]
fn public_dto_shapes_resolve_and_match_schema_fixture() {
    let expected = fixture();
    let (endpoint, registry) = public_shapes();
    let schema = endpoint.to_mcp_output_schema(&registry).unwrap();

    for (path, want) in expected["schema_expectations"].as_object().unwrap() {
        assert_eq!(schema.pointer(path).unwrap_or(&Value::Null), want, "{path}");
    }

    let mut stream_defs = BTreeMap::new();
    let stream_schema = endpoint
        .stream_response
        .as_ref()
        .unwrap()
        .to_json_schema(&registry, &mut stream_defs)
        .unwrap();
    assert_eq!(stream_schema, expected["stream_schema"]["root"]);
    assert_eq!(stream_defs["HealthSample"]["properties"]["latestSlot"]["type"], "integer");
    assert!(!stream_defs["HealthSample"]["required"].as_array().unwrap().contains(&Value::String("latestSlot".into())));
}

#[test]
fn unresolved_nested_references_fail_schema_conversion() {
    let registry = TypeRegistry::new();
    for ty in [
        Type::struct_ref("MissingShape"),
        Type::enum_ref("MissingAction", false),
        Type::struct_table("MissingRow"),
    ] {
        let mut defs = BTreeMap::new();
        assert!(ty.to_json_schema(&registry, &mut defs).is_err());
    }
}
