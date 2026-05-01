# Fixtures

The `tiny/repo` directory is built by `./build_tiny.sh` and is not committed.
Run the script before invoking the e2e test, or let the test invoke it.

The fixture has three commits:
1. `initial fetch` - a `fetch` function returning a String
2. `add retry with exponential backoff` - introduces retry logic with sleeps
3. `add basic login` - introduces an unrelated `login` function in auth.rs

A `find_pattern` query for "retry with backoff" should return commit 2 first.
