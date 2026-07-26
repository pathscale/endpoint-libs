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

1. **Exactly one `endpoint-libs` per dependency graph.** Two copies means the traits
   `honey_id-types` re-exports are different types with the same name. The resulting
   error names two different `endpoint-libs` paths and reads like a broken handler.
2. **`config/version.toml` agrees with `Cargo.lock`** in every backend. `endpoint-gen`
   compares its own requirement against `[libs] version` and refuses to run on a
   mismatch — a stale declaration is the usual cause of a baffling refusal.
3. **Generated artifacts still match their RON** (`endpoint-gen --check` per backend).
4. **Each tool repo builds and tests.**
5. **Local versions against crates.io**, so an unpublished bump is a known state rather
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

## Known-red, as of 2026-07-26

Two checks fail deliberately. Delete each entry when it is resolved.

- **The six backends fail check 3.** They declare `[libs] 2.0.0` while the installed
  `endpoint-gen` requires `^2.1`, so none can be regenerated. They are self-consistent
  and building fine; the rollout to 2.1 is a pending decision, not an accident.
- **`EndpointValidator` fails check 1** with two majors (1.9.1 and 2.1.1). Its
  `ws-load-test` member is held on 1.x because endpoint-libs 2.0 made `WsClient`'s
  futures non-`Send`, breaking its `JoinSet`-of-workers model. Moving it needs a runtime
  plus `LocalSet` per pinned core, which would change the concurrency characteristics its
  benchmarks measure. Recorded in that member's manifest.
