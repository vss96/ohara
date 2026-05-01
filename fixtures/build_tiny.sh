#!/usr/bin/env bash
# Builds fixtures/tiny/repo: a small synthetic git repo with three logical
# changes that the e2e test queries against.

set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
REPO="$HERE/tiny/repo"

rm -rf "$REPO"
mkdir -p "$REPO"
cd "$REPO"

git init -q -b main
git config user.email "fixture@ohara.test"
git config user.name "fixture"

cat > src.rs <<'EOF'
fn fetch(url: &str) -> String {
    String::from(url)
}
EOF
git add src.rs
GIT_COMMITTER_DATE="2024-01-01T00:00:00Z" git commit -q --date="2024-01-01T00:00:00Z" -m "initial fetch"

cat > src.rs <<'EOF'
fn fetch(url: &str) -> String {
    for attempt in 0..3 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(100 * (1 << attempt)));
        }
        // ...
    }
    String::from(url)
}
EOF
git add src.rs
GIT_COMMITTER_DATE="2024-02-01T00:00:00Z" git commit -q --date="2024-02-01T00:00:00Z" -m "add retry with exponential backoff"

cat > auth.rs <<'EOF'
fn login(user: &str, pass: &str) -> bool {
    !user.is_empty() && !pass.is_empty()
}
EOF
git add auth.rs
GIT_COMMITTER_DATE="2024-03-01T00:00:00Z" git commit -q --date="2024-03-01T00:00:00Z" -m "add basic login"

echo "fixture built at $REPO"
