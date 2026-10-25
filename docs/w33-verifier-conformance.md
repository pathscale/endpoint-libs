# W33 schema and verifier conformance handoff

## What endpoint-libs establishes

`TypeRegistry` indexes inline `Struct` and `Enum` definitions, including definitions nested under vectors and optionals. `Type::to_json_schema` resolves registered `StructRef`, `EnumRef`, and `StructTable` definitions, emits `$defs` references, describes vectors recursively, expresses `UInt32` and `Int32` bounds, and represents `BlockchainDecimal` as a string. A missing reference returns an error. Struct fields are camel-cased; required fields are listed and `Optional` fields are omitted from `required`.

There are two nullability forms: an object field with `Optional<T>` is optional but its property schema is only `T`; a standalone `Optional<T>` emits `anyOf` with null. The object-field conversion therefore documents omission but does not accept explicit null under standard JSON Schema interpretation. The string amount type records no decimal syntax, scale, positivity, asset precision, or base-unit rule.

The fixture's Pays `PaymentJobReceipt`, merchant `MerchantSummary`, and Karen `AdminAuditAction` are public shape examples with synthetic data. They exercise public identifiers, integer counters, enum values, and string-encoded amount fields without copying payloads.

## EndpointRunner gap and upstream patch specification

The inspected `EndpointRunner/src/runner.rs` response `check` validates returned top-level required fields, accepts omission for top-level Optional fields, then delegates each present value to `shape_matches`. That matcher accepts unresolved `StructRef`/`EnumRef` through its fallback; it checks vectors and struct tables only as arrays, structs only as objects, enums as any number or string, and integer types as any JSON integer. Thus invalid nested fields, invalid enum members, wrong vector/table items, negative unsigned values, and out-of-range Int32 values can pass. Optional null is accepted by the runner although object-field JSON Schema output does not allow it. Response checking only handles the immediate response frame; streamed update payloads do not pass through this check.

Assign an upstream EndpointRunner owner to implement a recursive response validator over the loaded endpoint type registry. Resolve every reference or fail closed at manifest load; validate object requiredness, field types, unknown keys according to the schema contract, enum values, every vector/table item, integer signedness/ranges, string types, and Optional omission/null semantics consistently with the published schema. Validate every stream update frame with its declared stream response type and report method, field path, and frame kind on failure. Keep malformed raw-request admission as a separate narrowly scoped transport test, since the current client validates request parameters before sending.

EndpointRunner also needs separate owner work for role evidence across every selected target, default-target limitations, and stable service-qualified capability identity. These are not solved by schema vectors and must not be counted as successful role or business E2E coverage.

No endpoint-libs validator API is proposed. If upstream chooses to share a validator, request a separately reviewed API such as `pub fn validate_json_value(ty: &Type, value: &serde_json::Value, registry: &TypeRegistry, mode: ValidationMode) -> Result<(), ValidationError>`; this would add a public API and may require a minor endpoint-libs release, then `honey_id-types` must bump/release after endpoint-libs before dependent backends bump both together. Current backend compatibility must be checked against the existing string-encoded amount and persistence contracts; preserve u64 base-unit arithmetic and per-asset decimals at most 9.

## Verification handoff

The integration test checks schema conversion only; its good/bad JSON entries are conformance vectors, not proof of runtime validation. A future integrator should inspect emitted vectors against an approved prebuilt test artifact. Rust test execution and the full check-chain require a separately authorized build-capable phase. Preserve the plan's separate static mentions, successful runtime endpoint evidence, role evidence, and finalized business E2E gates. No check-chain or runtime execution is claimed here.
