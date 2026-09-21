# Working across the endpoint-libs chain

`endpoint-libs` is the root of a coupled system, not a standalone crate. This document is
for anyone — human or agent — changing **any** node in it.

It lives here because this is the crate everything else depends on, and because a document
that matters has to be version-controlled, reviewable, and present in a fresh clone. Each
dependent repo's `AGENTS.md` links to it rather than restating it.

## The chain

```
endpoint-libs                    the runtime, and the schema model everything else uses
├── endpoint-gen                 reads RON, writes models/docs/MCP schemas/specs
├── honey_id-types               re-exports endpoint-libs' WsRequest/WsResponse traits
├── endpoint-validator           reads generated services.json, drives endpoint tests
└── six backends                 api.support.cafe · web3.trading-backend ·
                                 nofilter.io-backend · pays.online-backend ·
                                 auth.honey.id-backend · api.honey.id-backend
```

**Touching any node can break the others silently** — not at compile time in the repo you
edited, but later, in a different repo, with an error that does not name the cause.

## Before calling a change done

```bash
./scripts/check-chain.sh          # full
./scripts/check-chain.sh --quick  # metadata only, no cargo
```

Read-only and safe to run any time. It expects the sibling repos beside this one; set
`CODE_ROOT` if they live elsewhere, and it skips anything not checked out rather than
failing.

It verifies:

1. **Exactly one `endpoint-libs` per dependency graph.** Two copies in one graph means
   the traits `honey_id-types` re-exports are different types with the same name. The
   resulting error names two different `endpoint-libs` paths and reads like a broken
   handler.

   *Per graph*, not per lockfile. A `Cargo.lock` is per workspace and records every
   version any member resolved, not which member reached which, so counting copies in
   the lock cannot tell the real defect from a workspace whose members resolve
   different versions in graphs that never meet. The lock count is only a prefilter:
   above one, the check asks `cargo tree` per workspace member, and fails only when a
   single member reaches two versions. `EndpointValidator` is the standing example of
   the harmless shape, described at the end of this document.
2. **`config/version.toml` agrees with `Cargo.lock`** in every backend. `endpoint-gen`
   compares its own requirement against `[libs] version` and refuses to run on a
   mismatch — a stale declaration is the usual cause of a baffling refusal.
3. **Generated artifacts still match their RON** (`endpoint-gen --check` per backend).
4. **Each tool repo builds and tests.**
5. **Every `endpoint-libs` feature compiles standalone**
   (`./scripts/check-features.sh`). A consumer enables one feature, not the set you
   built with, so each feature must name every dependency its own code uses with
   `dep:`. Skipped under `--quick`.
6. **Local versions against crates.io**, so an unpublished bump is a known state rather
   than something a consumer discovers.

A red line is a real problem or a deliberate, documented one — never noise to skim past.
If it is deliberate, say so in the change that makes it red.

## Traps that have actually bitten

- **Schema types must not be copied.** `Type`, `Field`, `EnumVariant` and
  `EndpointSchema` live in `endpoint_libs::model`. `endpoint-validator` kept hand-copied
  duplicates; upstream renamed `EnumVariant.comment` to `description`, and the tool was
  silently unable to read any generated `services.json` until someone tried it. If you
  need those types, depend on this crate.
- **Release order is not optional.** endpoint-libs publishes first, then `honey_id-types`
  and `endpoint-gen`, then the backends bump **both** together.
  See [`release-order.md`](release-order.md).
- **Minor versions do not need to match** across endpoint-libs, endpoint-gen and
  honey_id-types, despite what older docs claimed. What is enforced is the
  `version.toml` check. Do not "fix" a version to make the numbers line up.
- **A feature that only ever builds with `types` can forget its own dependencies.**
  `types` pulls in `tracing`, `chrono` and most of the rest, so `signal` and
  `log_reader` compiled in every set anyone ever built while missing `dep:tracing`
  and `dep:chrono` of their own. It only shows up for a consumer who asks for just
  that feature, which is the whole point of an optional feature.
  `./scripts/check-features.sh` is the guard.
- **`cargo update` in one repo can move a shared dependency** into a range another repo
  cannot satisfy. Re-run the chain check after any dependency update.
- **`services.json` is not deprecated by the 2.1 specification documents.** It is the
  artifact we control and build internal tooling against; OpenAPI/AsyncAPI are opt-in
  outputs for consumers outside our control. Do not migrate internal tooling onto
  AsyncAPI on the assumption it supersedes it.

## Publishing

Irreversible: a version number can never be reused, and yanking does not delete.
`cargo publish --dry-run` first, publish from the default branch, tag the release. Ask a
human before publishing unless they have asked for it in this session.

## The `EndpointValidator` split, which is not a failure

`EndpointValidator` is a workspace whose two members sit on different majors on purpose.
`endpoint-validator` is on 2.1; `ws-load-test` is held on 1.9 because endpoint-libs 2.0
made `WsClient`'s futures non-`Send`, which breaks its `JoinSet`-of-workers model. Moving
it needs a runtime plus a `LocalSet` per pinned core, changing the very concurrency
characteristics its CIDR 2027 measurements record. The reason lives in
`ws-load-test/Cargo.toml`, where the pin is.

The two members do not depend on each other, so no binary ever sees two copies and
nothing can fail to unify. Check 1 used to report it anyway, on every run, because it
counted the lockfile instead of the graphs. It no longer does. **Do not "fix" this by
bumping `ws-load-test`**, and do not reintroduce a lockfile count.

## Known-red, as of 2026-09-22

Nothing. `./scripts/check-chain.sh --quick` is green across all ten repos. Add an entry
here the moment a check goes red on purpose, and delete it when it is resolved -- a red
line with no entry is a real problem.
