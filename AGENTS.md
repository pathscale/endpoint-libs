# Working agreement — endpoint-libs

The operating contract for **any** coding agent working in this repository. This file is
the single source of truth for the rules: Codex, Cursor and Gemini CLI read `AGENTS.md`
natively, and Claude Code loads it through the `@AGENTS.md` import in
[`CLAUDE.md`](CLAUDE.md). **Never fork these rules into a per-vendor file.**

**Mixed Rust + JavaScript repository.** The JS side (`endpoint-libs-examples`, built with `npm`) is the primary surface; the Cargo workspace holds supporting Rust (e.g. integration tests).

## Invariants (don't break these)

- **Verify the whole chain, not just this repo.** endpoint-gen, honey_id-types,
  endpoint-validator and the six backends all break silently when this crate moves.
  `./scripts/check-chain.sh` verifies all of it; run it before calling a change done.
  Cross-repo context: [`~/code/CLAUDE.md`](../CLAUDE.md).
- **Releasing this crate has a required order.** `honey_id-types` re-exports its
  `WsRequest`/`WsResponse` traits and must be published after it; the six backends
  bump both together. Getting it wrong puts two incompatible copies of endpoint-libs
  in a consumer's graph. See [`docs/release-order.md`](docs/release-order.md).
- **`npm` is the package manager** — its lockfile is authoritative. Don't introduce a second one by running npm/yarn/pnpm here.
- **Two toolchains live here.** A change to one side does not imply the other still builds — check both before calling it done.
- **Docs describe what is true now.** If you change behaviour, update the README and any affected doc in the same change.

## Build & run

```bash
npm install
```

```bash
cargo build
cargo test
```

## Verification

Run what you build before reporting it done. Type-checks and tests verify code correctness,
not feature correctness — **if you can't run it, say so explicitly** rather than implying
success.

- Compare against the base branch rather than asserting: a pre-existing failing test or lint
  error is not something you introduced, and saying so requires checking.
- A build that finishes suspiciously fast was cached, not rebuilt. Force a real rebuild when
  the rebuild is the thing you're verifying.

## PR discipline

**Always paste the full PR URL** (`https://github.com/pathscale/endpoint-libs/pull/<n>`), not just the number, so it's
clickable.

<!-- DORMANT — CI-green gating. Do not follow this rule yet; re-enable it as its own project.

Why it's off: CI here does not reliably attach checks to pull requests, so
`statusCheckRollup` comes back empty and "wait for green" would teach an agent to wait on
nothing. Verify per repo before switching this on.

To enable: ensure the workflow runs on `pull_request:`, confirm checks attach to a PR, then
uncomment the rule below.

    After any push or PR, **check CI and don't call it done until it's green**:

    ```bash
    gh pr view <number> --repo pathscale/endpoint-libs --json statusCheckRollup
    ```

    CI running → wait and recheck. CI failed → read the logs, fix, push, wait for green.
-->

## Keeping docs honest

Hit a factual error here — a stale path, a wrong command, a moved status? Fix it in the same
change. Don't open cosmetic rewording PRs.

Learned something durable — a gotcha, a decision, a constraint? It belongs **in this repo's
docs**, not in your agent's private memory. Repo docs are versioned, reviewable, and visible
to every agent and human; private memory dies with your machine.

## Git workflow

- **Always specify the branch when pushing**: `git push origin branch-name`
- **Branch naming**: `fix/issue-description` or `feat/issue-description`
- **Force-push your own branch freely.** Rebasing a feature branch onto a moved
  base, or amending before review, is normal and correct — use
  `--force-with-lease` so you don't clobber someone else's push.
- **Never force-push the default branch** (`main`/`master`). That is the history
  everyone else builds on, and it is protected server-side for a reason.

## Guardrails

[`.claude/settings.json`](.claude/settings.json) and [`.claude/hooks/`](.claude/hooks/) make
Claude Code prompt a human before prod-affecting or destructive commands — pushes, publishing
to a registry, `gh pr merge`, cloud CLIs, recursive deletes, deploy scripts.

**Other agents don't get that net automatically.** Apply the same rule yourself: ask before
running any command family listed in
[`.claude/hooks/ask-before-risky-commands.sh`](.claude/hooks/ask-before-risky-commands.sh).
It is one layer of defence, not a guarantee — a pattern match over a command string is
best-effort.
