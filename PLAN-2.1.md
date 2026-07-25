# endpoint-libs 2.1 / endpointgen 2.1 — OpenAPI + AsyncAPI emission (brief for Claude Code)

Repos: `~/code/endpoint-libs` (2.0.1) and `~/code/endpointgen` (1.10.1 — *not* in
lockstep; see the version-skew note in §1). Goal of 2.1: teach the RON pipeline to emit **OpenAPI 3.1** and
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
- Every phase lands green: `cargo all-features clippy --all-targets -- -D warnings` and
  `cargo all-features test` (the CI matrix — see §9), and the generated documents
  validate against a real spec validator (§7).

---

## 1. What 2.0 already gave you (do not redo this work)

These were the 2.0 groundwork this release depends on. All five were checked against the
tree on 2026-07-26 — the status column is fact, not aspiration, so don't re-verify the
✅ rows. One did not land:

| Invariant | Where | Status |
|---|---|---|
| Generated schemas are JSON-deserialized at runtime, not struct literals | `endpointgen/src/rust.rs:525` emits `serde_json::from_str(schema)` | ✅ (line 525, not 513) |
| New model fields carry `#[serde(default)]` | `model/endpoint.rs:26,30,34,41,109,111` | ✅ |
| `Type`, `Field`, `EnumVariant`, `EndpointSchema` are `#[non_exhaustive]` | `model/types.rs:62,115,154`, `model/endpoint.rs:11,105` | ✅ |
| `Field.meta` and `EndpointSchema.meta` round-trip unknown keys | `model/types.rs:76`, `model/endpoint.rs:47` (`MetaMap`) | ✅ |
| endpointgen has `--check` (regenerate → diff → non-zero on drift) | 2.0 lockstep item 3 | ❌ **Does not exist.** No `--check` in `endpointgen/src/main.rs`. Phase 4 must build it. |

**Version skew to fix before §9's consumer-corpus step.** The two repos are not in lockstep
and the six backends are pinned to an older generator than the one in the tree:

- `endpoint-libs` is 2.0.1, `endpointgen` is 1.10.1 (not 2.0.x as this plan assumed).
- The installed `endpoint-gen` binary is **1.9.0**, and all six consumers' `config/version.toml`
  declare `[binary] 1.9.0` / `[libs] 1.9.1` — self-consistent with that binary, so nothing is
  broken today.
- But their `Cargo.toml`/`Cargo.lock` build against **endpoint-libs 2.0.0**. So the declared
  `[libs]` version is already a lie, and the moment anyone installs endpointgen 1.10.1,
  `check_compatibility` compares its `^2.0` requirement against the declared `1.9.1` and
  **refuses to run in all six repos**.

Bumping `[binary]`/`[libs]` and regenerating six production backends is a coordinated
migration with real diffs in deployed services — it is not a prerequisite for writing the
emitters. Do Phases 1–3 against a fixture RON, and treat the consumer-corpus check as its
own scheduled piece of work.

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

New file `endpointgen/src/openapi.rs`, modelled on
`endpointgen/src/docs.rs::gen_mcp_tools_json` (`docs.rs:275`) — same registry
construction, same `docs/` output directory. It differs in one way: `gen_mcp_tools_json`
writes one file per service, while this walks every service to *accumulate* a single
merged document.

```rust
pub fn gen_openapi(data: &Data) -> eyre::Result<()>;
// writes ONE merged docs/openapi.json (and .yaml if the yaml feature is on)
// covering all services, grouped by per-service tags — matching the committed
// api.support.cafe docs/openapi.yaml. (Per-service splitting, if ever needed,
// is a --flag later; the merged document is the canonical artifact.)
```

Call it from `main.rs` next to the existing emitters (`main.rs:83-89`).

### 3.1 The modelling decision you must make first

A WS RPC method has no URL. OpenAPI needs paths. **Synthesize them per-service** — this
matches the golden fixture (§3.5), which is where these conventions come from:

```
POST /{serviceName}/{endpoint_snake_name}    e.g. POST /adminApi/delete_app
  operationId: {serviceName}_{endpoint_snake_name}   e.g. adminApi_delete_app
  tags: [{serviceName}]
  requestBody:  application/json  → object schema over `parameters`
  responses:
    200:     application/json → object schema over `returns`
    default: $ref to the shared error envelope → the endpoint's error catalog (§3.3)
```

