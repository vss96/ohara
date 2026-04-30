# ohara Plan 1: Foundation + find_pattern Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Reach a working ohara where `ohara index <path>` builds an index for a small fixture git repo and the `find_pattern` MCP tool returns ranked historical hunks for a natural-language query.

**Architecture:** Cargo workspace with 7 crates. `ohara-core` holds orchestration (`Indexer`, `Retriever`) and depends on traits, not concrete impls. `ohara-storage` (SQLite + sqlite-vec + FTS5), `ohara-embed` (fastembed-rs), `ohara-git` (git2 walker + diffs), `ohara-parse` (tree-sitter for Rust + Python in this plan; JS/TS/Go come later) are leaf crates that implement those traits. Two thin binary crates: `ohara-cli` (clap-based) and `ohara-mcp` (rmcp-based MCP server). The MCP server is read-only; indexing only happens through the CLI.

**Tech Stack:** Rust 2021, tokio, rayon, rusqlite (bundled) + sqlite-vec, fastembed-rs, git2, tree-sitter (with grammars for rust + python), clap v4, rmcp, refinery (migrations), anyhow, thiserror, tracing.

**Out of this plan (deferred to later plans):**
- `explain_change` MCP tool, blame walker, `symbol_lineage` table → Plan 2
- `ohara init` git hook installation, CLAUDE.md stanza writing, server-level MCP instructions → Plan 3
- JS/TS/Go tree-sitter languages → Plan 4
- Real-repo smoke tests at scale, `ohara-server` shared-index gRPC service → later

**TDD discipline:** Each task uses red/green commits. Write the failing test → commit (red) → run to verify failure → write minimal implementation → run to verify pass → commit (green). Refactor + commit only if needed. Verification runs (test fail/pass) are not commits.

---

## File Structure

Files this plan creates or modifies. Each is given a single responsibility; files that change together live together.

```
ohara/
├── Cargo.toml                              [new]  workspace manifest
├── rust-toolchain.toml                     [new]  pin to 1.78+
├── .gitignore                              [new]  /target, /fixtures/*/repo, ~/.ohara
├── crates/
│   ├── ohara-core/
│   │   ├── Cargo.toml                      [new]
│   │   └── src/
│   │       ├── lib.rs                      [new]  re-exports
│   │       ├── error.rs                    [new]  OhraError, Result
│   │       ├── types.rs                    [new]  RepoId, Commit, Hunk, Symbol, Provenance, ChangeKind
│   │       ├── query.rs                    [new]  PatternQuery, PatternHit, IndexStatus, ResponseMeta
│   │       ├── storage.rs                  [new]  Storage trait
│   │       ├── embed.rs                    [new]  EmbeddingProvider trait
│   │       ├── indexer.rs                  [new]  Indexer struct + run()
│   │       └── retriever.rs                [new]  Retriever struct + find_pattern()
│   ├── ohara-storage/
│   │   ├── Cargo.toml                      [new]
│   │   ├── migrations/
│   │   │   └── V1__initial.sql             [new]  full schema from spec §5
│   │   └── src/
│   │       ├── lib.rs                      [new]  re-exports SqliteStorage
│   │       ├── pool.rs                     [new]  deadpool-sqlite setup, pragmas, vec extension
│   │       ├── migrations.rs               [new]  refinery integration
│   │       ├── repo.rs                     [new]  repo CRUD
│   │       ├── commit.rs                   [new]  commit insert/get
│   │       ├── hunk.rs                     [new]  hunk insert + KNN query
│   │       ├── blob_cache.rs               [new]  blob_cache get/put
│   │       └── storage_impl.rs             [new]  impl Storage for SqliteStorage
│   ├── ohara-embed/
│   │   ├── Cargo.toml                      [new]
│   │   └── src/
│   │       ├── lib.rs                      [new]
│   │       └── fastembed.rs                [new]  FastEmbedProvider impl
│   ├── ohara-git/
│   │   ├── Cargo.toml                      [new]
│   │   └── src/
│   │       ├── lib.rs                      [new]
│   │       ├── walker.rs                   [new]  list commits parents-first
│   │       └── diff.rs                     [new]  per-commit hunks
│   ├── ohara-parse/
│   │   ├── Cargo.toml                      [new]
│   │   ├── queries/
│   │   │   ├── rust.scm                    [new]  tree-sitter-rust symbol query
│   │   │   └── python.scm                  [new]  tree-sitter-python symbol query
│   │   └── src/
│   │       ├── lib.rs                      [new]  Parser dispatch by language
│   │       ├── rust.rs                     [new]  rust extraction
│   │       └── python.rs                   [new]  python extraction
│   ├── ohara-cli/
│   │   ├── Cargo.toml                      [new]
│   │   └── src/
│   │       ├── main.rs                     [new]  clap entry, dispatch
│   │       └── commands/
│   │           ├── mod.rs                  [new]
│   │           ├── index.rs                [new]  ohara index
│   │           ├── query.rs                [new]  ohara query (debug helper)
│   │           └── status.rs               [new]  ohara status
│   └── ohara-mcp/
│       ├── Cargo.toml                      [new]
│       └── src/
│           ├── main.rs                     [new]  rmcp server bootstrap
│           ├── server.rs                   [new]  Server impl, dependency wiring
│           └── tools/
│               ├── mod.rs                  [new]
│               └── find_pattern.rs         [new]  the one tool in Plan 1
├── fixtures/
│   ├── README.md                           [new]  how the fixture is generated
│   └── build_tiny.sh                       [new]  builds fixtures/tiny/repo from a script
└── tests/
    └── e2e_find_pattern.rs                 [new]  workspace integration test
```

**Boundaries to preserve:**
- `ohara-core` has zero deps on storage/embed/parse/git crates. Only traits and types.
- `ohara-cli` and `ohara-mcp` both depend on `ohara-core` + concrete impl crates; they do NOT depend on each other.
- `ohara-storage::SqliteStorage` does not know about embeddings or git — it stores opaque `Vec<f32>` and SHAs.
- `ohara-parse` does not know about storage — it returns `Vec<Symbol>` from a blob.
- The Indexer (`ohara-core`) is the only piece that wires everything together.

---

### Task 1: Workspace scaffold

**Files:**
- Create: `Cargo.toml`
- Create: `rust-toolchain.toml`
- Create: `.gitignore`
- Create: `crates/ohara-core/Cargo.toml`
- Create: `crates/ohara-core/src/lib.rs`
- Create: `crates/ohara-storage/Cargo.toml`
- Create: `crates/ohara-storage/src/lib.rs`
- Create: `crates/ohara-embed/Cargo.toml`
- Create: `crates/ohara-embed/src/lib.rs`
- Create: `crates/ohara-git/Cargo.toml`
- Create: `crates/ohara-git/src/lib.rs`
- Create: `crates/ohara-parse/Cargo.toml`
- Create: `crates/ohara-parse/src/lib.rs`
- Create: `crates/ohara-cli/Cargo.toml`
- Create: `crates/ohara-cli/src/main.rs`
- Create: `crates/ohara-mcp/Cargo.toml`
- Create: `crates/ohara-mcp/src/main.rs`

- [ ] **Step 1: Write `rust-toolchain.toml`**

```toml
[toolchain]
channel = "1.78"
components = ["rustfmt", "clippy"]
```

- [ ] **Step 2: Write `.gitignore`**

```gitignore
/target
**/*.rs.bk
.DS_Store

# Build artifacts under fixtures (the repos themselves are scripted, not committed)
/fixtures/*/repo

# Local index data
.ohara/
```

- [ ] **Step 3: Write workspace `Cargo.toml`**

```toml
[workspace]
resolver = "2"
members = [
  "crates/ohara-core",
  "crates/ohara-storage",
  "crates/ohara-embed",
  "crates/ohara-git",
  "crates/ohara-parse",
  "crates/ohara-cli",
  "crates/ohara-mcp",
]

[workspace.package]
edition = "2021"
rust-version = "1.78"
version = "0.1.0"
license = "MIT OR Apache-2.0"

[workspace.dependencies]
anyhow = "1"
thiserror = "1"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "fmt"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread", "fs", "io-std", "io-util", "signal", "sync"] }
rayon = "1"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
hex = "0.4"
sha2 = "0.10"
chrono = { version = "0.4", features = ["serde"] }
deadpool-sqlite = "0.8"
rusqlite = { version = "0.31", features = ["bundled", "load_extension", "blob"] }
sqlite-vec = "0.1"
refinery = { version = "0.8", features = ["rusqlite"] }
fastembed = "4"
git2 = { version = "0.18", default-features = false }
tree-sitter = "0.22"
tree-sitter-rust = "0.21"
tree-sitter-python = "0.21"
clap = { version = "4", features = ["derive", "env"] }
rmcp = { version = "0.1", features = ["server", "transport-io"] }
schemars = "0.8"

[profile.release]
lto = "thin"
codegen-units = 1
```

- [ ] **Step 4: Write each crate's `Cargo.toml` and stub `lib.rs` / `main.rs`**

`crates/ohara-core/Cargo.toml`:
```toml
[package]
name = "ohara-core"
edition.workspace = true
rust-version.workspace = true
version.workspace = true
license.workspace = true

[dependencies]
anyhow.workspace = true
thiserror.workspace = true
tracing.workspace = true
serde.workspace = true
serde_json.workspace = true
chrono.workspace = true
sha2.workspace = true
hex.workspace = true
async-trait = "0.1"
```

`crates/ohara-core/src/lib.rs`:
```rust
//! Core orchestration types and traits for ohara.
//!
//! No concrete storage / embedding / git / parsing impls live here. Only
//! the contracts (`Storage`, `EmbeddingProvider`) and the orchestrators
//! (`Indexer`, `Retriever`) that depend on them.
```

`crates/ohara-storage/Cargo.toml`:
```toml
[package]
name = "ohara-storage"
edition.workspace = true
rust-version.workspace = true
version.workspace = true
license.workspace = true

[dependencies]
ohara-core = { path = "../ohara-core" }
anyhow.workspace = true
thiserror.workspace = true
tracing.workspace = true
serde.workspace = true
serde_json.workspace = true
deadpool-sqlite.workspace = true
rusqlite.workspace = true
sqlite-vec.workspace = true
refinery.workspace = true
async-trait = "0.1"
chrono.workspace = true
```

`crates/ohara-storage/src/lib.rs`:
```rust
//! SQLite + sqlite-vec implementation of `ohara_core::Storage`.
```

`crates/ohara-embed/Cargo.toml`:
```toml
[package]
name = "ohara-embed"
edition.workspace = true
rust-version.workspace = true
version.workspace = true
license.workspace = true

[dependencies]
ohara-core = { path = "../ohara-core" }
anyhow.workspace = true
fastembed.workspace = true
async-trait = "0.1"
tracing.workspace = true
```

`crates/ohara-embed/src/lib.rs`:
```rust
//! fastembed-rs implementation of `ohara_core::EmbeddingProvider`.
```

`crates/ohara-git/Cargo.toml`:
```toml
[package]
name = "ohara-git"
edition.workspace = true
rust-version.workspace = true
version.workspace = true
license.workspace = true

[dependencies]
ohara-core = { path = "../ohara-core" }
anyhow.workspace = true
git2.workspace = true
tracing.workspace = true
```

`crates/ohara-git/src/lib.rs`:
```rust
//! git2 wrapper: walk commits, extract per-file diffs.
```

`crates/ohara-parse/Cargo.toml`:
```toml
[package]
name = "ohara-parse"
edition.workspace = true
rust-version.workspace = true
version.workspace = true
license.workspace = true

[dependencies]
ohara-core = { path = "../ohara-core" }
anyhow.workspace = true
tree-sitter.workspace = true
tree-sitter-rust.workspace = true
tree-sitter-python.workspace = true
tracing.workspace = true
```

`crates/ohara-parse/src/lib.rs`:
```rust
//! tree-sitter symbol extraction for supported languages.
```

`crates/ohara-cli/Cargo.toml`:
```toml
[package]
name = "ohara-cli"
edition.workspace = true
rust-version.workspace = true
version.workspace = true
license.workspace = true

[[bin]]
name = "ohara"
path = "src/main.rs"

[dependencies]
ohara-core = { path = "../ohara-core" }
ohara-storage = { path = "../ohara-storage" }
ohara-embed = { path = "../ohara-embed" }
ohara-git = { path = "../ohara-git" }
ohara-parse = { path = "../ohara-parse" }
anyhow.workspace = true
clap.workspace = true
tokio.workspace = true
tracing.workspace = true
tracing-subscriber.workspace = true
```

`crates/ohara-cli/src/main.rs`:
```rust
fn main() {
    println!("ohara cli stub");
}
```

`crates/ohara-mcp/Cargo.toml`:
```toml
[package]
name = "ohara-mcp"
edition.workspace = true
rust-version.workspace = true
version.workspace = true
license.workspace = true

[[bin]]
name = "ohara-mcp"
path = "src/main.rs"

[dependencies]
ohara-core = { path = "../ohara-core" }
ohara-storage = { path = "../ohara-storage" }
ohara-embed = { path = "../ohara-embed" }
anyhow.workspace = true
tokio.workspace = true
tracing.workspace = true
tracing-subscriber.workspace = true
rmcp.workspace = true
schemars.workspace = true
serde.workspace = true
serde_json.workspace = true
```

`crates/ohara-mcp/src/main.rs`:
```rust
fn main() {
    println!("ohara-mcp stub");
}
```

- [ ] **Step 5: Run `cargo build` to verify the workspace compiles**

Run: `cargo build`
Expected: builds all 7 crates. Two binaries (`ohara`, `ohara-mcp`) produced under `target/debug/`. No errors.

- [ ] **Step 6: Run the binaries to confirm they execute**

Run: `cargo run -p ohara-cli`
Expected output: `ohara cli stub`

Run: `cargo run -p ohara-mcp`
Expected output: `ohara-mcp stub`

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml rust-toolchain.toml .gitignore crates/
git commit -m "Scaffold cargo workspace with seven crate stubs"
```

---

### Task 2: ohara-core error and domain types

**Files:**
- Create: `crates/ohara-core/src/error.rs`
- Create: `crates/ohara-core/src/types.rs`
- Modify: `crates/ohara-core/src/lib.rs`
- Test: same files (unit tests inline with `#[cfg(test)]`)

- [ ] **Step 1: Write the failing test for `RepoId` derivation**

In `crates/ohara-core/src/types.rs`:
```rust
use serde::{Deserialize, Serialize};

/// Stable identifier for a repository on a single machine.
///
/// Hash of `first_commit_sha` + canonical absolute path. Stable across
/// renames within the same path, unique across multiple clones.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RepoId(String);

impl RepoId {
    pub fn from_parts(first_commit_sha: &str, canonical_path: &str) -> Self {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(first_commit_sha.as_bytes());
        h.update(b"\0");
        h.update(canonical_path.as_bytes());
        Self(hex::encode(&h.finalize()[..16]))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_id_is_deterministic() {
        let a = RepoId::from_parts("deadbeef", "/Users/x/projects/foo");
        let b = RepoId::from_parts("deadbeef", "/Users/x/projects/foo");
        assert_eq!(a, b);
    }

    #[test]
    fn repo_id_distinguishes_clones_by_path() {
        let a = RepoId::from_parts("deadbeef", "/Users/x/foo");
        let b = RepoId::from_parts("deadbeef", "/Users/x/foo-2");
        assert_ne!(a, b);
    }

    #[test]
    fn repo_id_distinguishes_repos_by_first_commit() {
        let a = RepoId::from_parts("aaaa", "/Users/x/foo");
        let b = RepoId::from_parts("bbbb", "/Users/x/foo");
        assert_ne!(a, b);
    }
}
```

Add `pub mod types;` to `crates/ohara-core/src/lib.rs`.

