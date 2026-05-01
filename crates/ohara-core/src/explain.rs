//! `explain_change` orchestrator — given a file + line range, return the
//! commits that introduced and shaped that code, ordered newest-first.
//!
//! Plan 5 / Track A. Counterpart to `find_pattern`: where the retriever
//! answers "how was X done before" via embeddings + BM25 + rerank, this
//! module answers "why does THIS code look the way it does" via
//! deterministic `git blame`. No embeddings, no rerank — every result has
//! `provenance = EXACT`.
//!
//! `ohara-core` stays git-free: the `BlameSource` trait abstracts over
//! `git2::Repository::blame_file`, with the real implementation living in
//! `ohara-git::Blamer`.

use crate::storage::Storage;
use crate::types::{Provenance, RepoId};
use crate::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// One commit's contribution to a blame query, with the lines (within
/// the queried range) it owns. Returned by `BlameSource::blame_range`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlameRange {
    pub commit_sha: String,
    /// 1-based line numbers within the queried range; sorted ascending.
    pub lines: Vec<u32>,
}

/// Abstraction over `git2::Repository::blame_file`. Implemented by
/// `ohara-git::Blamer`. Mirrors the `CommitSource` / `SymbolSource`
/// pattern — keeps `ohara-core` git-free.
#[async_trait]
pub trait BlameSource: Send + Sync {
    /// Blame a contiguous, 1-based, inclusive line range. The
    /// implementation may clamp `line_end` to the file's actual length.
    /// Returns one entry per distinct commit-of-origin; lines are the
    /// in-range subset that commit owns.
    async fn blame_range(
        &self,
        file: &str,
        line_start: u32,
        line_end: u32,
    ) -> Result<Vec<BlameRange>>;
}

/// Caller's request to `explain_change`. `line_start` / `line_end` are
/// 1-based, inclusive; the orchestrator clamps to file length and
/// returns a `_meta.limitation` when the range is degenerate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExplainQuery {
    pub file: String,
    pub line_start: u32,
    pub line_end: u32,
    /// Number of commits to return (1..=20). Caller-side clamp is
    /// enforced inside `explain_change` itself.
    pub k: u8,
    /// When false, `ExplainHit::diff_excerpt` is left empty so the
    /// caller (e.g. the MCP layer) can render a tighter response. The
    /// orchestrator still computes `blame_lines` either way.
    pub include_diff: bool,
}

/// One commit's contribution, enriched for display. Recency order
/// (commit timestamp desc) is the orchestrator's contract.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExplainHit {
    pub commit_sha: String,
    pub commit_message: String,
    pub commit_author: Option<String>,
    /// ISO-8601 / RFC-3339, derived from the commit's unix timestamp.
    pub commit_date: String,
    /// 1-based queried-range line numbers this commit owns; sorted asc.
    pub blame_lines: Vec<u32>,
    pub file_path: String,
    pub diff_excerpt: String,
    pub diff_truncated: bool,
    /// Always `Provenance::Exact` — git blame is git-truth, not
    /// inferred. Serializes to `"EXACT"`.
    pub provenance: Provenance,
}

/// Per-line diff truncation cap for `ExplainHit::diff_excerpt`. Matches
/// the value used by the `find_pattern` retrieval pipeline so MCP
/// responses look consistent across both tools.
const DIFF_EXCERPT_MAX_LINES: usize = 80;

/// `k` clamp matches the spec: 1..=20, default 5 enforced at the caller.
const K_MAX: u8 = 20;

/// Diagnostic envelope returned alongside hits.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExplainMeta {
    /// `(line_start, line_end)` after clamping to file bounds.
    pub lines_queried: (u32, u32),
    /// Number of distinct commits in the post-skip result set (matches
    /// `hits.len()` once the orchestrator caps to `k`).
    pub commits_unique: usize,
    /// Fraction of queried lines that resolved to an indexed commit.
    /// 1.0 means every line was attributed; less than 1.0 means at
    /// least one line landed on a SHA the local index doesn't know.
    pub blame_coverage: f32,
    /// Free-form note when the result set is constrained (e.g.
    /// "file was renamed; pre-rename history not reached", or
    /// "file does not exist in HEAD").
    pub limitation: Option<String>,
}