Rationale: endpoint names are only conventionally unique across services — the RON
namespace is `(service_name, service_id)`, and the path convention must mirror it. A
flat `/rpc/{Name}` scheme collides the moment documents are merged or two services
reuse a name, and the `/rpc/` segment carries no information.

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
| `name` | path segment (snake_case) + `operationId` (`{service}_{snake_name}`) |
| `code` | `x-endpoint-code` extension |
| `description` | `summary` (first line) + `description` (full) |
| `parameters` | `requestBody` object schema, non-`Optional` → `required` |
| `returns` | `200` response schema |
| `stream_response` | `x-stream-response` extension + a note in `description` |
| `roles` | `security` + `x-roles` (§3.2) |
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
`"x-roles": ["Admin", "User"]` from `schema.roles` — `x-roles`, not `x-required-roles`,
matching the vendor-extension names in the golden fixture (§3.5). OpenAPI has no native
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

### 3.5 The golden fixture

`endpointgen/tests/fixtures/api_support_cafe_openapi.golden.yaml` — 1335 lines, 26
operations across 7 services. This is the hand-written projection that established the
§3.1 conventions, moved out of `api.support.cafe/docs/openapi.yaml` (commit `3b50c57`)
and into this repo, because a checked-in spec living next to generator output in a
service repo is a competing source of truth. See the README beside it for full
provenance.

Two things to know before you diff against it:

- **It is a shape reference, not a byte-exact expectation.** It was written by hand from
  an older RON state, so operation-for-operation equality is not the goal. Assert the
  path scheme, `operationId` scheme, tagging, and vendor extensions; do not assert
  whole-document equality until the emitter has been trusted once and the fixture
  regenerated from it.
- **Vendor-extension names: fixture and plan now agree.** §3.1/§3.2 were revised
  (2026-07-25 review) to adopt the fixture's conventions — per-service paths
  (`/{serviceName}/{endpoint_snake_name}`), `operationId` = `{service}_{snake_name}`,
  and `x-roles` (not `x-required-roles`). Rationale in §3.1: the RON namespace is
  `(service_name, service_id)`, and a flat `/rpc/{Name}` scheme collides across
  services while the `/rpc/` segment carries no information. If any further
  divergence between plan and fixture turns up during implementation, the
  *fixture's* convention wins unless it's demonstrably wrong — it is the deployed
  artifact.

---

## 4. Phase 3 — AsyncAPI 3.0 emitter (endpointgen)

New file `endpointgen/src/asyncapi.rs`. **This is the document that actually describes
your protocol** — the OpenAPI one is a tooling projection, this one is the truth.

```rust
pub fn gen_asyncapi(data: &Data) -> eyre::Result<()>;
// writes ONE merged docs/asyncapi.json covering all services, one channel each
```

**One document, matching §3's merged OpenAPI.** This was per-service (`docs/{service}_asyncapi.json`)
in the original draft, written before §3 moved to a single merged file. Leaving it
per-service would make §4.2's acceptance test impossible to satisfy: a merged OpenAPI
document's `components.schemas` holds every service's types, while a per-service AsyncAPI
document holds one service's, so `assert_eq!` between them could never pass. Two documents
also describe the protocol more honestly as one document, since the services share a
transport and are distinguished by the `method` code inside the envelope, not by endpoint.

### 4.1 Channel and operation model

AsyncAPI 3.0 separates channels (where messages flow), operations (send/receive), and
messages (payload shapes). Map as:

- **One channel per service**, each with `address: "/"` and a `ws` binding recording
  the subprotocol used for auth. Name them after the service so the channel set mirrors
  the OpenAPI tag set.
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
- **Build `--check` from scratch** — regenerate → diff → non-zero exit — and cover the
  new documents with it. §1 lists it as a 2.0 deliverable that was never implemented, so
  this is net-new work, not an extension. This is the dropshot `dropshot-api-manager`
  discipline; the whole value of a committed spec is that CI proves it matches the RON.
  Note that none of the six consumer repos' CI regenerates and diffs today, so `--check`
  only pays off once it is wired into *their* workflows, not just endpointgen's.
- `docs/openapi-README.md`: what each document is, the synthetic-path warning (§3.1),
  and the three consumption recipes in §6.