- [ ] **Step 2: Run test to verify it fails (because the type isn't yet exported)**

Run: `cargo test -p ohara-core --lib types`
Expected: at this point the tests should *pass* because the impl is co-located. If they pass, that's correct — proceed.

(Note: this task is a "definition" task, not a discovery task. The "red" is module-not-found until you add `pub mod types;`. After that and the impl above, tests pass.)

- [ ] **Step 3: Commit (red — types module added)**

```bash
git add crates/ohara-core/src/types.rs crates/ohara-core/src/lib.rs
git commit -m "Add RepoId domain type with deterministic hashing"
```

- [ ] **Step 4: Add the rest of the domain types**

In `crates/ohara-core/src/types.rs`, append:
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Provenance {
    Extracted,
    Inferred,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SymbolKind {
    Function,
    Method,
    Class,
    Const,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitMeta {
    pub sha: String,
    pub parent_sha: Option<String>,
    pub is_merge: bool,
    pub author: Option<String>,
    pub ts: i64,             // unix seconds
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hunk {
    pub commit_sha: String,
    pub file_path: String,
    pub language: Option<String>,
    pub change_kind: ChangeKind,
    pub diff_text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Symbol {
    pub file_path: String,
    pub language: String,
    pub kind: SymbolKind,
    pub name: String,
    pub qualified_name: Option<String>,
    pub span_start: u32,
    pub span_end: u32,
    pub blob_sha: String,
    pub source_text: String,
}
```

- [ ] **Step 5: Run tests to confirm everything compiles and passes**

Run: `cargo test -p ohara-core --lib`
Expected: `test result: ok. 3 passed`

- [ ] **Step 6: Commit (green — full domain types)**

```bash
git add crates/ohara-core/src/types.rs
git commit -m "Add CommitMeta, Hunk, Symbol, ChangeKind, Provenance, SymbolKind"
```

- [ ] **Step 7: Add `error.rs`**

In `crates/ohara-core/src/error.rs`:
```rust
use thiserror::Error;

#[derive(Debug, Error)]
pub enum OhraError {
    #[error("storage error: {0}")]
    Storage(String),

    #[error("embedding error: {0}")]
    Embedding(String),

    #[error("git error: {0}")]
    Git(String),

    #[error("parse error: {0}")]
    Parse(String),

    #[error("repo not indexed: {0}")]
    RepoNotIndexed(String),

    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, OhraError>;
```

In `crates/ohara-core/src/lib.rs`, add:
```rust
pub mod error;
pub mod types;

pub use error::{OhraError, Result};
pub use types::*;
```

- [ ] **Step 8: Run a quick build to verify exports**

Run: `cargo build -p ohara-core`
Expected: clean build.

- [ ] **Step 9: Commit (green — error type added and re-exports)**

```bash
git add crates/ohara-core/src/error.rs crates/ohara-core/src/lib.rs
git commit -m "Add OhraError and Result alias; re-export domain types"
```

---

### Task 3: ohara-core query types and traits

**Files:**
- Create: `crates/ohara-core/src/query.rs`
- Create: `crates/ohara-core/src/storage.rs`
- Create: `crates/ohara-core/src/embed.rs`
- Modify: `crates/ohara-core/src/lib.rs`

- [ ] **Step 1: Write the failing test for `PatternHit` JSON serialization**

In `crates/ohara-core/src/query.rs`:
```rust
use crate::types::Provenance;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatternQuery {
    pub query: String,
    pub k: u8,
    pub language: Option<String>,
    pub since_unix: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatternHit {
    pub commit_sha: String,
    pub commit_message: String,
    pub commit_author: Option<String>,
    pub commit_date: String,            // ISO 8601
    pub file_path: String,
    pub change_kind: String,
    pub diff_excerpt: String,
    pub diff_truncated: bool,
    pub related_head_symbols: Vec<String>,
    pub similarity: f32,
    pub recency_weight: f32,
    pub combined_score: f32,
    pub provenance: Provenance,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexStatus {
    pub last_indexed_commit: Option<String>,
    pub commits_behind_head: u64,
    pub indexed_at: Option<String>,     // ISO 8601
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseMeta {
    pub index_status: IndexStatus,
    pub hint: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Provenance;

    #[test]
    fn pattern_hit_serializes_to_expected_json_shape() {
        let hit = PatternHit {
            commit_sha: "abc".into(),
            commit_message: "msg".into(),
            commit_author: Some("alice".into()),
            commit_date: "2024-01-01T00:00:00Z".into(),
            file_path: "src/foo.rs".into(),
            change_kind: "added".into(),
            diff_excerpt: "+fn x() {}".into(),
            diff_truncated: false,
            related_head_symbols: vec!["foo::x".into()],
            similarity: 0.9,
            recency_weight: 0.5,
            combined_score: 0.78,
            provenance: Provenance::Inferred,
        };
        let s = serde_json::to_string(&hit).unwrap();
        assert!(s.contains("\"provenance\":\"INFERRED\""));
        assert!(s.contains("\"diff_truncated\":false"));
    }

    #[test]
    fn response_meta_round_trips() {
        let meta = ResponseMeta {
            index_status: IndexStatus {
                last_indexed_commit: Some("abc".into()),
                commits_behind_head: 7,
                indexed_at: None,
            },
            hint: None,
        };
        let s = serde_json::to_string(&meta).unwrap();
        let back: ResponseMeta = serde_json::from_str(&s).unwrap();
        assert_eq!(back.index_status.commits_behind_head, 7);
    }
}
```

In `crates/ohara-core/src/lib.rs` add `pub mod query;` and re-export `pub use query::*;`.

- [ ] **Step 2: Run test to verify it passes (definition task)**

Run: `cargo test -p ohara-core query`
Expected: `2 passed`.

- [ ] **Step 3: Commit (green — query types)**

```bash
git add crates/ohara-core/src/query.rs crates/ohara-core/src/lib.rs
git commit -m "Add PatternQuery, PatternHit, ResponseMeta query types"
```

- [ ] **Step 4: Define the Storage trait**

In `crates/ohara-core/src/storage.rs`:
```rust
use crate::query::IndexStatus;
use crate::types::{CommitMeta, Hunk, RepoId, Symbol};
use crate::Result;
use async_trait::async_trait;

/// Vector with the same dimension as `EmbeddingProvider::dimension()`.
pub type Vector = Vec<f32>;

#[derive(Debug, Clone)]
pub struct HunkRecord {
    pub hunk: Hunk,
    pub diff_emb: Vector,
}

#[derive(Debug, Clone)]
pub struct CommitRecord {
    pub meta: CommitMeta,
    pub message_emb: Vector,
}

#[derive(Debug, Clone)]
pub struct HunkHit {
    pub hunk: Hunk,
    pub commit: CommitMeta,
    pub similarity: f32,
}

#[async_trait]
pub trait Storage: Send + Sync {
    async fn open_repo(&self, repo_id: &RepoId, path: &str, first_commit_sha: &str) -> Result<()>;

    async fn get_index_status(&self, repo_id: &RepoId) -> Result<IndexStatus>;

    async fn set_last_indexed_commit(&self, repo_id: &RepoId, sha: &str) -> Result<()>;

    async fn put_commit(&self, repo_id: &RepoId, record: &CommitRecord) -> Result<()>;

    async fn put_hunks(&self, repo_id: &RepoId, records: &[HunkRecord]) -> Result<()>;

    async fn put_head_symbols(&self, repo_id: &RepoId, symbols: &[Symbol]) -> Result<()>;

    async fn knn_hunks(
        &self,
        repo_id: &RepoId,
        query_emb: &[f32],
        k: u8,
        language: Option<&str>,
        since_unix: Option<i64>,
    ) -> Result<Vec<HunkHit>>;

    async fn blob_was_seen(&self, blob_sha: &str, embedding_model: &str) -> Result<bool>;

    async fn record_blob_seen(&self, blob_sha: &str, embedding_model: &str) -> Result<()>;
}
```

- [ ] **Step 5: Define the EmbeddingProvider trait**

In `crates/ohara-core/src/embed.rs`:
```rust
use crate::Result;
use async_trait::async_trait;

#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    fn dimension(&self) -> usize;

    fn model_id(&self) -> &str;

    /// Embed a batch of texts. The output has the same length and order as the input.
    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
}
```

In `crates/ohara-core/src/lib.rs`:
```rust
pub mod embed;
pub mod error;
pub mod query;
pub mod storage;
pub mod types;

pub use embed::EmbeddingProvider;
pub use error::{OhraError, Result};
pub use query::*;
pub use storage::{CommitRecord, HunkHit, HunkRecord, Storage, Vector};
pub use types::*;
```

- [ ] **Step 6: Build to verify trait declarations compile**

Run: `cargo build -p ohara-core`
Expected: clean.

- [ ] **Step 7: Commit (green — traits)**

```bash
git add crates/ohara-core/src/storage.rs crates/ohara-core/src/embed.rs crates/ohara-core/src/lib.rs
git commit -m "Add Storage and EmbeddingProvider traits"
```

---

### Task 4: ohara-core Indexer and Retriever skeletons

**Files:**
- Create: `crates/ohara-core/src/indexer.rs`
- Create: `crates/ohara-core/src/retriever.rs`
- Modify: `crates/ohara-core/src/lib.rs`

The Indexer and Retriever orchestrate but do no I/O directly. They take `Arc<dyn Storage>` and `Arc<dyn EmbeddingProvider>` and additional inputs (e.g., a list of `CommitMeta` from a git source) provided by the caller. This keeps `ohara-core` free of git/parse dependencies; the CLI/MCP wire those in.

- [ ] **Step 1: Write the failing test for `Retriever::rank_hits`**

In `crates/ohara-core/src/retriever.rs`:
```rust
use crate::query::PatternHit;
use crate::storage::HunkHit;
use crate::types::{CommitMeta, Hunk, Provenance};
use chrono::{DateTime, Utc};
use std::sync::Arc;

pub struct RankingWeights {
    pub similarity: f32,
    pub recency: f32,
    pub message_match: f32,
    pub recency_half_life_days: f32,
}

impl Default for RankingWeights {
    fn default() -> Self {
        Self { similarity: 0.7, recency: 0.2, message_match: 0.1, recency_half_life_days: 365.0 }
    }
}

pub struct Retriever {
    weights: RankingWeights,
    storage: Arc<dyn crate::Storage>,
    embedder: Arc<dyn crate::EmbeddingProvider>,
}

impl Retriever {
    pub fn new(storage: Arc<dyn crate::Storage>, embedder: Arc<dyn crate::EmbeddingProvider>) -> Self {
        Self { weights: RankingWeights::default(), storage, embedder }
    }

    pub fn with_weights(mut self, w: RankingWeights) -> Self {
        self.weights = w;
        self
    }

    /// Pure ranking step, separated for testability.
    pub fn rank_hits(
        &self,
        hits: Vec<HunkHit>,
        message_similarities: &[f32],
        now_unix: i64,
    ) -> Vec<PatternHit> {
        assert_eq!(hits.len(), message_similarities.len());
        let mut out: Vec<PatternHit> = hits
            .into_iter()
            .zip(message_similarities.iter())
            .map(|(h, &msg_sim)| {
                let age_days = ((now_unix - h.commit.ts).max(0) as f32) / 86400.0;
                let recency = (-age_days / self.weights.recency_half_life_days).exp();
                let combined = self.weights.similarity * h.similarity
                    + self.weights.recency * recency
                    + self.weights.message_match * msg_sim;
                let date = DateTime::<Utc>::from_timestamp(h.commit.ts, 0)
                    .map(|d| d.to_rfc3339())
                    .unwrap_or_default();
                let (excerpt, truncated) = truncate_diff(&h.hunk.diff_text, 80);
                PatternHit {
                    commit_sha: h.commit.sha,
                    commit_message: h.commit.message,
                    commit_author: h.commit.author,
                    commit_date: date,
                    file_path: h.hunk.file_path,
                    change_kind: format!("{:?}", h.hunk.change_kind).to_lowercase(),
                    diff_excerpt: excerpt,
                    diff_truncated: truncated,
                    related_head_symbols: vec![],   // populated in a later plan if symbol attribution is added
                    similarity: h.similarity,
                    recency_weight: recency,
                    combined_score: combined,
                    provenance: Provenance::Inferred,
                }
            })
            .collect();
        out.sort_by(|a, b| b.combined_score.partial_cmp(&a.combined_score).unwrap());
        out
    }
}

fn truncate_diff(s: &str, max_lines: usize) -> (String, bool) {
    let mut count = 0;
    let mut end = 0;
    for (i, b) in s.bytes().enumerate() {
        if b == b'\n' {
            count += 1;
            if count == max_lines {
                end = i + 1;
                break;
            }
        }
    }
    if count < max_lines {
        return (s.to_string(), false);
    }
    let total_lines = s.bytes().filter(|&b| b == b'\n').count();
    let extra = total_lines.saturating_sub(max_lines);
    let mut out = s[..end].to_string();
    out.push_str(&format!("... ({} more lines)\n", extra));
    (out, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ChangeKind;

    fn fake_hit(sha: &str, ts: i64, sim: f32, diff: &str) -> HunkHit {
        HunkHit {
            hunk: Hunk {
                commit_sha: sha.into(),
                file_path: "src/x.rs".into(),
                language: Some("rust".into()),
                change_kind: ChangeKind::Added,
                diff_text: diff.into(),
            },
            commit: CommitMeta {
                sha: sha.into(),
                parent_sha: None,
                is_merge: false,
                author: Some("a".into()),
                ts,
                message: "m".into(),
            },
            similarity: sim,
        }
    }

    struct PanicStorage;
    #[async_trait::async_trait]
    impl crate::Storage for PanicStorage {
        async fn open_repo(&self, _: &crate::types::RepoId, _: &str, _: &str) -> crate::Result<()> { unreachable!() }
        async fn get_index_status(&self, _: &crate::types::RepoId) -> crate::Result<crate::query::IndexStatus> { unreachable!() }
        async fn set_last_indexed_commit(&self, _: &crate::types::RepoId, _: &str) -> crate::Result<()> { unreachable!() }
        async fn put_commit(&self, _: &crate::types::RepoId, _: &crate::CommitRecord) -> crate::Result<()> { unreachable!() }
        async fn put_hunks(&self, _: &crate::types::RepoId, _: &[crate::HunkRecord]) -> crate::Result<()> { unreachable!() }
        async fn put_head_symbols(&self, _: &crate::types::RepoId, _: &[crate::types::Symbol]) -> crate::Result<()> { unreachable!() }
        async fn knn_hunks(&self, _: &crate::types::RepoId, _: &[f32], _: u8, _: Option<&str>, _: Option<i64>) -> crate::Result<Vec<crate::HunkHit>> { unreachable!() }
        async fn blob_was_seen(&self, _: &str, _: &str) -> crate::Result<bool> { unreachable!() }
        async fn record_blob_seen(&self, _: &str, _: &str) -> crate::Result<()> { unreachable!() }
    }

    struct PanicEmbedder;
    #[async_trait::async_trait]
    impl crate::EmbeddingProvider for PanicEmbedder {
        fn dimension(&self) -> usize { unreachable!() }
        fn model_id(&self) -> &str { unreachable!() }
        async fn embed_batch(&self, _: &[String]) -> crate::Result<Vec<Vec<f32>>> { unreachable!() }
    }

    fn retriever_for_test() -> Retriever {
        Retriever {
            weights: RankingWeights::default(),
            storage: Arc::new(PanicStorage),
            embedder: Arc::new(PanicEmbedder),
        }
    }

    #[test]
    fn rank_orders_higher_similarity_first_when_recency_equal() {
        let now = 1_700_000_000;
        let hits = vec![
            fake_hit("a", now - 86400, 0.5, "+x"),
            fake_hit("b", now - 86400, 0.9, "+y"),
        ];
        let msg_sims = vec![0.0, 0.0];
        let out = retriever_for_test().rank_hits(hits, &msg_sims, now);
        assert_eq!(out[0].commit_sha, "b");
        assert_eq!(out[1].commit_sha, "a");
        assert!(out[0].combined_score > out[1].combined_score);
    }

    #[test]
    fn truncate_marks_truncation_for_long_diffs() {
        let big = (0..200).map(|i| format!("line {}\n", i)).collect::<String>();
        let (out, trunc) = super::truncate_diff(&big, 80);
        assert!(trunc);
        assert!(out.contains("more lines"));
    }

    #[test]
    fn truncate_passthrough_for_short_diffs() {
        let small = "line a\nline b\n";
        let (out, trunc) = super::truncate_diff(small, 80);
        assert!(!trunc);
        assert_eq!(out, small);
    }
}
```

In `crates/ohara-core/src/lib.rs`, add `pub mod retriever;` and `pub use retriever::{Retriever, RankingWeights};`.

- [ ] **Step 2: Run tests (red — `Indexer` is still missing, and ranking should pass)**

Run: `cargo test -p ohara-core retriever`
Expected: 3 passed.

- [ ] **Step 3: Commit (green — retriever ranking logic)**

```bash
git add crates/ohara-core/src/retriever.rs crates/ohara-core/src/lib.rs
git commit -m "Add Retriever with pure rank_hits and diff truncation"
```

- [ ] **Step 4: Add the Indexer skeleton**

In `crates/ohara-core/src/indexer.rs`:
```rust
use crate::query::IndexStatus;
use crate::storage::{CommitRecord, HunkRecord};
use crate::types::{CommitMeta, Hunk, RepoId, Symbol};
use crate::{EmbeddingProvider, Result, Storage};
use std::sync::Arc;

/// Source of commits + hunks. Implemented by `ohara-git` in a later task; defined
/// here so `ohara-core` stays git-free.
#[async_trait::async_trait]
pub trait CommitSource: Send + Sync {
    /// Yield commits in parents-first order, optionally starting after `since`.
    async fn list_commits(&self, since: Option<&str>) -> Result<Vec<CommitMeta>>;
    /// Yield the per-file hunks of a single commit.
    async fn hunks_for_commit(&self, sha: &str) -> Result<Vec<Hunk>>;
}

/// Source of HEAD symbols. Implemented by `ohara-parse` driver in a later task.
#[async_trait::async_trait]
pub trait SymbolSource: Send + Sync {
    async fn extract_head_symbols(&self) -> Result<Vec<Symbol>>;
}

pub struct Indexer {
    storage: Arc<dyn Storage>,
    embedder: Arc<dyn EmbeddingProvider>,
    batch_commits: usize,
    embed_batch: usize,
}

impl Indexer {
    pub fn new(storage: Arc<dyn Storage>, embedder: Arc<dyn EmbeddingProvider>) -> Self {
        Self { storage, embedder, batch_commits: 512, embed_batch: 32 }
    }

    /// Run a (full or incremental) indexing pass for `repo_id`.
    /// `commit_source` and `symbol_source` are wired by the caller.
    pub async fn run(
        &self,
        repo_id: &RepoId,
        commit_source: &dyn CommitSource,
        symbol_source: &dyn SymbolSource,
    ) -> Result<IndexerReport> {
        let status = self.storage.get_index_status(repo_id).await?;
        let commits = commit_source.list_commits(status.last_indexed_commit.as_deref()).await?;
        tracing::info!(new_commits = commits.len(), "begin index pass");

        let mut latest_sha: Option<String> = status.last_indexed_commit.clone();
        let mut total_hunks = 0usize;

        for chunk in commits.chunks(self.batch_commits) {
            for cm in chunk {
                let hunks = commit_source.hunks_for_commit(&cm.sha).await?;
                total_hunks += hunks.len();

                let texts: Vec<String> = std::iter::once(cm.message.clone())
                    .chain(hunks.iter().map(|h| h.diff_text.clone()))
                    .collect();
                let embs = self.embedder.embed_batch(&texts).await?;
                let (msg_emb, hunk_embs) = embs.split_first().expect("non-empty");

                self.storage
                    .put_commit(repo_id, &CommitRecord { meta: cm.clone(), message_emb: msg_emb.clone() })
                    .await?;

                let records: Vec<HunkRecord> = hunks
                    .into_iter()
                    .zip(hunk_embs.iter().cloned())
                    .map(|(h, e)| HunkRecord { hunk: h, diff_emb: e })
                    .collect();
                self.storage.put_hunks(repo_id, &records).await?;
                latest_sha = Some(cm.sha.clone());
            }
        }

        let symbols = symbol_source.extract_head_symbols().await?;
        self.storage.put_head_symbols(repo_id, &symbols).await?;

        if let Some(sha) = latest_sha.as_deref() {
            self.storage.set_last_indexed_commit(repo_id, sha).await?;
        }

        Ok(IndexerReport { new_commits: commits.len(), new_hunks: total_hunks, head_symbols: symbols.len() })
    }
}

#[derive(Debug, Clone)]
pub struct IndexerReport {
    pub new_commits: usize,
    pub new_hunks: usize,
    pub head_symbols: usize,
}
```

In `crates/ohara-core/src/lib.rs` add `pub mod indexer;` and `pub use indexer::{Indexer, IndexerReport, CommitSource, SymbolSource};`.

- [ ] **Step 5: Build to verify the orchestration compiles**

Run: `cargo build -p ohara-core`
Expected: clean.

- [ ] **Step 6: Commit (green — indexer skeleton)**

```bash
git add crates/ohara-core/src/indexer.rs crates/ohara-core/src/lib.rs
git commit -m "Add Indexer with batched embedding and CommitSource/SymbolSource traits"
```

---

### Task 5: SqliteStorage connection pool, pragmas, sqlite-vec loading

**Files:**
- Create: `crates/ohara-storage/src/pool.rs`
- Modify: `crates/ohara-storage/src/lib.rs`

- [ ] **Step 1: Write the failing test**

In `crates/ohara-storage/src/pool.rs`:
```rust
use anyhow::{Context, Result};
use deadpool_sqlite::{Config, Pool, Runtime};
use rusqlite::Connection;
use std::path::Path;

pub struct SqlitePoolBuilder {
    path: std::path::PathBuf,
}

impl SqlitePoolBuilder {
    pub fn new<P: AsRef<Path>>(path: P) -> Self {
        Self { path: path.as_ref().to_path_buf() }
    }

    pub async fn build(self) -> Result<Pool> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).context("create index dir")?;
        }
        let cfg = Config::new(&self.path);
        let pool = cfg.create_pool(Runtime::Tokio1).context("create sqlite pool")?;
        // Apply pragmas + load sqlite-vec on each connection by running it once on a checkout.
        let conn = pool.get().await.context("checkout from pool")?;
        conn.interact(|c| {
            apply_pragmas(c)?;
            load_vec_extension(c)?;
            Ok::<_, anyhow::Error>(())
        })
        .await
        .map_err(|e| anyhow::anyhow!("interact: {e}"))??;
        Ok(pool)
    }
}

pub(crate) fn apply_pragmas(c: &Connection) -> Result<()> {
    c.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=NORMAL;
         PRAGMA mmap_size=268435456;
         PRAGMA cache_size=-64000;
         PRAGMA temp_store=MEMORY;
         PRAGMA foreign_keys=ON;",
    )?;
    Ok(())
}

pub(crate) fn load_vec_extension(c: &Connection) -> Result<()> {
    unsafe {
        c.load_extension_enable()?;
        sqlite_vec::sqlite3_vec_init();
        c.load_extension_disable()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pool_opens_and_pragmas_apply() {
        let dir = tempfile::tempdir().unwrap();
        let pool = SqlitePoolBuilder::new(dir.path().join("idx.sqlite")).build().await.unwrap();
        let conn = pool.get().await.unwrap();
        let mode: String = conn
            .interact(|c| {
                c.query_row("PRAGMA journal_mode", [], |r| r.get(0))
                    .map_err(anyhow::Error::from)
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
    }

    #[tokio::test]
    async fn vec_extension_is_callable() {
        let dir = tempfile::tempdir().unwrap();
        let pool = SqlitePoolBuilder::new(dir.path().join("idx.sqlite")).build().await.unwrap();
        let conn = pool.get().await.unwrap();
        let v: String = conn
            .interact(|c| {
                c.query_row("SELECT vec_version()", [], |r| r.get(0))
                    .map_err(anyhow::Error::from)
            })
            .await
            .unwrap()
            .unwrap();
        assert!(!v.is_empty());
    }
}
```

Add `tempfile = "3"` and `tokio = { workspace = true }` (both as `dev-dependencies`) to `crates/ohara-storage/Cargo.toml`:
```toml
[dev-dependencies]
tempfile = "3"
tokio = { workspace = true }
```

In `crates/ohara-storage/src/lib.rs`, add `pub mod pool;`.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p ohara-storage pool`
Expected: tests build but `sqlite_vec::sqlite3_vec_init` may or may not be available. If the link fails, double-check that the `sqlite-vec` workspace dep is the version on crates.io that exposes the `sqlite3_vec_init` symbol (older crate names may differ). On a clean run this is a "passes immediately" task; treat the red commit as the test addition itself.

- [ ] **Step 3: Commit (red — pool + tests)**

```bash
git add crates/ohara-storage/src/pool.rs crates/ohara-storage/src/lib.rs crates/ohara-storage/Cargo.toml
git commit -m "Add SqlitePoolBuilder with pragmas and sqlite-vec loading"
```

- [ ] **Step 4: Run tests, fix any link/symbol issues**

Run: `cargo test -p ohara-storage pool`
Expected: 2 passed.

If sqlite-vec link fails, set in `crates/ohara-storage/Cargo.toml`:
```toml
[dependencies.sqlite-vec]
version = "0.1"
features = ["embed"]    # or whatever the published version uses to bundle the C source
```
Re-run.

- [ ] **Step 5: Commit (green — verified working)**

```bash
git commit --allow-empty -m "Verify sqlite-vec link works; pool tests green"
```

(If no fix was needed, skip this step.)

---

### Task 6: Schema migration V1

**Files:**
- Create: `crates/ohara-storage/migrations/V1__initial.sql`
- Create: `crates/ohara-storage/src/migrations.rs`
- Modify: `crates/ohara-storage/src/lib.rs`

- [ ] **Step 1: Write the failing test**

In `crates/ohara-storage/src/migrations.rs`:
```rust
use anyhow::Result;
use rusqlite::Connection;

mod embedded {
    refinery::embed_migrations!("migrations");
}

pub fn run(conn: &mut Connection) -> Result<()> {
    embedded::migrations::runner().run(conn)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::{apply_pragmas, load_vec_extension};

    #[test]
    fn migrations_apply_to_fresh_db() {
        let mut c = Connection::open_in_memory().unwrap();
        apply_pragmas(&c).unwrap();
        load_vec_extension(&c).unwrap();
        run(&mut c).unwrap();

        let count: i64 = c
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='hunk'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);

        let vec_count: i64 = c
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='vec_hunk'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(vec_count, 1);
    }
}
```

In `crates/ohara-storage/src/lib.rs`, add `pub mod migrations;`.

- [ ] **Step 2: Run test to verify it fails (no SQL file yet)**

Run: `cargo test -p ohara-storage migrations`
Expected: FAIL — refinery embeds zero migrations.

- [ ] **Step 3: Commit the failing test (red)**

```bash
git add crates/ohara-storage/src/migrations.rs crates/ohara-storage/src/lib.rs
git commit -m "Add migrations module with refinery; test fails until V1 added"
```

- [ ] **Step 4: Write `crates/ohara-storage/migrations/V1__initial.sql`**

```sql
-- ohara index schema, version 1.

CREATE TABLE repo (
    id TEXT PRIMARY KEY,
    path TEXT NOT NULL,
    first_commit_sha TEXT NOT NULL,
    last_indexed_commit TEXT,
    indexed_at TEXT,
    schema_version INTEGER NOT NULL
);

CREATE TABLE commit_record (
    sha TEXT PRIMARY KEY,
    parent_sha TEXT,
    is_merge INTEGER NOT NULL,
    ts INTEGER NOT NULL,
    author TEXT,
    message TEXT NOT NULL
);
CREATE INDEX idx_commit_ts ON commit_record (ts);

CREATE TABLE file_path (
    id INTEGER PRIMARY KEY,
    path TEXT NOT NULL,
    language TEXT,
    active INTEGER NOT NULL DEFAULT 1,
    UNIQUE(path)
);

CREATE TABLE symbol (
    id INTEGER PRIMARY KEY,
    file_path_id INTEGER NOT NULL REFERENCES file_path(id),
    kind TEXT NOT NULL,
    name TEXT NOT NULL,
    qualified_name TEXT,
    span_start INTEGER NOT NULL,
    span_end INTEGER NOT NULL,
    blob_sha TEXT NOT NULL,
    source_text TEXT NOT NULL
);
CREATE INDEX idx_symbol_file ON symbol (file_path_id);

CREATE TABLE hunk (
    id INTEGER PRIMARY KEY,
    commit_sha TEXT NOT NULL REFERENCES commit_record(sha),
    file_path_id INTEGER NOT NULL REFERENCES file_path(id),
    change_kind TEXT NOT NULL,
    diff_text TEXT NOT NULL
);
CREATE INDEX idx_hunk_file_commit ON hunk (file_path_id, commit_sha);
CREATE INDEX idx_hunk_commit ON hunk (commit_sha);

CREATE TABLE blob_cache (
    blob_sha TEXT NOT NULL,
    embedding_model TEXT NOT NULL,
    embedded_at INTEGER NOT NULL,
    PRIMARY KEY (blob_sha, embedding_model)
);

CREATE VIRTUAL TABLE vec_hunk USING vec0(hunk_id INTEGER PRIMARY KEY, diff_emb FLOAT[384]);
CREATE VIRTUAL TABLE vec_commit USING vec0(commit_sha TEXT PRIMARY KEY, message_emb FLOAT[384]);
CREATE VIRTUAL TABLE vec_symbol USING vec0(symbol_id INTEGER PRIMARY KEY, source_emb FLOAT[384]);

CREATE VIRTUAL TABLE fts_commit USING fts5(sha UNINDEXED, message);
CREATE VIRTUAL TABLE fts_symbol USING fts5(symbol_id UNINDEXED, qualified_name, source_text);
```

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test -p ohara-storage migrations`
Expected: 1 passed.

- [ ] **Step 6: Commit (green — schema)**

```bash
git add crates/ohara-storage/migrations/V1__initial.sql
git commit -m "Add V1 schema migration covering all tables and vector indexes"
```

---

### Task 7: Repo CRUD on SqliteStorage

**Files:**
- Create: `crates/ohara-storage/src/repo.rs`
- Create: `crates/ohara-storage/src/storage_impl.rs`
- Modify: `crates/ohara-storage/src/lib.rs`

- [ ] **Step 1: Write the failing test**

In `crates/ohara-storage/src/storage_impl.rs`:
```rust
use crate::pool::SqlitePoolBuilder;
use crate::{migrations, repo};
use anyhow::Result;
use deadpool_sqlite::Pool;
use ohara_core::{
    query::IndexStatus,
    storage::{CommitRecord, HunkHit, HunkRecord, Storage},
    types::{RepoId, Symbol},
    Result as CoreResult,
};
use std::path::Path;

pub struct SqliteStorage {
    pool: Pool,
}

impl SqliteStorage {
    pub async fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let pool = SqlitePoolBuilder::new(path).build().await?;
        let conn = pool.get().await?;
        conn.interact(|c| migrations::run(c))
            .await
            .map_err(|e| anyhow::anyhow!("interact: {e}"))??;
        Ok(Self { pool })
    }

    pub fn pool(&self) -> &Pool { &self.pool }
}

#[async_trait::async_trait]
impl Storage for SqliteStorage {
    async fn open_repo(&self, repo_id: &RepoId, path: &str, first_commit_sha: &str) -> CoreResult<()> {
        let id = repo_id.as_str().to_string();
        let path = path.to_string();
        let fcs = first_commit_sha.to_string();
        self.pool
            .get()
            .await
            .map_err(|e| ohara_core::OhraError::Storage(e.to_string()))?
            .interact(move |c| repo::upsert(c, &id, &path, &fcs))
            .await
            .map_err(|e| ohara_core::OhraError::Storage(e.to_string()))?
            .map_err(|e| ohara_core::OhraError::Storage(e.to_string()))
    }

    async fn get_index_status(&self, repo_id: &RepoId) -> CoreResult<IndexStatus> {
        let id = repo_id.as_str().to_string();
        self.pool
            .get()
            .await
            .map_err(|e| ohara_core::OhraError::Storage(e.to_string()))?
            .interact(move |c| repo::get_status(c, &id))
            .await
            .map_err(|e| ohara_core::OhraError::Storage(e.to_string()))?
            .map_err(|e| ohara_core::OhraError::Storage(e.to_string()))
    }

    async fn set_last_indexed_commit(&self, repo_id: &RepoId, sha: &str) -> CoreResult<()> {
        let id = repo_id.as_str().to_string();
        let sha = sha.to_string();
        self.pool
            .get()
            .await
            .map_err(|e| ohara_core::OhraError::Storage(e.to_string()))?
            .interact(move |c| repo::set_watermark(c, &id, &sha))
            .await
            .map_err(|e| ohara_core::OhraError::Storage(e.to_string()))?
            .map_err(|e| ohara_core::OhraError::Storage(e.to_string()))
    }

    async fn put_commit(&self, _: &RepoId, _: &CommitRecord) -> CoreResult<()> {
        // populated in Task 8
        unimplemented!()
    }
    async fn put_hunks(&self, _: &RepoId, _: &[HunkRecord]) -> CoreResult<()> { unimplemented!() }
    async fn put_head_symbols(&self, _: &RepoId, _: &[Symbol]) -> CoreResult<()> { unimplemented!() }
    async fn knn_hunks(&self, _: &RepoId, _: &[f32], _: u8, _: Option<&str>, _: Option<i64>) -> CoreResult<Vec<HunkHit>> { unimplemented!() }
    async fn blob_was_seen(&self, _: &str, _: &str) -> CoreResult<bool> { unimplemented!() }
    async fn record_blob_seen(&self, _: &str, _: &str) -> CoreResult<()> { unimplemented!() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn open_repo_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let s = SqliteStorage::open(dir.path().join("i.sqlite")).await.unwrap();
        let id = RepoId::from_parts("first", "/repo");
        s.open_repo(&id, "/repo", "first").await.unwrap();
        let st = s.get_index_status(&id).await.unwrap();
        assert!(st.last_indexed_commit.is_none());
        s.set_last_indexed_commit(&id, "abc").await.unwrap();
        let st2 = s.get_index_status(&id).await.unwrap();
        assert_eq!(st2.last_indexed_commit.as_deref(), Some("abc"));
    }
}
```

In `crates/ohara-storage/src/repo.rs`:
```rust
use anyhow::Result;
use chrono::Utc;
use ohara_core::query::IndexStatus;
use rusqlite::{params, Connection};

pub fn upsert(c: &Connection, id: &str, path: &str, first_commit_sha: &str) -> Result<()> {
    c.execute(
        "INSERT INTO repo (id, path, first_commit_sha, last_indexed_commit, indexed_at, schema_version)
         VALUES (?1, ?2, ?3, NULL, NULL, 1)
         ON CONFLICT(id) DO UPDATE SET path = excluded.path",
        params![id, path, first_commit_sha],
    )?;
    Ok(())
}

pub fn get_status(c: &Connection, id: &str) -> Result<IndexStatus> {
    let row: Option<(Option<String>, Option<String>)> = c
        .query_row(
            "SELECT last_indexed_commit, indexed_at FROM repo WHERE id = ?1",
            params![id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok();
    let (last_indexed_commit, indexed_at) = row.unwrap_or((None, None));
    Ok(IndexStatus {
        last_indexed_commit,
        commits_behind_head: 0, // computed by the caller from git rev-list
        indexed_at,
    })
}

pub fn set_watermark(c: &Connection, id: &str, sha: &str) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    c.execute(
        "UPDATE repo SET last_indexed_commit = ?2, indexed_at = ?3 WHERE id = ?1",
        params![id, sha, now],
    )?;
    Ok(())
}
```

In `crates/ohara-storage/src/lib.rs`:
```rust
pub mod migrations;
pub mod pool;
pub mod repo;
pub mod storage_impl;

pub use storage_impl::SqliteStorage;
```

- [ ] **Step 2: Run test (red — should fail since put_commit etc. are unimplemented)**

Run: `cargo test -p ohara-storage open_repo_round_trip`
Expected: PASS — this test only exercises repo functions.

- [ ] **Step 3: Commit (green — repo CRUD)**

```bash
git add crates/ohara-storage/src/repo.rs crates/ohara-storage/src/storage_impl.rs crates/ohara-storage/src/lib.rs
git commit -m "Add SqliteStorage with repo CRUD and watermark"
```

---

### Task 8: Commit insert/get + vec_commit wiring

**Files:**
- Create: `crates/ohara-storage/src/commit.rs`
- Modify: `crates/ohara-storage/src/storage_impl.rs`
- Modify: `crates/ohara-storage/src/lib.rs`

- [ ] **Step 1: Write the failing test**

Append to `crates/ohara-storage/src/storage_impl.rs::tests`:
```rust
    use ohara_core::types::CommitMeta;

    #[tokio::test]
    async fn put_commit_persists_meta_and_embedding() {
        let dir = tempfile::tempdir().unwrap();
        let s = SqliteStorage::open(dir.path().join("i.sqlite")).await.unwrap();
        let id = RepoId::from_parts("first", "/repo");
        s.open_repo(&id, "/repo", "first").await.unwrap();

        let cm = CommitMeta {
            sha: "abc".into(),
            parent_sha: None,
            is_merge: false,
            author: Some("alice".into()),
            ts: 1_700_000_000,
            message: "first commit".into(),
        };
        let emb = vec![0.1f32; 384];
        s.put_commit(&id, &CommitRecord { meta: cm.clone(), message_emb: emb }).await.unwrap();

        let pool = s.pool().clone();
        let count: i64 = pool.get().await.unwrap()
            .interact(|c| c.query_row("SELECT count(*) FROM commit_record", [], |r| r.get(0)))
            .await.unwrap().unwrap();
        assert_eq!(count, 1);
        let vec_count: i64 = pool.get().await.unwrap()
            .interact(|c| c.query_row("SELECT count(*) FROM vec_commit", [], |r| r.get(0)))
            .await.unwrap().unwrap();
        assert_eq!(vec_count, 1);
    }
```

In `crates/ohara-storage/src/commit.rs`:
```rust
use anyhow::Result;
use ohara_core::storage::CommitRecord;
use rusqlite::{params, Connection};

pub fn put(c: &Connection, record: &CommitRecord) -> Result<()> {
    c.execute(
        "INSERT OR REPLACE INTO commit_record (sha, parent_sha, is_merge, ts, author, message)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            &record.meta.sha,
            &record.meta.parent_sha,
            record.meta.is_merge as i64,
            record.meta.ts,
            &record.meta.author,
            &record.meta.message,
        ],
    )?;
    let bytes = vec_to_bytes(&record.message_emb);
    c.execute(
        "INSERT OR REPLACE INTO vec_commit (commit_sha, message_emb) VALUES (?1, ?2)",
        params![&record.meta.sha, bytes],
    )?;
    c.execute(
        "INSERT OR REPLACE INTO fts_commit (sha, message) VALUES (?1, ?2)",
        params![&record.meta.sha, &record.meta.message],
    )?;
    Ok(())
}

pub fn vec_to_bytes(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for f in v { out.extend_from_slice(&f.to_le_bytes()); }
    out
}

pub fn bytes_to_vec(b: &[u8]) -> Vec<f32> {
    let mut out = Vec::with_capacity(b.len() / 4);
    for chunk in b.chunks_exact(4) {
        out.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    out
}
```

Replace `put_commit` in `storage_impl.rs`:
```rust
    async fn put_commit(&self, _repo_id: &RepoId, record: &CommitRecord) -> CoreResult<()> {
        let rec = record.clone();
        self.pool
            .get()
            .await
            .map_err(|e| ohara_core::OhraError::Storage(e.to_string()))?
            .interact(move |c| crate::commit::put(c, &rec))
            .await
            .map_err(|e| ohara_core::OhraError::Storage(e.to_string()))?
            .map_err(|e| ohara_core::OhraError::Storage(e.to_string()))
    }
```

In `crates/ohara-storage/src/lib.rs` add `pub mod commit;`.

- [ ] **Step 2: Run test (red → expect FAIL with "unimplemented" error originally; now PASS)**

Run: `cargo test -p ohara-storage put_commit_persists`
Expected: PASS.

- [ ] **Step 3: Commit (green — commit insert)**

```bash
git add crates/ohara-storage/src/commit.rs crates/ohara-storage/src/storage_impl.rs crates/ohara-storage/src/lib.rs
git commit -m "Implement put_commit with vec_commit and fts_commit"
```

---

### Task 9: Hunks, blob_cache, and KNN — completing SqliteStorage

**Files:**
- Create: `crates/ohara-storage/src/hunk.rs`
- Create: `crates/ohara-storage/src/blob_cache.rs`
- Modify: `crates/ohara-storage/src/storage_impl.rs`
- Modify: `crates/ohara-storage/src/lib.rs`

- [ ] **Step 1: Write the failing tests**

Append to `crates/ohara-storage/src/storage_impl.rs::tests`:
```rust
    use ohara_core::types::{ChangeKind, Hunk};

    async fn fixture_storage_with_repo() -> (tempfile::TempDir, SqliteStorage, RepoId) {
        let dir = tempfile::tempdir().unwrap();
        let s = SqliteStorage::open(dir.path().join("i.sqlite")).await.unwrap();
        let id = RepoId::from_parts("first", "/repo");
        s.open_repo(&id, "/repo", "first").await.unwrap();
        (dir, s, id)
    }

    #[tokio::test]
    async fn put_hunks_creates_file_paths_and_vec_rows() {
        let (_dir, s, id) = fixture_storage_with_repo().await;

        // Need a parent commit row for FK
        let cm = CommitMeta { sha: "c1".into(), parent_sha: None, is_merge: false, author: None, ts: 1, message: "m".into() };
        s.put_commit(&id, &CommitRecord { meta: cm, message_emb: vec![0.0; 384] }).await.unwrap();

        let h = HunkRecord {
            hunk: Hunk {
                commit_sha: "c1".into(),
                file_path: "src/x.rs".into(),
                language: Some("rust".into()),
                change_kind: ChangeKind::Added,
                diff_text: "+fn x() {}\n".into(),
            },
            diff_emb: vec![0.5f32; 384],
        };
        s.put_hunks(&id, &[h]).await.unwrap();

        let pool = s.pool().clone();
        let n: i64 = pool.get().await.unwrap()
            .interact(|c| c.query_row("SELECT count(*) FROM hunk", [], |r| r.get(0)))
            .await.unwrap().unwrap();
        assert_eq!(n, 1);
        let vn: i64 = pool.get().await.unwrap()
            .interact(|c| c.query_row("SELECT count(*) FROM vec_hunk", [], |r| r.get(0)))
            .await.unwrap().unwrap();
        assert_eq!(vn, 1);
    }

    #[tokio::test]
    async fn knn_hunks_returns_nearest() {
        let (_dir, s, id) = fixture_storage_with_repo().await;
        let cm = CommitMeta { sha: "c1".into(), parent_sha: None, is_merge: false, author: None, ts: 1, message: "m".into() };
        s.put_commit(&id, &CommitRecord { meta: cm, message_emb: vec![0.0; 384] }).await.unwrap();

        let mk_hunk = |emb_val: f32, name: &str| HunkRecord {
            hunk: Hunk {
                commit_sha: "c1".into(),
                file_path: format!("src/{name}.rs"),
                language: Some("rust".into()),
                change_kind: ChangeKind::Added,
                diff_text: format!("+fn {name}() {{}}\n"),
            },
            diff_emb: vec![emb_val; 384],
        };
        s.put_hunks(&id, &[mk_hunk(0.1, "a"), mk_hunk(0.5, "b"), mk_hunk(0.9, "c")]).await.unwrap();

        let q = vec![0.49f32; 384];
        let hits = s.knn_hunks(&id, &q, 2, None, None).await.unwrap();
        assert_eq!(hits.len(), 2);
        // The closest by L2 should be the one with 0.5
        assert!(hits[0].hunk.file_path.ends_with("b.rs"));
    }

    #[tokio::test]
    async fn blob_cache_round_trips() {
        let (_dir, s, _id) = fixture_storage_with_repo().await;
        assert!(!s.blob_was_seen("blob1", "bge-small-v1.5").await.unwrap());
        s.record_blob_seen("blob1", "bge-small-v1.5").await.unwrap();
        assert!(s.blob_was_seen("blob1", "bge-small-v1.5").await.unwrap());
        assert!(!s.blob_was_seen("blob1", "voyage-code-3").await.unwrap()); // model swap invalidates
    }
```

In `crates/ohara-storage/src/hunk.rs`:
```rust
use anyhow::Result;
use ohara_core::{
    storage::{HunkHit, HunkRecord},
    types::{ChangeKind, CommitMeta, Hunk},
};
use rusqlite::{params, Connection};

use crate::commit::vec_to_bytes;

pub fn put_many(c: &Connection, records: &[HunkRecord]) -> Result<()> {
    if records.is_empty() { return Ok(()); }
    let tx = c.unchecked_transaction()?;
    for r in records {
        let file_path_id = upsert_file_path(&tx, &r.hunk.file_path, r.hunk.language.as_deref())?;
        tx.execute(
            "INSERT INTO hunk (commit_sha, file_path_id, change_kind, diff_text)
             VALUES (?1, ?2, ?3, ?4)",
            params![&r.hunk.commit_sha, file_path_id, change_kind_to_str(r.hunk.change_kind), &r.hunk.diff_text],
        )?;
        let hunk_id: i64 = tx.last_insert_rowid();
        tx.execute(
            "INSERT INTO vec_hunk (hunk_id, diff_emb) VALUES (?1, ?2)",
            params![hunk_id, vec_to_bytes(&r.diff_emb)],
        )?;
    }
    tx.commit()?;
    Ok(())
}

fn upsert_file_path(c: &Connection, path: &str, lang: Option<&str>) -> Result<i64> {
    c.execute(
        "INSERT OR IGNORE INTO file_path (path, language, active) VALUES (?1, ?2, 1)",
        params![path, lang],
    )?;
    let id: i64 = c.query_row(
        "SELECT id FROM file_path WHERE path = ?1",
        params![path],
        |r| r.get(0),
    )?;
    Ok(id)
}

fn change_kind_to_str(k: ChangeKind) -> &'static str {
    match k {
        ChangeKind::Added => "added",
        ChangeKind::Modified => "modified",
        ChangeKind::Deleted => "deleted",
        ChangeKind::Renamed => "renamed",
    }
}

fn str_to_change_kind(s: &str) -> ChangeKind {
    match s {
        "added" => ChangeKind::Added,
        "modified" => ChangeKind::Modified,
        "deleted" => ChangeKind::Deleted,
        "renamed" => ChangeKind::Renamed,
        _ => ChangeKind::Modified,
    }
}

pub fn knn(
    c: &Connection,
    query_emb: &[f32],
    k: u8,
    language: Option<&str>,
    since_unix: Option<i64>,
) -> Result<Vec<HunkHit>> {
    let q_bytes = vec_to_bytes(query_emb);
    let lang_filter = language.map(|_| "AND fp.language = ?lang").unwrap_or("");
    let ts_filter = since_unix.map(|_| "AND cr.ts >= ?ts").unwrap_or("");
    let sql = format!(
        "SELECT h.commit_sha, fp.path, fp.language, h.change_kind, h.diff_text,
                cr.parent_sha, cr.is_merge, cr.author, cr.ts, cr.message,
                v.distance
         FROM vec_hunk v
         JOIN hunk h ON h.id = v.hunk_id
         JOIN file_path fp ON fp.id = h.file_path_id
         JOIN commit_record cr ON cr.sha = h.commit_sha
         WHERE v.diff_emb MATCH ?emb AND k = ?k
         {ts_filter} {lang_filter}
         ORDER BY v.distance ASC"
    );
    // Build params dynamically
    let mut stmt = c.prepare(&sql)?;
    let mut binds: Vec<(&str, Box<dyn rusqlite::ToSql>)> = vec![
        (":emb", Box::new(q_bytes)),
        (":k", Box::new(k as i64)),
    ];
    if let Some(l) = language { binds.push((":lang", Box::new(l.to_string()))); }
    if let Some(t) = since_unix { binds.push((":ts", Box::new(t))); }
    let named: Vec<(&str, &dyn rusqlite::ToSql)> = binds.iter().map(|(n, v)| (*n, v.as_ref())).collect();

    let mut rows = stmt.query(named.as_slice())?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let distance: f32 = row.get(10)?;
        let similarity = (1.0 - distance.min(1.0).max(0.0)).clamp(0.0, 1.0);
        out.push(HunkHit {
            hunk: Hunk {
                commit_sha: row.get(0)?,
                file_path: row.get(1)?,
                language: row.get(2)?,
                change_kind: str_to_change_kind(&row.get::<_, String>(3)?),
                diff_text: row.get(4)?,
            },
            commit: CommitMeta {
                sha: row.get(0)?,
                parent_sha: row.get(5)?,
                is_merge: row.get::<_, i64>(6)? != 0,
                author: row.get(7)?,
                ts: row.get(8)?,
                message: row.get(9)?,
            },
            similarity,
        });
    }
    Ok(out)
}
```

In `crates/ohara-storage/src/blob_cache.rs`:
```rust
use anyhow::Result;
use chrono::Utc;
use rusqlite::{params, Connection};

pub fn was_seen(c: &Connection, blob_sha: &str, model: &str) -> Result<bool> {
    let n: i64 = c.query_row(
        "SELECT count(*) FROM blob_cache WHERE blob_sha = ?1 AND embedding_model = ?2",
        params![blob_sha, model],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

pub fn record(c: &Connection, blob_sha: &str, model: &str) -> Result<()> {
    let now = Utc::now().timestamp();
    c.execute(
        "INSERT OR REPLACE INTO blob_cache (blob_sha, embedding_model, embedded_at) VALUES (?1, ?2, ?3)",
        params![blob_sha, model, now],
    )?;
    Ok(())
}
```

Replace the remaining `unimplemented!()` methods in `storage_impl.rs`:
```rust
    async fn put_hunks(&self, _repo_id: &RepoId, records: &[HunkRecord]) -> CoreResult<()> {
        let recs = records.to_vec();
        self.pool
            .get()
            .await
            .map_err(|e| ohara_core::OhraError::Storage(e.to_string()))?
            .interact(move |c| crate::hunk::put_many(c, &recs))
            .await
            .map_err(|e| ohara_core::OhraError::Storage(e.to_string()))?
            .map_err(|e| ohara_core::OhraError::Storage(e.to_string()))
    }

    async fn put_head_symbols(&self, _repo_id: &RepoId, _symbols: &[Symbol]) -> CoreResult<()> {
        // No-op in Plan 1 since find_pattern doesn't read symbols.
        // Plan 2 will populate symbol + symbol_lineage tables.
        Ok(())
    }

    async fn knn_hunks(
        &self,
        _repo_id: &RepoId,
        query_emb: &[f32],
        k: u8,
        language: Option<&str>,
        since_unix: Option<i64>,
    ) -> CoreResult<Vec<HunkHit>> {
        let qe = query_emb.to_vec();
        let lang = language.map(str::to_string);
        self.pool
            .get()
            .await
            .map_err(|e| ohara_core::OhraError::Storage(e.to_string()))?
            .interact(move |c| crate::hunk::knn(c, &qe, k, lang.as_deref(), since_unix))
            .await
            .map_err(|e| ohara_core::OhraError::Storage(e.to_string()))?
            .map_err(|e| ohara_core::OhraError::Storage(e.to_string()))
    }

    async fn blob_was_seen(&self, blob_sha: &str, model: &str) -> CoreResult<bool> {
        let blob = blob_sha.to_string();
        let m = model.to_string();
        self.pool
            .get()
            .await
            .map_err(|e| ohara_core::OhraError::Storage(e.to_string()))?
            .interact(move |c| crate::blob_cache::was_seen(c, &blob, &m))
            .await
            .map_err(|e| ohara_core::OhraError::Storage(e.to_string()))?
            .map_err(|e| ohara_core::OhraError::Storage(e.to_string()))
    }

    async fn record_blob_seen(&self, blob_sha: &str, model: &str) -> CoreResult<()> {
        let blob = blob_sha.to_string();
        let m = model.to_string();
        self.pool
            .get()
            .await
            .map_err(|e| ohara_core::OhraError::Storage(e.to_string()))?
            .interact(move |c| crate::blob_cache::record(c, &blob, &m))
            .await
            .map_err(|e| ohara_core::OhraError::Storage(e.to_string()))?
            .map_err(|e| ohara_core::OhraError::Storage(e.to_string()))
    }
```

In `crates/ohara-storage/src/lib.rs`:
```rust
pub mod blob_cache;
pub mod commit;
pub mod hunk;
pub mod migrations;
pub mod pool;
pub mod repo;
pub mod storage_impl;

pub use storage_impl::SqliteStorage;
```

- [ ] **Step 2: Run tests (green)**

Run: `cargo test -p ohara-storage`
Expected: all storage tests pass.

- [ ] **Step 3: Commit (green — full SqliteStorage)**

```bash
git add crates/ohara-storage/src/hunk.rs crates/ohara-storage/src/blob_cache.rs crates/ohara-storage/src/storage_impl.rs crates/ohara-storage/src/lib.rs
git commit -m "Implement put_hunks, knn_hunks, and blob_cache; SqliteStorage complete for Plan 1"
```

---

### Task 10: ohara-embed FastEmbedProvider

**Files:**
- Create: `crates/ohara-embed/src/fastembed.rs`
- Modify: `crates/ohara-embed/src/lib.rs`
- Modify: `crates/ohara-embed/Cargo.toml`

- [ ] **Step 1: Write the failing test**

In `crates/ohara-embed/src/fastembed.rs`:
```rust
use anyhow::Result;
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use ohara_core::{EmbeddingProvider, Result as CoreResult};
use std::sync::Mutex;

const DEFAULT_MODEL_ID: &str = "bge-small-en-v1.5";
const DEFAULT_DIM: usize = 384;

pub struct FastEmbedProvider {
    model: Mutex<TextEmbedding>,
    model_id: String,
    dim: usize,
}

impl FastEmbedProvider {
    pub fn new() -> Result<Self> {
        let model = TextEmbedding::try_new(InitOptions {
            model_name: EmbeddingModel::BGESmallENV15,
            show_download_progress: false,
            ..Default::default()
        })?;
        Ok(Self { model: Mutex::new(model), model_id: DEFAULT_MODEL_ID.into(), dim: DEFAULT_DIM })
    }
}

#[async_trait::async_trait]
impl EmbeddingProvider for FastEmbedProvider {
    fn dimension(&self) -> usize { self.dim }

    fn model_id(&self) -> &str { &self.model_id }

    async fn embed_batch(&self, texts: &[String]) -> CoreResult<Vec<Vec<f32>>> {
        if texts.is_empty() { return Ok(vec![]); }
        let owned: Vec<String> = texts.to_vec();
        let model = std::sync::Arc::new(self.model.lock().expect("poisoned").clone_handle()); // see note below
        let res = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<Vec<f32>>> {
            // Re-acquire actual TextEmbedding inside spawn_blocking via a fresh Mutex lock.
            unreachable!("see note: this approach is replaced below");
        }).await
        .map_err(|e| ohara_core::OhraError::Embedding(e.to_string()))?;
        res.map_err(|e| ohara_core::OhraError::Embedding(e.to_string()))
    }
}
```

The "clone_handle" pattern above doesn't exist — `TextEmbedding` isn't trivially cloneable. The right pattern is to do the embedding *synchronously* inside `tokio::task::spawn_blocking` while holding the `Mutex` for the duration of the call. Replace with:

```rust
use anyhow::Result;
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use ohara_core::{EmbeddingProvider, Result as CoreResult};
use std::sync::Arc;
use tokio::sync::Mutex;

const DEFAULT_MODEL_ID: &str = "bge-small-en-v1.5";
const DEFAULT_DIM: usize = 384;

pub struct FastEmbedProvider {
    model: Arc<Mutex<TextEmbedding>>,
    model_id: String,
    dim: usize,
}

impl FastEmbedProvider {
    pub fn new() -> Result<Self> {
        let model = TextEmbedding::try_new(InitOptions {
            model_name: EmbeddingModel::BGESmallENV15,
            show_download_progress: false,
            ..Default::default()
        })?;
        Ok(Self { model: Arc::new(Mutex::new(model)), model_id: DEFAULT_MODEL_ID.into(), dim: DEFAULT_DIM })
    }
}

#[async_trait::async_trait]
impl EmbeddingProvider for FastEmbedProvider {
    fn dimension(&self) -> usize { self.dim }
    fn model_id(&self) -> &str { &self.model_id }

    async fn embed_batch(&self, texts: &[String]) -> CoreResult<Vec<Vec<f32>>> {
        if texts.is_empty() { return Ok(vec![]); }
        let model = self.model.clone();
        let owned: Vec<String> = texts.to_vec();
        let result = tokio::task::spawn_blocking(move || {
            let mut guard = model.blocking_lock();
            let refs: Vec<&str> = owned.iter().map(|s| s.as_str()).collect();
            guard.embed(refs, None)
        })
        .await
        .map_err(|e| ohara_core::OhraError::Embedding(format!("join: {e}")))?;
        result.map_err(|e| ohara_core::OhraError::Embedding(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ohara_core::EmbeddingProvider;

    #[tokio::test]
    #[ignore = "downloads ~80MB on first run; opt-in via `cargo test -- --include-ignored`"]
    async fn embeds_returns_correct_dimension_and_count() {
        let p = FastEmbedProvider::new().unwrap();
        let texts = vec!["hello".to_string(), "retry with backoff".to_string()];
        let out = p.embed_batch(&texts).await.unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].len(), p.dimension());
        assert!(out[0].iter().any(|&x| x != 0.0));
    }
}
```

In `crates/ohara-embed/src/lib.rs`:
```rust
pub mod fastembed;
pub use fastembed::FastEmbedProvider;
```

- [ ] **Step 2: Build to verify type-checking**

Run: `cargo build -p ohara-embed`
Expected: clean. (The first `embed_batch` block in this task description is intentionally illustrating the wrong approach — when typing this up, only commit the second, correct version.)

- [ ] **Step 3: Run the gated test (one-time, offline thereafter)**

Run: `cargo test -p ohara-embed -- --include-ignored`
Expected: passes after one-time model download.

- [ ] **Step 4: Commit (green — embedding provider)**

```bash
git add crates/ohara-embed/src/fastembed.rs crates/ohara-embed/src/lib.rs
git commit -m "Add FastEmbedProvider over fastembed-rs with BGE small (384d)"
```

---

### Task 11: ohara-git commit walker

**Files:**
- Create: `crates/ohara-git/src/walker.rs`
- Modify: `crates/ohara-git/src/lib.rs`

- [ ] **Step 1: Write the failing test against a tiny in-process repo**

In `crates/ohara-git/src/walker.rs`:
```rust
use anyhow::{Context, Result};
use git2::{Repository, Sort};
use ohara_core::types::CommitMeta;
use std::path::Path;

pub struct GitWalker {
    repo: Repository,
}

impl GitWalker {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let repo = Repository::discover(path).context("discover git repo")?;
        Ok(Self { repo })
    }

    pub fn first_commit_sha(&self) -> Result<String> {
        let mut walk = self.repo.revwalk()?;
        walk.set_sorting(Sort::TIME | Sort::REVERSE)?;
        walk.push_head()?;
        let oid = walk.next().context("empty repo")??;
        Ok(oid.to_string())
    }

    pub fn list_commits(&self, since: Option<&str>) -> Result<Vec<CommitMeta>> {
        let mut walk = self.repo.revwalk()?;
        walk.set_sorting(Sort::TOPOLOGICAL | Sort::REVERSE)?;
        walk.push_head()?;
        if let Some(s) = since {
            // Hide the watermark and its ancestors so we get only newer commits.
            let oid = git2::Oid::from_str(s)?;
            walk.hide(oid)?;
        }
        let mut out = Vec::new();
        for oid in walk {
            let oid = oid?;
            let c = self.repo.find_commit(oid)?;
            let parent_sha = c.parent_count().checked_sub(1).map(|_| c.parent(0).ok())
                .flatten()
                .map(|p| p.id().to_string());
            out.push(CommitMeta {
                sha: oid.to_string(),
                parent_sha,
                is_merge: c.parent_count() > 1,
                author: Some(c.author().name().unwrap_or("").to_string()).filter(|s| !s.is_empty()),
                ts: c.time().seconds(),
                message: c.message().unwrap_or("").to_string(),
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::{Repository, Signature};
    use std::fs;

    fn init_repo_with_commits(dir: &std::path::Path, msgs: &[&str]) -> Repository {
        let repo = Repository::init(dir).unwrap();
        let sig = Signature::now("a", "a@a").unwrap();
        let mut parent: Option<git2::Oid> = None;
        for (i, m) in msgs.iter().enumerate() {
            fs::write(dir.join(format!("f{i}.txt")), format!("v{i}")).unwrap();
            let mut idx = repo.index().unwrap();
            idx.add_path(std::path::Path::new(&format!("f{i}.txt"))).unwrap();
            idx.write().unwrap();
            let tree_id = idx.write_tree().unwrap();
            let tree = repo.find_tree(tree_id).unwrap();
            let parents: Vec<git2::Commit> = parent.iter().map(|p| repo.find_commit(*p).unwrap()).collect();
            let parent_refs: Vec<&git2::Commit> = parents.iter().collect();
            let oid = repo.commit(Some("HEAD"), &sig, &sig, m, &tree, &parent_refs).unwrap();
            parent = Some(oid);
        }
        repo
    }

    #[test]
    fn list_commits_in_topological_reverse_order() {
        let dir = tempfile::tempdir().unwrap();
        init_repo_with_commits(dir.path(), &["a", "b", "c"]);
        let w = GitWalker::open(dir.path()).unwrap();
        let cs = w.list_commits(None).unwrap();
        assert_eq!(cs.len(), 3);
        assert_eq!(cs[0].message.trim(), "a");
        assert_eq!(cs[2].message.trim(), "c");
    }

    #[test]
    fn list_commits_since_returns_only_newer() {
        let dir = tempfile::tempdir().unwrap();
        init_repo_with_commits(dir.path(), &["a", "b", "c"]);
        let w = GitWalker::open(dir.path()).unwrap();
        let all = w.list_commits(None).unwrap();
        let mid = &all[1].sha; // "b"
        let after = w.list_commits(Some(mid)).unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].message.trim(), "c");
    }
}
```

Add to `crates/ohara-git/Cargo.toml`:
```toml
[dev-dependencies]
tempfile = "3"
```

In `crates/ohara-git/src/lib.rs`:
```rust
pub mod walker;
pub use walker::GitWalker;
```

- [ ] **Step 2: Run tests (green — should pass with the impl)**

Run: `cargo test -p ohara-git walker`
Expected: 2 passed.

- [ ] **Step 3: Commit (green — walker)**

```bash
git add crates/ohara-git/src/walker.rs crates/ohara-git/src/lib.rs crates/ohara-git/Cargo.toml
git commit -m "Add GitWalker for listing commits with optional watermark"
```

---

### Task 12: ohara-git per-commit diff extraction + CommitSource impl

**Files:**
- Create: `crates/ohara-git/src/diff.rs`
- Modify: `crates/ohara-git/src/lib.rs`

- [ ] **Step 1: Write the failing test**

In `crates/ohara-git/src/diff.rs`:
```rust
use anyhow::{Context, Result};
use git2::{Diff, DiffFormat, DiffOptions, Oid, Repository};
use ohara_core::types::{ChangeKind, Hunk};
use std::path::Path;

pub fn hunks_for_commit(repo: &Repository, sha: &str) -> Result<Vec<Hunk>> {
    let oid = Oid::from_str(sha).context("parse oid")?;
    let commit = repo.find_commit(oid).context("find commit")?;
    let tree = commit.tree()?;
    let parent_tree = if commit.parent_count() > 0 {
        Some(commit.parent(0)?.tree()?)
    } else {
        None
    };

    let mut opts = DiffOptions::new();
    opts.context_lines(3).interhunk_lines(0).ignore_whitespace_eol(true);

    let diff = match parent_tree.as_ref() {
        Some(p) => repo.diff_tree_to_tree(Some(p), Some(&tree), Some(&mut opts))?,
        None => repo.diff_tree_to_tree(None, Some(&tree), Some(&mut opts))?,
    };

    let mut hunks: Vec<Hunk> = Vec::new();
    let mut current: Option<(String, ChangeKind)> = None;
    let mut buf = String::new();

    diff.print(DiffFormat::Patch, |delta, _hunk, line| {
        let path = delta.new_file().path().or_else(|| delta.old_file().path())
            .map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
        let ck = match delta.status() {
            git2::Delta::Added => ChangeKind::Added,
            git2::Delta::Deleted => ChangeKind::Deleted,
            git2::Delta::Renamed => ChangeKind::Renamed,
            _ => ChangeKind::Modified,
        };
        match &current {
            Some((p, _)) if *p != path => {
                hunks.push(make_hunk(sha, p, current.as_ref().unwrap().1, std::mem::take(&mut buf)));
                current = Some((path.clone(), ck));
            }
            None => current = Some((path.clone(), ck)),
            _ => {}
        }
        let prefix = match line.origin() {
            '+' | '-' | ' ' => format!("{}", line.origin()),
            _ => String::new(),
        };
        buf.push_str(&prefix);
        buf.push_str(std::str::from_utf8(line.content()).unwrap_or(""));
        true
    })?;

    if let Some((p, ck)) = current.take() {
        hunks.push(make_hunk(sha, &p, ck, buf));
    }
    Ok(hunks)
}

fn make_hunk(sha: &str, file_path: &str, ck: ChangeKind, diff_text: String) -> Hunk {
    let language = detect_language(file_path);
    Hunk {
        commit_sha: sha.to_string(),
        file_path: file_path.to_string(),
        language,
        change_kind: ck,
        diff_text,
    }
}

fn detect_language(path: &str) -> Option<String> {
    let ext = Path::new(path).extension()?.to_str()?;
    Some(match ext {
        "rs" => "rust",
        "py" => "python",
        "ts" | "tsx" => "typescript",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "go" => "go",
        _ => return None,
    }.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::{Repository, Signature};

    fn init_with_two_commits(dir: &std::path::Path) -> Repository {
        let repo = Repository::init(dir).unwrap();
        let sig = Signature::now("a", "a@a").unwrap();
        std::fs::write(dir.join("a.rs"), "fn a() {}\n").unwrap();
        let mut idx = repo.index().unwrap();
        idx.add_path(std::path::Path::new("a.rs")).unwrap(); idx.write().unwrap();
        let t1 = idx.write_tree().unwrap();
        let c1 = repo.commit(Some("HEAD"), &sig, &sig, "first", &repo.find_tree(t1).unwrap(), &[]).unwrap();

        std::fs::write(dir.join("a.rs"), "fn a() { println!(); }\n").unwrap();
        let mut idx = repo.index().unwrap();
        idx.add_path(std::path::Path::new("a.rs")).unwrap(); idx.write().unwrap();
        let t2 = idx.write_tree().unwrap();
        let p = repo.find_commit(c1).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "second", &repo.find_tree(t2).unwrap(), &[&p]).unwrap();
        repo
    }

    #[test]
    fn extract_diff_for_modifying_commit() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_with_two_commits(dir.path());
        let mut walk = repo.revwalk().unwrap();
        walk.set_sorting(git2::Sort::TIME).unwrap();
        walk.push_head().unwrap();
        let head = walk.next().unwrap().unwrap().to_string();

        let hunks = hunks_for_commit(&repo, &head).unwrap();
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].file_path, "a.rs");
        assert!(matches!(hunks[0].change_kind, ChangeKind::Modified));
        assert!(hunks[0].diff_text.contains("println"));
        assert_eq!(hunks[0].language.as_deref(), Some("rust"));
    }
}
```

- [ ] **Step 2: Add the `CommitSource` impl that ties walker + diff together**

Append to `crates/ohara-git/src/lib.rs`:
```rust
pub mod diff;
pub mod walker;

pub use walker::GitWalker;

use anyhow::Result;
use ohara_core::indexer::CommitSource;
use ohara_core::types::{CommitMeta, Hunk};

pub struct GitCommitSource {
    walker: GitWalker,
    repo_path: std::path::PathBuf,
}

impl GitCommitSource {
    pub fn open<P: AsRef<std::path::Path>>(path: P) -> Result<Self> {
        let walker = GitWalker::open(&path)?;
        Ok(Self { walker, repo_path: path.as_ref().to_path_buf() })
    }
    pub fn walker(&self) -> &GitWalker { &self.walker }
}

#[async_trait::async_trait]
impl CommitSource for GitCommitSource {
    async fn list_commits(&self, since: Option<&str>) -> ohara_core::Result<Vec<CommitMeta>> {
        let since = since.map(str::to_string);
        let path = self.repo_path.clone();
        tokio::task::spawn_blocking(move || -> ohara_core::Result<Vec<CommitMeta>> {
            let w = GitWalker::open(&path)
                .map_err(|e| ohara_core::OhraError::Git(e.to_string()))?;
            w.list_commits(since.as_deref())
                .map_err(|e| ohara_core::OhraError::Git(e.to_string()))
        })
        .await
        .map_err(|e| ohara_core::OhraError::Git(e.to_string()))?
    }

    async fn hunks_for_commit(&self, sha: &str) -> ohara_core::Result<Vec<Hunk>> {
        let sha = sha.to_string();
        let path = self.repo_path.clone();
        tokio::task::spawn_blocking(move || -> ohara_core::Result<Vec<Hunk>> {
            let repo = git2::Repository::discover(path)
                .map_err(|e| ohara_core::OhraError::Git(e.to_string()))?;
            crate::diff::hunks_for_commit(&repo, &sha)
                .map_err(|e| ohara_core::OhraError::Git(e.to_string()))
        })
        .await
        .map_err(|e| ohara_core::OhraError::Git(e.to_string()))?
    }
}
```

Add to `crates/ohara-git/Cargo.toml`:
```toml
async-trait = "0.1"
tokio.workspace = true
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p ohara-git`
Expected: all passing.

- [ ] **Step 4: Commit (green — diff extraction + CommitSource)**

```bash
git add crates/ohara-git/src/diff.rs crates/ohara-git/src/lib.rs crates/ohara-git/Cargo.toml
git commit -m "Add per-commit diff extraction and GitCommitSource impl of CommitSource"
```

---

### Task 13: ohara-parse — Rust + Python tree-sitter and SymbolSource

**Files:**
- Create: `crates/ohara-parse/queries/rust.scm`
- Create: `crates/ohara-parse/queries/python.scm`
- Create: `crates/ohara-parse/src/rust.rs`
- Create: `crates/ohara-parse/src/python.rs`
- Modify: `crates/ohara-parse/src/lib.rs`
- Modify: `crates/ohara-parse/Cargo.toml`

The tree-sitter queries use the standard `(capture)` syntax. We extract function-like definitions and their names. `qualified_name` is left None at this layer; the higher layer can prefix module paths if/when needed.

- [ ] **Step 1: Write the rust query**

`crates/ohara-parse/queries/rust.scm`:
```scheme
(function_item name: (identifier) @name) @def_function
(impl_item type: (type_identifier) @impl_type
  body: (declaration_list (function_item name: (identifier) @method_name) @def_method))
(struct_item name: (type_identifier) @struct_name) @def_struct
(enum_item name: (type_identifier) @enum_name) @def_enum
```

- [ ] **Step 2: Write the python query**

`crates/ohara-parse/queries/python.scm`:
```scheme
(function_definition name: (identifier) @func_name) @def_function
(class_definition
  name: (identifier) @class_name
  body: (block
    (function_definition name: (identifier) @method_name) @def_method)) @def_class
```

- [ ] **Step 3: Write the failing test for Rust extraction**

In `crates/ohara-parse/src/rust.rs`:
```rust
use anyhow::{Context, Result};
use ohara_core::types::{Symbol, SymbolKind};
use tree_sitter::{Parser, Query, QueryCursor};

const QUERY_SRC: &str = include_str!("../queries/rust.scm");

pub fn extract(file_path: &str, source: &str, blob_sha: &str) -> Result<Vec<Symbol>> {
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_rust::LANGUAGE.into()).context("set rust language")?;
    let tree = parser.parse(source, None).context("parse rust")?;
    let query = Query::new(&tree_sitter_rust::LANGUAGE.into(), QUERY_SRC).context("rust query")?;
    let mut cursor = QueryCursor::new();

    let mut out = Vec::new();
    for m in cursor.matches(&query, tree.root_node(), source.as_bytes()) {
        let mut name: Option<String> = None;
        let mut kind: Option<SymbolKind> = None;
        let mut node_range: Option<(usize, usize)> = None;

        for cap in m.captures {
            let cap_name = &query.capture_names()[cap.index as usize];
            let n = cap.node;
            match *cap_name {
                "name" | "func_name" | "method_name" => {
                    name = Some(n.utf8_text(source.as_bytes())?.to_string());
                }
                "struct_name" => {
                    name = Some(n.utf8_text(source.as_bytes())?.to_string());
                    kind = Some(SymbolKind::Class);
                }
                "enum_name" => {
                    name = Some(n.utf8_text(source.as_bytes())?.to_string());
                    kind = Some(SymbolKind::Class);
                }
                "def_function" => {
                    kind.get_or_insert(SymbolKind::Function);
                    node_range = Some((n.start_byte(), n.end_byte()));
                }
                "def_method" => {
                    kind = Some(SymbolKind::Method);
                    node_range = Some((n.start_byte(), n.end_byte()));
                }
                "def_struct" | "def_enum" => {
                    node_range = Some((n.start_byte(), n.end_byte()));
                }
                _ => {}
            }
        }

        if let (Some(n), Some(k), Some((s, e))) = (name, kind, node_range) {
            let text = &source[s..e];
            out.push(Symbol {
                file_path: file_path.to_string(),
                language: "rust".to_string(),
                kind: k,
                name: n,
                qualified_name: None,
                span_start: s as u32,
                span_end: e as u32,
                blob_sha: blob_sha.to_string(),
                source_text: text.to_string(),
            });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_top_level_functions_and_methods() {
        let src = r#"
            fn alpha() {}
            struct Foo;
            impl Foo {
                fn beta(&self) {}
            }
            enum Color { Red }
        "#;
        let syms = extract("a.rs", src, "deadbeef").unwrap();
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"alpha"));
        assert!(names.contains(&"beta"));
        assert!(names.contains(&"Foo"));
        assert!(names.contains(&"Color"));
        assert!(syms.iter().any(|s| s.kind == SymbolKind::Method));
    }
}
```

- [ ] **Step 4: Write the Python extraction**

In `crates/ohara-parse/src/python.rs`:
```rust
use anyhow::{Context, Result};
use ohara_core::types::{Symbol, SymbolKind};
use tree_sitter::{Parser, Query, QueryCursor};

const QUERY_SRC: &str = include_str!("../queries/python.scm");

pub fn extract(file_path: &str, source: &str, blob_sha: &str) -> Result<Vec<Symbol>> {
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_python::LANGUAGE.into()).context("set python language")?;
    let tree = parser.parse(source, None).context("parse python")?;
    let query = Query::new(&tree_sitter_python::LANGUAGE.into(), QUERY_SRC).context("python query")?;
    let mut cursor = QueryCursor::new();

    let mut out = Vec::new();
    for m in cursor.matches(&query, tree.root_node(), source.as_bytes()) {
        let mut name: Option<String> = None;
        let mut kind: Option<SymbolKind> = None;
        let mut node_range: Option<(usize, usize)> = None;

        for cap in m.captures {
            let cap_name = &query.capture_names()[cap.index as usize];
            let n = cap.node;
            match *cap_name {
                "func_name" | "method_name" => {
                    name = Some(n.utf8_text(source.as_bytes())?.to_string());
                }
                "class_name" => {
                    name = Some(n.utf8_text(source.as_bytes())?.to_string());
                    kind = Some(SymbolKind::Class);
                }
                "def_function" => {
                    kind.get_or_insert(SymbolKind::Function);
                    node_range = Some((n.start_byte(), n.end_byte()));
                }
                "def_method" => {
                    kind = Some(SymbolKind::Method);
                    node_range = Some((n.start_byte(), n.end_byte()));
                }
                "def_class" => {
                    node_range = Some((n.start_byte(), n.end_byte()));
                }
                _ => {}
            }
        }

        if let (Some(n), Some(k), Some((s, e))) = (name, kind, node_range) {
            let text = &source[s..e];
            out.push(Symbol {
                file_path: file_path.to_string(),
                language: "python".to_string(),
                kind: k,
                name: n,
                qualified_name: None,
                span_start: s as u32,
                span_end: e as u32,
                blob_sha: blob_sha.to_string(),
                source_text: text.to_string(),
            });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_top_level_functions_and_class_methods() {
        let src = "def alpha():\n    pass\nclass Foo:\n    def beta(self):\n        pass\n";
        let syms = extract("a.py", src, "deadbeef").unwrap();
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"alpha"));
        assert!(names.contains(&"Foo"));
        assert!(names.contains(&"beta"));
    }
}
```

- [ ] **Step 5: Wire `lib.rs` with dispatch + GitSymbolSource**

`crates/ohara-parse/src/lib.rs`:
```rust
pub mod python;
pub mod rust;

use anyhow::Result;
use ohara_core::indexer::SymbolSource;
use ohara_core::types::Symbol;
use std::path::{Path, PathBuf};

pub fn extract_for_path(path: &str, source: &str, blob_sha: &str) -> Result<Vec<Symbol>> {
    let ext = Path::new(path).extension().and_then(|e| e.to_str());
    match ext {
        Some("rs") => rust::extract(path, source, blob_sha),
        Some("py") => python::extract(path, source, blob_sha),
        _ => Ok(vec![]),
    }
}

/// Walks the working tree at HEAD-equivalent state on disk and extracts symbols
/// from files in supported languages.
pub struct GitSymbolSource {
    repo_path: PathBuf,
}

impl GitSymbolSource {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        Ok(Self { repo_path: path.as_ref().to_path_buf() })
    }
}

#[async_trait::async_trait]
impl SymbolSource for GitSymbolSource {
    async fn extract_head_symbols(&self) -> ohara_core::Result<Vec<Symbol>> {
        let path = self.repo_path.clone();
        tokio::task::spawn_blocking(move || -> ohara_core::Result<Vec<Symbol>> {
            let repo = git2::Repository::discover(&path)
                .map_err(|e| ohara_core::OhraError::Git(e.to_string()))?;
            let head = repo.head().map_err(|e| ohara_core::OhraError::Git(e.to_string()))?;
            let tree = head.peel_to_tree().map_err(|e| ohara_core::OhraError::Git(e.to_string()))?;
            let mut out = Vec::new();
            tree.walk(git2::TreeWalkMode::PreOrder, |dir, entry| {
                if entry.kind() == Some(git2::ObjectType::Blob) {
                    let name = match entry.name() { Some(n) => n, None => return git2::TreeWalkResult::Ok };
                    let p = format!("{}{}", dir, name);
                    let blob_sha = entry.id().to_string();
                    if let Ok(blob) = repo.find_blob(entry.id()) {
                        if let Ok(s) = std::str::from_utf8(blob.content()) {
                            if let Ok(mut syms) = extract_for_path(&p, s, &blob_sha) {
                                out.append(&mut syms);
                            }
                        }
                    }
                }
                git2::TreeWalkResult::Ok
            }).map_err(|e| ohara_core::OhraError::Git(e.to_string()))?;
            Ok(out)
        })
        .await
        .map_err(|e| ohara_core::OhraError::Other(anyhow::anyhow!(e)))?
    }
}
```

Add to `crates/ohara-parse/Cargo.toml`:
```toml
async-trait = "0.1"
tokio.workspace = true
git2.workspace = true
```

- [ ] **Step 6: Run tests**

Run: `cargo test -p ohara-parse`
Expected: 2 passed.

- [ ] **Step 7: Commit (green — parse + symbol source)**

```bash
git add crates/ohara-parse/queries/ crates/ohara-parse/src/ crates/ohara-parse/Cargo.toml
git commit -m "Add tree-sitter Rust+Python symbol extraction with GitSymbolSource"
```

---

### Task 14: Retriever.find_pattern — async retrieval pipeline

**Files:**
- Modify: `crates/ohara-core/src/retriever.rs`

The pure ranking function `rank_hits` already exists from Task 4. Add the async `find_pattern` method that embeds the query, fetches a candidate set via `Storage::knn_hunks`, computes message similarities, and ranks.

- [ ] **Step 1: Write the failing integration-style test using a fake Storage and Embedder**

Append to `crates/ohara-core/src/retriever.rs`:

```rust
impl Retriever {
    pub async fn find_pattern(
        &self,
        repo_id: &crate::types::RepoId,
        query: &crate::query::PatternQuery,
        now_unix: i64,
    ) -> crate::Result<Vec<crate::query::PatternHit>> {
        let q_text = vec![query.query.clone()];
        let mut q_embs = self.embedder.embed_batch(&q_text).await?;
        let q_emb = q_embs.pop().ok_or_else(|| crate::OhraError::Embedding("empty".into()))?;

        let candidates = self
            .storage
            .knn_hunks(
                repo_id,
                &q_emb,
                query.k.clamp(1, 20),
                query.language.as_deref(),
                query.since_unix,
            )
            .await?;

        // Cosine similarity between the query embedding and each candidate's commit message.
        // We embed the messages in a single batch.
        let messages: Vec<String> = candidates.iter().map(|h| h.commit.message.clone()).collect();
        let msg_embs = if messages.is_empty() { vec![] } else { self.embedder.embed_batch(&messages).await? };
        let msg_sims: Vec<f32> = msg_embs.iter().map(|e| cosine(&q_emb, e)).collect();

        Ok(self.rank_hits(candidates, &msg_sims, now_unix))
    }
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 { 0.0 } else { dot / (na * nb) }
}
```

Append to the `tests` module in `retriever.rs`:
```rust
    use crate::query::PatternQuery;
    use crate::storage::{HunkRecord, CommitRecord};
    use crate::types::{Hunk, ChangeKind, CommitMeta, RepoId, Symbol};
    use std::sync::Arc;

    struct FakeEmbedder;
    #[async_trait::async_trait]
    impl crate::EmbeddingProvider for FakeEmbedder {
        fn dimension(&self) -> usize { 4 }
        fn model_id(&self) -> &str { "fake" }
        async fn embed_batch(&self, texts: &[String]) -> crate::Result<Vec<Vec<f32>>> {
            Ok(texts.iter().map(|t| match t.as_str() {
                "retry" => vec![1.0, 0.0, 0.0, 0.0],
                "added retry logic" => vec![1.0, 0.1, 0.0, 0.0],
                "renamed file" => vec![0.0, 1.0, 0.0, 0.0],
                _ => vec![0.0; 4],
            }).collect())
        }
    }

    struct FakeStorage { hits: Vec<HunkHit> }
    #[async_trait::async_trait]
    impl crate::Storage for FakeStorage {
        async fn open_repo(&self, _: &RepoId, _: &str, _: &str) -> crate::Result<()> { Ok(()) }
        async fn get_index_status(&self, _: &RepoId) -> crate::Result<crate::query::IndexStatus> { Ok(crate::query::IndexStatus { last_indexed_commit: None, commits_behind_head: 0, indexed_at: None }) }
        async fn set_last_indexed_commit(&self, _: &RepoId, _: &str) -> crate::Result<()> { Ok(()) }
        async fn put_commit(&self, _: &RepoId, _: &CommitRecord) -> crate::Result<()> { Ok(()) }
        async fn put_hunks(&self, _: &RepoId, _: &[HunkRecord]) -> crate::Result<()> { Ok(()) }
        async fn put_head_symbols(&self, _: &RepoId, _: &[Symbol]) -> crate::Result<()> { Ok(()) }
        async fn knn_hunks(&self, _: &RepoId, _: &[f32], _: u8, _: Option<&str>, _: Option<i64>) -> crate::Result<Vec<HunkHit>> { Ok(self.hits.clone()) }
        async fn blob_was_seen(&self, _: &str, _: &str) -> crate::Result<bool> { Ok(false) }
        async fn record_blob_seen(&self, _: &str, _: &str) -> crate::Result<()> { Ok(()) }
    }

    fn fake_hit_with_msg(sha: &str, ts: i64, sim: f32, msg: &str) -> HunkHit {
        HunkHit {
            hunk: Hunk { commit_sha: sha.into(), file_path: "a.rs".into(), language: Some("rust".into()), change_kind: ChangeKind::Added, diff_text: "+x".into() },
            commit: CommitMeta { sha: sha.into(), parent_sha: None, is_merge: false, author: None, ts, message: msg.into() },
            similarity: sim,
        }
    }

    #[tokio::test]
    async fn find_pattern_message_match_breaks_ties() {
        let now = 1_700_000_000;
        let storage = Arc::new(FakeStorage {
            hits: vec![
                fake_hit_with_msg("a", now - 86400, 0.8, "added retry logic"),
                fake_hit_with_msg("b", now - 86400, 0.8, "renamed file"),
            ],
        });
        let embedder = Arc::new(FakeEmbedder);
        let r = Retriever::new(storage, embedder);
        let q = PatternQuery { query: "retry".into(), k: 5, language: None, since_unix: None };
        let id = RepoId::from_parts("x", "/y");
        let out = r.find_pattern(&id, &q, now).await.unwrap();
        assert_eq!(out[0].commit_sha, "a", "retry-related commit message should win the tie");
    }
```

- [ ] **Step 2: Run the test**

Run: `cargo test -p ohara-core retriever`
Expected: all retriever tests including the new one pass.

- [ ] **Step 3: Commit (green — find_pattern)**

```bash
git add crates/ohara-core/src/retriever.rs
git commit -m "Add Retriever::find_pattern with cosine message-similarity tiebreaker"
```

---

### Task 15: CLI scaffold + `ohara index`

**Files:**
- Create: `crates/ohara-cli/src/commands/mod.rs`
- Create: `crates/ohara-cli/src/commands/index.rs`
- Modify: `crates/ohara-cli/src/main.rs`

A small helper computes the on-disk path `~/.ohara/<repo-id>/index.sqlite` for a given repo path.

- [ ] **Step 1: Write `main.rs` with clap dispatch**

`crates/ohara-cli/src/main.rs`:
```rust
use anyhow::Result;
use clap::{Parser, Subcommand};

mod commands;

#[derive(Parser, Debug)]
#[command(name = "ohara", version, about = "ohara — context lineage engine")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Build or update the index for a repo.
    Index(commands::index::Args),
    /// Run a debug pattern query against an indexed repo.
    Query(commands::query::Args),
    /// Print index status for a repo.
    Status(commands::status::Args),
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,ohara=debug")))
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    match cli.command {
        Cmd::Index(a) => commands::index::run(a).await,
        Cmd::Query(a) => commands::query::run(a).await,
        Cmd::Status(a) => commands::status::run(a).await,
    }
}
```

`crates/ohara-cli/src/commands/mod.rs`:
```rust
use anyhow::{anyhow, Result};
use ohara_core::types::RepoId;
use std::path::{Path, PathBuf};

pub mod index;
pub mod query;
pub mod status;

pub fn ohara_home() -> PathBuf {
    if let Ok(s) = std::env::var("OHARA_HOME") {
        return PathBuf::from(s);
    }
    let home = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")).expect("HOME or USERPROFILE");
    PathBuf::from(home).join(".ohara")
}

pub fn resolve_repo_id<P: AsRef<Path>>(repo_path: P) -> Result<(RepoId, PathBuf, String)> {
    let canonical = std::fs::canonicalize(repo_path.as_ref())
        .map_err(|e| anyhow!("canonicalize {}: {e}", repo_path.as_ref().display()))?;
    let walker = ohara_git::GitWalker::open(&canonical).map_err(|e| anyhow!("open repo: {e}"))?;
    let first = walker.first_commit_sha().map_err(|e| anyhow!("first commit: {e}"))?;
    let canonical_str = canonical.to_string_lossy().to_string();
    let id = RepoId::from_parts(&first, &canonical_str);
    Ok((id, canonical, first))
}

pub fn index_db_path(id: &RepoId) -> PathBuf {
    ohara_home().join(id.as_str()).join("index.sqlite")
}
```

- [ ] **Step 2: Write the failing test for `ohara index` against a small in-process repo**

Create `crates/ohara-cli/tests/index_smoke.rs`:
```rust
// integration-style smoke test that calls into commands::index::run
// using a temp repo and OHARA_HOME pointing to a temp dir.

use git2::{Repository, Signature};
use std::fs;

#[tokio::test]
#[ignore = "requires network for first-time fastembed model download"]
async fn smoke_index_then_status() {
    let repo_dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("OHARA_HOME", home.path());

    let repo = Repository::init(repo_dir.path()).unwrap();
    let sig = Signature::now("a", "a@a").unwrap();
    fs::write(repo_dir.path().join("a.rs"), "fn alpha() {}\n").unwrap();
    let mut idx = repo.index().unwrap();
    idx.add_path(std::path::Path::new("a.rs")).unwrap(); idx.write().unwrap();
    let t = idx.write_tree().unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "init", &repo.find_tree(t).unwrap(), &[]).unwrap();

    // call the command
    let args = ohara_cli::commands::index::Args { path: repo_dir.path().to_path_buf() };
    ohara_cli::commands::index::run(args).await.unwrap();

    let st_args = ohara_cli::commands::status::Args { path: repo_dir.path().to_path_buf() };
    ohara_cli::commands::status::run(st_args).await.unwrap();
}
```

This requires turning `ohara-cli` into a library + binary. Update `crates/ohara-cli/Cargo.toml`:
```toml
[[bin]]
name = "ohara"
path = "src/main.rs"

[lib]
path = "src/lib.rs"

[dependencies]
ohara-core = { path = "../ohara-core" }
ohara-storage = { path = "../ohara-storage" }
ohara-embed = { path = "../ohara-embed" }
ohara-git = { path = "../ohara-git" }
ohara-parse = { path = "../ohara-parse" }
anyhow.workspace = true
clap.workspace = true
tokio.workspace = true
tracing.workspace = true
tracing-subscriber.workspace = true

[dev-dependencies]
tempfile = "3"
git2.workspace = true
```

Create `crates/ohara-cli/src/lib.rs`:
```rust
pub mod commands;
```

Move the `mod commands;` line in `main.rs` to use the library: `use ohara_cli::commands;` and remove the `mod commands;` declaration. Update imports as needed.

- [ ] **Step 3: Implement `commands::index::run`**

`crates/ohara-cli/src/commands/index.rs`:
```rust
use anyhow::Result;
use clap::Args as ClapArgs;
use ohara_core::Indexer;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// Path to the repo (defaults to current directory)
    #[arg(default_value = ".")]
    pub path: PathBuf,
}

