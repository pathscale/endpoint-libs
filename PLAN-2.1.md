# endpoint-libs 2.1 / endpointgen 2.1 — OpenAPI + AsyncAPI emission (brief for Claude Code)

Repos: `~/code/endpoint-libs` (2.0.x after `PLAN-2.0.md` lands) and `~/code/endpointgen`
(lockstep). Goal of 2.1: teach the RON pipeline to emit **OpenAPI 3.1** and
**AsyncAPI 3.0** documents as *additional artifacts* alongside the Rust/docs/MCP output
it already produces, so the project collects the standard-format dividends (third-party
client SDKs, hosted docs, spec-driven fuzzing, OpenAPI→MCP bridging) without giving up
the RON source of truth, the role/error model, or the deployed WS protocol.

**Read first:** `~/code/iaai-27/rpc-crate-survey.md` §A — the research this release
comes from, including why the alternative (adopt an OpenAPI-first framework) was
rejected and why the codegen arrow has to keep pointing RON → artifacts.

Ground rules:

- **This is a MINOR release. Nothing here may break a 2.0 consumer.** If you find
  yourself needing a breaking change to `Type`, `Field`, or `EndpointSchema`, stop and
  re-read §1 — the 2.0 Phase 2b groundwork exists precisely so you don't have to.
- The RON stays the single source of truth. These emitters are **outputs**. Never add
  an OpenAPI/AsyncAPI *input* path (that is the "adopt OpenAPI" plan that was
  rejected).
- No new required dependencies in endpoint-libs default features. The emitters live in
  endpointgen, which may take `serde_yaml` (or emit JSON only — see §4.4).
- Every phase lands green: `cargo clippy --all-features`, `cargo test --all-features`,
  and the generated documents validate against a real spec validator (§7).

---

## 1. What 2.0 already gave you (do not redo this work)

Confirm these hold before starting; if any is false, the 2.0 plan did not land as
written and this release will be breaking:

| Invariant | Where | Why 2.1 needs it |
|---|---|---|
| Generated schemas are JSON-deserialized at runtime, not struct literals | `endpointgen/src/rust.rs:513` emits `serde_json::from_str(schema)` | New model fields don't break generated code |
| New model fields carry `#[serde(default)]` | `model/endpoint.rs` | Old committed schema JSON still deserializes |
| `Type`, `Field`, `EnumVariant`, `EndpointSchema` are `#[non_exhaustive]` | `model/types.rs`, `model/endpoint.rs` | New `Type` variants / fields stay additive |
| `Field.meta` and `EndpointSchema.meta` round-trip unknown keys | 2.0 Phase 2b | Per-field examples/constraints/tags land without a model change |
| endpointgen has `--check` (regenerate → diff → non-zero on drift) | 2.0 lockstep item 3 | Committed spec documents can be trusted in CI |

**The single most important existing asset:** `Type::to_json_schema`
(`endpoint-libs/src/model/json_schema.rs:161`) already emits **JSON Schema 2020-12** —
`$defs`, `$ref`, `format: uuid`, `contentEncoding: base64`, `pattern` for blockchain
addresses/hashes, `minimum`/`maximum` for sized ints, `anyOf: [T, null]` for
`Optional`. **OpenAPI 3.1 is a superset of JSON Schema 2020-12**, so these schema
objects drop into an OpenAPI document essentially verbatim. AsyncAPI 3.0 also uses
JSON Schema for payloads. You are not writing a type-to-schema converter — you are
writing two document *envelopes* around an existing one.

The only structural mismatch is location: `to_json_schema` puts shared definitions in
`#/$defs/X`, while OpenAPI wants `#/components/schemas/X` and AsyncAPI wants
`#/components/schemas/X` too. That is a mechanical `$ref` rewrite (§2.2).

---

## 2. Phase 1 — shared document plumbing (endpoint-libs, additive)

New module `src/model/api_document.rs`, exported from `model`. This lives in
endpoint-libs (not endpointgen) because the MCP server already needs the same
registry-walking logic at startup, and both emitters plus any future OpenRPC emitter
should share one implementation.

