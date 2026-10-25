# Security closure ledger: endpoint-libs

Review date: 2026-10-25 (Asia/Phnom_Penh)

## Evidence boundary

Historical source audit: `docs/SECURITY-AUDIT-2026-10-14.md`.

- Historical audit SHA-256: `d9827b61a370d51056afbbde68e49f30b267c78fed0f71bbbef12ac88cd09fa2`
- Audited local master baseline: `1160c7a22b3a892bc0949bcec6ab3c5255adb803`
- Live remote master observed by the coordinator: `9bea1725597961fffdf158aecf9b91386ab4e261`
- Proposed source revision: not assigned; this worker provides a patch for integrator application.

The historical audit remains unchanged. Its inventory is 0 Critical, 2 High, and 6 Medium. Status `S` below means source mitigation is present at the audited baseline, not production closure. `P/E` means source mitigation is present but external evidence is still required. No runtime test, build, feature check, CI check, or production check is claimed as run by this ledger.

## Original Critical, High, and Medium findings

| ID | Severity | Source status | Source evidence and regression evidence |
|---|---|---|---|
| H1 | High | S | `src/libs/ws/headers.rs:180-188` rejects malformed non-ASCII protocol tokens before byte slicing. Regression names: header parser malformed Unicode cases in `headers.rs` and unauthenticated malformed-header cases in `ws_admission_acceptance.rs`. Run them under Coordinator CI. |
| H2 | High | S | `src/libs/types.rs:123` uses fallible string parsing for blockchain hashes; the address deserializer is also fallible and feature-gated. Regression names: blockchain address/hash serde roundtrip and malformed-input cases. Run once with default features and once with `blockchain`; neither state was built here. |
| M1 | Medium | S | `src/libs/ws/server.rs:91-103` uses `ConnectionCleanup` to remove state on failed auth and cancellation, while scheduling disconnect hooks separately. Regression names: failed-auth cleanup, cancellation cleanup, and disconnect cleanup cases in `ws_admission_acceptance.rs`. |
| M2 | Medium | S | `src/libs/ws/session.rs:393,403,458` admits MCP tool calls through the configured inflight slot and request timeout. Regression names: MCP slot saturation, MCP timeout, and slot release after cancellation/error cases in `ws_admission_acceptance.rs`. |
| M3 | Medium | S | `src/libs/ws/server.rs:485,609-638,901-957` and `src/libs/ws/session.rs:80-82` apply handshake deadlines, accept backoff and idle timeout. Regression names: handshake timeout, accept backoff/shutdown and idle timeout cases in `ws_admission_acceptance.rs`. |
| M4 | Medium | S | `src/libs/ws/server.rs:1239-1270` supplies finite inflight, request, handshake and idle defaults. Regression names: finite-default and explicit override cases in server configuration acceptance tests. |
| M5 | Medium | P/E | `.github/workflows/publish.yml` gates the job to master, requests contents read and names the `crates-io` environment. The checkout does not prove environment branch rules, required reviewers, token scope, or deployed protection. Coordinator must collect redacted repository environment and crates.io trusted-publishing evidence. |
| M6 | Medium | S | `src/libs/config.rs` no longer prints the deserialized configuration. `examples/config-log-acceptance.rs` uses a synthetic sentinel and a child process to assert empty output without invoking config `Debug`. Regression name: `config-log-acceptance`; execute in Coordinator CI. |

## Resource, logging, persistence, and dependency controls

- Listener admission uses finite defaults for connections, inflight requests, request time, handshake time, and idle time in `src/libs/ws/server.rs`. These are listener admission controls and do not impose process-wide limits on a library consumer.
- This library does not install process RSS, CPU, wall-clock, or file-descriptor limits. Runnable examples and consumer executables need their own entrypoint limits. No child process is launched by the audited listener path. Coordinator should inventory every executable and child process before claiming caps closure, recording RSS, CPU, and wall limits separately from listener admission.
- Configuration logging has a synthetic no-output acceptance fixture. `OtelConfig` and `LoggingConfig` still derive `Debug`, so callers must not log those values; audit all public logging paths and consumer call sites before broadening the claim.
- This patch adds `clippy.toml` rules for `std::fs::write`, `std::fs::File::create`, and `std::fs::create_dir_all`, and explicitly denies `clippy::disallowed_methods` in every existing all-target feature-boundary CI invocation. Method-level exceptions cover only the public schema dump, deprecated empty-file writability probe, and synthetic acceptance fixture. This guard does not cover `OpenOptions`, `Write` trait calls, alternate filesystem APIs, FFI, subprocesses, or writes in consumers; these remain review obligations. Verify effective Clippy inheritance on each listed feature and example target in Coordinator CI.
- `std::thread` remains in logging, log reading, utility tests, server shard startup and integration tests. No supported bounded Nagoya replacement was applied in this worker patch. Track this as open source work until each active direct thread is migrated or an explicit compatibility exception is reviewed. Historical Tokio mentions are comments/docs; dependency edges must be checked from manifests and the resolved graph rather than text matches.
- Maintained em dash cleanup is not included in this patch. Recheck with provenance-sensitive exclusions for historical and third-party text.

