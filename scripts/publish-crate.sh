#!/usr/bin/env bash
set -euo pipefail

fail() {
  printf '%s\n' "$*" >&2
  exit 1
}

[ "${GITHUB_REF:-}" = 'refs/heads/master' ] || fail 'publishing is allowed only from master'
[ "${LOCAL_CHECKS_ATTESTED:-}" = 'yes' ] || fail 'local checks were not attested'
[ -n "${CARGO_REGISTRY_TOKEN:-}" ] || fail 'CARGO_REGISTRY_TOKEN is required'

expected_commit=${EXPECTED_COMMIT:-}
case "$expected_commit" in
  ''|*[!0-9a-fA-F]*) fail 'EXPECTED_COMMIT must be a full hexadecimal commit SHA' ;;
esac
[ "${#expected_commit}" -eq 40 ] || fail 'EXPECTED_COMMIT must be a full 40-character commit SHA'
actual_commit=$(git rev-parse HEAD)
[ "$actual_commit" = "$expected_commit" ] || fail 'checked out commit does not match EXPECTED_COMMIT'
[ -z "$(git status --porcelain --untracked-files=all)" ] || fail 'working tree must be clean'

manifest=$(cargo read-manifest --manifest-path Cargo.toml)
package_name=$(jq -er '.name | select(type == "string" and length > 0)' <<<"$manifest")
version=$(jq -er '.version | select(type == "string" and length > 0)' <<<"$manifest")

# The reviewed manifest is the authority for public registry eligibility.
jq -e '(.publish | type) == "array" and (.publish | index("crates-io") != null)' <<<"$manifest" >/dev/null \
  || fail 'Cargo.toml must explicitly allow crates-io in package.publish'

case "$package_name" in
  ''|*[!A-Za-z0-9_-]*) fail 'manifest package name cannot be safely used as a crates.io identifier' ;;
esac

crate_id=$(jq -nr --arg name "$package_name" '$name | ascii_downcase | @uri')
version_id=$(jq -nr --arg version "$version" '$version | @uri')
status=$(curl --silent --show-error --location --max-redirs 3 \
  --user-agent 'manifest-cargo-publisher (https://github.com/pathscale/endpoint-libs)' \
  --connect-timeout 10 --max-time 30 --retry 0 \
  --output /dev/null --write-out '%{http_code}' \
  "https://crates.io/api/v1/crates/$crate_id/$version_id") \
  || fail 'crates.io registry request failed'

case "$status" in
  200)
    printf '%s %s is already published on crates.io; nothing to do\n' "$package_name" "$version"
    exit 0
    ;;
  404)
    printf '%s %s is absent from crates.io; publishing\n' "$package_name" "$version"
    ;;
  *)
    fail "crates.io returned HTTP $status; refusing to treat it as an absent version"
    ;;
esac

[ -z "$(git status --porcelain --untracked-files=all)" ] || fail 'working tree must be clean'
cargo publish --registry crates-io --package "$package_name"
