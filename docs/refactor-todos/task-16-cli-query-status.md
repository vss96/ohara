# Task 16: `ohara query` + `ohara status` — refactor backlog

Captured at HEAD `f524ad5`. Long-tail items only; spec compliance and gating
quality issues are owned by parallel reviewers. Items already raised in the
Task 15 backlog (notably `RepoContext` hoist and smoke-test expansion) are
not duplicated here. Items in Task 17+ proper scope (MCP server) are out of
scope. `cargo clippy -p ohara-cli --all-targets` is clean at this HEAD.

---

### 1. `query` pays the FastEmbed cold-start cost on every invocation

- **Severity:** Medium
- **Location:** `crates/ohara-cli/src/commands/query.rs:26-28`
- **What:** Every `ohara query` builds a fresh `FastEmbedProvider` —
  ~30 MB BGE-small ONNX weights, ~1–2 s warm, longer cold — for a
  single 384-d encode. Interactive tweak-the-query loops feel sluggish;
  shell `for` loops are unworkable.
- **Why:** The MCP server (Task 17) keeps the provider hot for free.
  The CLI won't, until a daemon mode exists. Shapes the answer to "is
  `ohara query` the right surface for repeated experimentation?" — for
  v1, no; it's a debug helper.
- **Suggestion:** Defer. Post-v1, route the CLI through a daemon. Until
  then, mention in `--help` that this is a debug command and add a
  `tracing::info!` timing the embedder build.
- **Effort:** S (doc + tracing) / L (real fix)

### 2. `query` output is JSON; `status` output is free-form text — inconsistent

- **Severity:** Medium
- **Location:** `crates/ohara-cli/src/commands/query.rs:39`,
  `crates/ohara-cli/src/commands/status.rs:26-33`
- **What:** Two sibling subcommands emit incompatible formats with no
  toggle: `query` always JSON, `status` always text. Scripts that want
  both together (e.g., "index, then check status, then query") need two
  parsers.
- **Why:** A `--json` flag on each command (defaulting to JSON for
  `query`, text for `status` to preserve current behaviour) gives a
  uniform scriptable surface without breaking humans. Pairs naturally
  with the `tracing-on-summary` thread from Task 15 #4 / Task 11 #4.
- **Suggestion:** Add `#[arg(long)] json: bool` on `status::Args` first
  (cheap, additive); reuse `IndexStatus` from `ohara_core::query` as
  the JSON shape — it already exists and matches what the MCP server
  will emit via `index_status_meta`.
- **Effort:** S

### 3. CLI does not expose `--since` despite `PatternQuery::since_unix`

- **Severity:** Low
- **Location:** `crates/ohara-cli/src/commands/query.rs:35`
- **What:** `since_unix` is hard-wired to `None`. The plumbing all the
  way down to `hunk::knn` already reads it (plan §1926, §2032), so the
  CLI is the only piece blocking the user from filtering "patterns from
  the last 30 days." Spec doesn't *require* CLI exposure (it's a debug
  helper), but the missing flag is a footgun for anyone testing recency
  weighting.
- **Suggestion:** Add `#[arg(long)] since: Option<String>` accepting an
  ISO 8601 date or relative (`30d`, `2w`); parse to unix seconds. A
  `humantime`-style parser is overkill — `chrono::DateTime::parse_from_rfc3339`
  + a tiny regex for `Nd`/`Nw` is enough.
- **Effort:** S

### 4. `commits_behind_head` walks the entire history when no watermark exists

- **Severity:** Medium
- **Location:** `crates/ohara-cli/src/commands/status.rs:21-24`
- **What:** When `last_indexed_commit` is `None` (fresh repo, never
  indexed) we call `list_commits(None)` and `.len()` — that materialises
  every `CommitMeta` (sha, parent, author, ts, message) just to discard
  them and read a count. On a 100k-commit repo this is multiple seconds
  and a non-trivial allocation spike, paid every time someone runs
  `ohara status`.
- **Why:** Worse, the same shape will be duplicated by
  `OharaServer::index_status_meta` (plan §3423-3429), so the cost will
  surface on *every* MCP find_pattern response that includes the meta.
  Fixing in one place fixes both.