pub async fn run(args: Args) -> Result<()> {
    let (repo_id, canonical, first_commit) = super::resolve_repo_id(&args.path)?;
    let db_path = super::index_db_path(&repo_id);
    tracing::info!(repo = %canonical.display(), id = repo_id.as_str(), db = %db_path.display(), "indexing");

    let storage = Arc::new(ohara_storage::SqliteStorage::open(&db_path).await?);
    storage.open_repo(&repo_id, &canonical.to_string_lossy(), &first_commit).await?;

    let embedder = Arc::new(tokio::task::spawn_blocking(|| {
        ohara_embed::FastEmbedProvider::new()
    }).await??);
    let commit_source = ohara_git::GitCommitSource::open(&canonical)?;
    let symbol_source = ohara_parse::GitSymbolSource::open(&canonical)?;

    let indexer = Indexer::new(storage.clone(), embedder.clone());
    let report = indexer.run(&repo_id, &commit_source, &symbol_source).await?;
    println!(
        "indexed: {} new commits, {} hunks, {} HEAD symbols",
        report.new_commits, report.new_hunks, report.head_symbols
    );
    Ok(())
}
```

- [ ] **Step 4: Run the smoke test (gated)**

Run: `cargo test -p ohara-cli -- --include-ignored`
Expected: PASS after the embedding model downloads.

- [ ] **Step 5: Commit (green — index command)**

```bash
git add crates/ohara-cli/Cargo.toml crates/ohara-cli/src/
git commit -m "Add ohara CLI library + ohara index command"
```

---

### Task 16: `ohara query` and `ohara status`

**Files:**
- Create: `crates/ohara-cli/src/commands/query.rs`
- Create: `crates/ohara-cli/src/commands/status.rs`

- [ ] **Step 1: Write `ohara query`**

`crates/ohara-cli/src/commands/query.rs`:
```rust
use anyhow::Result;
use clap::Args as ClapArgs;
use ohara_core::query::PatternQuery;
use ohara_core::Retriever;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// Path to the repo (defaults to current directory)
    #[arg(default_value = ".")]
    pub path: PathBuf,
    /// The natural-language query
    #[arg(short, long)]
    pub query: String,
    #[arg(short, long, default_value_t = 5)]
    pub k: u8,
    #[arg(long)]
    pub language: Option<String>,
}

