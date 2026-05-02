#!/usr/bin/env bash
# Plan 10 Task 1.2 — builds the deterministic context-engine eval fixture.
#
# Creates a small synthetic git repo at
# `target/perf-fixtures/context-engine-eval` whose commit messages match
# the labels in `tests/perf/fixtures/context_engine_eval/golden.jsonl`
# (the runner resolves label -> SHA via commit-message lookup so the
# JSONL doesn't have to pin hashes that change every time the script
# is edited).
#
# Idempotent: if HEAD already matches the final expected message, the
# script is a fast no-op.
#
# Stability: every commit pins GIT_AUTHOR_DATE / GIT_COMMITTER_DATE,
# author name, and email so the SHAs are byte-stable across machines.
# That stability is for human debuggability ("which sha was the
# retry commit?"); the harness itself doesn't depend on the hash.
#
# Usage:
#   ./tests/perf/build_context_eval_fixture.sh
#
# Run by `tests/perf/context_engine_eval.rs` automatically — manual
# invocation is for inspecting the fixture.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
DEST="$REPO_ROOT/target/perf-fixtures/context-engine-eval"

# The last commit's message — used to detect "already built" state.
FINAL_MESSAGE="config: load configuration from environment"

if [ -d "$DEST/.git" ]; then
  head_message="$(git -C "$DEST" log -1 --pretty=%s 2>/dev/null || true)"
  if [ "$head_message" = "$FINAL_MESSAGE" ]; then
    echo "context-engine eval fixture already built at $DEST — skipping"
    exit 0
  fi
  echo "fixture present but stale (HEAD message=\"$head_message\"); rebuilding"
  rm -rf "$DEST"
fi

mkdir -p "$DEST"
cd "$DEST"

git init -q -b main
git config user.email "fixture@ohara.test"
git config user.name "ohara-fixture"

commit() {
  # Stable author + committer date so SHAs are deterministic across hosts.
  local date="$1"
  local message="$2"
  GIT_AUTHOR_DATE="$date" \
  GIT_COMMITTER_DATE="$date" \
  GIT_AUTHOR_NAME="ohara-fixture" \
  GIT_AUTHOR_EMAIL="fixture@ohara.test" \
  GIT_COMMITTER_NAME="ohara-fixture" \
  GIT_COMMITTER_EMAIL="fixture@ohara.test" \
  git commit -q --date="$date" -m "$message"
}

mkdir -p src app

# ---------------------------------------------------------------------------
# Commit 1 — initial project skeleton. Establishes baseline files so each
# later change is a meaningful diff rather than a wholesale add.
# ---------------------------------------------------------------------------
cat > src/fetch.rs <<'EOF'
pub fn fetch(url: &str) -> String {
    String::from(url)
}
EOF
cat > src/error.rs <<'EOF'
#[derive(Debug)]
pub struct AppError(pub String);
EOF
cat > src/auth.rs <<'EOF'
// Auth module placeholder.
EOF
cat > src/lib.rs <<'EOF'
pub mod auth;
pub mod error;
pub mod fetch;
EOF
cat > README.md <<'EOF'
# eval-fixture
EOF
git add -A
commit "2024-01-01T00:00:00Z" "initial: project skeleton"

# ---------------------------------------------------------------------------
# Commit 2 — README touch-up. Pure noise: ensures the rank-1 retrieval
# decision can't be "the only file ever changed".
# ---------------------------------------------------------------------------
cat > README.md <<'EOF'
# eval-fixture

Synthetic repo used by ohara's plan-10 retrieval-quality harness.
EOF
git add README.md
commit "2024-01-15T00:00:00Z" "docs: expand README"

# ---------------------------------------------------------------------------
# Commit 3 — timeout_commit (golden id "timeout_handling_rust").
# ---------------------------------------------------------------------------
cat > src/fetch.rs <<'EOF'
use std::time::Duration;

pub fn fetch(url: &str) -> String {
    let _timeout = Duration::from_secs(5);
    // Per-request timeout: bail out after _timeout elapsed.
    String::from(url)
}
EOF
git add src/fetch.rs
commit "2024-02-01T00:00:00Z" "fetch: add request timeout handling"

# ---------------------------------------------------------------------------
# Commit 4 — retry_backoff_commit (golden id "retry_backoff_rust"). The
# canonical 'find_pattern' demo case.
# ---------------------------------------------------------------------------
cat > src/fetch.rs <<'EOF'
use std::time::Duration;

pub fn fetch(url: &str) -> String {
    let _timeout = Duration::from_secs(5);
    for attempt in 0..3 {
        if attempt > 0 {
            // Exponential backoff between retries.
            std::thread::sleep(Duration::from_millis(100 * (1 << attempt)));
        }
        // Attempt the request; on transient failure, retry.
    }
    String::from(url)
}
EOF
git add src/fetch.rs
commit "2024-03-01T00:00:00Z" "fetch: add retry with exponential backoff"

# ---------------------------------------------------------------------------
# Commit 5 — login_commit (golden id "symbol_lookup_login").
# ---------------------------------------------------------------------------
cat > src/auth.rs <<'EOF'
// Auth module.

pub fn login(user: &str, password: &str) -> bool {
    !user.is_empty() && !password.is_empty()
}
EOF
git add src/auth.rs
commit "2024-04-01T00:00:00Z" "auth: introduce login function"

# ---------------------------------------------------------------------------
# Commit 6 — error_context_commit (golden id "error_wrapping_rust"). Tests
# semantic phrasing match without the symbol name appearing in the query.
# ---------------------------------------------------------------------------
cat > src/error.rs <<'EOF'
use std::fmt;

#[derive(Debug)]
pub struct AppError {
    pub message: String,
    pub source: Option<Box<dyn std::error::Error + Send + Sync>>,
}

impl AppError {
    /// Wrap an underlying error with a human-readable context message.
    pub fn wrap<E>(message: impl Into<String>, source: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        AppError {
            message: message.into(),
            source: Some(Box::new(source)),
        }
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}
EOF
git add src/error.rs
commit "2024-05-01T00:00:00Z" "error: wrap errors with context"

# ---------------------------------------------------------------------------
# Commit 7 — unrelated noise touching auth.rs, so symbol_lookup_login
# can't trivially win on "only commit touching auth.rs".
# ---------------------------------------------------------------------------
cat >> src/auth.rs <<'EOF'

pub fn logout() {
    // Stub.
}
EOF
git add src/auth.rs
commit "2024-05-15T00:00:00Z" "auth: stub logout"

# ---------------------------------------------------------------------------
# Commit 8 — config_loader_commit (golden id "config_loading_python").
# Cross-language sanity for the Python tree-sitter chunker.
# ---------------------------------------------------------------------------
cat > app/config.py <<'EOF'
"""Application configuration loader."""

import os


def load_config_from_env() -> dict:
    """Load configuration from environment variables."""
    return {
        "database_url": os.environ.get("DATABASE_URL", ""),
        "log_level": os.environ.get("LOG_LEVEL", "INFO"),
        "cache_ttl": int(os.environ.get("CACHE_TTL", "60")),
    }
EOF
git add app/config.py
commit "2024-06-01T00:00:00Z" "config: load configuration from environment"

echo "context-engine eval fixture built at $DEST (HEAD=$(git rev-parse HEAD))"