## Deferred checks and external blockers

Not run: `./scripts/check-chain.sh`, `./scripts/check-features.sh`, all Cargo build/test/clippy commands, semantic dependency graph checks, feature builds with and without `blockchain`, OS-level cap checks, cancellation/timeout/backoff runtime cases, protected publish-environment inspection, and deployed token/protection evidence. `check-chain.sh` and `check-features.sh` compile and belong in Coordinator CI, not this worker lane.

External blocker: M5 remains open until the `crates-io` environment's master-only branch policy, required reviewer configuration, and token/trusted-publishing setup are verified through redacted control-plane evidence. No live secret was requested or read.

Cross-repository release contract: publish `honey_id-types` only after the compatible `endpoint-libs` release; dependent backends must update both in the documented order. This ledger does not edit sibling repositories or publish a release.

## Executable, child, and direct-thread inventory

| Entry point or child | Current resource boundary | Required cap evidence |
|---|---|---|
| Library listener `WebsocketServer::listen` in `src/libs/ws/server.rs` | Listener-level connection, message, inflight, handshake, request, and idle limits only. No process-global cap is installed, by design. Shard threads are created internally when configured or detected. | Exercise bounded listener admission separately. Do not count these settings as RSS, CPU, wall-clock, or file-descriptor process caps. Shard creation remains an open direct-thread item. |
| `examples/ws-echo/main.rs` | Runnable example. No repo-local process RSS, CPU, or wall-clock cap was found. | Coordinator should classify this as a demo executable and decide whether it is launched in any supported production context; if so, enforce caps in its launcher. |
| `examples/config-log-acceptance.rs` | Synthetic acceptance executable. It launches its own current executable once with `ENDPOINT_CONFIG_LOG_CHILD=1`; no RSS, CPU, or wall-clock cap was found. | Run only in CI with an outer job limit. Verify child termination and output sentinels there; this worker did not execute it. |
| Integration test executables under `tests/` | Test harness processes. `tests/sharded_listen.rs` starts a server thread; `tests/ws_tls_policy.rs` starts a local TLS fixture thread. No child executable launch was found in the scanned test sources. | Coordinator CI timeout and runner isolation are the applicable outer bounds. No production process-cap claim follows from test-runner bounds. |

Direct thread sites still present in source at the audited baseline: `src/libs/log_reader.rs` has one spawned thread for blocking reverse-file reads; `src/libs/ws/server.rs` creates shard threads; `src/libs/utils.rs` uses scoped threads in an atomic-ID test; `src/libs/log.rs` and `src/libs/log/level_filter.rs` use thread sleeps in tests; `tests/sharded_listen.rs` uses a thread for current-thread identity and one server thread; `tests/ws_tls_policy.rs` starts a fixture thread. `Cargo.toml` and source comments also mention historical or explanatory uses. No migration is claimed. The blocking reader and reactor-per-shard topology need a supported bounded Nagoya facility or an explicitly reviewed API redesign before this gate can pass.

## Required semantic gates

- No Tokio dependency edge in any supported feature state, verified from the resolved dependency graph. Historical prose matches are not dependency evidence.
- No direct production `std::thread` creation or blocking thread handoff. Test-only thread use must be separately reviewed and must not be mistaken for production reachability.
- No maintained em dash in owned prose, with provenance-sensitive exclusions for historical and third-party content.
- Clippy filesystem write denies inherit in every CI feature boundary and target. The only source exceptions are the three method-level annotations documented above; inspect the effective lint set rather than relying only on the existence of `clippy.toml`.

These gates remain unrun and open. No source-level or production closure claim is made for them.