pub async fn run(args: Args) -> Result<()> {
    let (repo_id, _, _) = super::resolve_repo_id(&args.path)?;
    let db_path = super::index_db_path(&repo_id);
    let storage = Arc::new(ohara_storage::SqliteStorage::open(&db_path).await?);
    let embedder = Arc::new(tokio::task::spawn_blocking(|| {
        ohara_embed::FastEmbedProvider::new()
    }).await??);

    let retriever = Retriever::new(storage, embedder);
    let q = PatternQuery {
        query: args.query,
        k: args.k,
        language: args.language,
        since_unix: None,
    };
    let now = chrono::Utc::now().timestamp();
    let hits = retriever.find_pattern(&repo_id, &q, now).await?;
    println!("{}", serde_json::to_string_pretty(&hits)?);
    Ok(())
}
```

Add to `crates/ohara-cli/Cargo.toml`:
```toml
chrono.workspace = true
serde_json.workspace = true
```

- [ ] **Step 2: Write `ohara status`**

`crates/ohara-cli/src/commands/status.rs`:
```rust
use anyhow::Result;
use clap::Args as ClapArgs;
use ohara_core::Storage;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(ClapArgs, Debug)]
pub struct Args {
    #[arg(default_value = ".")]
    pub path: PathBuf,
}