### 2.1 Collect every shared definition once

```rust
/// Walks all endpoints in a service, emitting each referenced Struct/Enum exactly
/// once into a component map. Mirrors what `to_mcp_input_schema` does per-endpoint,
/// but hoisted to document scope so `$ref`s are shared across operations.
pub struct SchemaComponents {
    pub schemas: BTreeMap<String, serde_json::Value>,
}

impl SchemaComponents {
    pub fn collect(
        endpoints: &[EndpointSchema],
        registry: &TypeRegistry,
    ) -> Result<Self>;
}
```

Implementation note: reuse `Type::to_json_schema` with a shared `defs` map across all
endpoints instead of a fresh `BTreeMap` per endpoint (which is what
`to_mcp_input_schema`/`to_mcp_output_schema` do today). Do **not** change those two
methods — MCP tool schemas are self-contained by design and consumers depend on that.

### 2.2 `$ref` relocation

```rust
/// Rewrites `#/$defs/X` → `#/components/schemas/X` throughout a schema value.
/// Recursive over objects and arrays; only touches string values under a `$ref` key.
pub fn relocate_refs(value: &mut serde_json::Value, prefix: &str);
```

Unit-test this against a deeply nested case (Vec<Optional<StructRef>> inside a Struct
field) and a recursive struct — `to_json_schema` reserves a slot to terminate
recursion (`json_schema.rs:211`), so the rewrite must not loop.

### 2.3 The `meta` passthrough

Any key in `Field.meta` / `EndpointSchema.meta` that starts with `x-` is copied
verbatim onto the corresponding schema/operation object (both OpenAPI and AsyncAPI
allow arbitrary `x-` extensions). Recognised non-`x-` keys are mapped explicitly:
`example`, `examples`, `deprecated`, `tags`, plus the JSON Schema constraint keywords
(`minimum`, `maximum`, `minLength`, `maxLength`, `pattern`, `enum`). Unrecognised
non-`x-` keys are a **hard error** with the endpoint and field name — silently dropping
metadata is how these documents rot.

**Acceptance:** unit tests only; no emitter yet. `cargo test -p endpoint-libs`.

---

## 3. Phase 2 — OpenAPI 3.1 emitter (endpointgen)

New file `endpointgen/src/openapi.rs`, modelled directly on
`endpointgen/src/docs.rs::gen_mcp_tools_json` (`docs.rs:275`) — same registry
construction, same per-service loop, same `docs/` output directory.

```rust
pub fn gen_openapi(data: &Data) -> eyre::Result<()>;
// writes docs/{service}_openapi.json  (and .yaml if the yaml feature is on)
```

Call it from `main.rs` next to the existing emitters (`main.rs:83-89`).

### 3.1 The modelling decision you must make first

A WS RPC method has no URL. OpenAPI needs paths. **Synthesize them** — do not try to
be clever:

```
POST /rpc/{EndpointName}      operationId: endpointName (camelCase)
  requestBody:  application/json  → object schema over `parameters`
  responses:
    200: application/json → object schema over `returns`
    4xx: application/json → the endpoint's error catalog (§3.3)
