# Context-engine eval fixture

A small synthetic git repo built by
[`tests/perf/build_context_eval_fixture.sh`](../../build_context_eval_fixture.sh)
into `target/perf-fixtures/context-engine-eval`. The eval runner
([`tests/perf/context_engine_eval.rs`](../../context_engine_eval.rs))
indexes it and runs every case in [`golden.jsonl`](golden.jsonl).

## Commit roles

`golden.jsonl` references commits by **label** (`expected_commit_labels`),
not by SHA. The runner resolves a label to a SHA at runtime by matching
the table below against `git log`. Edit the script intentionally and
the JSONL still works; rename a commit and the harness fails loudly.

| Label | Commit message | Touches | Why |
|---|---|---|---|
| `initial_skeleton_commit` | `initial: project skeleton` | `src/{lib,fetch,error,auth}.rs`, `README.md` | Baseline so later changes are real diffs. Not used by any golden case. |
| `readme_noise_commit` | `docs: expand README` | `README.md` | Pure noise. Ensures rank-1 isn't trivially "the only file ever changed". |
| `timeout_commit` | `fetch: add request timeout handling` | `src/fetch.rs` | Used by `timeout_handling_rust`. |
| `retry_backoff_commit` | `fetch: add retry with exponential backoff` | `src/fetch.rs` | Used by `retry_backoff_rust`. The canonical demo case. |
| `login_commit` | `auth: introduce login function` | `src/auth.rs` | Used by `symbol_lookup_login`. Bare symbol-name lookup. |
| `error_context_commit` | `error: wrap errors with context` | `src/error.rs` | Used by `error_wrapping_rust`. Tests semantic phrasing match without the symbol name in the query. |
| `logout_noise_commit` | `auth: stub logout` | `src/auth.rs` | Same-file noise so `symbol_lookup_login` can't trivially win on "the only commit touching auth.rs". |
| `config_loader_commit` | `config: load configuration from environment` | `app/config.py` | Used by `config_loading_python`. Cross-language sanity. |

## Determinism

Every commit pins `GIT_AUTHOR_DATE`, `GIT_COMMITTER_DATE`, author name,
and email so the SHAs are byte-stable across machines. The fixture
builder is idempotent: it skips rebuild when the HEAD message matches
the last expected commit (`config: load configuration from
environment`).

## Adding a case

1. Add the new commit to `build_context_eval_fixture.sh` (don't reuse
   message strings — the resolver matches on equality).
2. Update the table above.
3. Append the corresponding row to `golden.jsonl`, referencing the new
   label in `expected_commit_labels`.
4. Re-run `cargo test -p ohara-perf-tests -- --ignored
   context_engine_eval --nocapture` and paste the JSON summary into the
   PR.
