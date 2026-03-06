# endpoint-libs

[![Crates.io](https://img.shields.io/crates/v/endpoint-libs)](https://crates.io/crates/endpoint-libs)
[![Docs.rs](https://docs.rs/endpoint-libs/badge.svg)](https://docs.rs/endpoint-libs)
[![CI](https://github.com/pathscale/endpoint-libs/actions/workflows/rust.yml/badge.svg)](https://github.com/pathscale/endpoint-libs/actions/workflows/rust.yml)
[![License: MIT](https://img.shields.io/crates/l/endpoint-libs)](LICENSE)
[![Security audit](https://deps.rs/crate/endpoint-libs/1.3.0/status.svg)](https://deps.rs/crate/endpoint-libs/1.3.0)

Common library used across Pathscale projects. Contains generic utilities and shared types that are not specific to any single service, including a WebSocket server implementation, endpoint schema types for use with [endpoint-gen](https://github.com/pathscale/endpoint-gen), structured logging setup, and more.

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

## Config Loading

A `load_config` utility parses a JSON config file, defaulting to `etc/config.json` or overridden via `--config`/`CONFIG` env var. Supports an optional `--config-entry` for selecting a sub-key within the config object.
