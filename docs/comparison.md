# How endpoint-libs compares

An honest look at what else you could use, and when you should. Condensed from the full
survey in `iaai-27/rpc-crate-survey.md` and re-checked against crates.io on 2026-07-26 —
the original survey could not query the registry API and judged maturity from GitHub
stars, so the download figures here are new and a few of its conclusions moved.

**Short version:** no single crate replaces the *pipeline* — one declarative config file
driving generated types, docs, MCP tools, typed handlers, role gating and a WebSocket
protocol browsers already speak. But several crates are better than this one at individual
layers, and if you only need one of those layers you should use them instead.

## Downloads, for calibration

`endpoint-libs` has ~8.6k downloads. Everything it is measured against below is one to
four orders of magnitude larger. That is not an argument against using it in-house — it is
purpose-built and proven across six production backends — but it should set expectations
about ecosystem depth, third-party integrations, and how much Stack Overflow exists.

| Crate | Latest | Downloads | Last release |
|---|---|---|---|
| tonic | 0.14.6 | 338M | 2026-05 |
| utoipa | 5.5.0 | 37.1M | 2026-05 |
| typify | 0.7.0 | 26.1M | 2026-06 |
| jsonrpsee | 0.26.0 | 22.8M | 2026-05 |
| rmcp | 3.0.0-beta.2 | 17.3M | 2026-07 |
| tarpc | 0.37.0 | 8.9M | 2025-08 |
| progenitor | 0.14.0 | 4.4M | 2026-04 |
| capnp-rpc | 0.26.3 | 4.0M | 2026-07 |
| poem-openapi | 5.1.16 | 3.2M | 2025-07 |
| aide | 0.16.0-alpha.4 | 2.3M | 2026-04 |
| socketioxide | 0.18.5 | 1.0M | 2026-07 |
| dropshot | 0.17.1 | 773k | 2026-06 |
| volo | 0.12.3 | 382k | 2026-03 |
| oasgen | 0.25.0 | 256k | 2025-02 |
| remoc | 0.18.3 | 205k | 2025-09 |
| apistos | 1.0.0-pre-release.14 | 185k | 2026-06 |
| **endpoint-libs** | **2.1.0** | **8.6k** | **2026-07** |

## Use something else if…

**You want a REST/HTTP API.** `axum` + `utoipa` (37M downloads) or `aide`, or `dropshot`
if you want the spec to be the contract and a CI check to prove it. All three are more
mature at HTTP than this crate will ever be, and OpenAPI describes HTTP natively rather
than through a projection.

**You want gRPC.** `tonic`, at 338M downloads, is not a close call. It also gives you the
config-file-first workflow (`.proto`) that is the main reason to reach for this crate.

**You want Rust-to-Rust RPC with no config file.** `tarpc` is cleaner and better designed.
Its `Transport` abstraction is the blueprint the 2.0 transport seam here was modelled on —
we adopted the idea rather than the crate because migrating the deployed protocol was not
worth it. Note tarpc's last release was 2025-08; momentum has slowed.

**You want standards-first JSON-RPC.** `jsonrpsee` (22.8M) is the mature choice, with
server and client, WS and HTTP.

**You want one multiplexed typed link between two Rust processes.** `remoc` does that one
job better than anything here.

**You want to generate a Rust client from someone else's OpenAPI spec.** `progenitor` +
`typify`. Nothing in this repo competes; use them.

**You want an MCP server and nothing else.** `rmcp`, the official SDK, has grown to 17.3M
downloads. If MCP is your product rather than a facet of an existing RPC surface, start
there. Note it is still `3.0.0-beta`.

## Why this crate still exists

Three things, together, that nothing in the list above provides as a set:

1. **The codegen arrow points from spec to code.** `utoipa`, `aide`, `poem-openapi`,
   `apistos`, `oasgen` and `dropshot` all generate a *spec from your code*. They delete no
   boilerplate — you still hand-write every handler signature, every type, every doc
   comment. `endpoint-gen` goes the other way: one RON file produces the Rust models, the
   docs, the MCP tool schemas and (2.1) the OpenAPI/AsyncAPI documents.

   The one part of the ecosystem that goes spec → Rust *server* is its weakest corner:
   `openapi-generator`'s `rust-axum` is BETA, cannot express `oneOf`/union types, and
   needs a JVM.

2. **OpenAPI structurally cannot describe a WebSocket message protocol.** It describes
   HTTP requests and responses. It has no vocabulary for "a persistent socket carrying
   `{method, seq, params}` frames correlated by `seq`, sharing that socket with MCP
   JSON-RPC". The standards that *can* — AsyncAPI, OpenRPC — have an order of magnitude
   less Rust tooling. This is why 2.1 emits AsyncAPI as the accurate one of the two
   specification documents and OpenAPI as an explicitly-labelled projection, rather than
   adopting OpenAPI as the source of truth. Note that both are *additional* outputs: the
   RON remains the source of truth, and `services.json` remains the artifact internal
   tooling is built against.

3. **Roles and typed public errors are in the schema.** No crate in the table above
   generates role gating or a typed public error contract from a declarative definition.
   That is the part that has actually prevented bugs in production here.

## Where the criticism lands

The strongest objection to this design is that it carries a bespoke schema format and
collects none of the standard-format dividends: no third-party SDK generation, no
Swagger/Scalar docs, no spec-driven fuzzing, no OpenAPI→MCP bridging.

That objection was correct, and 2.1 is the answer to it — the RON stays the source of
truth, and the standard documents are emitted as artifacts. What remains unaddressed:

- **`rmcp-openapi` erodes the MCP differentiator.** Anyone with an OpenAPI document can
  now bridge it to MCP tools, so "generates MCP tools" is no longer unique to this
  pipeline for REST-shaped APIs. It remains unique for *this* protocol, which has no
  REST shape to bridge.
- **Spec-driven fuzzing is still out of reach.** Schemathesis wants a real HTTP surface;
  the synthetic paths are not one. That needs a REST adapter that does not exist.
- **`typify` is a plausible future codegen backend** if the RON model ever wants to be
  expressed as JSON Schema first.

## Things worth stealing, not adopting

- **`dropshot`'s committed-spec CI check** — regenerate, diff, fail on drift. Adopted in
  endpoint-gen 1.11 as `--check`.
- **`tarpc`'s `Transport` shape** — adopted as the 2.0 transport seam.
- **`capnp`'s capability model** — prior art for mission-token design, not yet used.