---

## 6. What this unlocks (validate at least the first one)

1. **OpenAPI → MCP bridging**: point `rmcp-openapi`
   (`gitlab.com/lx-industries/rmcp-openapi`) at the emitted document and confirm the
   tool list matches endpoint-libs' own `tools/list` output. Since §3 emits one merged
   document, compare against the union of every service's `tools/list`, or filter the
   document by tag to check a service at a time.
   **This is the highest-value check in the release** — a mismatch means the hand-rolled
   MCP metadata and the emitted spec disagree, and one of them is lying to an agent.

   Useful sanity anchor: for api.support.cafe those tool lists total **26** operations
   across 7 services, which is exactly the operation count in the §3.5 fixture.
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
- **No runtime behaviour change.** endpoint-libs serves the same frames; the emitters are
  build-time artifacts, and the only addition is `src/model/api_document.rs`.

  The one `src/libs/` change in 2.1 is a *deletion*: `src/libs/ws/wtx/` (see §8's
  breaking-change carve-out). It changes no runtime behaviour either, because that code
  could not be compiled into a build in the first place.
- **No REST/HTTP adapter.** The synthetic paths are for tooling, not for serving. If a
  real REST surface is ever wanted, that is its own release with its own plan.
- **No OpenRPC emitter yet.** It is arguably the best-fitting standard (MCP is
  JSON-RPC, and so is half your protocol) but Rust/ecosystem tooling for it is thin —
  see `rpc-crate-survey.md` §A.5. Revisit if OpenRPC tooling matures; the
  `SchemaComponents` plumbing in §2 is deliberately emitter-agnostic so adding it later
  is one more file.
- **No breaking changes to anything a consumer can compile against.** If one seems
  necessary, it belongs in 3.0 and needs its own plan — not a quiet bump here.

  One carve-out, already taken: 2.1 removes the `ws-wtx` / `ws-wtx-http2` features and
  the wtx backend. That is marked breaking and is not one, in the sense the rule cares
  about — enabling `ws-wtx` on 2.0.x was a hard compile error (verified:
  `cargo check --no-default-features --features ws-wtx` fails), so no working consumer
  existed to break. See the CHANGELOG. The rule protects consumers who can build; it is
  not a reason to carry code that never built. Anything that *does* compile for a
  consumer today still falls under the rule.

## 9. Order of work & verification loop

1. Phase 1 (`api_document.rs` + `relocate_refs` + meta passthrough) → unit tests.
2. Phase 2 (OpenAPI emitter) → validator + recursive/enum/optional round-trip test.
3. Phase 3 (AsyncAPI emitter) → validator + shared-components equality test.
4. Phase 4 (build wiring + `--check` extension + docs).
5. Consumption check §6.1 (`rmcp-openapi` tool-list parity) — treat a mismatch as a
   release blocker, not a curiosity.

After each phase, in both repos:

```bash
cargo all-features clippy --all-targets -- -D warnings
cargo all-features test
cargo fmt --all -- --check
```

`cargo all-features` (the `cargo-all-features` subcommand, installed in CI) walks the
feature matrix honouring the `denylist` in `[package.metadata.cargo-all-features]`. It is
what `.github/workflows/rust.yml:73` runs, so it is the invocation that decides whether CI
is green — prefer it.

**Plain `cargo clippy --all-features` also works again as of this release.** It used to be
permanently red: enabling every feature pulled in the deprecated `ws-wtx` / `ws-wtx-http2`
backends, which were mutually exclusive with `ws` and fired `compile_error!`s by design,
*plus* genuine rot (`src/libs/ws/wtx/upgrader.rs` still implemented the pre-2.0
`WsUpgrader::upgrade` instead of `upgrade_stream`, giving `E0046`/`E0407`). 2.1 removes
that backend outright — see the CHANGELOG — so `--all-features` now compiles clean with
no errors and no warnings. Both invocations are valid; they simply cover different things
(`--all-features` is one union build, `all-features` is the combinatorial matrix).

Also: `endpointgen --check` does not exist yet (§1 lists it as a 2.0 deliverable; it was not
built). Phase 4 must implement it, not extend it. Until it exists, verify emitters against a
fixture RON rather than a consumer repo — see the version-skew note in §1.
