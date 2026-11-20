//! `ohara prune` (plan-33 follow-up F4): the in-process path removes an
//! unreachable retired tip and its commits. The index is seeded straight
//! through `ohara-storage`, so no embedding model is needed.

use ohara_core::storage::CommitRecord;
use ohara_core::types::CommitMeta;
use ohara_core::{CommitSha, Storage, TipStore, WorktreeKey};
use std::path::Path;
use std::process::Command;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.email=a@a", "-c", "user.name=a"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn commit_file(dir: &Path, file: &str, msg: &str) -> String {
    std::fs::write(dir.join(file), format!("// {msg}\n")).unwrap();
    git(dir, &["add", file]);
    git(dir, &["commit", "-q", "-m", msg]);
    git(dir, &["rev-parse", "HEAD"])
}

fn record(sha: &str) -> CommitRecord {
    CommitRecord {
        meta: CommitMeta {
            commit_sha: sha.into(),
            parent_sha: None,
            is_merge: false,
            author: Some("a".into()),
            ts: 1_700_000_000,
            message: "m".into(),
        },
        message_emb: vec![0.0; 384],
        ulid: String::new(),
    }
}

fn sha(s: &str) -> CommitSha {
    CommitSha::parse(s).unwrap()
}

#[tokio::test]
async fn prune_in_process_removes_an_unreachable_retired_tip_and_its_commits() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    // Only this test runs in this binary, so setting the env is safe.
    std::env::set_var("OHARA_HOME", home.path());
    let main = root.path().join("main");
    std::fs::create_dir(&main).unwrap();
    git(&main, &["init", "-q"]);
    let c1 = commit_file(&main, "a.rs", "first");
    let c2 = commit_file(&main, "b.rs", "second");
    // A commit on a branch that is then deleted: nothing reaches it.
    git(&main, &["checkout", "-q", "-b", "gone"]);
    let gone = commit_file(&main, "c.rs", "gone");
    git(&main, &["checkout", "-q", "-"]);
    git(&main, &["branch", "-q", "-D", "gone"]);

    // Seed the shared store: three indexed commits, main's tip, and a
    // retired tip on the unreachable commit (as a reset would leave).
    let id = ohara_git::RepoIdentity::resolve(&main).unwrap();
    let db_path = ohara_core::paths::index_db_path(&id.repo_id).unwrap();
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    let store = ohara_storage::SqliteStorage::open(&db_path).await.unwrap();
    store
        .open_repo(&id.repo_id, &id.id_path, &id.first_commit_sha)
        .await
        .unwrap();
    for c in [&c1, &c2, &gone] {
        store.put_commit(&id.repo_id, &record(c)).await.unwrap();
    }
    let slot = WorktreeKey::linked("slot-1").unwrap();
    store
        .set_indexed_tip(&id.repo_id, &WorktreeKey::main(), &sha(&c2))
        .await
        .unwrap();
    store
        .set_indexed_tip(
            &id.repo_id,
            &WorktreeKey::retired(&slot, &sha(&gone)),
            &sha(&gone),
        )
        .await
        .unwrap();
    drop(store);

    let out = Command::new(env!("CARGO_BIN_EXE_ohara"))
        .env("OHARA_HOME", home.path())
        .args(["--no-daemon", "prune"])
        .arg(&main)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "prune failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("removed 1 unreachable commit(s) and 1 tip(s)"),
        "stdout: {stdout}"
    );

    let store = ohara_storage::SqliteStorage::open(&db_path).await.unwrap();
    let mut left: Vec<String> = store
        .indexed_commit_shas(&id.repo_id)
        .await
        .unwrap()
        .into_iter()
        .map(|c| c.as_str().to_string())
        .collect();
    left.sort();
    let mut want = vec![c1, c2.clone()];
    want.sort();
    assert_eq!(left, want);
    let tips = store.indexed_tips(&id.repo_id).await.unwrap();
    assert_eq!(tips.len(), 1, "got {tips:?}");
    assert!(tips[0].worktree.is_main());
    assert_eq!(tips[0].sha.as_str(), c2);
}
