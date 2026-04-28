# endpoint-libs

[![Crates.io](https://img.shields.io/crates/v/endpoint-libs)](https://crates.io/crates/endpoint-libs)
[![Docs.rs](https://docs.rs/endpoint-libs/badge.svg)](https://docs.rs/endpoint-libs)
[![CI](https://github.com/pathscale/endpoint-libs/actions/workflows/rust.yml/badge.svg)](https://github.com/pathscale/endpoint-libs/actions/workflows/rust.yml)
[![License: MIT](https://img.shields.io/crates/l/endpoint-libs)](LICENSE)
[![Security audit](https://deps.rs/crate/endpoint-libs/1.7.16/status.svg)](https://deps.rs/crate/endpoint-libs/1.7.16)

Common library used across Pathscale projects. Contains generic utilities and shared types that are not specific to any single service, including a WebSocket server implementation, endpoint schema types for use with [endpoint-gen](https://github.com/pathscale/endpoint-gen), structured logging setup, and more.

## Releasing

Releases are managed with [`cargo-release`](https://github.com/crate-ci/cargo-release) and [`git-cliff`](https://github.com/orhun/git-cliff). Both must be installed:

```sh
cargo install cargo-release git-cliff
```

To cut a release:

```sh
./scripts/release.sh [--skip-bump] <patch|minor|major>
```

The script will:
1. Run `cargo release --execute <level>` — bumps the version in `Cargo.toml`, updates the deps.rs badge in this README, regenerates `CHANGELOG.md`, and commits everything as `chore(release): vX.Y.Z`.
2. Open your `$EDITOR` with the auto-generated tag notes (from `git-cliff`) for review.
3. Create an annotated tag using the edited notes as the tag body (shown as GitHub Release notes).
4. Push the commit and tag.
5. Prompt whether to publish to crates.io.

To preview what `cargo-release` would do without making changes:

```sh
cargo release patch  # omit --execute for a dry run
```

## Version Compatibility

When using `endpoint-libs` alongside `endpoint-gen` or `honey_id-types` in the same project, **minor versions must match** between all of them. Patch versions are stable across these crates but should ideally be kept in sync as well.

For example, `endpoint-libs 1.3.x` must be paired with `endpoint-gen 1.3.x` and `honey_id-types 1.3.x`.

## Features

The crate is feature-gated. The default feature set is `types` only.

### `types` (default)

Endpoint schema types shared between services and `endpoint-gen`:

- `Type`, `Field`, `EnumVariant` — the type system used to describe endpoint request/response schemas
- Blockchain primitive types: `BlockchainAddress`, `BlockchainTransactionHash`, `U256`, `H256`

### `ws`

Async WebSocket server built on `tokio-tungstenite` with TLS support via `rustls`. Includes:

- Connection management and session tracking
- Push/subscription infrastructure
- Request handler trait and toolbox utilities
- HTTP header parsing helpers

### `signal`

Unix signal handling (`SIGTERM`/`SIGINT`) with a global `CancellationToken` for coordinating graceful shutdown across async tasks.

### `scheduler`

Task scheduling utilities built on `tokio-cron-scheduler`:

- Fixed-interval repeated jobs
- `AdaptiveJob` — jobs whose interval can be changed at runtime via a `JobTrigger` handle

### `log_reader`

Utilities for reading and parsing structured log files, including reverse-line iteration for reading recent entries efficiently.

### `error_aggregation`

A `tracing` layer that captures recent error-level log events into an in-memory container, allowing them to be queried programmatically (e.g. to expose recent errors via an API endpoint).

### `log_throttling`

> **Do not use.** This feature is currently non-functional and is excluded from CI. It is present for future development only.

Rate-limiting layer for `tracing` events to suppress repeated log spam.

## Logging Setup

The `setup_logging` function (available without any optional features) provides a batteries-included `tracing` subscriber with:

- Stdout logging with thread names and line numbers
- Optional file logging with configurable rotation
- Runtime log level reloading via `LogReloadHandle`
- Optional `error_aggregation` layer (requires `error_aggregation` feature)

## OpenTelemetry (OTel) Integration

The logging framework supports forwarding all `tracing` spans and log events to an OpenTelemetry (OTLP) collector as primary signals (**Traces** and **Logs**). This operates as a parallel layer and does not affect stdout or file logging.

### Enabling OTel

To enable OTLP forwarding, configure `OtelConfig` in your `LoggingConfig`:

```rust
use std::collections::HashMap;
use endpoint_libs::libs::log::{LoggingConfig, OtelConfig, OtelProtocol};

let mut headers = HashMap::new();
headers.insert("x-api-key".to_string(), "your-token".to_string());

let config = LoggingConfig {
    otel_config: OtelConfig {
        enabled: true,
        service_name: Some("my-service".into()),
        endpoint: Some("http://localhost:4317".into()), // OTLP collector endpoint
        protocol: OtelProtocol::Grpc,
        headers,
    },
    ..Default::default()
};

let setup = setup_logging(config)?;
// CRITICAL: Keep `setup.otel_guards` alive for the duration of the program.
// It flushes pending traces and logs to the collector on drop.
```

### Environment Variables

OTel can also be configured via standard environment variables:

| Variable | Description |
|----------|-------------|
| `OTEL_SERVICE_NAME` | Name of the service |
| `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` | Collector endpoint for traces |
| `OTEL_EXPORTER_OTLP_LOGS_ENDPOINT` | Collector endpoint for logs (falls back to traces endpoint) |
| `OTEL_EXPORTER_OTLP_PROTOCOL` | `grpc` or `http/protobuf` |
| `OTEL_EXPORTER_OTLP_HEADERS` | Key-value pairs for auth (e.g. `api-key=val,other=val`) |
| `OTEL_PROPAGATORS` | Context propagators (default: `tracecontext,baggage`) |

Note: Values in `OtelConfig` override environment variables. To prevent recursive logging, OTel internal crates are capped at the `WARN` level when global logging is set to `DEBUG` or `TRACE`.

## Config Loading

A `load_config` utility parses a JSON config file, defaulting to `etc/config.json` or overridden via `--config`/`CONFIG` env var. Supports an optional `--config-entry` for selecting a sub-key within the config object.