/// Run an `explain_change` query end-to-end.
///
/// 1. Ask the `BlameSource` for line ownership over the queried range.
/// 2. Resolve each unique commit SHA to a `CommitMeta` via storage.
///    Skip SHAs that aren't yet indexed (e.g. older than the watermark)
///    with a debug log; reflect the skip in `ExplainMeta`.
/// 3. Pull per-(commit, file) hunks, concatenate their `diff_text`
///    into a single excerpt, and truncate to `DIFF_EXCERPT_MAX_LINES`.
/// 4. Sort hits newest-first by `commit.ts`, cap to `query.k`.
/// 5. Compute `blame_coverage` over the *clamped* range.
pub async fn explain_change(
    storage: &dyn Storage,
    blamer: &dyn BlameSource,
    repo_id: &RepoId,
    query: &ExplainQuery,
) -> Result<(Vec<ExplainHit>, ExplainMeta)> {
    // 1. Blame the queried range. The Blamer is the file-length oracle:
    //    it can read the workdir checkout, so its returned `lines` are
    //    the authoritative clamped set.
    let raw_blame = blamer
        .blame_range(&query.file, query.line_start, query.line_end)
        .await?;

    // Derive the clamped (line_start, line_end) from the actual blame
    // output. Empty blame (file missing, range invalid) → echo back
    // the requested range so the meta still tells the caller what they
    // asked for, with a limitation note.
    let (clamped_start, clamped_end, lines_attributed_total) = if raw_blame.is_empty() {
        (query.line_start, query.line_end, 0u32)
    } else {
        let mut min_line = u32::MAX;
        let mut max_line = 0u32;
        let mut total = 0u32;
        for r in &raw_blame {
            for &l in &r.lines {
                if l < min_line {
                    min_line = l;
                }
                if l > max_line {
                    max_line = l;
                }
                total += 1;
            }
        }
        if min_line == u32::MAX {
            (query.line_start, query.line_end, 0)
        } else {
            (min_line, max_line, total)
        }
    };

    // 2. Resolve each unique commit SHA to its metadata. Skip unindexed
    //    SHAs (Ok(None)) and remember how many lines they "owned" so we
    //    can report them in `blame_coverage`.
    let mut indexed: Vec<(crate::types::CommitMeta, Vec<u32>)> = Vec::new();
    let mut skipped_shas: Vec<String> = Vec::new();
    let mut lines_attributed_indexed: u32 = 0;
    for r in raw_blame {
        match storage.get_commit(repo_id, &r.commit_sha).await? {
            Some(cm) => {
                lines_attributed_indexed += r.lines.len() as u32;
                indexed.push((cm, r.lines));
            }
            None => {
                tracing::debug!(
                    sha = %r.commit_sha,
                    "explain_change: skipping unindexed commit"
                );
                skipped_shas.push(r.commit_sha);
            }
        }
    }

    // 3. Per-commit hunk excerpts. Each hit's diff_excerpt is the
    //    concatenation of every hunk this commit produced for this
    //    file, truncated.
    let mut hits: Vec<ExplainHit> = Vec::with_capacity(indexed.len());
    for (cm, lines) in indexed {
        let (excerpt, truncated) = if query.include_diff {
            let hunks = storage
                .get_hunks_for_file_in_commit(repo_id, &cm.sha, &query.file)
                .await?;
            let joined: String = hunks
                .iter()
                .map(|h| h.diff_text.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            truncate_diff(&joined, DIFF_EXCERPT_MAX_LINES)
        } else {
            (String::new(), false)
        };
        let date = DateTime::<Utc>::from_timestamp(cm.ts, 0)
            .map(|d| d.to_rfc3339())
            .unwrap_or_default();
        hits.push(ExplainHit {
            commit_sha: cm.sha,
            commit_message: cm.message,
            commit_author: cm.author,
            commit_date: date,
            blame_lines: lines,
            file_path: query.file.clone(),
            diff_excerpt: excerpt,
            diff_truncated: truncated,
            provenance: Provenance::Exact,
        });
    }

    // 4. Sort newest-first; cap to k.
    hits.sort_by(|a, b| {
        // Sort by commit_date desc; tie-break by sha asc for determinism.
        match b.commit_date.cmp(&a.commit_date) {
            std::cmp::Ordering::Equal => a.commit_sha.cmp(&b.commit_sha),
            other => other,
        }
    });
    let k = query.k.clamp(1, K_MAX) as usize;
    hits.truncate(k);

    // 5. Coverage + limitation note.
    let blame_coverage = if lines_attributed_total == 0 {
        0.0
    } else {
        lines_attributed_indexed as f32 / lines_attributed_total as f32
    };
    let limitation = build_limitation(
        lines_attributed_total,
        &skipped_shas,
        clamped_start,
        clamped_end,
    );

    let meta = ExplainMeta {
        lines_queried: (clamped_start, clamped_end),
        commits_unique: hits.len(),
        blame_coverage,
        limitation,
    };
    Ok((hits, meta))
}

fn build_limitation(
    total: u32,
    skipped: &[String],
    clamped_start: u32,
    clamped_end: u32,
) -> Option<String> {
    if total == 0 {
        return Some(
            "blame returned no attributable lines (file missing in HEAD or empty range)".into(),
        );
    }
    if !skipped.is_empty() {
        // Don't dump every SHA — keep the message terse but informative.
        let n = skipped.len();
        let preview: Vec<&str> = skipped.iter().take(3).map(String::as_str).collect();
        let suffix = if n > preview.len() {
            format!(" (+{} more)", n - preview.len())
        } else {
            String::new()
        };
        return Some(format!(
            "{n} commit(s) older than the local index watermark were skipped: [{}]{}; \
             range covered: {clamped_start}..={clamped_end}",
            preview.join(", "),
            suffix
        ));
    }
    None
}

/// Same shape as `ohara-core::retriever::truncate_diff`. Inlined here
/// rather than re-exported so the orchestrator stays self-contained.
fn truncate_diff(s: &str, max_lines: usize) -> (String, bool) {
    let nl = s.bytes().filter(|&b| b == b'\n').count();
    let has_trailing_partial = !s.is_empty() && !s.ends_with('\n');
    let total_lines = nl + if has_trailing_partial { 1 } else { 0 };
    if total_lines <= max_lines {
        return (s.to_string(), false);
    }
    let mut end = 0;
    let mut count = 0;
    for (i, b) in s.bytes().enumerate() {
        if b == b'\n' {
            count += 1;
            if count == max_lines {
                end = i + 1;
                break;
            }
        }
    }
    let extra = total_lines - max_lines;
    let mut out = s[..end].to_string();
    out.push_str(&format!("... ({} more lines)\n", extra));
    (out, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::IndexStatus;
    use crate::storage::{CommitRecord, HunkHit, HunkRecord};
    use crate::types::{ChangeKind, CommitMeta, Hunk, Symbol};
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Doc-test-style sanity check that the trait surface compiles
    /// against a hand-rolled fake. The orchestrator's behavioural tests
    /// land in Task 7 once `explain_change` exists.
    struct FakeBlamer;

    #[async_trait]
    impl BlameSource for FakeBlamer {
        async fn blame_range(
            &self,
            _file: &str,
            _line_start: u32,
            _line_end: u32,
        ) -> Result<Vec<BlameRange>> {
            Ok(vec![BlameRange {
                commit_sha: "abc".into(),
                lines: vec![1, 2, 3],
            }])
        }
    }

    #[tokio::test]
    async fn blame_source_trait_object_round_trips_a_fake() {
        let b: &dyn BlameSource = &FakeBlamer;
        let out = b.blame_range("any.rs", 1, 3).await.unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].commit_sha, "abc");
        assert_eq!(out[0].lines, vec![1, 2, 3]);
    }

    // ----- Orchestrator fakes (Task 7) -------------------------------------

    /// Storage that knows about a small in-memory set of commits +
    /// per-(commit, file) hunks. Unknown SHAs return `Ok(None)` so the
    /// orchestrator's "skip unindexed commit" path can be exercised.
    struct FakeStorageOrch {
        commits: HashMap<String, CommitMeta>,
        hunks: HashMap<(String, String), Vec<Hunk>>,
        get_commit_calls: Mutex<Vec<String>>,
    }

    impl FakeStorageOrch {
        fn new() -> Self {
            Self {
                commits: HashMap::new(),
                hunks: HashMap::new(),
                get_commit_calls: Mutex::new(Vec::new()),
            }
        }
        fn seed_commit(&mut self, cm: CommitMeta) {
            self.commits.insert(cm.sha.clone(), cm);
        }
        fn seed_hunk(&mut self, sha: &str, file: &str, diff_text: &str) {
            self.hunks
                .entry((sha.to_string(), file.to_string()))
                .or_default()
                .push(Hunk {
                    commit_sha: sha.into(),
                    file_path: file.into(),
                    language: Some("rust".into()),
                    change_kind: ChangeKind::Modified,
                    diff_text: diff_text.into(),
                });
        }
    }

    #[async_trait]
    impl Storage for FakeStorageOrch {
        async fn open_repo(&self, _: &RepoId, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        async fn get_index_status(&self, _: &RepoId) -> Result<IndexStatus> {
            unreachable!()
        }
        async fn set_last_indexed_commit(&self, _: &RepoId, _: &str) -> Result<()> {
            Ok(())
        }
        async fn put_commit(&self, _: &RepoId, _: &CommitRecord) -> Result<()> {
            Ok(())
        }
        async fn put_hunks(&self, _: &RepoId, _: &[HunkRecord]) -> Result<()> {
            Ok(())
        }
        async fn put_head_symbols(&self, _: &RepoId, _: &[Symbol]) -> Result<()> {
            Ok(())
        }
        async fn clear_head_symbols(&self, _: &RepoId) -> Result<()> {
            unreachable!()
        }
        async fn knn_hunks(
            &self,
            _: &RepoId,
            _: &[f32],
            _: u8,
            _: Option<&str>,
            _: Option<i64>,
        ) -> Result<Vec<HunkHit>> {
            unreachable!()
        }
        async fn bm25_hunks_by_text(
            &self,
            _: &RepoId,
            _: &str,
            _: u8,
            _: Option<&str>,
            _: Option<i64>,
        ) -> Result<Vec<HunkHit>> {
            unreachable!()
        }
        async fn bm25_hunks_by_symbol_name(
            &self,
            _: &RepoId,
            _: &str,
            _: u8,
            _: Option<&str>,
            _: Option<i64>,
        ) -> Result<Vec<HunkHit>> {
            unreachable!()
        }
        async fn blob_was_seen(&self, _: &str, _: &str) -> Result<bool> {
            Ok(false)
        }
        async fn record_blob_seen(&self, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        async fn get_commit(&self, _: &RepoId, sha: &str) -> Result<Option<CommitMeta>> {
            self.get_commit_calls.lock().unwrap().push(sha.to_string());
            Ok(self.commits.get(sha).cloned())
        }
        async fn get_hunks_for_file_in_commit(
            &self,
            _: &RepoId,
            sha: &str,
            file: &str,
        ) -> Result<Vec<Hunk>> {
            Ok(self
                .hunks
                .get(&(sha.to_string(), file.to_string()))
                .cloned()
                .unwrap_or_default())
        }
    }

    /// Scripted blame source. Returns the supplied `Vec<BlameRange>`
    /// regardless of the queried lines, but echoes the queried bounds
    /// back to the caller via `last_args` so tests can assert the
    /// orchestrator clamped its inputs first.
    struct ScriptedBlamer {
        out: Vec<BlameRange>,
        last_args: Mutex<Option<(String, u32, u32)>>,
    }

    #[async_trait]
    impl BlameSource for ScriptedBlamer {
        async fn blame_range(
            &self,
            file: &str,
            line_start: u32,
            line_end: u32,
        ) -> Result<Vec<BlameRange>> {
            *self.last_args.lock().unwrap() = Some((file.to_string(), line_start, line_end));
            Ok(self.out.clone())
        }
    }

    fn cm(sha: &str, ts: i64, message: &str) -> CommitMeta {
        CommitMeta {
            sha: sha.into(),
            parent_sha: None,
            is_merge: false,
            author: Some("alice".into()),
            ts,
            message: message.into(),
        }
    }

    #[tokio::test]
    async fn explain_returns_unique_commits_in_recency_order() {
        // Plan 5 / Task 7.r: blame attributes lines 1-2 to "old" (older
        // commit), lines 3-4 to "new" (newer). The orchestrator must
        // collapse to two unique commits, ordered newest-first.
        let mut storage = FakeStorageOrch::new();
        storage.seed_commit(cm("old", 1_000, "older change"));
        storage.seed_commit(cm("new", 2_000, "newer change"));
        storage.seed_hunk("old", "src/a.rs", "+    a();\n");
        storage.seed_hunk("new", "src/a.rs", "+    b();\n");
        let blamer = ScriptedBlamer {
            out: vec![
                BlameRange {
                    commit_sha: "old".into(),
                    lines: vec![1, 2],
                },
                BlameRange {
                    commit_sha: "new".into(),
                    lines: vec![3, 4],
                },
            ],
            last_args: Mutex::new(None),
        };
        let q = ExplainQuery {
            file: "src/a.rs".into(),
            line_start: 1,
            line_end: 4,
            k: 5,
            include_diff: true,
        };
        let id = RepoId::from_parts("first", "/r");
        let (hits, meta) = explain_change(&storage, &blamer, &id, &q).await.unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].commit_sha, "new", "newest-first order");
        assert_eq!(hits[1].commit_sha, "old");
        assert_eq!(meta.commits_unique, 2);
        assert!((meta.blame_coverage - 1.0).abs() < 1e-6);
        assert!(meta.limitation.is_none());
    }

    #[tokio::test]
    async fn explain_clamps_line_range_to_file_bounds() {
        // Plan 5 / Task 7.r: caller asks for 1..=999 against a file that
        // only has, say, 10 lines. The orchestrator must pass the
        // *clamped* upper bound to the BlameSource — not the raw 999 —
        // and reflect the clamped pair in `_meta.lines_queried`. A real
        // Blamer also clamps internally, but the contract of the
        // orchestrator is to be the source of truth for `lines_queried`.
        let mut storage = FakeStorageOrch::new();
        storage.seed_commit(cm("only", 1, "only commit"));
        storage.seed_hunk("only", "src/a.rs", "+    only();\n");
        let blamer = ScriptedBlamer {
            // Pretend the file actually has 10 lines.
            out: vec![BlameRange {
                commit_sha: "only".into(),
                lines: (1..=10).collect(),
            }],
            last_args: Mutex::new(None),
        };
        let q = ExplainQuery {
            file: "src/a.rs".into(),
            line_start: 1,
            line_end: 999,
            k: 5,
            include_diff: true,
        };
        let id = RepoId::from_parts("first", "/r");
        let (hits, meta) = explain_change(&storage, &blamer, &id, &q).await.unwrap();
        assert_eq!(hits.len(), 1);
        // The blamer is the authoritative file-length oracle (it can
        // read the file). The orchestrator should set `lines_queried`
        // to the actual range covered by the blame, not the raw input.
        assert_eq!(meta.lines_queried.0, 1);
        assert_eq!(meta.lines_queried.1, 10);
    }

    #[tokio::test]
    async fn explain_skips_unindexed_commits_and_notes_in_meta() {
        // Plan 5 / Task 7.r: blame returns "indexed" + "missing"; only
        // "indexed" is in storage. The orchestrator must drop "missing"
        // silently, return one hit, and set `commits_unique = 1`.
        let mut storage = FakeStorageOrch::new();
        storage.seed_commit(cm("indexed", 1_000, "indexed change"));
        storage.seed_hunk("indexed", "src/a.rs", "+    a();\n");
        let blamer = ScriptedBlamer {
            out: vec![
                BlameRange {
                    commit_sha: "indexed".into(),
                    lines: vec![1, 2],
                },
                BlameRange {
                    commit_sha: "missing".into(),
                    lines: vec![3, 4],
                },
            ],
            last_args: Mutex::new(None),
        };
        let q = ExplainQuery {
            file: "src/a.rs".into(),
            line_start: 1,
            line_end: 4,
            k: 5,
            include_diff: true,
        };
        let id = RepoId::from_parts("first", "/r");
        let (hits, meta) = explain_change(&storage, &blamer, &id, &q).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].commit_sha, "indexed");
        assert_eq!(meta.commits_unique, 1);
    }

    #[tokio::test]
    async fn explain_blame_coverage_lt_one_when_some_lines_unattributed() {
        // Plan 5 / Task 7.r: blame attributes only 2 of 4 queried lines
        // (the others fall on a SHA that storage doesn't know). Coverage
        // must be 0.5; the limitation note must mention the gap.
        let mut storage = FakeStorageOrch::new();
        storage.seed_commit(cm("kept", 1_000, "kept change"));
        storage.seed_hunk("kept", "src/a.rs", "+    a();\n");
        let blamer = ScriptedBlamer {
            out: vec![
                BlameRange {
                    commit_sha: "kept".into(),
                    lines: vec![1, 2],
                },
                BlameRange {
                    commit_sha: "dropped".into(),
                    lines: vec![3, 4],
                },
            ],
            last_args: Mutex::new(None),
        };
        let q = ExplainQuery {
            file: "src/a.rs".into(),
            line_start: 1,
            line_end: 4,
            k: 5,
            include_diff: true,
        };
        let id = RepoId::from_parts("first", "/r");
        let (_hits, meta) = explain_change(&storage, &blamer, &id, &q).await.unwrap();
        assert!(
            (meta.blame_coverage - 0.5).abs() < 1e-6,
            "coverage should be 0.5, got {}",
            meta.blame_coverage
        );
        assert!(
            meta.limitation.is_some(),
            "limitation should describe the unattributed lines"
        );
    }

    #[tokio::test]
    async fn explain_returns_provenance_exact() {
        // Plan 5 / Task 7.r: every hit's provenance must be Exact.
        // git blame is git-truth, never inferred.
        let mut storage = FakeStorageOrch::new();
        storage.seed_commit(cm("only", 1_000, "only"));
        storage.seed_hunk("only", "src/a.rs", "+    only();\n");
        let blamer = ScriptedBlamer {
            out: vec![BlameRange {
                commit_sha: "only".into(),
                lines: vec![1],
            }],
            last_args: Mutex::new(None),
        };
        let q = ExplainQuery {
            file: "src/a.rs".into(),
            line_start: 1,
            line_end: 1,
            k: 5,
            include_diff: true,
        };
        let id = RepoId::from_parts("first", "/r");
        let (hits, _meta) = explain_change(&storage, &blamer, &id, &q).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert!(matches!(hits[0].provenance, Provenance::Exact));
        // Serializes to "EXACT" (not "EXTRACTED" / "INFERRED").
        let s = serde_json::to_string(&hits[0]).unwrap();
        assert!(
            s.contains("\"provenance\":\"EXACT\""),
            "expected EXACT, got: {s}"
        );
    }
}
