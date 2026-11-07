# Cargo publishing contract

Publishing is a separate manual workflow dispatch on master. The version in
Cargo.toml is the version to publish; it is not inferred from commits, a tag,
or a second version file. The manifest must explicitly allow crates.io with
package.publish = ["crates-io"]. Keep the package version unchanged for a
workflow-only migration.

Follow docs/release-order.md: publish endpoint-libs before honey_id-types and update dependent backends together.

Before dispatch, review the source and package contents, record the exact full
40-character commit SHA, and complete the release checks locally on that commit:

- cargo test --release
- cargo check --no-default-features
- ./scripts/check-features.sh, which checks each feature independently
- ./scripts/check-chain.sh, including downstream compatibility checks
- cargo publish --dry-run

Dispatch Publish crate from master with that SHA and attest that the local
checks passed. The workflow checks out the dispatch SHA; the publishing helper
requires GITHUB_REF=refs/heads/master, the attestation, a matching full SHA,
and a clean working tree. It reads the package name and version from Cargo
manifest metadata, verifies that crates-io is explicitly allowed, and checks
that exact version on crates.io. Only HTTP 404 means the version is absent; an
existing version is a no-op, and request failures fail closed. The workflow
does not run a remote test suite. Cargo's publish verification and the token
remain in the publishing environment.

The workflow serializes publishing runs within this repository. Process
dependencies before their consumers and pace crates.io publication across the
fleet; this concurrency group does not coordinate other repositories. Verify
the published version is available.

The contract continues to support crates.vip through the existing cargo vip
publish path and its existing configuration. This crates.io workflow neither
creates nor enables crates.vip configuration, and one registry's success does
not imply success at another. Private packages must exclude crates.io from
their manifest allowlist.

The standard does not require synchronized versions across repositories,
automatic dependency bumps, application deployments, or GitHub Releases.
Compatible dependency requirements remain unchanged unless the new code needs
a newer minimum or compatibility changes. Existing lockfile policies remain in
force.
