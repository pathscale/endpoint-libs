#!/usr/bin/env bash
set -euo pipefail

# Usage: ./scripts/release.sh
# Reads the version from Cargo.toml, tags the release as vX.Y.Z,
# pushes the tag to git, and publishes the crate to crates.io.

CARGO_TOML="$(dirname "$0")/../Cargo.toml"

VERSION=$(grep '^version' "$CARGO_TOML" | head -1 | sed 's/.*"\(.*\)".*/\1/')
TAG="v$VERSION"

if [ -z "$VERSION" ]; then
    echo "Error: could not read version from Cargo.toml" >&2
    exit 1
fi

# Ensure working tree is clean
if ! git -C "$(dirname "$0")/.." diff --quiet || ! git -C "$(dirname "$0")/.." diff --cached --quiet; then
    echo "Error: working tree has uncommitted changes. Please commit or stash them first." >&2
    exit 1
fi

# Check tag does not already exist
if git -C "$(dirname "$0")/.." tag | grep -qx "$TAG"; then
    echo "Error: tag $TAG already exists." >&2
    exit 1
fi

echo "Releasing $TAG"

git -C "$(dirname "$0")/.." tag "$TAG"
echo "Created tag $TAG"

git -C "$(dirname "$0")/.." push origin "$TAG"
echo "Pushed tag $TAG to origin"

cargo publish --manifest-path "$CARGO_TOML"
echo "Published $TAG to crates.io"