- **Suggestion:** Add `GitWalker::count_commits(since: Option<&str>)`
  that runs the revwalk but only `.count()`s OIDs — no `find_commit`,
  no allocation per row. Use it from `status::run` and from the future
  `index_status_meta`. (libgit2 doesn't expose `git rev-list --count`
  natively, but skipping the per-commit `find_commit` recovers ~all of
  the gain.)
- **Effort:** S

### 5. `commits_behind_head` logic will be duplicated in MCP `index_status_meta`

- **Severity:** Medium
- **Location:** `crates/ohara-cli/src/commands/status.rs:19-24`
  (mirrors plan §3423-3434 verbatim)
- **What:** The five-line `match st.last_indexed_commit { ... }` block
  is byte-for-byte identical to the planned MCP server's
  `index_status_meta`. When Task 17 lands the duplication is locked in;
  any change to the "behind" definition (e.g., excluding merge commits,
  capping at 10k for huge repos) has to be made twice.
- **Why:** The natural home is a free function on `ohara_git` or a
  helper on `ohara_core::query::IndexStatus`. Both call sites already
  depend on `ohara_git`.
- **Suggestion:** When Task 17 lands, extract
  `pub fn commits_behind(walker: &GitWalker, watermark: Option<&str>) -> Result<u64>`
  in `ohara-git`; call from both. Pairs with item #4 (do them
  together — same function). Flagging now so the MCP author knows not
  to copy-paste.
- **Effort:** S (when Task 17 triggers)

### 6. No tests for `query::run` or `status::run`

- **Severity:** Medium
- **Location:** `crates/ohara-cli/tests/index_smoke.rs` (only test file)
- **What:** Task 16 added zero tests. `query::run`'s JSON shape and
  `status::run`'s text template are both implicit contracts that
  scripts will depend on. A typo'd field name or a swapped `println!`
  arg ships unnoticed. Distinct from Task 15 #5 (helper-level): these
  are async commands depending on storage + walker.
- **Suggestion:** Extend the existing smoke test (Task 15 #6 is the
  parent) to: (a) call `status::run` after `index::run` and parse its
  output to assert `last_indexed_commit` is non-`<none>`, (b) call
  `query::run` with a known query and parse the JSON array. Keep
  `#[ignore]` until the model-cache story is resolved.
- **Effort:** S

### 7. `IndexStatus` type exists in `ohara-core` but `status` builds ad-hoc `println!`

- **Severity:** Low
- **Location:** `crates/ohara-cli/src/commands/status.rs:26-33` vs
  `crates/ohara-core/src/query.rs:29-34`
- **What:** `ohara-core::query::IndexStatus { last_indexed_commit,
  commits_behind_head, indexed_at }` is shaped exactly for this output,
  but `status::run` ignores it and prints raw fields. The MCP server's
  `index_status_meta` (Task 17) will serialize that very type — so
  adopting it here now prevents drift in the JSON shape later.
- **Suggestion:** Build an `IndexStatus` value, `Display`-it for humans
  or `serde_json::to_string_pretty` for `--json`. Couples cleanly with #2.
- **Effort:** XS

---

### See also

- `cargo clippy -p ohara-cli --all-targets` — already clean at HEAD
  `f524ad5`. No inherited lint warnings to flag here.
- Cross-task: items #4 + #5 should land together with Task 17 (MCP
  `index_status_meta`); the shared helper avoids the duplication
  before it's introduced.
- Cross-task: item #2 (`--json` flag) extends the tracing/structured-output
  thread from Task 15 #4, Task 14 #7, Task 13 #3, Task 12 #6, Task 11 #4.
- Inherited from Task 15 (still open, still relevant here):
  - #5 (helper-level tests) — `resolve_repo_id` now has 3 callers.
  - #6 (smoke-test expansion) — item #6 above is the concrete Task 16
    follow-through.
  - #7 (`RepoHandle`/`RepoContext` hoist) — `query`, `status`, and
    `index` now triplicate the `resolve_repo_id` + `index_db_path` +
    `SqliteStorage::open` boilerplate. Three call sites is the
    canonical "extract" trigger.
- Time-sensitive: #4 + #5 before Task 17 lands. #2 + #3 + #6 anytime;
  they compound the longer the CLI ships without them.