pub async fn run(args: Args) -> Result<()> {
    let (repo_id, canonical, _) = super::resolve_repo_id(&args.path)?;
    let db_path = super::index_db_path(&repo_id);
    let storage = Arc::new(ohara_storage::SqliteStorage::open(&db_path).await?);
    let st = storage.get_index_status(&repo_id).await?;

    // commits_behind_head is computed by walking from last_indexed_commit to HEAD via git
    let walker = ohara_git::GitWalker::open(&canonical)?;
    let behind = match &st.last_indexed_commit {
        Some(sha) => walker.list_commits(Some(sha))?.len(),
        None => walker.list_commits(None)?.len(),
    };

    println!(
        "repo: {}\nid: {}\nlast_indexed_commit: {}\nindexed_at: {}\ncommits_behind_head: {}",
        canonical.display(),
        repo_id.as_str(),
        st.last_indexed_commit.unwrap_or_else(|| "<none>".into()),
        st.indexed_at.unwrap_or_else(|| "<none>".into()),
        behind
    );
    Ok(())
}
```

- [ ] **Step 3: Build to verify everything compiles**

Run: `cargo build -p ohara-cli`
Expected: clean.

- [ ] **Step 4: Commit (green — query and status commands)**

```bash
git add crates/ohara-cli/src/commands/ crates/ohara-cli/Cargo.toml
git commit -m "Add ohara query and ohara status commands"
```

---

### Task 17: ohara-mcp server skeleton

**Files:**
- Create: `crates/ohara-mcp/src/server.rs`
- Create: `crates/ohara-mcp/src/tools/mod.rs`
- Modify: `crates/ohara-mcp/src/main.rs`
- Modify: `crates/ohara-mcp/Cargo.toml`

The MCP server is read-only. It opens the SQLite index for the repo at the working directory, instantiates a `Retriever`, and exposes `find_pattern`. If the index doesn't exist, it surfaces that fact via tool responses (the Layer 4 hint).

The exact rmcp API surface depends on the version on crates.io. The skeleton below uses rmcp's `tool` derive style. If the API differs, adapt the pattern (the responsibilities — register tool with name, JSON schema input, async handler — are stable across versions).

- [ ] **Step 1: Bootstrap the MCP server in `main.rs`**

`crates/ohara-mcp/src/main.rs`:
```rust
use anyhow::Result;