```

Document prominently in the generated file's `info.description` **and** in
`docs/openapi-README.md` that this is a *projection for tooling purposes*: the real
transport is a persistent WebSocket carrying `{method, seq, params}` frames, and the
authoritative description of that is the AsyncAPI document (§4). Generating an HTTP
client from this and pointing it at the server will not work. This warning is not
optional — an undocumented synthetic path map is worse than no document, because it
looks usable.

Fields that carry over directly:

| RON / `EndpointSchema` | OpenAPI |
|---|---|
| `name` | `operationId` (camelCase), path segment |
| `code` | `x-endpoint-code` extension |
| `description` | `summary` (first line) + `description` (full) |
| `parameters` | `requestBody` object schema, non-`Optional` → `required` |
| `returns` | `200` response schema |
| `stream_response` | `x-stream-response` extension + a note in `description` |
| `roles` | `security` + `x-required-roles` (§3.2) |
| `errors` | error responses (§3.3) |
| `frontend_facing` (on the element, not the schema) | `x-frontend-facing`; also drives `--public-only` filtering |

### 3.2 Roles → security

Emit one `securitySchemes` entry describing the WS subprotocol auth token:

```json
"securitySchemes": {
  "sessionToken": { "type": "apiKey", "in": "header", "name": "Sec-WebSocket-Protocol",
                    "description": "Auth token passed as WS subprotocol; see AuthController." }
}
```

Each operation gets `"security": [{"sessionToken": []}]` plus
`"x-required-roles": ["Admin", "User"]` from `schema.roles`. OpenAPI has no native
role concept — do not attempt to encode roles as scopes, it misleads generators into
emitting OAuth2 flows that do not exist.

### 3.3 Error catalog → responses

`EndpointSchema.errors` (`Vec<EndpointErrorSchema>`) plus the global error-code catalog
(`endpointgen/src/error_codes.rs`) become response entries. One response object per
distinct HTTP-ish class is enough — the wire protocol has no status codes, so:

- `default` response → the standard error envelope schema (code, message, params),
  with `x-error-codes` listing the codes this endpoint may return, each with its
  description from the catalog.

Do not invent per-code HTTP statuses. The envelope is the contract.

### 3.4 Filtering

`--public-only` (or a config key) emits only `frontend_facing` endpoints, for the
document you would hand to a third party. Default emits everything.

**Acceptance:** the emitted document validates (§7); an endpoint with a recursive
struct, an enum ref, an optional vec, and two error codes round-trips into readable
schemas; `--public-only` drops exactly the non-frontend-facing operations.

---

## 4. Phase 3 — AsyncAPI 3.0 emitter (endpointgen)

New file `endpointgen/src/asyncapi.rs`. **This is the document that actually describes
your protocol** — the OpenAPI one is a tooling projection, this one is the truth.

```rust
pub fn gen_asyncapi(data: &Data) -> eyre::Result<()>;
// writes docs/{service}_asyncapi.json
```

### 4.1 Channel and operation model

AsyncAPI 3.0 separates channels (where messages flow), operations (send/receive), and
messages (payload shapes). Map as:

- **One channel** per service: `ws`, with `address: "/"` and a `ws` binding recording
  the subprotocol used for auth.
- **Two operations**: `sendRequest` (client → server, `action: send`) and
  `receiveResponse` (server → client, `action: receive`).
- **Messages**: `Request`, `Response`, `Error`, and — because they share the socket —
  `McpJsonRpc`. The `Request` payload is the envelope:

```json
{ "type": "object",
  "properties": {
    "method": { "type": "integer", "description": "endpoint code" },
    "seq":    { "type": "integer" },
    "params": { "oneOf": [ /* $ref per endpoint parameter schema */ ] } },
  "required": ["method", "seq", "params"] }
