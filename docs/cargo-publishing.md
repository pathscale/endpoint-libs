# Cargo publishing

Set the release version in `Cargo.toml`; the manifest is the version source and
must keep `publish = ["crates-io"]`. Publishing is a manual workflow dispatch
on `master`. The workflow runs `cargo publish --registry crates-io` directly.

Before dispatch, review the package and run the applicable local release checks,
including `cargo publish --dry-run`. Follow [the release order](release-order.md):
publish endpoint-libs before honey_id-types and update dependent backends together.
The existing `crates-io` environment must restrict deployments to `master` and
provide `CARGO_REGISTRY_TOKEN`.

Do not add `crates-io` to a private package's `publish` allowlist.
This workflow does not create tags, GitHub Releases, or dependency bumps.
