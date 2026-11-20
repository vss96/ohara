//! `ohara task` create / approve / list / show against a temp repo, a temp
//! `OHARA_HOME` and a scripted `FakeRuntime` (never a real agent CLI).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, OnceLock};

use chrono::{DateTime, Duration, Utc};
use clap::{Parser, Subcommand};
use ohara_cli::commands::task::approve::{approval_report, approve};
use ohara_cli::commands::task::clock::{parse_duration, parse_until, Budget};
use ohara_cli::commands::task::create::{create, CreateOpts};
use ohara_cli::commands::task::view::{render_list, render_task};
use ohara_cli::commands::task::{create_flow, resolve_budget, Args as TaskArgs, TaskCmd};
use ohara_core::task::decision::DecisionRecord;
use ohara_core::task::{RuntimeName, StepRecord, TaskId, TaskRecord, TaskState, TaskStore};
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

fn parse_task(argv: &[&str]) -> Result<TaskArgs, clap::Error> {
    #[derive(Subcommand, Debug)]
    enum C {
        Task(TaskArgs),
    }
    #[derive(Parser, Debug)]
    struct W {
        #[command(subcommand)]
        c: C,
    }
    let mut full = vec!["ohara", "task"];
    full.extend_from_slice(argv);
    W::try_parse_from(full).map(|w| match w.c {
        C::Task(a) => a,
    })
}

#[tokio::test]
async fn detach_flow_prints_approve_hint_and_never_prompts() {
    let _g = env_lock().lock().await;
    let env = setup(true).await;
    let repo = env.repo.path().to_str().unwrap().to_string();
    let args = parse_task(&["do it", "--for", "1h", "--detach", "--repo", &repo]).unwrap();
    assert!(args.detach);
    let mut out = Vec::new();
    let mut ask = |_: &str| -> anyhow::Result<String> { panic!("detach must not prompt") };
    let t = create_flow(
        &env.store,
        &env.runner,
        args,
        Utc::now(),
        &mut out,
        &mut ask,
    )
    .await
    .unwrap();
    assert_eq!(t.state, TaskState::AwaitingApproval);
    let text = String::from_utf8(out).unwrap();
    assert!(
        text.contains(&format!("ohara task approve {}", t.id)),
        "{text}"
    );
    assert!(text.contains("no sandbox"), "{text}");
    // Declining at the prompt also leaves it awaiting approval.
    let args = parse_task(&["do it", "--for", "1h", "--repo", &repo]).unwrap();
    let mut out = Vec::new();
    let mut ask = |_: &str| -> anyhow::Result<String> { Ok("n".into()) };
    let t2 = create_flow(
        &env.store,
        &env.runner,
        args,
        Utc::now(),
        &mut out,
        &mut ask,
    )
    .await
    .unwrap();
    assert_eq!(t2.state, TaskState::AwaitingApproval);
}

#[test]
fn clap_surface() {
    let list = parse_task(&["list"]).unwrap();
    assert!(list.outcome.is_none());
    assert!(matches!(list.command, Some(TaskCmd::List { .. })));
    let create = parse_task(&["ship it", "--until", "23:59"]).unwrap();
    assert_eq!(create.outcome.as_deref(), Some("ship it"));
    assert!(create.command.is_none());
    assert!(parse_task(&["x", "--for", "1h", "--until", "23:59"]).is_err());
    let none = parse_task(&["x"]).unwrap();
    let err = resolve_budget(&none, Utc::now()).unwrap_err();
    assert!(err.to_string().contains("--for"), "{err}");
}

#[tokio::test]
async fn approve_refuses_pending_contract_changed_since_display() {
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
    let pending = pending_file(&t);
    let text = std::fs::read_to_string(&pending).unwrap();
    std::fs::write(
        &pending,
        text.replace("add a retry helper", "something else"),
    )
    .unwrap();
    let err = approve(&env.store, &t.id, now, None).await.unwrap_err();
    assert!(err.to_string().contains("changed since"), "{err}");
    let stored = env.store.get_task(&t.id).await.unwrap().unwrap();
    assert_eq!(stored.state, TaskState::AwaitingApproval);
    // Reviewing through --edit accepts the new text.
    let mut keep = |_: &Path| -> anyhow::Result<()> { Ok(()) };
    let run = approve(&env.store, &t.id, now, Some(&mut keep))
        .await
        .unwrap();
    assert_eq!(run.state, TaskState::Running);
}

#[tokio::test]
async fn approve_prints_frozen_contract_and_hash() {
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
    let hash = run.contract_hash.unwrap();
    let (c, _) = load_frozen(&Git::new(worktree_of(&t)), &t.id, &hash).unwrap();
    let report = approval_report(&run, &c);
    assert!(report.contains(&c.to_toml()), "{report}");
    assert!(report.contains(&hash.to_string()), "{report}");
    assert!(report.contains("no sandbox"), "{report}");
    assert!(
        report.contains(&(now + Duration::hours(1)).to_rfc3339()[..19]),
        "{report}"
    );
}

