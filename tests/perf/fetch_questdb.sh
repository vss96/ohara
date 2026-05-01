#!/usr/bin/env bash
# Shallow-clone QuestDB into target/perf-fixtures/questdb at a pinned
# SHA so the v0.6 throughput benchmark (Plan 6 Phase 1) has a stable
# reference fixture without vendoring a 200MB+ pack into the source
# tarball.
#
# Idempotent: if the fixture is already present at the pinned SHA, the
# script is a fast no-op. Override the SHA via OHARA_QUESTDB_SHA when
# bumping the pin (e.g. after a baseline re-run).
#
# Usage:
#   ./tests/perf/fetch_questdb.sh
#   OHARA_QUESTDB_SHA=<sha> ./tests/perf/fetch_questdb.sh

set -euo pipefail

# Pinned to a recent QuestDB main commit. Bump (and re-run the
# baseline in docs/perf/v0.6-baseline.md) when the upstream surface
# changes enough that the numbers stop being comparable. Override at
# call site with OHARA_QUESTDB_SHA=<sha> ./fetch_questdb.sh.
PINNED_SHA="${OHARA_QUESTDB_SHA:-c9f79257c8acaaab1d05e8c6614a37c810c2d1b6}"
REPO_URL="https://github.com/questdb/questdb"

# Resolve the repo root from this script's location so the fixture
# always lands in the workspace's `target/` regardless of cwd.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
DEST="$REPO_ROOT/target/perf-fixtures/questdb"

mkdir -p "$REPO_ROOT/target/perf-fixtures"

if [ -d "$DEST/.git" ]; then
  current="$(git -C "$DEST" rev-parse HEAD 2>/dev/null || true)"
  if [ "$current" = "$PINNED_SHA" ]; then
    echo "questdb fixture already at pinned SHA $PINNED_SHA — skipping clone"
    exit 0
  fi
  echo "questdb fixture present but at $current; refetching to $PINNED_SHA"
  rm -rf "$DEST"
fi

# Shallow clone of just the pinned commit. `--filter=tree:0` keeps the
# pack small until git2 actually needs the trees during the index walk.
echo "shallow-cloning questdb @ $PINNED_SHA into $DEST"
git clone --no-checkout --filter=tree:0 "$REPO_URL" "$DEST"
git -C "$DEST" fetch --depth 1 origin "$PINNED_SHA"
git -C "$DEST" checkout "$PINNED_SHA"

echo "questdb fixture ready at $DEST (HEAD=$(git -C "$DEST" rev-parse HEAD))"
