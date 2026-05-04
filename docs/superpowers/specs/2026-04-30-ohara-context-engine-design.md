# ohara — Context Lineage Engine, v1 Design

**Date:** 2026-04-30
**Status:** Draft, awaiting user approval
**Authors:** Vikas, Claude

## 1. Problem

Coding agents like Claude Code have full access to the *current* state of a codebase but no first-class view of how it got there. When a developer asks "add retry logic like we did before" or "why is this code here", the agent has to fall back on filesystem grep or guess. Augment-style "context engines" close this gap by indexing not just current code but its *lineage* — the commits, diffs, and rationale that produced it — and surfacing the right historical evidence at query time.

ohara is a local-first context lineage engine, written in Rust, that exposes this capability to Claude Code (and any other MCP client) as structured tools. Its differentiator is lineage: pattern reuse from history and change tracing, not generic code search.

## 2. Goals & non-goals

### In scope (v1)

- Two MCP tools served by `ohara-mcp`:
  - `find_pattern` — "how was X done before" pattern reuse from git history.
  - `explain_change` — "why is this code here / what introduced it" change tracing.
- A `ohara` CLI for indexing, querying, admin, and git-hook installation.
- Local-first per-developer index living at `~/.ohara/<repo-id>/`.
- Local embeddings via `fastembed-rs` (no network dependency by default).
- Git-hook driven incremental indexing + lazy MCP startup check.
- Provenance tagging (`EXTRACTED` / `INFERRED`) on every retrieved fact.
- Architecture engineered so a shared-index server (Section 3) is a thin frontend later — same Rust core.

### Explicitly out of scope (v1)