/// Fails the first `update_task` that moves a task to Running.
struct Flaky<'a> {
    inner: &'a SqliteTaskStore,
    armed: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl TaskStore for Flaky<'_> {
    async fn create_task(&self, t: &TaskRecord) -> ohara_core::Result<()> {
        self.inner.create_task(t).await
    }
    async fn update_task(&self, t: &TaskRecord) -> ohara_core::Result<()> {
        let running = t.state == TaskState::Running;
        if running && self.armed.swap(false, std::sync::atomic::Ordering::SeqCst) {
            return Err(ohara_core::OhraError::Storage("injected failure".into()));
        }
        self.inner.update_task(t).await
    }
    async fn get_task(&self, id: &TaskId) -> ohara_core::Result<Option<TaskRecord>> {
        self.inner.get_task(id).await
    }
    async fn list_tasks(&self) -> ohara_core::Result<Vec<TaskRecord>> {
        self.inner.list_tasks().await
    }
    async fn heartbeat(&self, id: &TaskId, at: DateTime<Utc>) -> ohara_core::Result<()> {
        self.inner.heartbeat(id, at).await
    }
    async fn record_step(&self, s: &StepRecord) -> ohara_core::Result<()> {
        self.inner.record_step(s).await
    }
    async fn list_steps(&self, id: &TaskId) -> ohara_core::Result<Vec<StepRecord>> {
        self.inner.list_steps(id).await
    }
    async fn put_decision(&self, d: &DecisionRecord) -> ohara_core::Result<()> {
        self.inner.put_decision(d).await
    }
    async fn list_decisions(&self, id: &TaskId) -> ohara_core::Result<Vec<DecisionRecord>> {
        self.inner.list_decisions(id).await
    }
}

fn pending_file(t: &TaskRecord) -> PathBuf {
    worktree_of(t).with_file_name("contract.pending.toml")
}

#[tokio::test]
async fn approve_recovers_after_freeze_succeeded_but_record_update_failed() {
    let _g = env_lock().lock().await;
    let env = setup(true).await;
    let t0 = Utc::now();
    let t = create(
        &env.store,
        &env.runner,
        opts(&env, Budget::For(Duration::hours(1))),
        t0,
    )
    .await
    .unwrap();
    let flaky = Flaky {
        inner: &env.store,
        armed: std::sync::atomic::AtomicBool::new(true),
    };
    assert!(approve(&flaky, &t.id, t0, None).await.is_err());
    let stuck = env.store.get_task(&t.id).await.unwrap().unwrap();
    assert_eq!(stuck.state, TaskState::AwaitingApproval);
    let run = approve(&flaky, &t.id, t0 + Duration::seconds(5), None)
        .await
        .unwrap();
    assert_eq!(run.state, TaskState::Running);
    assert_eq!(
        run.deadline.0,
        t0 + Duration::hours(1),
        "frozen deadline adopted"
    );
    let (c, h) = load_frozen(
        &Git::new(worktree_of(&t)),
        &t.id,
        &run.contract_hash.unwrap(),
    )
    .unwrap();
    assert_eq!(h, c.hash());

    // A different contract under the ref is not adopted.
    let t2 = create(
        &env.store,
        &env.runner,
        opts(&env, Budget::For(Duration::hours(1))),
        t0,
    )
    .await
    .unwrap();
    let wt = Git::new(worktree_of(&t2));
    let other = ohara_harness::contract::Contract {
        outcome: "tampered".into(),
        ..c
    };
    ohara_harness::contract::freeze::freeze(&wt, &t2.id, &other).unwrap();
    let err = approve(&env.store, &t2.id, t0, None).await.unwrap_err();
    assert!(
        err.to_string()
            .contains(&format!("refs/ohara/tasks/{}/contract", t2.id)),
        "{err}"
    );
}

#[tokio::test]
async fn failed_create_removes_worktree_and_branch() {
    let _g = env_lock().lock().await;
    let mut env = setup(true).await;
    // A runner with no contract role makes drafting fail after the worktree exists.
    let cfg = HarnessConfig {
        runtimes: BTreeMap::new(),
        roles: RoleMap(BTreeMap::new()),
    };
    env.runner = RoleRunner::new(&cfg, |_, _| unreachable!()).unwrap();
    let err = create(
        &env.store,
        &env.runner,
        opts(&env, Budget::For(Duration::hours(1))),
        Utc::now(),
    )
    .await;
    assert!(err.is_err());
    let git = Git::new(env.repo.path());
    let trees = git.run(&["worktree", "list", "--porcelain"]).unwrap();
    assert_eq!(trees.matches("worktree ").count(), 1, "{trees}");
    let branches = git.run(&["branch", "--list", "ohara/*"]).unwrap();
    assert!(branches.trim().is_empty(), "{branches}");
    let tasks = env.store.list_tasks().await.unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].state, TaskState::Aborted);
    assert!(!worktree_of(&tasks[0]).exists());
}

#[tokio::test]
async fn refuses_ohara_home_inside_the_repo() {
    let _g = env_lock().lock().await;
    let env = setup(true).await;
    std::env::set_var("OHARA_HOME", env.repo.path().join(".ohara-home"));
    let err = create(
        &env.store,
        &env.runner,
        opts(&env, Budget::For(Duration::hours(1))),
        Utc::now(),
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("OHARA_HOME"), "{err}");
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
    let past = Utc::now() - Duration::hours(1);
    let err = create(
        &env.store,
        &env.runner,
        opts(&env, Budget::Until(past)),
        Utc::now(),
    )
    .await
    .unwrap_err();
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
