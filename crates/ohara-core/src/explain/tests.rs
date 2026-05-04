//! Tests for `explain_change` orchestrator. Moved here from `mod.rs`
//! to keep `mod.rs` under the 500-line limit (plan-21 D.1).

use super::*;
use crate::query::IndexStatus;
use crate::storage::{CommitRecord, HunkHit, HunkRecord};
use crate::types::{ChangeKind, CommitMeta, Hunk, Symbol};
use std::collections::HashMap;
use std::sync::Mutex;

/// Doc-test-style sanity check that the trait surface compiles
/// against a hand-rolled fake. The orchestrator's behavioural tests
/// live in the surrounding `tests` module.
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
    /// Plan 12 Task 3.2: per-(file_path, anchor_sha) neighbour
    /// list returned verbatim by `get_neighboring_file_commits`.
    neighbours: HashMap<(String, String), Vec<(u32, CommitMeta)>>,
}

impl FakeStorageOrch {
    fn new() -> Self {
        Self {
            commits: HashMap::new(),
            hunks: HashMap::new(),
            get_commit_calls: Mutex::new(Vec::new()),
            neighbours: HashMap::new(),
        }
    }
    fn seed_commit(&mut self, cm: CommitMeta) {
        self.commits.insert(cm.commit_sha.clone(), cm);
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
    fn seed_neighbours(&mut self, file: &str, anchor: &str, neighbours: Vec<(u32, CommitMeta)>) {
        self.neighbours
            .insert((file.to_string(), anchor.to_string()), neighbours);
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
    async fn commit_exists(&self, _: &str) -> Result<bool> {
        unreachable!("explain orchestrator should not exercise commit_exists")
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
    async fn bm25_hunks_by_semantic_text(
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
    async fn bm25_hunks_by_historical_symbol(
        &self,
        _: &RepoId,
        _: &str,
        _: u8,
        _: Option<&str>,
        _: Option<i64>,
    ) -> Result<Vec<HunkHit>> {
        unreachable!()
    }
    async fn get_hunk_symbols(
        &self,
        _: &RepoId,
        _: crate::storage::HunkId,
    ) -> Result<Vec<crate::types::HunkSymbol>> {
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
    async fn get_neighboring_file_commits(
        &self,
        _: &RepoId,
        file: &str,
        anchor: &str,
        _: u8,
        _: u8,
    ) -> Result<Vec<(u32, crate::types::CommitMeta)>> {
        Ok(self
            .neighbours
            .get(&(file.to_string(), anchor.to_string()))
            .cloned()
            .unwrap_or_default())
    }
    async fn get_index_metadata(
        &self,
        _: &RepoId,
    ) -> Result<crate::index_metadata::StoredIndexMetadata> {
        Ok(crate::index_metadata::StoredIndexMetadata::default())
    }
    async fn put_index_metadata(&self, _: &RepoId, _: &[(String, String)]) -> Result<()> {
        Ok(())
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
        commit_sha: sha.into(),
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
        include_related: false,
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
        include_related: false,
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
        include_related: false,
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
        include_related: false,
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
        include_related: false,
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

#[tokio::test]
async fn explain_change_attaches_related_commits_when_include_related_is_true() {
    // Plan 12 Task 3.2: include_related=true causes the
    // orchestrator to call get_neighboring_file_commits per
    // anchor. The related commits land in ExplainMeta with
    // Provenance::Inferred (NOT Exact); blame hits keep
    // Provenance::Exact.
    let mut storage = FakeStorageOrch::new();
    let anchor_sha = "anchor";
    storage.seed_commit(CommitMeta {
        commit_sha: anchor_sha.into(),
        parent_sha: None,
        is_merge: false,
        author: Some("alice".into()),
        ts: 1_700_001_000,
        message: "anchor change".into(),
    });
    storage.seed_hunk(anchor_sha, "src/a.rs", "@@ -1,1 +1,1 @@\n+changed");
    storage.seed_neighbours(
        "src/a.rs",
        anchor_sha,
        vec![
            (
                1,
                CommitMeta {
                    commit_sha: "older".into(),
                    parent_sha: None,
                    is_merge: false,
                    author: Some("bob".into()),
                    ts: 1_700_000_000,
                    message: "older context".into(),
                },
            ),
            (
                2,
                CommitMeta {
                    commit_sha: "newer".into(),
                    parent_sha: None,
                    is_merge: false,
                    author: Some("carol".into()),
                    ts: 1_700_002_000,
                    message: "newer follow-up".into(),
                },
            ),
        ],
    );
    let blamer = ScriptedBlamer {
        out: vec![BlameRange {
            commit_sha: anchor_sha.into(),
            lines: vec![1],
        }],
        last_args: Mutex::new(None),
    };
    let q = ExplainQuery {
        file: "src/a.rs".into(),
        line_start: 1,
        line_end: 1,
        k: 5,
        include_diff: false,
        include_related: true,
    };
    let id = RepoId::from_parts("first", "/r");
    let (hits, meta) = explain_change(&storage, &blamer, &id, &q).await.unwrap();

    // Blame hit stays Exact.
    assert_eq!(hits.len(), 1);
    assert!(matches!(hits[0].provenance, Provenance::Exact));

    // Related commits exist + are labelled Inferred.
    assert_eq!(meta.related_commits.len(), 2);
    for r in &meta.related_commits {
        assert!(matches!(r.provenance, Provenance::Inferred));
    }
    let related_shas: Vec<&str> = meta
        .related_commits
        .iter()
        .map(|r| r.commit_sha.as_str())
        .collect();
    assert!(related_shas.contains(&"older"));
    assert!(related_shas.contains(&"newer"));
    // Anchor should never appear in the related list.
    assert!(!related_shas.contains(&"anchor"));
    // touched_hunks round-trips.
    assert_eq!(meta.related_commits[0].touched_hunks, 1);
    assert!(meta.enrichment_limitation.is_none());
}

#[tokio::test]
async fn explain_change_omits_related_commits_when_include_related_false() {
    let mut storage = FakeStorageOrch::new();
    storage.seed_commit(CommitMeta {
        commit_sha: "anchor".into(),
        parent_sha: None,
        is_merge: false,
        author: None,
        ts: 1,
        message: "m".into(),
    });
    storage.seed_neighbours(
        "src/a.rs",
        "anchor",
        vec![(
            1,
            CommitMeta {
                commit_sha: "would-not-appear".into(),
                parent_sha: None,
                is_merge: false,
                author: None,
                ts: 1,
                message: "x".into(),
            },
        )],
    );
    let blamer = ScriptedBlamer {
        out: vec![BlameRange {
            commit_sha: "anchor".into(),
            lines: vec![1],
        }],
        last_args: Mutex::new(None),
    };
    let q = ExplainQuery {
        file: "src/a.rs".into(),
        line_start: 1,
        line_end: 1,
        k: 5,
        include_diff: false,
        include_related: false,
    };
    let id = RepoId::from_parts("first", "/r");
    let (_hits, meta) = explain_change(&storage, &blamer, &id, &q).await.unwrap();
    assert!(meta.related_commits.is_empty());
}

#[test]
fn explain_change_emits_blame_and_hydrate_phases() {
    let (seen, _guard) = crate::perf_trace::test_phase_capture::acquire_phase_collector();

    let mut storage = FakeStorageOrch::new();
    storage.seed_commit(cm("abc", 1_000, "msg"));
    storage.seed_hunk("abc", "src/a.rs", "+    a();\n");
    let blamer = ScriptedBlamer {
        out: vec![BlameRange {
            commit_sha: "abc".into(),
            lines: vec![1, 2, 3],
        }],
        last_args: Mutex::new(None),
    };
    let q = ExplainQuery {
        file: "src/a.rs".into(),
        line_start: 1,
        line_end: 3,
        k: 5,
        include_diff: true,
        include_related: false,
    };
    let id = RepoId::from_parts("seed", "/r");

    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    rt.block_on(async {
        let _ = explain_change(&storage, &blamer, &id, &q).await.unwrap();
    });

    let seen = seen.lock().unwrap();
    for required in ["blame", "hydrate_explain"] {
        assert!(
            seen.contains(required),
            "missing phase event {required}; seen = {:?}",
            *seen
        );
    }
}
