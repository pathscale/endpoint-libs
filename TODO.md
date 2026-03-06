# TODO

## Maintenance

- Fix or remove the `log_throttling` feature (currently broken and excluded from CI)
- Remove or repurpose all deprecated code after confirming it is unused across dependent projects
- Document the code thoroughly
- Add custom rustfmt rules file
- Add typos check
- Add cargo-deny dep check


## Infrastructure / Performance

- Improve codebase structure
- Investigate build time improvements

## Transport Layer

- Investigate WebSocket over HTTP/2 for the WS server implementation
- Investigate raw QUIC as a server transport option, with a usage API compatible with the current WS server
- Investigate WebTransport after the above two are explored

## Server API / Handler Model

- Investigate adding an optional handler type that bundles multiple message handlers together within the same message loop, for handlers that conceptually share a connection context (e.g. `LoginStep1`, `LoginStep2` that need to run on the same connection). WS natively supports this, but the current handler abstraction hides it behind a REST-like interface.

- Investigate using the Typestate pattern when constructing the WS server to enforce at compile-time that all endpoint-gen configured handlers and auth handlers have been registered.

- Simplify state consumption in endpoint handlers so they can share a simple global `AppState`, similar to axum/actix-web.

- Add the concept of extractors to endpoint handlers, similar to axum.