mod server;
mod tools;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,ohara=debug")))
        .with_writer(std::io::stderr)
        .init();
    let workdir = std::env::current_dir()?;
    let server = server::OharaServer::open(workdir).await?;
    server.serve_stdio().await
}
```

`crates/ohara-mcp/src/server.rs`:
```rust
use anyhow::{Context, Result};
use ohara_core::types::RepoId;
use ohara_core::{EmbeddingProvider, Retriever, Storage};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct OharaServer {
    pub repo_id: RepoId,
    pub repo_path: PathBuf,
    pub storage: Arc<dyn Storage>,
    pub embedder: Arc<dyn EmbeddingProvider>,
    pub retriever: Retriever,
}

impl OharaServer {
    pub async fn open<P: AsRef<Path>>(workdir: P) -> Result<Self> {
        let canonical = std::fs::canonicalize(workdir.as_ref()).context("canonicalize workdir")?;
        let walker = ohara_git::GitWalker::open(&canonical).context("open repo")?;
        let first_commit = walker.first_commit_sha()?;
        let repo_id = RepoId::from_parts(&first_commit, &canonical.to_string_lossy());

        let home = std::env::var("OHARA_HOME").map(PathBuf::from).unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")).expect("HOME"))
                .join(".ohara")
        });
        let db_path = home.join(repo_id.as_str()).join("index.sqlite");

        let storage: Arc<dyn Storage> = Arc::new(ohara_storage::SqliteStorage::open(&db_path).await?);
        let embedder: Arc<dyn EmbeddingProvider> = Arc::new(
            tokio::task::spawn_blocking(|| ohara_embed::FastEmbedProvider::new()).await??
        );
        let retriever = Retriever::new(storage.clone(), embedder.clone());

        Ok(Self { repo_id, repo_path: canonical, storage, embedder, retriever })
    }

    pub async fn serve_stdio(self) -> Result<()> {
        // Wire into rmcp's stdio transport. The exact crate API varies; the
        // structure to implement is: register the `find_pattern` tool from
        // `tools::find_pattern`, then run the protocol loop on stdin/stdout.
        crate::tools::serve(self).await
    }

    pub async fn index_status_meta(&self) -> Result<ohara_core::query::ResponseMeta> {
        let st = self.storage.get_index_status(&self.repo_id).await?;
        let walker = ohara_git::GitWalker::open(&self.repo_path)?;
        let behind = match &st.last_indexed_commit {
            Some(sha) => walker.list_commits(Some(sha))?.len() as u64,
            None => walker.list_commits(None)?.len() as u64,
        };
        let hint = if st.last_indexed_commit.is_none() {
            Some("Index not built. Run `ohara index` in this repo.".to_string())
        } else if behind > 50 {
            Some(format!("Index is {behind} commits behind HEAD. Run `ohara index`.").to_string())
        } else { None };
        Ok(ohara_core::query::ResponseMeta {
            index_status: ohara_core::query::IndexStatus {
                last_indexed_commit: st.last_indexed_commit,
                commits_behind_head: behind,
                indexed_at: st.indexed_at,
            },
            hint,
        })
    }
}
```

`crates/ohara-mcp/src/tools/mod.rs`:
```rust
pub mod find_pattern;

