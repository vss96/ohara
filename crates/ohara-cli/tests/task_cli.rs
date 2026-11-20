//! `ohara task` create / approve / list / show against a temp repo, a temp
//! `OHARA_HOME` and a scripted `FakeRuntime` (never a real agent CLI).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, OnceLock};

use chrono::{DateTime, Duration, Utc};
use ohara_cli::commands::task::approve::approve;
use ohara_cli::commands::task::clock::{parse_duration, parse_until, Budget};
use ohara_cli::commands::task::create::{create, CreateOpts};
use ohara_cli::commands::task::view::{render_list, render_task};
use ohara_core::task::{RuntimeName, TaskRecord, TaskState, TaskStore};
use ohara_harness::agent::fake::FakeRuntime;
use ohara_harness::agent::roles::RoleRunner;
use ohara_harness::agent::{AgentOutcome, AgentStatus};
use ohara_harness::config::{HarnessConfig, RoleMap, RuntimeKind, RuntimeSpec};
use ohara_harness::contract::freeze::load_frozen;
use ohara_harness::git::Git;
use ohara_storage::harness::SqliteTaskStore;

/// `OHARA_HOME` is process-global; one test at a time.
fn env_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

struct Env {
    _home: tempfile::TempDir,
    repo: tempfile::TempDir,
    store: SqliteTaskStore,
    runner: RoleRunner,
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

/// A fake that completes without writing a draft, so drafting falls back to
/// the workflow template.
fn runner() -> RoleRunner {
    let name = RuntimeName("fake".into());
    let spec = RuntimeSpec {
        kind: RuntimeKind::Fake,
        model: None,
        command: None,
        prompt: None,
        done: None,
    };
    let roles = [ohara_core::task::Role::Contract]
        .into_iter()
        .map(|r| (r, vec![name.clone()]))
        .collect();
    let cfg = HarnessConfig {
        runtimes: BTreeMap::from([(name.clone(), spec)]),
        roles: RoleMap(roles),
    };
    let outcome = AgentOutcome {
        status: AgentStatus::Completed,
        final_message: "done".into(),
        cost: None,
        exit_code: Some(0),
    };
    let fake = Arc::new(FakeRuntime::new(name, vec![outcome]));
    RoleRunner::new(&cfg, move |_, _| fake.clone()).unwrap()
}

async fn setup(rust_repo: bool) -> Env {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("OHARA_HOME", home.path());
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q"]);
    let marker = if rust_repo { "Cargo.toml" } else { "notes.txt" };
    std::fs::write(repo.path().join(marker), "x = 1\n").unwrap();
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-q", "-m", "init"]);
    let store = SqliteTaskStore::open_default().await.unwrap();
    Env {
        _home: home,
        repo,
        store,
        runner: runner(),
    }
}

fn opts(env: &Env, budget: Budget) -> CreateOpts {
    CreateOpts {
        outcome: "add a retry helper".into(),
        budget,
        workflow: "feature".into(),
        repo: env.repo.path().to_path_buf(),
    }
}

fn worktree_of(t: &TaskRecord) -> PathBuf {
    ohara_core::paths::ohara_home()
        .unwrap()
        .join("tasks")
        .join(t.id.to_string())
        .join("worktree")
}

#[tokio::test]
async fn detach_leaves_task_awaiting_approval() {
    let _g = env_lock().lock().await;
    let env = setup(true).await;
    let now = Utc::now();
    let t = create(
        &env.store,
        &env.runner,
        opts(&env, Budget::For(Duration::hours(1))),
        now,
    )
    .await
    .unwrap();
    assert_eq!(t.state, TaskState::AwaitingApproval);
    assert!(t.contract_hash.is_none());
    assert_eq!(
        t.branch,
        format!("ohara/{}", t.id.to_string().to_lowercase())
    );
    assert!(worktree_of(&t).join("Cargo.toml").is_file());
    let stored = env.store.get_task(&t.id).await.unwrap().unwrap();
    assert_eq!(stored.state, TaskState::AwaitingApproval);
    assert_eq!(
        stored.base_sha,
        Git::new(env.repo.path()).head().unwrap(),
        "worktree starts at the repo's HEAD"
    );
}

#[tokio::test]
async fn approve_freezes_contract_and_sets_running() {
    let _g = env_lock().lock().await;
    let env = setup(true).await;
    let now = Utc::now();
    let t = create(
        &env.store,
        &env.runner,
        opts(&env, Budget::For(Duration::hours(1))),
        now,
    )
    .await
    .unwrap();
    let run = approve(&env.store, &t.id, now, None).await.unwrap();
    assert_eq!(run.state, TaskState::Running);
    let hash = run.contract_hash.expect("hash stored at approval");
    let (c, h) = load_frozen(&Git::new(worktree_of(&t)), &t.id, &hash).unwrap();
    assert_eq!(h, hash);
    assert_eq!(c.outcome, "add a retry helper");
    let again = approve(&env.store, &t.id, now, None).await;
    assert!(again.is_err(), "only awaiting tasks can be approved");
}

#[tokio::test]
async fn for_duration_counts_from_approval() {
    let _g = env_lock().lock().await;
    let env = setup(true).await;
    let t0 = Utc::now();
    let t = create(
        &env.store,
        &env.runner,
        opts(&env, Budget::For(Duration::hours(2))),
        t0,
    )
    .await
    .unwrap();
    let approved_at = t0 + Duration::hours(3);
    let run = approve(&env.store, &t.id, approved_at, None).await.unwrap();
    assert_eq!(run.deadline.0, approved_at + Duration::hours(2));
    let (c, _) = load_frozen(
        &Git::new(worktree_of(&t)),
        &t.id,
        &run.contract_hash.unwrap(),
    )
    .unwrap();
    assert_eq!(c.budget.deadline.0, approved_at + Duration::hours(2));
}

#[tokio::test]
async fn until_in_past_is_error() {
    let _g = env_lock().lock().await;
    let env = setup(true).await;
    let err = parse_until("2001-01-01T00:00:00Z", Utc::now()).unwrap_err();
    assert!(err.to_string().contains("past"), "{err}");
    assert!(env.store.list_tasks().await.unwrap().is_empty());
    assert!(parse_duration("bogus").is_err());
    assert_eq!(parse_duration("1h30m").unwrap(), Duration::minutes(90));
    // Absolute deadlines pass through unchanged.
    let at: DateTime<Utc> = Utc::now() + Duration::hours(5);
    let b = parse_until(&at.to_rfc3339(), Utc::now()).unwrap();
    assert!(matches!(b, Budget::Until(d) if d.timestamp() == at.timestamp()));
}

#[tokio::test]
async fn list_shows_interrupted_for_stale_heartbeat() {
    let _g = env_lock().lock().await;
    let env = setup(true).await;
    let now = Utc::now();
    let t = create(
        &env.store,
        &env.runner,
        opts(&env, Budget::For(Duration::hours(1))),
        now - Duration::seconds(300),
    )
    .await
    .unwrap();
    approve(&env.store, &t.id, now - Duration::seconds(120), None)
        .await
        .unwrap();
    let tasks = env.store.list_tasks().await.unwrap();
    let listing = render_list(&tasks, now);
    assert!(listing.contains("interrupted"), "{listing}");
    assert!(render_task(&tasks[0], now).contains("interrupted"));
    let fresh = render_list(&tasks, now - Duration::seconds(100));
    assert!(fresh.contains("running") && !fresh.contains("interrupted"));
}

#[tokio::test]
async fn approve_refuses_empty_scripts_for_non_research() {
    let _g = env_lock().lock().await;
    let env = setup(false).await;
    let now = Utc::now();
    let t = create(
        &env.store,
        &env.runner,
        opts(&env, Budget::For(Duration::hours(1))),
        now,
    )
    .await
    .unwrap();
    let err = approve(&env.store, &t.id, now, None).await.unwrap_err();
    assert!(err.to_string().contains("scripts"), "{err}");
    let stored = env.store.get_task(&t.id).await.unwrap().unwrap();
    assert_eq!(stored.state, TaskState::AwaitingApproval);
}

#[tokio::test]
async fn edit_replaces_contract_and_reprompts_on_invalid() {
    let _g = env_lock().lock().await;
    let env = setup(true).await;
    let now = Utc::now();
    let t = create(
        &env.store,
        &env.runner,
        opts(&env, Budget::For(Duration::hours(1))),
        now,
    )
    .await
    .unwrap();
    let pending = ohara_core::paths::ohara_home()
        .unwrap()
        .join("tasks")
        .join(t.id.to_string())
        .join("contract.pending.toml");
    let original = std::fs::read_to_string(&pending).unwrap();
    let mut calls = 0;
    // First pass saves invalid TOML (the loop must re-open the editor), the
    // second saves a valid contract with a new outcome.
    let mut editor2 = |p: &Path| -> anyhow::Result<()> {
        calls += 1;
        let body = match calls {
            1 => "this is not toml [".to_string(),
            _ => original.replace("add a retry helper", "edited outcome"),
        };
        std::fs::write(p, body)?;
        Ok(())
    };
    let run = approve(&env.store, &t.id, now, Some(&mut editor2))
        .await
        .unwrap();
    let (c, _) = load_frozen(
        &Git::new(worktree_of(&t)),
        &t.id,
        &run.contract_hash.unwrap(),
    )
    .unwrap();
    assert_eq!(c.outcome, "edited outcome");
    assert_eq!(calls, 2);
}
