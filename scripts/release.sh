#!/usr/bin/env bash
set -euo pipefail

# Usage: ./scripts/release.sh [--skip-bump] [--no-tag] <patch|minor|major|release>

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CARGO_TOML="$REPO_ROOT/Cargo.toml"
CRATE_NAME=$(grep '^name' "$CARGO_TOML" | head -1 | sed 's/.*"\(.*\)".*/\1/')

SKIP_BUMP=false
NO_TAG=false
while [[ "${1:-}" == --* ]]; do
    case "${1}" in
        --skip-bump) SKIP_BUMP=true ;;
        --no-tag)    NO_TAG=true ;;
        *) echo "Unknown option: $1" >&2; exit 1 ;;
    esac
    shift
done

LEVEL="${1:-}"
if [[ "$LEVEL" != "patch" && "$LEVEL" != "minor" && "$LEVEL" != "major" && "$LEVEL" != "release" ]]; then
    echo "Usage: $0 [--skip-bump] [--no-tag] <patch|minor|major|release>" >&2
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

if [ "$SKIP_BUMP" = false ]; then
    echo "Running cargo-release $LEVEL ..."
    cargo release --manifest-path "$CARGO_TOML" --execute --no-confirm "$LEVEL"
    echo ""
fi

# Read version (current if --skip-bump, bumped if not)
VERSION=$(grep '^version' "$CARGO_TOML" | head -1 | sed 's/.*"\(.*\)".*/\1/')
TAG="v$VERSION"

if [ -z "$VERSION" ]; then
    echo "Error: could not read version from Cargo.toml" >&2
    exit 1
fi

if [ "$NO_TAG" = false ]; then
    echo "Preparing tag notes for $TAG..."

    # Generate tag notes with git-cliff, open in editor for review
    TMPFILE=$(mktemp /tmp/release_notes.XXXXXX)
    git -C "$REPO_ROOT" cliff --latest --strip all > "$TMPFILE"

    EDITOR_CMD="${VISUAL:-${EDITOR:-}}"
    if [ -z "$EDITOR_CMD" ]; then
        for e in nano vim vi; do
            if command -v "$e" &>/dev/null; then
                EDITOR_CMD="$e"
                break
            fi
        done
    fi
    if [ -z "$EDITOR_CMD" ]; then
        echo "Error: no editor found. Set \$VISUAL or \$EDITOR." >&2
        rm -f "$TMPFILE"
        exit 1
    fi
    $EDITOR_CMD "$TMPFILE"

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
else
    # Push commit only
    echo "Pushing commit..."
    git -C "$REPO_ROOT" push origin HEAD
fi

echo ""

# Optionally publish to crates.io
CRATES_IO_STATUS=$(curl -sf "https://crates.io/api/v1/crates/$CRATE_NAME/$VERSION" -o /dev/null -w "%{http_code}" || true)
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