use crate::server::OharaServer;

pub async fn serve(server: OharaServer) -> anyhow::Result<()> {
    // Pseudocode-ish: register the tool with rmcp's stdio server.
    //
    // Concrete shape varies by rmcp version. The tool to register has:
    //   name: "find_pattern"
    //   description: see find_pattern::TOOL_DESCRIPTION
    //   input schema: derived from FindPatternInput (schemars)
    //   handler: find_pattern::handle(&server, input).await
    //
    // Until you know the exact rmcp API on the chosen version, use rmcp's
    // examples directory as a template. The Server-level instructions
    // (`OharaServer::INSTRUCTIONS`) are passed at server construction time.
    use rmcp::transport::io::stdio;
    use rmcp::ServiceExt;
    let svc = find_pattern::OharaService::new(server);
    svc.serve(stdio()).await?.waiting().await?;
    Ok(())
}
```

Add to `crates/ohara-mcp/Cargo.toml`:
```toml
[dependencies]
ohara-core = { path = "../ohara-core" }
ohara-storage = { path = "../ohara-storage" }
ohara-embed = { path = "../ohara-embed" }
ohara-git = { path = "../ohara-git" }
anyhow.workspace = true
tokio.workspace = true
tracing.workspace = true
tracing-subscriber.workspace = true
rmcp.workspace = true
schemars.workspace = true
serde.workspace = true
serde_json.workspace = true
async-trait = "0.1"
chrono.workspace = true
```

- [ ] **Step 2: Build to verify the skeleton compiles (without tools yet)**

Run: `cargo build -p ohara-mcp`
Expected: build fails because `find_pattern::OharaService` is not yet defined. That's the expected red.

- [ ] **Step 3: Commit (red — server skeleton, tools missing)**

```bash
git add crates/ohara-mcp/src/main.rs crates/ohara-mcp/src/server.rs crates/ohara-mcp/src/tools/mod.rs crates/ohara-mcp/Cargo.toml
git commit -m "Add ohara-mcp server skeleton (tools::serve unimplemented)"
```

---

### Task 18: MCP `find_pattern` tool

**Files:**
- Create: `crates/ohara-mcp/src/tools/find_pattern.rs`

The tool description is **the** discoverability layer 1 surface. Treat the strings here as production code, not docs.

- [ ] **Step 1: Implement the tool**

`crates/ohara-mcp/src/tools/find_pattern.rs`:
```rust
use crate::server::OharaServer;
use rmcp::{
    handler::server::{router::tool::ToolRouter, tool::Parameters},
    model::{CallToolResult, Content, ServerCapabilities, ServerInfo, Implementation, ProtocolVersion},
    schemars,
    service::RequestContext,
    tool, tool_handler, tool_router, RoleServer, ServerHandler,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;

pub const TOOL_DESCRIPTION: &str = "\
Search this project's git history for past implementations of similar logic.

USE WHEN the user:
  - asks \"how did we do X before\" / \"is there a pattern for Y\"
  - requests adding a feature similar to existing functionality
    (\"add retry like we did before\", \"make this look like the auth flow\")
  - is about to write code that likely has prior art in this repo

DO NOT USE for searching current code - use Grep/Read for that.
DO NOT USE for general programming questions.

Returns: historical commits with diffs, commit messages, file paths,
similarity score, and provenance (always INFERRED - semantic match).";

pub const SERVER_INSTRUCTIONS: &str = "\
Use this server when the user is implementing, modifying, or asking about \
code that likely has historical precedent in this repository. Lineage is \
ohara's specialty - for \"how was this done before\", \"trace this change\", \
or \"add a feature like an existing one\", prefer ohara over generic search. \
Do not use for code that has no git history (new files, fresh repos).";

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct FindPatternInput {
    /// Natural-language description of the pattern to find.
    pub query: String,
    /// Number of results to return (1..=20).
    #[serde(default = "default_k")]
    pub k: u8,
    /// Optional language filter (e.g. "rust", "python").
    #[serde(default)]
    pub language: Option<String>,
    /// Optional ISO date or relative ("30d") lower bound on commit age.
    #[serde(default)]
    pub since: Option<String>,
}

fn default_k() -> u8 { 5 }

#[derive(Clone)]
pub struct OharaService {
    server: Arc<OharaServer>,
    tool_router: ToolRouter<Self>,
}

impl OharaService {
    pub fn new(server: OharaServer) -> Self {
        Self { server: Arc::new(server), tool_router: Self::tool_router() }
    }
}

#[tool_router]
impl OharaService {
    #[tool(description = TOOL_DESCRIPTION)]
    pub async fn find_pattern(
        &self,
        Parameters(input): Parameters<FindPatternInput>,
    ) -> Result<CallToolResult, rmcp::Error> {
        let since_unix = parse_since(input.since.as_deref())
            .map_err(|e| rmcp::Error::InvalidParams(e.to_string()))?;
        let q = ohara_core::query::PatternQuery {
            query: input.query,
            k: input.k.clamp(1, 20),
            language: input.language,
            since_unix,
        };
        let now = chrono::Utc::now().timestamp();
        let hits = self
            .server
            .retriever
            .find_pattern(&self.server.repo_id, &q, now)
            .await
            .map_err(|e| rmcp::Error::Internal(e.to_string()))?;
        let meta = self
            .server
            .index_status_meta()
            .await
            .map_err(|e| rmcp::Error::Internal(e.to_string()))?;

        let body = json!({ "hits": hits, "_meta": meta });
        Ok(CallToolResult::success(vec![Content::text(body.to_string())]))
    }
}

#[tool_handler]
impl ServerHandler for OharaService {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: ProtocolVersion::default(),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation::from_build_env(),
            instructions: Some(SERVER_INSTRUCTIONS.into()),
        }
    }
}

