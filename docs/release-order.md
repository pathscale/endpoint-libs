# Release order — endpoint-libs and its dependents

`endpoint-libs` sits at the root of a small dependency chain. Releasing it out of
order produces a confusing compile failure in every downstream service at once, so
this is the order to follow and the check that proves it worked.

## The chain

```
endpoint-libs
├── honey_id-types ──┐
├── endpoint-gen     │   (independent of honey_id-types)
└──────────────────┐ │
                   ▼ ▼
   api.support.cafe · auth.honey.id-backend · api.honey.id-backend
   nofilter.io-backend · pays.online-backend · web3.trading-backend
```

## Why the order matters

**`honey_id-types` re-exports endpoint-libs' `WsRequest` and `WsResponse` traits.**

So a consumer that bumps `endpoint-libs` while still on an older `honey_id-types`
ends up with **two incompatible copies of endpoint-libs in one dependency graph**.
The traits are then different types with the same name, and you get this:

```
error[E0277]: the trait bound `InitRequest: endpoint_libs::libs::ws::basics::WsRequest`
              is not satisfied
    --> endpoint-libs-1.9.1/src/libs/ws/basics.rs:20:1
    --> honey_id-types-1.14.0/src/handlers/convenience_utils/generic_auth_handler.rs:145:37
```

Note the two different `endpoint-libs` paths in that error — that is the tell. It
looks like a broken handler; it is actually a version split. Chasing it in the
handler wastes an afternoon.

`endpoint-gen` depends on endpoint-libs too, but nothing depends on `endpoint-gen`
at build time (it is a binary), so it can move independently of `honey_id-types`.

## The order

1. **`endpoint-libs`** — publish first. Nothing else can move until it is on
   crates.io, because the downstream manifests must resolve a real version.
2. **`honey_id-types`** — bump its `endpoint-libs` requirement, then publish.
   - A **major** endpoint-libs bump means a **major** bump here too, even with zero
     source changes: it is breaking for *consumers*, because a consumer on the old
     endpoint-libs cannot use this version. A minor bump would let Cargo silently
     hand it to them and produce the error above.
3. **`endpoint-gen`** — bump its `endpoint-libs` requirement and publish. Can happen
   in parallel with step 2.
4. **The six backends** — bump **both** `endpoint-libs` and `honey_id-types`
   together, in one commit. Never one without the other.

## The check that proves it

After any bump, in each consumer:

```bash
grep -c 'name = "endpoint-libs"' Cargo.lock     # must be exactly 1
```

More than one means the split has happened. Also confirm it resolves from the
registry rather than a local path override:

```bash
grep -A2 'name = "endpoint-libs"' Cargo.lock | grep source
# source = "registry+https://github.com/rust-lang/crates.io-index"
```

## Pre-release versions

A pre-release (`2.0.0-alpha.1`, `-beta`, `-rc`) needs an **exact** pin:

```toml
endpoint-libs = "2.0.0-alpha.1"   # exact — required for a pre-release
endpoint-libs = "2.0"             # will NOT match a pre-release
```

So while a pre-release is in flight, every consumer carries an exact pin and each
new pre-release needs a deliberate bump everywhere. On promotion to stable, relax
them all to a caret range (`"2.0"`) so patch releases flow without a dependency PR.

## Local verification before publishing

To test the chain end to end before anything is on crates.io, use a temporary
override in the consumer — and **remove it before merging**:

```toml
# TEMPORARY — remove before release.
[patch.crates-io]
honey_id-types = { path = "../honey_id-types" }
```

Note the override only applies if the version requirement can match the local
crate's version. If the manifest still says `"1.14"` and the local checkout is
`2.0.0`, Cargo silently ignores the patch and you keep building the old one.

## Worked example — the 2.0 release

1. `endpoint-libs 2.0.0-alpha.1` published.
2. `honey_id-types 1.14.0 → 2.0.0` — zero source changes, major bump for the reason
   in step 2 above.
3. `endpoint-gen 1.10.0` — with the exact alpha pin.
4. Six backends bumped both, each verified with the `grep -c` check. All six ported
   with **zero source changes**.
5. Promotion to stable: `endpoint-libs 2.0.0` → `honey_id-types 2.0.1` and
   `endpoint-gen 1.10.1` relaxed to `"2.0"` → all six backends relaxed to `"2.0"`.
