//! `propose_decision` tool: writes a `Proposed` row, validates its input,
//! and is only registered when the server is bound to a harness task.

use chrono::{TimeZone, Utc};
use ohara_core::task::decision::{CostIfWrong, DecisionState};
use ohara_core::task::{CostCents, Deadline, Outcome, TaskId, TaskRecord, TaskState, TaskStore};
use ohara_mcp::server::OharaServer;
use ohara_mcp::tools::find_pattern::OharaService;
use ohara_mcp::tools::propose_decision::{
    AssumptionInput, CostInput, HarnessCtx, ProposeDecisionInput,
};
use ohara_storage::SqliteTaskStore;
use std::sync::Arc;

fn running_task(round: u32, state: TaskState) -> TaskRecord {
    let at = Utc.timestamp_millis_opt(1_000).single().unwrap();
    TaskRecord {
        id: TaskId::new(),
        outcome: Outcome("ship".into()),
        workflow: "default".into(),
        repo_root: "/tmp/repo".into(),
        base_sha: "abc".into(),
        branch: "ohara/x".into(),
        state,
        deadline: Deadline(Utc.timestamp_millis_opt(9_000_000).single().unwrap()),
        cost_ceiling: Some(CostCents(500)),
        cost_spent: CostCents(0),
        contract_hash: None,
        heartbeat_at: None,
        round,
        created_at: at,
        finished_at: None,
        detail: None,
    }
}

fn input() -> ProposeDecisionInput {
    ProposeDecisionInput {
        question: "which store?".into(),
        options: vec!["sqlite".into(), "files".into()],
        chosen: "sqlite".into(),
        runner_up: Some("files".into()),
        assumptions: vec![AssumptionInput {
            text: "sqlite is available".into(),
            check: Some(vec!["true".into()]),
        }],
        cost_if_wrong: CostInput::Medium,
        parent_ids: vec![],
    }
}

async fn fixture(
    round: u32,
    state: TaskState,
) -> (
    tempfile::TempDir,
    Arc<SqliteTaskStore>,
    TaskRecord,
    OharaService,
) {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        SqliteTaskStore::open(&dir.path().join("harness.sqlite"))
            .await
            .unwrap(),
    );
    let task = running_task(round, state);
    store.create_task(&task).await.unwrap();
    let server = OharaServer::open(dir.path()).unwrap();
    let svc = OharaService::new(server).with_harness(HarnessCtx::new(task.id, store.clone()));
    (dir, store, task, svc)
}

#[tokio::test]
async fn propose_writes_proposed_row() {
    let (_d, store, task, svc) = fixture(3, TaskState::Running).await;
    let res = svc.propose_decision(input()).await.unwrap();
    let text = serde_json::to_value(&res).unwrap()["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    let out: serde_json::Value = serde_json::from_str(&text).unwrap();

    let rows = store.list_decisions(&task.id).await.unwrap();
    assert_eq!(rows.len(), 1);
    let d = &rows[0];
    assert_eq!(d.id.0.to_string(), out["decision_id"].as_str().unwrap());
    assert_eq!(d.state, DecisionState::Proposed);
    assert_eq!(d.round, 3);
    assert_eq!(d.chosen, "sqlite");
    assert_eq!(d.cost_if_wrong, CostIfWrong::Medium);
    assert!(d.evidence.script_results.is_empty());
    assert!(d.evidence.runtime.is_none());
    assert!(d.checkpoint_sha.is_none());
    assert_eq!(
        d.assumptions[0].check.as_deref(),
        Some(&["true".to_string()][..])
    );
}

#[tokio::test]
async fn chosen_not_in_options_is_tool_error() {
    let (_d, store, task, svc) = fixture(1, TaskState::Running).await;

    let mut bad = input();
    bad.chosen = "redis".into();
    assert!(svc.propose_decision(bad).await.is_err());

    let mut one = input();
    one.options = vec!["sqlite".into()];
    assert!(svc.propose_decision(one).await.is_err());

    let mut parent = input();
    parent.parent_ids = vec!["not-an-id".into()];
    assert!(svc.propose_decision(parent).await.is_err());

    assert!(store.list_decisions(&task.id).await.unwrap().is_empty());
}

#[tokio::test]
async fn task_not_running_is_tool_error() {
    let (_d, store, task, svc) = fixture(1, TaskState::Stopped).await;
    assert!(svc.propose_decision(input()).await.is_err());
    assert!(store.list_decisions(&task.id).await.unwrap().is_empty());
}

#[tokio::test]
async fn tool_absent_without_harness_flag() {
    let (dir, _store, _task, with) = fixture(1, TaskState::Running).await;
    assert!(with.tool_names().contains(&"propose_decision".to_string()));

    let without = OharaService::new(OharaServer::open(dir.path()).unwrap());
    let names = without.tool_names();
    assert!(
        !names.contains(&"propose_decision".to_string()),
        "{names:?}"
    );
    assert!(names.contains(&"find_pattern".to_string()));
    assert!(without.propose_decision(input()).await.is_err());
}

#[tokio::test]
async fn malformed_assumption_check_is_tool_error() {
    let (_d, store, task, svc) = fixture(1, TaskState::Running).await;
    let with_check = |check: Vec<String>| {
        let mut i = input();
        i.assumptions = vec![AssumptionInput {
            text: "x".into(),
            check: Some(check),
        }];
        i
    };

    // Empty argv, empty program, too many args, too many bytes.
    assert!(svc.propose_decision(with_check(vec![])).await.is_err());
    assert!(svc
        .propose_decision(with_check(vec!["".into()]))
        .await
        .is_err());
    let many = (0..65).map(|_| "a".to_string()).collect();
    assert!(svc.propose_decision(with_check(many)).await.is_err());
    let big = vec!["echo".to_string(), "x".repeat(8 * 1024)];
    assert!(svc.propose_decision(with_check(big)).await.is_err());
    assert!(store.list_decisions(&task.id).await.unwrap().is_empty());

    // The limits are inclusive: 64 args and exactly 8 KiB are fine.
    let at_cap: Vec<String> = (0..64).map(|_| "a".to_string()).collect();
    assert!(svc.propose_decision(with_check(at_cap)).await.is_ok());
}