- Pattern clustering / pattern naming.
- Iterative / agentic retrieval loops.
- Reranker model — basic recency + similarity ranking only.
- File-watch / live working-tree awareness.
- The shared-index server itself (we design *for* it; we don't build it).
- ~~Multi-repo indexing in a single instance.~~ *(Shipped in v0.7.5 — see plan-16.)*
- IDE plugin, slash-command skill (post-v1).
- LLM-based intent extraction from commit messages.

### Success criteria

- On a real 5–50k commit repo, `find_pattern("retry with backoff")` returns the actual past commits where retry was added, ranked by relevance, with diff + commit message + the resulting code.
- `explain_change(path, line)` returns the introducing commit + last 3 modifying commits with messages.
- Cold index of a 10k-commit repo finishes in under 10 minutes on a modern laptop.
- Warm query latency p95 under 200 ms.

## 3. Architecture overview

### Cargo workspace layout

```
ohara/
├── crates/
│   ├── ohara-core/        ← library: indexing, retrieval, storage traits
│   ├── ohara-storage/     ← SQLite + sqlite-vec + FTS5 impls of the storage trait
│   ├── ohara-embed/       ← fastembed-rs impl of the EmbeddingProvider trait
│   ├── ohara-git/         ← git2-rs wrapper: walk commits, parse diffs, blame
│   ├── ohara-parse/       ← tree-sitter wrapper: function/class extraction per language
│   ├── ohara-cli/         ← `ohara` binary
│   └── ohara-mcp/         ← `ohara-mcp` binary (using `rmcp` for MCP protocol)
└── Cargo.toml             ← workspace root
```

`ohara-core` holds orchestration (`Indexer`, `Retriever`, query types) and depends on traits, not concrete implementations. Storage / embed / parse / git in separate crates so each can be swapped, tested in isolation, and feature-gated. Two thin binary crates so neither pulls in the other's deps.

### Runtime topology

```
       ┌────────────────────────┐
       │ Claude Code (process)  │
       └──────────┬─────────────┘
                  │ MCP over stdio (spawned subprocess)
       ┌──────────▼─────────────┐
       │ ohara-mcp              │  read-only against the index
       │   - find_pattern       │
       │   - explain_change     │
       └──────────┬─────────────┘
                  │ ohara-core API (read)
       ┌──────────▼─────────────┐
       │ Index at                │
       │ ~/.ohara/<repo-id>/    │  ← SQLite + FTS5
       │   index.sqlite         │
       └──────────▲─────────────┘
                  │ ohara-core API (read+write)
       ┌──────────┴─────────────┐
       │ ohara CLI               │  invoked by user OR git hooks
       │   ohara init            │
       │   ohara index           │
       │   ohara query           │
       │   ohara status          │
       └────────────────────────┘
```

### Key invariants

- **`ohara-mcp` is read-only.** It never indexes, never mutates SQLite. If the index is stale, it surfaces that fact in tool responses but does not try to fix it itself. Indexing in the MCP request path would blow latency budgets unpredictably and create lock contention.
- **Indexing happens via the CLI, period.** Triggered by user, by git hooks, or by a future cron / shared-index server. One write path; simpler concurrency story.
- **`<repo-id>` is `hash(first_commit_sha + canonical_path)`.** Stable across renames; unique across multiple clones of the same repo on the same machine.
- **Originally no daemon (v1).** The MCP server lived only as long as Claude Code kept it spawned. As of v0.7.5 / plan-16, `ohara serve` provides an optional Unix-socket daemon that keeps the embedder and per-repo storage warm; indexing remains one-shot CLI invocations. See plan-16 for the daemon design and shipping commit (21bc1af).

### Concurrency model

- Tokio multi-threaded runtime in both binaries.
- Indexing: rayon for CPU-bound parsing/embedding, tokio for git2 and SQLite I/O. SQLite in WAL mode handles writes; transactions batched ~512 commits.
- MCP queries are read-only, served from a connection pool. WAL allows many concurrent readers.

### Path to option B (shared index server)

Post-v1, swap the CLI's indexing call for a `ohara-server` binary that wraps the same `ohara-core` Indexer behind tonic gRPC, exposes a `ReadIndex` service consumed by a remote `ohara-mcp` variant. Zero changes in `ohara-core`. This is the entire reason for the trait boundaries in Section 3's crate layout.

## 4. How Claude Code finds and uses ohara

Discoverability is layered. No single mechanism is sufficient — they stack.

### Layer 1 — Tool descriptions

The `description` field on each MCP tool is the only thing Claude reads when deciding whether to call it. Vague descriptions get random use; trigger-specific descriptions get reliable use. Both `USE WHEN` and `DO NOT USE` lines are required:

```
find_pattern:
Search this project's git history for past implementations of similar logic.

USE WHEN the user:
  • asks "how did we do X before" / "is there a pattern for Y"
  • requests adding a feature similar to existing functionality
    ("add retry like we did before", "make this look like the auth flow")
  • is about to write code that likely has prior art in this repo

DO NOT USE for searching current code — use Grep/Read for that.
DO NOT USE for general programming questions.

Returns: historical commits with diffs, commit messages, file paths,
similarity score, and provenance (always INFERRED — semantic match).
```

### Layer 2 — MCP server-level instructions

ohara-mcp ships a server-level instruction block (the same surface `context7` uses) injected into the system prompt for any MCP-aware client:

```
Use this server when the user is implementing, modifying, or asking about
code that likely has historical precedent in this repository. Lineage is
ohara's specialty — for "how was this done before", "trace this change", or
"add a feature like an existing one", prefer ohara over generic search.
Do not use for code that has no git history (new files, fresh repos).
```

### Layer 3 — `ohara init` writes a CLAUDE.md stanza

With user consent, `ohara init` appends to (or creates) the repo's `CLAUDE.md`:

```markdown
## Code lineage (ohara)
This repo is indexed by ohara. Before implementing a new feature or
modifying existing logic, call `ohara.find_pattern` to surface prior
implementations. When fixing bugs in existing code, call
`ohara.explain_change` to understand intent before changing behavior.
```

Repo-level CLAUDE.md is the highest-priority instruction surface in Claude Code.

### Layer 4 — Result quality is a feedback loop

Within a session, Claude learns from tool results. Two implications:
- The "no results" response always includes a `hint` field describing what *was* searched (`"index covers 12,450 commits since 2022-01-15"`), not just an empty array.
- Stale-index responses include `hint: "Index is N commits behind HEAD; run \`ohara index\`"`.

### Layer 5 (post-v1) — Slash command / skill

A `/ohara` Claude Code skill that wraps the MCP tools for explicit invocation. Additive; not in v1.

### Layer 6 (deferred indefinitely) — UserPromptSubmit hook

A hook that proactively reminds Claude to call `find_pattern` on relevant prompts. Aggressive; only build if observed underuse warrants it.

### v1 commitment

Layers 1, 2, 3, 4 are all in v1.

### Sharp trap to avoid

`find_pattern` must NOT be a generic "search the repo" tool. Naming and description must scope it to *historical / lineage* queries. Current-code search stays with Grep/Read. If `find_pattern` becomes a Grep replacement, results degrade and trust collapses.

## 5. Data model

```sql
-- repository identity
repo (
  id TEXT PRIMARY KEY,            -- hash(first_commit_sha + canonical_path)
  path TEXT NOT NULL,
  first_commit_sha TEXT NOT NULL,
  last_indexed_commit TEXT,       -- watermark for incremental indexing
  schema_version INTEGER NOT NULL
)

-- commits, with embedded message
commit (
  sha TEXT PRIMARY KEY,
  parent_sha TEXT,                 -- first parent only; merges noted via flag
  is_merge BOOL NOT NULL,
  ts INTEGER NOT NULL,             -- author time, unix
  author TEXT,
  message TEXT NOT NULL,
  message_emb BLOB                 -- via sqlite-vec; nullable if embed deferred
)

-- file paths over time (rename-aware)
file_path (
  id INTEGER PRIMARY KEY,
  path TEXT NOT NULL,
  language TEXT,                   -- 'rust' | 'python' | ...
  active BOOL NOT NULL             -- false if deleted at HEAD
)

-- function / class / etc. extracted by tree-sitter at HEAD
symbol (
  id INTEGER PRIMARY KEY,
  file_path_id INTEGER NOT NULL,
  kind TEXT NOT NULL,              -- 'function' | 'class' | 'method' | 'const'
  name TEXT NOT NULL,
  qualified_name TEXT,             -- e.g. 'mymod::Foo::bar'
  span_start INTEGER NOT NULL,     -- byte offsets in HEAD blob
  span_end INTEGER NOT NULL,
  blob_sha TEXT NOT NULL,          -- HEAD blob this came from
  source_text TEXT NOT NULL,
  source_emb BLOB
)

-- one row per file touched in a commit (the heart of lineage)
hunk (
  id INTEGER PRIMARY KEY,
  commit_sha TEXT NOT NULL,
  file_path_id INTEGER NOT NULL,
  change_kind TEXT NOT NULL,       -- 'added' | 'modified' | 'deleted' | 'renamed'
  diff_text TEXT NOT NULL,
  diff_emb BLOB,                   -- the lineage retrieval signal
  symbol_ids_json TEXT             -- best-effort attribution to symbols at commit time
)

-- materialized "introduced by" / "last modified by" for HEAD symbols (capability B)
symbol_lineage (
  symbol_id INTEGER PRIMARY KEY,
  introducing_commit_sha TEXT NOT NULL,
  last_modified_commit_sha TEXT NOT NULL,
  modification_count INTEGER NOT NULL
)

-- provenance + confidence on every inferred edge
edge (
  from_kind TEXT NOT NULL, from_id TEXT NOT NULL,
  to_kind   TEXT NOT NULL, to_id   TEXT NOT NULL,
  edge_kind TEXT NOT NULL,         -- 'introduced_by' | 'modified_by' | 'similar_to' | 'calls'
  provenance TEXT NOT NULL,        -- 'EXTRACTED' | 'INFERRED'
  confidence REAL                  -- null for EXTRACTED, 0..1 for INFERRED
)

-- content-hash cache so re-runs skip unchanged blobs
blob_cache (
  blob_sha TEXT PRIMARY KEY,
  symbols_json TEXT,
  embedding_model TEXT,            -- model swaps invalidate cache entries
  embedded_at INTEGER
)

-- vector indices via sqlite-vec
CREATE VIRTUAL TABLE vec_symbol USING vec0(symbol_id  INTEGER PRIMARY KEY, source_emb  FLOAT[384]);
CREATE VIRTUAL TABLE vec_hunk   USING vec0(hunk_id    INTEGER PRIMARY KEY, diff_emb    FLOAT[384]);
CREATE VIRTUAL TABLE vec_commit USING vec0(commit_sha TEXT    PRIMARY KEY, message_emb FLOAT[384]);

-- FTS5 for keyword search
CREATE VIRTUAL TABLE fts_commit USING fts5(sha UNINDEXED, message);
CREATE VIRTUAL TABLE fts_symbol USING fts5(symbol_id UNINDEXED, qualified_name, source_text);
```

### Modeling notes

- **Symbols are HEAD-only.** A `symbol` row exists for what's currently in the working tree. Historical versions of functions live in `hunk` rows (as diff text). Symbol-level historical replay would 10x storage and add real complexity, and the hunk-level view is what `find_pattern` actually needs anyway.
- **Hunks are the hot table.** Both `find_pattern` (semantic search over `diff_emb`) and `explain_change` (walk hunks for a file path) read from this table. Indexed on `(file_path_id, commit_sha)` and `(commit_sha)`.
- **`symbol_lineage` is materialized.** Computed during indexing so `explain_change` is a single PK lookup, not a recursive CTE walk every query. Recomputed incrementally on each new commit.
- **`edge` is generic** so we don't proliferate join tables. Slight type-safety cost; big schema-stability win.
- **384-dim embeddings** match `BAAI/bge-small-en-v1.5`, the default fastembed-rs model. Larger-dim models are pluggable but require schema-versioned reindex.

## 6. Indexing pipeline

### Lineage extraction strategy: hybrid

Three options were considered:

- **Blame-only.** Cheap, gives capability B, fails capability A entirely (no historical diffs to embed).
- **Full per-commit AST replay.** Complete picture, but 80% of the cost for 20% of the marginal value over diff-embedding.
- **Hybrid (chosen):** walk commits and embed diffs (best-effort symbol attribution); use `git blame` on HEAD blobs to populate `symbol_lineage`.

Diff embedding gives `find_pattern` complete lineage signal even for code that no longer exists at HEAD. Blame gives `explain_change` accurate, ground-truth attribution for HEAD code. Each tool uses the right primitive.

### Pipeline stages

```
  ┌─────────────┐
  │ git walk    │  parents-first, batch of N=512 commits
  └──────┬──────┘
         ▼
  ┌─────────────┐
  │ diff extract│  per commit: list of (file_path, change_kind, diff_text)
  └──────┬──────┘
         ▼
  ┌─────────────┐    ┌──────────────┐
  │ blob cache  │───▶│ skip if seen │
  └──────┬──────┘    └──────────────┘
         ▼
  ┌─────────────┐
  │ embed batch │  fastembed-rs, batched per 32 hunks
  └──────┬──────┘
         ▼
  ┌─────────────┐
  │ persist     │  one transaction per N commits
  └─────────────┘

   (separately, after walk completes)

  ┌─────────────┐
  │ HEAD parse  │  tree-sitter over HEAD blobs → symbol rows
  └──────┬──────┘
         ▼
  ┌─────────────┐
  │ blame walk  │  git blame per HEAD symbol's span → symbol_lineage rows
  │             │  (cached by (file_blob_sha, span); cap blame to active symbols)
  └─────────────┘
```

### Incremental path (common case after first index)

1. Read `last_indexed_commit` from `repo`.
2. `git rev-list <last>..HEAD` → new commits since last index.
3. Run only the diff/embed/persist legs for new commits.
4. Reparse HEAD; recompute `symbol_lineage` for *changed* symbols only (detected via `blob_sha` mismatch).
5. Update `last_indexed_commit`.

Target: a 5-commit incremental update finishes in under 2 seconds. This matters because `post-commit` hooks run synchronously on commit.

### Indexing trigger lifecycle

- `ohara init` installs `post-commit` and `post-merge` git hooks (after asking for consent), then kicks off the initial full index in the foreground.
- Hooks invoke `ohara index --incremental --background` which forks a worker, writes a PID + log file under `~/.ohara/<repo-id>/`, and returns to the shell in under 50 ms. The background worker takes the SQLite write lock; concurrent hook invocations no-op if a worker is already running.
- `ohara-mcp` on launch checks the watermark vs HEAD and surfaces staleness via tool responses (Layer 4 above). It does NOT index.

### Concurrency

- rayon for parsing+embedding (CPU-bound, parallelizable).
- Single SQLite writer (WAL allows concurrent readers; writes serialized — fine, throughput is gated by embedding compute).

### Failure mode

If indexing crashes mid-batch, the watermark is not advanced; the next run reindexes from the last committed transaction. Idempotent.

## 7. MCP tool surface

Two tools in v1.

```rust
// tool 1
find_pattern(
  query: String,                    // natural-language description
  k: u8 = 5,                        // results to return; clamped 1..=20
  language: Option<String>,         // filter by detected language
  since: Option<String>,            // ISO date or "30d"; default = no limit
) -> Vec<PatternHit>

PatternHit {
  commit_sha: String,
  commit_message: String,
  commit_author: String,
  commit_date: String,              // ISO 8601
  file_path: String,
  change_kind: String,              // 'added' | 'modified' | 'deleted' | 'renamed'
  diff_excerpt: String,             // first 80 lines; if truncated, ends with "\n... (N more lines)"
  diff_truncated: bool,             // true if the original hunk exceeded 80 lines
  related_head_symbols: Vec<String>, // qualified names still present at HEAD
  similarity: f32,                  // 0..1
  recency_weight: f32,              // 0..1
  combined_score: f32,
  provenance: String,               // 'INFERRED' (always — semantic match)
}

// tool 2
explain_change(
  file_path: String,
  line: Option<u32>,                // if omitted, explains the whole file's history
) -> ChangeExplanation

ChangeExplanation {
  introducing_commit: CommitInfo,
  last_modified_commit: CommitInfo,
  recent_modifications: Vec<CommitInfo>,  // up to 5
  symbol_context: Option<SymbolInfo>,     // if line resolved to a symbol
  provenance: String,                     // 'EXTRACTED' (blame is ground truth)
}
```

### Ranking for `find_pattern`

```
combined_score =   0.7 * cosine(query_emb, hunk.diff_emb)
                 + 0.2 * exp(-age_days / 365)
                 + 0.1 * cosine(query_emb, commit.message_emb)
```

Coefficients are config knobs; defaults chosen to weight semantic match heavily but prevent ancient commits from dominating.

### Status surfaces

Every response includes a `_meta` field:

```json
"_meta": {
  "index_status": {
    "last_indexed_commit": "abc123...",
    "commits_behind_head": 7,
    "indexed_at": "2026-04-30T12:34:00Z"
  }
}
```

If `commits_behind_head > 50`, the response also includes `hint: "Index is stale; run \`ohara index\`"`.

## 8. Storage details

- **Path:** `~/.ohara/<repo-id>/index.sqlite`. Optional `~/.ohara/<repo-id>/blobs.cache/` for spillover if blob cache grows large.
- **SQLite settings on open:** `journal_mode=WAL`, `synchronous=NORMAL`, `mmap_size=268435456` (256 MB), `cache_size=-64000` (64 MB), `temp_store=MEMORY`.
- **Migrations:** `refinery` crate. Version stamped in `repo.schema_version`; mismatch requires `ohara reindex`.
- **`sqlite-vec`:** loaded as a runtime extension, statically linked into the binaries.
- **FTS5:** built into SQLite, no extra crate. Sufficient for v1 keyword search. Tantivy considered for v1.x if BM25 quality matters more than expected.
- **Connection pool:** `deadpool-sqlite`. Many readers, one writer.

## 9. Embedding

- **Default:** `fastembed-rs` with `BAAI/bge-small-en-v1.5` (384d). Model files cached at `~/.ohara/models/`.
- **Trait:**
  ```rust
  pub trait EmbeddingProvider: Send + Sync {
      fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;
      fn dimension(&self) -> usize;
      fn model_id(&self) -> &str;
  }
  ```
- **Why a trait if we ship only one impl in v1:** post-v1 the shared-index server (option B) likely wants `voyage-code-3` for higher quality. Trait now means zero core changes later.
- **Model swap invalidates `blob_cache`.** Cache rows tagged with `embedding_model`; entries from a different model are ignored.

## 10. Languages v1

Tree-sitter grammar bindings exist for ~all popular languages; the work is per-language symbol extraction queries.

**v1 ships with:** Rust, Python, TypeScript, JavaScript, Go.

Files in unsupported languages still get hunk-embedded — they just don't get symbol-level extraction or blame attribution. Lineage retrieval (`find_pattern`) works on every file regardless of language; symbol-level features (`explain_change` resolving to a symbol, `qualified_name` filters) are gated to the supported five.

Adding a language post-v1 is a new file in `ohara-parse` plus a tree-sitter query — no core changes.

## 11. Testing

- **Unit tests** in each crate against in-memory fixtures.
- **Indexing fixture repos.** A `fixtures/` directory with synthetic git repos (10 commits, 1k commits, optional 10k for slow CI). Hand-traced ground truth for capability B in the small repo.
- **Snapshot tests for retrieval** using `insta`. A query like `"retry with backoff"` against a fixture repo returns a stable ranked list; ranking changes show up as snapshot diffs.
- **Property tests with `proptest`** for diff-attribution: renames, binary files, merges, empty commits.
- **End-to-end MCP test.** Spawn `ohara-mcp`, send MCP requests over stdio, assert tool output shape. Catches drift between the wire contract and `ohara-core`.
- **Real-repo smoke test** (gated, opt-in): index `tokio-rs/tokio` or `cli/cli`, assert basic invariants on commit count, latency.

## 12. Risks & open questions

| Risk | Mitigation |
|---|---|
| 384d local embeddings underperform on code-specific retrieval | Pluggable trait; switch to `voyage-code-3` per-deployment. Measure with snapshot tests. |
| Hunk-level attribution to symbols is best-effort and noisy | We don't promise symbol-level historical replay; capability A surfaces *commits* (with diffs and HEAD-symbols-still-affected), not historical-symbol-state. |
| `git blame` on huge files is slow | Cap blame to active HEAD symbols; cache blame results keyed on `(file_blob_sha, symbol_span)`. |
| `post-commit` hook latency surprises users | Hook runs `ohara index --incremental --background`; exits in under 50 ms. Background process logs to `~/.ohara/<repo-id>/index.log`. |
| Index path collisions across users on shared filesystems | `<repo-id>` includes canonical path; future shared-index server keys by repo URL instead. |
| Renames not handled by basic diff | Use `git log --follow` semantics during walk; record rename edges in `file_path` history. |

## 13. v1 milestone plan (high level)

Deliberately rough; the writing-plans skill will turn this into a step-by-step implementation plan.

1. Workspace scaffold + `ohara-core` traits + `ohara-storage` SQLite impl with migrations.
2. `ohara-git` walker + `ohara-parse` for Rust + Python.
3. `ohara-embed` with fastembed-rs.
4. End-to-end indexing on a small fixture repo.
5. `ohara` CLI: `init`, `index`, `query`, `status`.
6. `ohara-mcp` with `find_pattern`.
7. `ohara-mcp` adds `explain_change` + `symbol_lineage` materialization.
8. Add JS/TS/Go languages.
9. Hardening: snapshot tests, real-repo smoke, latency budget enforcement.
10. Layer 1+2+3 discoverability polish (tool descriptions, server instructions, `ohara init` writes CLAUDE.md stanza).

## 14. Out-of-scope, deferred to vNext

- Pattern clustering / pattern naming.
- Iterative agentic retrieval.
- Reranker model.
- File-watch daemon for working-tree awareness.
- `ohara-server` shared index gRPC service (option B implementation).
- Multi-repo support.
- `/ohara` slash command skill.
- Voyage / OpenAI embedding providers (trait is in v1; impl is later).
- IDE plugin.
- LLM-based intent extraction from commit messages.