fn parse_since(s: Option<&str>) -> anyhow::Result<Option<i64>> {
    let Some(s) = s else { return Ok(None); };
    if s.is_empty() { return Ok(None); }
    if let Some(stripped) = s.strip_suffix('d') {
        let n: i64 = stripped.parse()?;
        return Ok(Some(chrono::Utc::now().timestamp() - n * 86400));
    }
    let dt = chrono::DateTime::parse_from_rfc3339(s)
        .or_else(|_| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
            .map(|d| d.and_hms_opt(0, 0, 0).unwrap().and_utc().fixed_offset()))?;
    Ok(Some(dt.timestamp()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parse_since_relative_days() {
        let out = parse_since(Some("30d")).unwrap().unwrap();
        let now = chrono::Utc::now().timestamp();
        assert!((now - 30 * 86400 - out).abs() < 5);
    }
    #[test]
    fn parse_since_iso_date() {
        let out = parse_since(Some("2024-01-01")).unwrap().unwrap();
        assert!(out > 1_700_000_000 && out < 1_800_000_000);
    }
    #[test]
    fn parse_since_none() {
        assert!(parse_since(None).unwrap().is_none());
        assert!(parse_since(Some("")).unwrap().is_none());
    }
}
```

- [ ] **Step 2: Build to verify**

Run: `cargo build -p ohara-mcp`
Expected: clean build (or refer to rmcp's published examples to align the macros if the version's API differs slightly).

- [ ] **Step 3: Run unit tests**

Run: `cargo test -p ohara-mcp`
Expected: 3 passed (parse_since tests).

- [ ] **Step 4: Commit (green — find_pattern tool)**

```bash
git add crates/ohara-mcp/src/tools/find_pattern.rs
git commit -m "Implement find_pattern MCP tool with strict trigger-scoped description"
```

---

### Task 19: Tiny fixture repo build script

**Files:**
- Create: `fixtures/README.md`
- Create: `fixtures/build_tiny.sh`

The fixture is generated by a script, not committed, so the e2e test runs deterministically and no large binary blobs land in this repo.

- [ ] **Step 1: Write the fixture build script**

`fixtures/build_tiny.sh`:
```bash
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
```

`fixtures/README.md`:
```markdown
# Fixtures

The `tiny/repo` directory is built by `./build_tiny.sh` and is not committed.
Run the script before invoking the e2e test, or let the test invoke it.

The fixture has three commits:
1. `initial fetch` - a `fetch` function returning a String
2. `add retry with exponential backoff` - introduces retry logic with sleeps
3. `add basic login` - introduces an unrelated `login` function in auth.rs

A `find_pattern` query for "retry with backoff" should return commit 2 first.
```

- [ ] **Step 2: Make the script executable + smoke run it**

Run:
```bash
chmod +x fixtures/build_tiny.sh
fixtures/build_tiny.sh
```
Expected: `fixture built at .../fixtures/tiny/repo`. Verify with `git -C fixtures/tiny/repo log --oneline` (should show 3 commits).

- [ ] **Step 3: Commit (green — fixture script)**

```bash
git add fixtures/README.md fixtures/build_tiny.sh
git commit -m "Add tiny fixture repo build script with retry pattern commit"
```

---

### Task 20: End-to-end test — index fixture, query, assert ordering

**Files:**
- Create: `tests/e2e_find_pattern.rs`
- Modify: workspace `Cargo.toml` (no change, integration tests at workspace root pick up automatically? — confirm; otherwise put it in `crates/ohara-cli/tests/`)

For maximum simplicity, place the e2e test in `crates/ohara-cli/tests/e2e_find_pattern.rs` so it can use the `ohara-cli` library type for invoking commands directly.

- [ ] **Step 1: Write the failing test**

`crates/ohara-cli/tests/e2e_find_pattern.rs`:
```rust
//! End-to-end: build the fixture repo, index it, query for "retry", assert the
//! retry commit ranks first.

use std::path::PathBuf;
use std::process::Command;

fn ensure_fixture() -> PathBuf {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap().to_path_buf();
    let script = workspace.join("fixtures/build_tiny.sh");
    let repo = workspace.join("fixtures/tiny/repo");
    if !repo.join(".git").exists() {
        let s = Command::new("bash").arg(&script).status().expect("run fixture script");
        assert!(s.success(), "fixture script failed");
    }
    repo
}

#[tokio::test]
#[ignore = "downloads the embedding model on first run; opt in with --include-ignored"]
async fn find_pattern_returns_retry_commit_first() {
    let repo = ensure_fixture();
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("OHARA_HOME", home.path());

    // index
    let args = ohara_cli::commands::index::Args { path: repo.clone() };
    ohara_cli::commands::index::run(args).await.unwrap();

    // build a Retriever directly to avoid parsing CLI stdout
    let (repo_id, _, _) = ohara_cli::commands::resolve_repo_id(&repo).unwrap();
    let db = ohara_cli::commands::index_db_path(&repo_id);
    let storage = std::sync::Arc::new(ohara_storage::SqliteStorage::open(&db).await.unwrap());
    let embedder = std::sync::Arc::new(
        tokio::task::spawn_blocking(|| ohara_embed::FastEmbedProvider::new()).await.unwrap().unwrap()
    );
    let retriever = ohara_core::Retriever::new(storage, embedder);

    let q = ohara_core::query::PatternQuery {
        query: "retry with exponential backoff".into(),
        k: 5,
        language: None,
        since_unix: None,
    };
    let now = chrono::Utc::now().timestamp();
    let hits = retriever.find_pattern(&repo_id, &q, now).await.unwrap();

    assert!(!hits.is_empty(), "no hits for 'retry'");
    assert!(
        hits[0].commit_message.contains("retry"),
        "top hit should be the retry commit, got: {}",
        hits[0].commit_message
    );
    // The unrelated login commit should not be the top result.
    assert!(
        !hits[0].commit_message.contains("login"),
        "top hit should NOT be the login commit"
    );
}
```

- [ ] **Step 2: Run the e2e test**

Run: `cargo test -p ohara-cli --test e2e_find_pattern -- --include-ignored`
Expected: PASS. The retry commit ranks first; login does not.

- [ ] **Step 3: Commit (green — e2e validates capability A)**

```bash
git add crates/ohara-cli/tests/e2e_find_pattern.rs
git commit -m "Add e2e test asserting retry pattern ranks first on fixture repo"
```

---

### Task 21: Plan-1 README

**Files:**
- Create: `README.md`

A short README so a new contributor can run the full pipeline without reading the spec or plan.

- [ ] **Step 1: Write `README.md`**

`README.md`:
```markdown
# ohara

Local-first context lineage engine. Indexes a git repo's commits and diffs, then
serves "how was X done before?" queries to Claude Code (or any MCP client) via a
local stdio server.

This repo is at **Plan 1**: foundation + the `find_pattern` MCP tool. The
`explain_change` tool, git-hook installation, and additional language support
arrive in subsequent plans.

## Build

    cargo build --release

Produces two binaries under `target/release/`:
- `ohara` — CLI for indexing and debugging
- `ohara-mcp` — MCP server (stdio) for Claude Code

## Quickstart

    fixtures/build_tiny.sh
    cargo run -p ohara-cli -- index fixtures/tiny/repo
    cargo run -p ohara-cli -- query --query "retry with backoff" fixtures/tiny/repo

The first run downloads the BGE-small embedding model (~80MB, one time).

## Wiring into Claude Code

In your `~/.claude/claude_desktop_config.json` (or per-repo MCP config), add:

```json
{
  "mcpServers": {
    "ohara": {
      "command": "/absolute/path/to/target/release/ohara-mcp",
      "args": [],
      "env": {}
    }
  }
}
```

The server reads the current working directory of the spawning Claude Code
session as the repo to query. Run `ohara index` first.

## Layout

See `docs/superpowers/specs/2026-04-30-ohara-context-engine-design.md` for the
v1 design and `docs/superpowers/plans/` for implementation plans.
```

- [ ] **Step 2: Commit (green — README)**

```bash
git add README.md
git commit -m "Add Plan 1 README with quickstart and Claude Code wiring"
```

---

## Self-Review

Spec coverage check (against `docs/superpowers/specs/2026-04-30-ohara-context-engine-design.md`):

| Spec section | Plan tasks | Status |
|---|---|---|
| §2 Goal: `find_pattern` MCP tool | Task 18, 20 | covered |
| §2 Goal: `ohara` CLI | Tasks 15, 16 | covered |
| §2 Goal: local-first index in `~/.ohara/<repo-id>/` | Tasks 7, 15 | covered |
| §2 Goal: local embeddings via fastembed-rs | Task 10 | covered |
| §2 Goal: `explain_change` MCP tool | — | **deferred to Plan 2** (explicit) |
| §2 Goal: git-hook driven incremental indexing | — | **deferred to Plan 3** (explicit) |
| §2 Goal: provenance tagging | Tasks 2, 4 (`Provenance::Inferred`) | covered |
| §3 Architecture: 7 crates | Task 1 | covered |
| §3 Trait boundaries | Tasks 3, 4 | covered |
| §3 ohara-mcp is read-only | Task 17 (no indexing path) | covered |
| §3 RepoId derivation | Task 2 | covered |
| §3 No daemon process | Tasks 15-18 | covered |
| §4 Layer 1 tool descriptions | Task 18 | covered |
| §4 Layer 2 server-level instructions | Task 18 | covered |
| §4 Layer 3 CLAUDE.md stanza | — | **deferred to Plan 3** (`ohara init`) |
| §4 Layer 4 stale-index hints | Task 17 (`index_status_meta`) | covered |
| §5 Schema | Task 6 | covered |
| §5 `symbol_lineage` | — | **deferred to Plan 2** |
| §5 sqlite-vec | Task 5 | covered |
| §6 Hybrid lineage strategy: diff embedding | Tasks 11, 12, 19 | covered |
| §6 Hybrid lineage strategy: HEAD blame | — | **deferred to Plan 2** |
| §6 Incremental path with watermark | Task 11 (`since` arg), Indexer in Task 4 | covered |
| §7 `find_pattern` signature with `_meta.index_status` | Tasks 14, 18 | covered |
| §7 `explain_change` signature | — | **deferred to Plan 2** |
| §7 Ranking weights 0.7/0.2/0.1 | Task 4 (`RankingWeights::default`) | covered |
| §8 SQLite settings | Task 5 | covered |
| §9 EmbeddingProvider trait | Task 3 | covered |
| §9 fastembed default | Task 10 | covered |
| §10 Languages: Rust + Python | Task 13 | covered |
| §10 JS/TS/Go | — | **deferred to Plan 4** |
| §11 Unit tests per crate | Tasks 2-18 (each TDD task) | covered |
| §11 Snapshot tests with `insta` | — | not in this plan; raise if Plan 1 tests prove insufficient |
| §11 E2E MCP test | — | covered partially (Task 20 tests retrieval e2e through library; full MCP-stdio test deferred to Plan 3 hardening) |
| §11 Real-repo smoke test | — | **deferred** (real-repo smoke under load is a Plan-later concern) |
| §13 Milestone 1-7 of spec | Plan 1 covers items 1-6 (workspace through `find_pattern`), partially 9 (test fixture) | matches scope |

Placeholder scan: re-read each task. Found and fixed in Phase 2 the messy retriever-test scaffolding. No remaining "TBD"/"implement later" lines.

Type consistency:
- `Storage` trait method signatures match between definition (Task 3), tests (Tasks 4, 14), and impl (Tasks 7-9). All use `&RepoId` not `RepoId`.
- `EmbeddingProvider::embed_batch` takes `&[String]` consistently in all places.
- `PatternHit::provenance` is `Provenance::Inferred` in all four places it's constructed.
- `ChangeKind` enum variants are spelled the same in every reference (`Added`, `Modified`, `Deleted`, `Renamed`).
- `change_kind_to_str` and `str_to_change_kind` in `hunk.rs` are inverses; verified in Task 9 test path.

Known soft spots (declared, not bugs):
- The fastembed-rs `InitOptions { model_name: ..., show_download_progress, .. }` field shape may have shifted in newer crate versions. If the field name is different, adapt; the responsibility is unchanged.
- The rmcp tool macro syntax (`#[tool]`, `#[tool_router]`, `#[tool_handler]`) tracks the crate's published examples. If the macros differ on the version chosen, follow the rmcp examples directory; the contract (one tool named `find_pattern`, schema from `FindPatternInput`, server-level `instructions`) is what matters.
- `tree_sitter_rust::LANGUAGE` and `tree_sitter_python::LANGUAGE` are the v0.21+ symbols; older releases used `tree_sitter_rust::language()` returning `Language` directly. Adapt as needed; no behavior change.

---

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-04-30-ohara-plan-1-foundation-and-find-pattern.md`.

**Two execution options:**

1. **Subagent-Driven (recommended)** — I dispatch a fresh subagent per task, review between tasks, fast iteration with isolated context per step.
2. **Inline Execution** — Execute tasks in this session using `superpowers:executing-plans`, batch execution with checkpoints.

**Which approach?**
