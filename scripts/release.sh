#!/usr/bin/env bash
set -euo pipefail

# Usage: ./scripts/release.sh <patch|minor|major>

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CARGO_TOML="$REPO_ROOT/Cargo.toml"

LEVEL="${1:-}"
if [[ "$LEVEL" != "patch" && "$LEVEL" != "minor" && "$LEVEL" != "major" ]]; then
    echo "Usage: $0 <patch|minor|major>" >&2
    exit 1
fi

# Check required tools
for tool in cargo-release git-cliff; do
    if ! command -v "$tool" &>/dev/null; then
        echo "Error: '$tool' is not installed. Run: cargo install $tool" >&2
        exit 1
    fi
done

# Ensure working tree is clean
if ! git -C "$REPO_ROOT" diff --quiet || ! git -C "$REPO_ROOT" diff --cached --quiet; then
    echo "Error: working tree has uncommitted changes. Please commit or stash them first." >&2
    exit 1
fi

# Ensure we are on main
CURRENT_BRANCH=$(git -C "$REPO_ROOT" rev-parse --abbrev-ref HEAD)
if [[ "$CURRENT_BRANCH" != "main" ]]; then
    echo "Error: must be on branch 'main' (currently on '$CURRENT_BRANCH')." >&2
    exit 1
fi

echo "Running cargo-release $LEVEL ..."
cargo release --manifest-path "$CARGO_TOML" --execute --no-confirm "$LEVEL"

# Read the now-bumped version
VERSION=$(grep '^version' "$CARGO_TOML" | head -1 | sed 's/.*"\(.*\)".*/\1/')
TAG="v$VERSION"

if [ -z "$VERSION" ]; then
    echo "Error: could not read version from Cargo.toml after release" >&2
    exit 1
fi

echo ""
echo "cargo-release committed version $TAG. Preparing tag notes..."

# Generate tag notes with git-cliff, open in editor for review
TMPFILE=$(mktemp /tmp/release_notes.XXXXXX)
git -C "$REPO_ROOT" cliff --latest --strip all > "$TMPFILE"

${VISUAL:-${EDITOR:-vi}} "$TMPFILE"

TAG_MESSAGE=$(cat "$TMPFILE")
rm -f "$TMPFILE"

if [ -z "$TAG_MESSAGE" ]; then
    echo "Error: tag message is empty, aborting." >&2
    exit 1
fi

# Create annotated tag with cliff-generated notes
echo "Tagging $TAG ..."
git -C "$REPO_ROOT" tag -a "$TAG" -m "$TAG_MESSAGE"

# Push commit and tag
echo "Pushing commit and tag..."
git -C "$REPO_ROOT" push origin HEAD "$TAG"

echo ""

# Optionally publish to crates.io
CRATES_IO_STATUS=$(curl -sf "https://crates.io/api/v1/crates/endpoint-libs/$VERSION" -o /dev/null -w "%{http_code}" || true)
if [ "$CRATES_IO_STATUS" = "200" ]; then
    echo "Version $VERSION is already published to crates.io, skipping."
else
    read -r -p "Publish $TAG to crates.io? [y/N] " CONFIRM
    if [[ "$CONFIRM" =~ ^[Yy]$ ]]; then
        cargo publish --manifest-path "$CARGO_TOML"
        echo "Published $TAG to crates.io"
    else
        echo "Skipping crates.io publish."
    fi
fi
