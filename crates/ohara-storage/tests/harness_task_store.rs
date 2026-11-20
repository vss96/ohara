use chrono::{DateTime, TimeZone, Utc};
use ohara_core::task::decision::{
    Assumption, CostIfWrong, DecisionId, DecisionRecord, DecisionState, Evidence, Verdict,
};
use ohara_core::task::{
    ContractHash, CostCents, Deadline, Outcome, Role, RuntimeName, StepRecord, StepState, TaskId,
    TaskRecord, TaskState, TaskStore,
};
use ohara_storage::SqliteTaskStore;
use std::path::PathBuf;

fn ts(ms: i64) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(ms).single().unwrap()
}

fn task(created_ms: i64) -> TaskRecord {
    TaskRecord {
        id: TaskId::new(),
        outcome: Outcome("ship it".into()),
        workflow: "default".into(),
        repo_root: PathBuf::from("/tmp/repo"),
        base_sha: "abc123".into(),
        branch: "ohara/x".into(),
        state: TaskState::Drafting,
        deadline: Deadline(ts(9_000_000)),
        cost_ceiling: Some(CostCents(500)),
        cost_spent: CostCents(0),
        contract_hash: None,
        heartbeat_at: None,
        round: 0,
        created_at: ts(created_ms),
        finished_at: None,
        detail: None,
    }
}

fn step(task_id: TaskId, round: u32, seq: u32, state: StepState) -> StepRecord {
    StepRecord {
        task_id,
        round,
        seq,
        role: Role::Builder,
        runtime: RuntimeName("claude".into()),
        model: Some("m".into()),
        state,
        started_at: ts(1_000),
        finished_at: None,
        head_sha: None,
        cost: Some(CostCents(3)),
        failover_from: Some(RuntimeName("codex".into())),
        transcript: PathBuf::from("t.log"),
        summary: None,
    }
}

fn decision(task_id: TaskId, chosen: &str) -> DecisionRecord {
    DecisionRecord {
        id: DecisionId::new(),
        task_id,
        round: 1,
        parent_ids: vec![DecisionId::new()],
        question: "q".into(),
        options: vec!["a".into(), "b".into()],
        chosen: chosen.into(),
        runner_up: Some("b".into()),
        assumptions: vec![Assumption {
            text: "t".into(),
            check: Some(vec!["true".into()]),
        }],
        cost_if_wrong: CostIfWrong::High,
        checkpoint_sha: None,
        evidence: Evidence {
            script_results: vec![],
            runtime: None,
        },
        state: DecisionState::Proposed,
        rejected_reason: None,
        human_verdict: None,
        verdict_note: None,
    }
}

async fn open(dir: &tempfile::TempDir) -> SqliteTaskStore {
    SqliteTaskStore::open(&dir.path().join("harness.sqlite"))
        .await
        .unwrap()
}

#[tokio::test]
async fn create_get_update_task_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(&dir).await;
    let mut t = task(1_000);
    s.create_task(&t).await.unwrap();
    assert_eq!(s.get_task(&t.id).await.unwrap(), Some(t.clone()));
    assert_eq!(s.get_task(&TaskId::new()).await.unwrap(), None);

    t.state = TaskState::Running;
    t.contract_hash = Some(ContractHash([7; 32]));
    t.heartbeat_at = Some(ts(2_000));
    t.cost_ceiling = None;
    t.cost_spent = CostCents(42);
    t.round = 2;
    t.finished_at = Some(ts(3_000));
    t.detail = Some("note".into());
    s.update_task(&t).await.unwrap();
    assert_eq!(s.get_task(&t.id).await.unwrap(), Some(t));
}

#[tokio::test]
async fn list_tasks_newest_first() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(&dir).await;
    let (a, b, c) = (task(1_000), task(3_000), task(2_000));
    for t in [&a, &b, &c] {
        s.create_task(t).await.unwrap();
    }
    let ids: Vec<_> = s
        .list_tasks()
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.id)
        .collect();
    assert_eq!(ids, vec![b.id, c.id, a.id]);
}

#[tokio::test]
async fn record_step_upserts_on_key() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(&dir).await;
    let t = task(1_000);
    s.create_task(&t).await.unwrap();
    let mut st = step(t.id, 1, 1, StepState::Running);
    s.record_step(&st).await.unwrap();
    s.record_step(&step(t.id, 1, 2, StepState::Running))
        .await
        .unwrap();
    st.state = StepState::Passed;
    st.finished_at = Some(ts(5_000));
    st.summary = Some("done".into());
    s.record_step(&st).await.unwrap();
    let steps = s.list_steps(&t.id).await.unwrap();
    assert_eq!(steps.len(), 2);
    assert_eq!(steps[0], st);
    assert_eq!(steps[1].seq, 2);
}

#[tokio::test]
async fn put_decision_upserts_and_lists_by_task() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(&dir).await;
    let t = task(1_000);
    let other = task(2_000);
    let mut d = decision(t.id, "a");
    s.put_decision(&d).await.unwrap();
    s.put_decision(&decision(other.id, "z")).await.unwrap();
    d.state = DecisionState::Settled;
    d.human_verdict = Some(Verdict::Overturned);
    d.verdict_note = Some("no".into());
    s.put_decision(&d).await.unwrap();
    let got = s.list_decisions(&t.id).await.unwrap();
    assert_eq!(got, vec![d]);
}

#[tokio::test]
async fn harness_migrations_do_not_touch_index_schema() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("harness.sqlite");
    drop(SqliteTaskStore::open(&path).await.unwrap());
    let c = rusqlite::Connection::open(&path).unwrap();
    let n = |name: &str| -> i64 {
        c.query_row(
            "SELECT count(*) FROM sqlite_master WHERE name = ?1",
            [name],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(n("commit_record"), 0);
    assert_eq!(n("task"), 1);
    assert_eq!(n("step"), 1);
    assert_eq!(n("decision"), 1);
}

#[tokio::test]
async fn concurrent_writers_do_not_fail_busy() {
    let dir = tempfile::tempdir().unwrap();
    let a = open(&dir).await;
    let b = open(&dir).await;
    let t = task(1_000);
    a.create_task(&t).await.unwrap();
    let id = t.id;
    let run = |s: SqliteTaskStore| async move {
        for i in 0..50 {
            s.heartbeat(&id, ts(10_000 + i)).await.unwrap();
        }
    };
    tokio::join!(run(a), run(b));
}
