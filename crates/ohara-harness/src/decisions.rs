//! Decision settlement: the supervisor turns an agent's `Proposed`
//! decisions into `Settled` or `Rejected` ones and attaches evidence only it
//! can produce.

use std::path::{Path, PathBuf};

use ohara_core::task::decision::{Assumption, DecisionId, DecisionRecord, DecisionState, Evidence};
use ohara_core::task::{RuntimeName, TaskId, TaskRecord, TaskStore};

use crate::contract::check::ScriptResult;
use crate::error::Result;
use crate::git::Git;

/// What `settle_round` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettleReport {
    /// Decisions accepted this call.
    pub settled: u32,
    /// Decisions rejected this call.
    pub rejected: u32,
}

/// Settle or reject every `Proposed` decision of `round`.
pub async fn settle_round(
    _store: &dyn TaskStore,
    _git: &Git,
    _task: &TaskRecord,
    _round: u32,
    _checkpoint_sha: &str,
    _scripts: &[ScriptResult],
    _runtime: &RuntimeName,
) -> Result<SettleReport> {
    Ok(SettleReport {
        settled: 0,
        rejected: 0,
    })
}

/// Write an MCP config that runs `ohara-mcp --harness-task <id>`.
pub fn mcp_config_for(_task: &TaskId, dir: &Path) -> Result<PathBuf> {
    Ok(dir.join("mcp.json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use chrono::{TimeZone, Utc};
    use ohara_core::task::decision::{CostIfWrong, Evidence};
    use ohara_core::task::{CostCents, Deadline, Outcome, StepRecord, TaskRecord, TaskState};
    use std::sync::Mutex;

    #[derive(Default)]
    struct MemStore {
        decisions: Mutex<Vec<DecisionRecord>>,
    }

    #[async_trait]
    impl TaskStore for MemStore {
        async fn create_task(&self, _: &TaskRecord) -> ohara_core::Result<()> {
            Ok(())
        }
        async fn update_task(&self, _: &TaskRecord) -> ohara_core::Result<()> {
            Ok(())
        }
        async fn get_task(&self, _: &TaskId) -> ohara_core::Result<Option<TaskRecord>> {
            Ok(None)
        }
        async fn list_tasks(&self) -> ohara_core::Result<Vec<TaskRecord>> {
            Ok(vec![])
        }
        async fn heartbeat(&self, _: &TaskId, _: chrono::DateTime<Utc>) -> ohara_core::Result<()> {
            Ok(())
        }
        async fn record_step(&self, _: &StepRecord) -> ohara_core::Result<()> {
            Ok(())
        }
        async fn list_steps(&self, _: &TaskId) -> ohara_core::Result<Vec<StepRecord>> {
            Ok(vec![])
        }
        async fn put_decision(&self, d: &DecisionRecord) -> ohara_core::Result<()> {
            let mut all = self.decisions.lock().unwrap();
            match all.iter_mut().find(|x| x.id == d.id) {
                Some(slot) => *slot = d.clone(),
                None => all.push(d.clone()),
            }
            Ok(())
        }
        async fn list_decisions(&self, id: &TaskId) -> ohara_core::Result<Vec<DecisionRecord>> {
            let mut all: Vec<_> = self
                .decisions
                .lock()
                .unwrap()
                .iter()
                .filter(|d| d.task_id == *id)
                .cloned()
                .collect();
            all.sort_by_key(|d| d.round);
            Ok(all)
        }
    }

    fn task_record(base_sha: &str) -> TaskRecord {
        TaskRecord {
            id: TaskId::new(),
            outcome: Outcome("o".into()),
            workflow: "w".into(),
            repo_root: "/r".into(),
            base_sha: base_sha.into(),
            branch: "b".into(),
            state: TaskState::Running,
            deadline: Deadline(Utc.timestamp_millis_opt(9_000_000).single().unwrap()),
            cost_ceiling: None,
            cost_spent: CostCents(0),
            contract_hash: None,
            heartbeat_at: None,
            round: 1,
            created_at: Utc.timestamp_millis_opt(1_000).single().unwrap(),
            finished_at: None,
            detail: None,
        }
    }

    fn proposed(task: &TaskRecord, round: u32, checks: Vec<Vec<String>>) -> DecisionRecord {
        DecisionRecord {
            id: DecisionId::new(),
            task_id: task.id,
            round,
            parent_ids: vec![],
            question: "q".into(),
            options: vec!["a".into(), "b".into()],
            chosen: "a".into(),
            runner_up: None,
            assumptions: checks
                .into_iter()
                .map(|c| Assumption {
                    text: "holds".into(),
                    check: Some(c),
                })
                .collect(),
            cost_if_wrong: CostIfWrong::Low,
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

    fn repo() -> (tempfile::TempDir, Git, String) {
        let dir = tempfile::tempdir().unwrap();
        let git = Git::new(dir.path());
        git.run(&["init", "-q"]).unwrap();
        std::fs::write(dir.path().join("base.txt"), "0").unwrap();
        let base = git.commit_all("base").unwrap();
        (dir, git, base)
    }

    fn commit_file(dir: &tempfile::TempDir, git: &Git, name: &str) -> String {
        std::fs::write(dir.path().join(name), name).unwrap();
        git.commit_all(name).unwrap()
    }

    fn script_result() -> ScriptResult {
        ScriptResult {
            name: "tests".into(),
            passed: true,
            exit_code: Some(0),
            tests_run: Some(3),
            missing_required: vec![],
            failures: vec![],
            log: "tests.log".into(),
        }
    }

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[tokio::test]
    async fn settle_attaches_supervisor_evidence() {
        let (dir, git, base) = repo();
        let ckpt = commit_file(&dir, &git, "a.txt");
        let task = task_record(&base);
        let store = MemStore::default();
        let d = proposed(&task, 1, vec![argv(&["true"])]);
        store.put_decision(&d).await.unwrap();

        let rt = RuntimeName("claude".into());
        let report = settle_round(&store, &git, &task, 1, &ckpt, &[script_result()], &rt)
            .await
            .unwrap();
        assert_eq!(
            report,
            SettleReport {
                settled: 1,
                rejected: 0
            }
        );

        let got = &store.list_decisions(&task.id).await.unwrap()[0];
        assert_eq!(got.state, DecisionState::Settled);
        assert_eq!(got.checkpoint_sha.as_deref(), Some(ckpt.as_str()));
        assert_eq!(got.evidence.runtime, Some(rt));
        assert_eq!(got.evidence.script_results.len(), 1);
        assert_eq!(got.evidence.script_results[0].name, "tests");
        assert!(got.rejected_reason.is_none());
    }

    #[tokio::test]
    async fn failing_assumption_check_rejects() {
        let (dir, git, base) = repo();
        let ckpt = commit_file(&dir, &git, "a.txt");
        let task = task_record(&base);
        let store = MemStore::default();
        let bad = proposed(&task, 1, vec![argv(&["true"]), argv(&["false"])]);
        let missing = proposed(&task, 1, vec![argv(&["ohara-no-such-binary-xyz"])]);
        let mut orphan = proposed(&task, 1, vec![]);
        orphan.parent_ids = vec![DecisionId::new()];
        let ok = proposed(&task, 1, vec![]);
        for d in [&bad, &missing, &orphan, &ok] {
            store.put_decision(d).await.unwrap();
        }

        let rt = RuntimeName("claude".into());
        let report = settle_round(&store, &git, &task, 1, &ckpt, &[], &rt)
            .await
            .unwrap();
        assert_eq!(
            report,
            SettleReport {
                settled: 1,
                rejected: 3
            }
        );

        let all = store.list_decisions(&task.id).await.unwrap();
        let by = |id| all.iter().find(|d| d.id == id).unwrap();
        for id in [bad.id, missing.id, orphan.id] {
            assert_eq!(by(id).state, DecisionState::Rejected);
            assert!(by(id).rejected_reason.is_some());
            assert!(by(id).checkpoint_sha.is_none());
        }
        assert!(by(orphan.id)
            .rejected_reason
            .as_deref()
            .unwrap()
            .contains("parent"));
        assert_eq!(by(ok.id).state, DecisionState::Settled);
    }

    #[tokio::test]
    async fn overlapping_files_add_parent_edge() {
        let (dir, git, base) = repo();
        let task = task_record(&base);
        let store = MemStore::default();
        let rt = RuntimeName("claude".into());

        // Round 1 touches shared.txt, round 2 touches only other.txt,
        // round 3 touches shared.txt again.
        let c1 = commit_file(&dir, &git, "shared.txt");
        let d1 = proposed(&task, 1, vec![]);
        store.put_decision(&d1).await.unwrap();
        settle_round(&store, &git, &task, 1, &c1, &[], &rt)
            .await
            .unwrap();

        let c2 = commit_file(&dir, &git, "other.txt");
        let d2 = proposed(&task, 2, vec![]);
        store.put_decision(&d2).await.unwrap();
        settle_round(&store, &git, &task, 2, &c2, &[], &rt)
            .await
            .unwrap();

        std::fs::write(dir.path().join("shared.txt"), "changed").unwrap();
        let c3 = git.commit_all("again").unwrap();
        let d3 = proposed(&task, 3, vec![]);
        store.put_decision(&d3).await.unwrap();
        settle_round(&store, &git, &task, 3, &c3, &[], &rt)
            .await
            .unwrap();

        let all = store.list_decisions(&task.id).await.unwrap();
        let by = |id| all.iter().find(|d| d.id == id).unwrap();
        assert!(by(d1.id).parent_ids.is_empty());
        assert!(by(d2.id).parent_ids.is_empty());
        assert_eq!(by(d3.id).parent_ids, vec![d1.id]);
    }

    #[test]
    fn mcp_config_names_the_harness_task() {
        let dir = tempfile::tempdir().unwrap();
        let task = TaskId::new();
        let path = mcp_config_for(&task, dir.path()).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let server = &v["mcpServers"]["ohara"];
        assert_eq!(server["command"], "ohara-mcp");
        assert_eq!(
            server["args"],
            serde_json::json!(["--harness-task", task.to_string()])
        );
    }
}