```

Use `oneOf` over the per-endpoint parameter schemas with a `discriminator` on `method`
if the generator you test with supports it; otherwise emit the `oneOf` plus an
`x-method-map` extension mapping code → schema name. Note which you did in the file
header.

### 4.2 Per-endpoint detail

Each endpoint contributes a `components.messages.{Name}Request` /
`{Name}Response` pair with the same descriptions, roles extensions, and error lists as
the OpenAPI operations. Reuse `SchemaComponents` from §2.1 — both documents must
reference **identical** schema objects, and a test should assert that
(`assert_eq!(openapi.components.schemas, asyncapi.components.schemas)`).

### 4.3 The framed_json binding

2.0 defines a length-delimited frame format for non-WS transports
(`u32 BE length | u8 kind | payload`, `PLAN-2.0.md` §4). Record it in the AsyncAPI
document as a second channel entry with a custom `x-framing` extension describing the
byte layout, so a non-Rust peer implementing the local transport has one authoritative
reference. This is the only place that format is machine-readable.

### 4.4 YAML

JSON is mandatory; YAML is nice-to-have for humans. If you add it, put `serde_yaml`
behind an endpointgen feature — do not make it a default dependency for a cosmetic
output.

**Acceptance:** validates against an AsyncAPI 3.0 validator (§7); the shared-components
equality test passes; a hand-written peer can reconstruct the frame layout from
`x-framing` alone.

---

## 5. Phase 4 — wire it into the build and CI

- `main.rs`: call `openapi::gen_openapi` and `asyncapi::gen_asyncapi` after
  `docs::gen_mcp_tools_json` (`main.rs:87`).
- Both documents are **committed artifacts**, like the existing generated Rust/docs.
- Extend the 2.0 `--check` mode to cover them: regenerate → diff → non-zero exit.
  This is the dropshot `dropshot-api-manager` discipline; the whole value of a
  committed spec is that CI proves it matches the RON.
- `docs/openapi-README.md`: what each document is, the synthetic-path warning (§3.1),
  and the three consumption recipes in §6.

---

## 6. What this unlocks (validate at least the first one)

1. **OpenAPI → MCP bridging**: point `rmcp-openapi`
   (`gitlab.com/lx-industries/rmcp-openapi`) at the emitted document and confirm the
   tool list matches endpoint-libs' own `tools/list` output for the same service.
   **This is the highest-value check in the release** — a mismatch means the hand-rolled
   MCP metadata and the emitted spec disagree, and one of them is lying to an agent.
2. **Third-party client SDKs**: `openapi-generator` (any of 50+ languages) against the
   `--public-only` document. Expect the synthetic paths to be wrong for real use —
   that is exactly why §3.1's warning exists; validate that it *generates*, not that
   it *connects*.
3. **Spec-driven fuzzing**: Schemathesis against the document is the interesting one
   long-term, but it needs an HTTP surface the server does not have. Note it as future
   work behind a REST adapter; do not build the adapter here.

---

## 7. Validation tooling

- OpenAPI 3.1: `redocly lint` or the `oas3` Rust crate for a parse check in a test.
  Prefer a real linter in CI over a parse check.
- AsyncAPI 3.0: the official `@asyncapi/cli validate`.
- Both: a test that every `$ref` in the document resolves against
  `components.schemas` (catches the §2.2 rewrite regressing) and that no `$defs` key
  survives anywhere.

---

## 8. Explicit non-goals for 2.1

- **No OpenAPI/AsyncAPI as input.** No spec → Rust codegen, ever, in this direction.
  The RON is the source of truth.
- **No runtime behaviour change.** endpoint-libs serves the same frames; these are
  build-time artifacts. Nothing in `src/libs/` changes except the additive
  `model/api_document.rs`.
- **No REST/HTTP adapter.** The synthetic paths are for tooling, not for serving. If a
  real REST surface is ever wanted, that is its own release with its own plan.
- **No OpenRPC emitter yet.** It is arguably the best-fitting standard (MCP is
  JSON-RPC, and so is half your protocol) but Rust/ecosystem tooling for it is thin —
  see `rpc-crate-survey.md` §A.5. Revisit if OpenRPC tooling matures; the
  `SchemaComponents` plumbing in §2 is deliberately emitter-agnostic so adding it later
  is one more file.
- **No breaking changes.** If one seems necessary, it belongs in 3.0 and needs its own
  plan — not a quiet bump here.

## 9. Order of work & verification loop

1. Phase 1 (`api_document.rs` + `relocate_refs` + meta passthrough) → unit tests.
2. Phase 2 (OpenAPI emitter) → validator + recursive/enum/optional round-trip test.
3. Phase 3 (AsyncAPI emitter) → validator + shared-components equality test.
4. Phase 4 (build wiring + `--check` extension + docs).
5. Consumption check §6.1 (`rmcp-openapi` tool-list parity) — treat a mismatch as a
   release blocker, not a curiosity.

After each phase: `cargo clippy --all-targets --all-features -- -D warnings`,
`cargo test --all-features` in both repos, and `endpointgen --check` clean on a real
service RON (use `api.support.cafe` or `web3.trading-backend` as the corpus).
