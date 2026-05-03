#!/usr/bin/env bash
# Builds fixtures/medium/repo: a shallow clone of ripgrep at tag 14.1.1
# used by the perf harness binaries (cli_query_bench, mcp_query_bench).
#
# Idempotent: re-running re-uses the existing checkout. The first
# successful clone records the resolved tag SHA into
# fixtures/medium/.fixture-sha; subsequent runs assert it matches so
# upstream tag re-points are caught by the harness rather than
# silently shifting numbers.
#
# Run:
#   fixtures/build_medium.sh
#
# Wipe and re-clone:
#   rm -rf fixtures/medium && fixtures/build_medium.sh

set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
DEST="$HERE/medium/repo"
SHA_FILE="$HERE/medium/.fixture-sha"
TAG="${OHARA_RIPGREP_TAG:-14.1.1}"
URL="${OHARA_RIPGREP_URL:-https://github.com/BurntSushi/ripgrep.git}"

mkdir -p "$HERE/medium"

if [ ! -d "$DEST/.git" ]; then
    echo "[medium] cloning $URL @ $TAG"
    # Shallow clone; depth 5000 covers the full ripgrep history at
    # 14.1.1 (~3500 commits) plus headroom for future tag bumps.
    git clone --depth 5000 --branch "$TAG" "$URL" "$DEST"
fi

resolved="$(git -C "$DEST" rev-parse HEAD)"

if [ -f "$SHA_FILE" ]; then
    expected="$(cat "$SHA_FILE")"
    if [ "$resolved" != "$expected" ]; then
        echo "[medium] fixture SHA drift: expected $expected, got $resolved" >&2
        echo "[medium] either upstream re-pointed $TAG or your checkout is stale." >&2
        echo "[medium] inspect with: git -C $DEST log -1 --oneline" >&2
        exit 1
    fi
else
    echo "[medium] recording fixture SHA: $resolved"
    echo "$resolved" > "$SHA_FILE"
fi

echo "[medium] ready: $DEST @ $resolved"
