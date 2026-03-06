#!/usr/bin/env bash
set -euo pipefail

# Usage: ./scripts/update_readme.sh
# Updates version-pinned URLs in README.md to match the current version in Cargo.toml.
# Commits the change if the README was modified.

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CARGO_TOML="$REPO_ROOT/Cargo.toml"
README="$REPO_ROOT/README.md"

VERSION=$(grep '^version' "$CARGO_TOML" | head -1 | sed 's/.*"\(.*\)".*/\1/')
TAG="v$VERSION"

if [ -z "$VERSION" ]; then
    echo "Error: could not read version from Cargo.toml" >&2
    exit 1
fi

sed -i "s|deps.rs/crate/endpoint-libs/[^/]*/status\.svg|deps.rs/crate/endpoint-libs/$VERSION/status.svg|g" "$README"
sed -i "s|](https://deps.rs/crate/endpoint-libs/[^)]*)$|](https://deps.rs/crate/endpoint-libs/$VERSION)|" "$README"

echo "Updated deps.rs badge in README to $TAG"
