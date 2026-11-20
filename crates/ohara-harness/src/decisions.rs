//! Decision settlement: the supervisor turns an agent's `Proposed`
//! decisions into `Settled` or `Rejected` ones and attaches evidence only it
//! can produce.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use chrono::Utc;
use ohara_core::task::decision::{DecisionId, DecisionRecord, DecisionState, Evidence};
use ohara_core::task::{Deadline, RuntimeName, TaskId, TaskRecord, TaskStore};

use crate::agent::process::run_streaming_env;
use crate::contract::check::ScriptResult;
use crate::error::{Error, Result};
use crate::git::Git;

/// Longest an assumption check may run before it counts as failed.
const CHECK_TIMEOUT: chrono::Duration = chrono::Duration::seconds(60);

/// What `settle_round` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettleReport {
    /// Decisions accepted this call.
    pub settled: u32,
    /// Decisions rejected this call.
    pub rejected: u32,
}

/// What a round settles against: where it ended and what the supervisor saw.
#[derive(Debug, Clone, Copy)]
pub struct Settlement<'a> {
    /// Round whose `Proposed` decisions are settled.
    pub round: u32,
    /// Commit the round's work was checkpointed at.
    pub checkpoint_sha: &'a str,
    /// Contract script results of this checkpoint.
    pub scripts: &'a [ScriptResult],
    /// Runtime that produced the round.
    pub runtime: &'a RuntimeName,
    /// Directory assumption-check output is logged under.
    pub logs: &'a Path,
}

/// Settle or reject every `Proposed` decision of `round`.
///
/// `git` is rooted at the task worktree, which must be checked out at
/// `checkpoint_sha`; assumption `check` argvs are agent-controlled and run
/// there (never in the user's repo) as plain argv under a deadline, with
/// their output logged under `logs`. A decision is
/// rejected when a check exits non-zero (or cannot run) or when a
/// `parent_id` names no decision of this task. Otherwise it is settled with
/// the checkpoint, the contract `scripts` results and the `runtime`, and
/// gains a parent edge to every earlier settled decision whose checkpoint
/// diff touches a file this round's diff touches (over-link, never
/// under-link).
pub async fn settle_round(
    store: &dyn TaskStore,
    git: &Git,
    task: &TaskRecord,
    at: &Settlement<'_>,
) -> Result<SettleReport> {
    let Settlement {
        round,
        checkpoint_sha,
        scripts,
        runtime,
        logs,
    } = *at;
    let all = store.list_decisions(&task.id).await?;
    let proposed: Vec<&DecisionRecord> = all
        .iter()
        .filter(|d| d.round == round && d.state == DecisionState::Proposed)
        .collect();
    let mut report = SettleReport {
        settled: 0,
        rejected: 0,
    };
    if proposed.is_empty() {
        return Ok(report);
    }
    let known: HashSet<DecisionId> = all.iter().map(|d| d.id).collect();
    let overlaps = overlapping_ancestors(git, task, &all, round, checkpoint_sha).await?;
    let evidence = Evidence {
        script_results: scripts.iter().map(Into::into).collect(),
        runtime: Some(runtime.clone()),
    };
    for d in proposed {
        let mut next = d.clone();
        match rejection(d, &known, git.cwd(), logs).await {
            Some(reason) => {
                next.state = DecisionState::Rejected;
                next.rejected_reason = Some(reason);
                report.rejected += 1;
            }
            None => {
                next.state = DecisionState::Settled;
                next.checkpoint_sha = Some(checkpoint_sha.to_string());
                next.evidence = evidence.clone();
                for id in &overlaps {
                    if !next.parent_ids.contains(id) {
                        next.parent_ids.push(*id);
                    }
                }
                report.settled += 1;
            }
        }
        store.put_decision(&next).await?;
    }
    Ok(report)
}

/// Why `d` must be rejected, or `None` when it stands.
async fn rejection(
    d: &DecisionRecord,
    known: &HashSet<DecisionId>,
    cwd: &Path,
    logs: &Path,
) -> Option<String> {
    if let Some(p) = d.parent_ids.iter().find(|p| !known.contains(p)) {
        return Some(format!("parent decision {} does not exist", p.0));
    }
    for (i, a) in d.assumptions.iter().enumerate() {
        let Some(argv) = &a.check else {
            continue;
        };
        if let Err(why) = run_check(argv, cwd, &logs.join(format!("{}-{i}.log", d.id.0))).await {
            return Some(format!("assumption {:?} failed: {why}", a.text));
        }
    }
    None
}

/// Environment variables an assumption check may see.
const CHECK_ENV: [&str; 6] = [
    "PATH",
    "HOME",
    "TMPDIR",
    "LANG",
    "CARGO_HOME",
    "RUSTUP_HOME",
];

/// The minimal environment checks run with: [`CHECK_ENV`] entries that are set.
fn check_env() -> Vec<(String, String)> {
    CHECK_ENV
        .iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v)))
        .collect()
}

/// Run one assumption check: argv only (no shell), cwd fixed to the task
/// worktree, minimal environment, killed at [`CHECK_TIMEOUT`], output teed to
/// `log`.
///
/// Assumption checks are agent-authored. Phase 1 runs them with the agent's
/// own authority (worktree cwd, minimal env, deadline). Sandboxing is
/// required before distribution (spec §12a sandbox gate).
async fn run_check(argv: &[String], cwd: &Path, log: &Path) -> std::result::Result<(), String> {
    let deadline = Deadline(Utc::now() + CHECK_TIMEOUT);
    let env = check_env();
    let exit = run_streaming_env(argv, None, cwd, deadline, log, Some(&env))
        .await
        .map_err(|e| format!("could not run check: {e}"))?;
    match (exit.killed_at_deadline, exit.code) {
        (true, _) => Err("timed out".to_string()),
        (false, Some(0)) => Ok(()),
        (false, code) => Err(format!("exited with {code:?}")),
    }
}

/// Ids of earlier settled decisions whose checkpoint diff shares a file with
/// this round's diff.
///
/// A checkpoint's diff runs from the previous distinct checkpoint (or the
/// task's base commit) to itself, so rounds that settled nothing widen the
/// next diff rather than leaving a gap.
async fn overlapping_ancestors(
    git: &Git,
    task: &TaskRecord,
    all: &[DecisionRecord],
    round: u32,
    checkpoint_sha: &str,
) -> Result<Vec<DecisionId>> {
    let mut earlier: Vec<&DecisionRecord> = all
        .iter()
        .filter(|d| d.round < round && d.state == DecisionState::Settled)
        .filter(|d| d.checkpoint_sha.is_some())
        .collect();
    earlier.sort_by_key(|d| d.round);
    let mut checkpoints: Vec<&str> = Vec::new();
    for d in &earlier {
        let c = d.checkpoint_sha.as_deref().unwrap_or_default();
        if checkpoints.last() != Some(&c) {
            checkpoints.push(c);
        }
    }
    let base = checkpoints
        .last()
        .copied()
        .unwrap_or(task.base_sha.as_str());
    let this: BTreeSet<String> = diff_names(git, base, checkpoint_sha).await?;
    let mut out = Vec::new();
    for d in earlier {
        let c = d.checkpoint_sha.as_deref().unwrap_or_default();
        let from = match checkpoints.iter().position(|x| *x == c) {
            Some(i) if i > 0 => checkpoints[i - 1],
            _ => task.base_sha.as_str(),
        };
        let files = diff_names(git, from, c).await?;
        if !files.is_disjoint(&this) {
            out.push(d.id);
        }
    }
    Ok(out)
}

async fn diff_names(git: &Git, from: &str, to: &str) -> Result<BTreeSet<String>> {
    let git = git.clone();
    let (from, to) = (from.to_string(), to.to_string());
    let names = tokio::task::spawn_blocking(move || git.diff_names(&from, &to))
        .await
        .map_err(|e| Error::Git(format!("diff task: {e}")))??;
    Ok(names.into_iter().collect())
}

/// Key of the ohara server in the MCP config written by [`mcp_config_for`].
pub const MCP_SERVER_NAME: &str = "ohara";

/// Fully qualified name Claude Code gives the `propose_decision` tool.
pub fn propose_decision_tool_name() -> String {
    format!("mcp__{MCP_SERVER_NAME}__propose_decision")
}

/// Write an MCP config that runs `ohara-mcp --harness-task <id>` and return
/// its path (`<dir>/mcp.json`).
pub fn mcp_config_for(task: &TaskId, dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let config = serde_json::json!({
        "mcpServers": {
            MCP_SERVER_NAME: {
                "command": "ohara-mcp",
                "args": ["--harness-task", task.to_string()],
            }
        }
    });
    let path = dir.join("mcp.json");
    let body = serde_json::to_string_pretty(&config)
        .map_err(|e| Error::Config(format!("mcp config: {e}")))?;
    std::fs::write(&path, body)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use chrono::{TimeZone, Utc};
    use ohara_core::task::decision::{Assumption, CostIfWrong};
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
        let logs = tempfile::tempdir().unwrap();
        let scripts = [script_result()];
        let report = settle_round(
            &store,
            &git,
            &task,
            &Settlement {
                round: 1,
                checkpoint_sha: &ckpt,
                scripts: &scripts,
                runtime: &rt,
                logs: logs.path(),
            },
        )
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
        let logs = tempfile::tempdir().unwrap();
        let report = settle_round(
            &store,
            &git,
            &task,
            &Settlement {
                round: 1,
                checkpoint_sha: &ckpt,
                scripts: &[],
                runtime: &rt,
                logs: logs.path(),
            },
        )
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
        let logs = tempfile::tempdir().unwrap();

        // Round 1 touches shared.txt, round 2 touches only other.txt,
        // round 3 touches shared.txt again.
        let c1 = commit_file(&dir, &git, "shared.txt");
        let d1 = proposed(&task, 1, vec![]);
        store.put_decision(&d1).await.unwrap();
        settle_round(
            &store,
            &git,
            &task,
            &Settlement {
                round: 1,
                checkpoint_sha: &c1,
                scripts: &[],
                runtime: &rt,
                logs: logs.path(),
            },
        )
        .await
        .unwrap();

        let c2 = commit_file(&dir, &git, "other.txt");
        let d2 = proposed(&task, 2, vec![]);
        store.put_decision(&d2).await.unwrap();
        settle_round(
            &store,
            &git,
            &task,
            &Settlement {
                round: 2,
                checkpoint_sha: &c2,
                scripts: &[],
                runtime: &rt,
                logs: logs.path(),
            },
        )
        .await
        .unwrap();

        std::fs::write(dir.path().join("shared.txt"), "changed").unwrap();
        let c3 = git.commit_all("again").unwrap();
        let d3 = proposed(&task, 3, vec![]);
        store.put_decision(&d3).await.unwrap();
        settle_round(
            &store,
            &git,
            &task,
            &Settlement {
                round: 3,
                checkpoint_sha: &c3,
                scripts: &[],
                runtime: &rt,
                logs: logs.path(),
            },
        )
        .await
        .unwrap();

        let all = store.list_decisions(&task.id).await.unwrap();
        let by = |id| all.iter().find(|d| d.id == id).unwrap();
        assert!(by(d1.id).parent_ids.is_empty());
        assert!(by(d2.id).parent_ids.is_empty());
        assert_eq!(by(d3.id).parent_ids, vec![d1.id]);
    }

    #[tokio::test]
    async fn checks_run_in_the_worktree_as_plain_argv() {
        let (dir, git, base) = repo();
        let ckpt = commit_file(&dir, &git, "a.txt");
        let task = task_record(&base);
        let store = MemStore::default();
        let in_worktree = proposed(&task, 1, vec![argv(&["test", "-f", "a.txt"])]);
        // One argv element is a program name, never shell text.
        let shell_text = proposed(&task, 1, vec![argv(&["true && true"])]);
        store.put_decision(&in_worktree).await.unwrap();
        store.put_decision(&shell_text).await.unwrap();

        let rt = RuntimeName("claude".into());
        let logs = tempfile::tempdir().unwrap();
        let report = settle_round(
            &store,
            &git,
            &task,
            &Settlement {
                round: 1,
                checkpoint_sha: &ckpt,
                scripts: &[],
                runtime: &rt,
                logs: logs.path(),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            report,
            SettleReport {
                settled: 1,
                rejected: 1
            }
        );
        let all = store.list_decisions(&task.id).await.unwrap();
        let state = |id| all.iter().find(|d| d.id == id).unwrap().state;
        assert_eq!(state(in_worktree.id), DecisionState::Settled);
        assert_eq!(state(shell_text.id), DecisionState::Rejected);
    }

    #[tokio::test]
    async fn check_runs_with_minimal_env() {
        // Only this test sets the variable, and it never reads it back.
        std::env::set_var("OHARA_TEST_SECRET_T10", "hunter2");
        let (dir, git, base) = repo();
        let ckpt = commit_file(&dir, &git, "a.txt");
        let task = task_record(&base);
        let store = MemStore::default();
        // Passes only when the secret is NOT visible; PATH still resolves `sh`.
        let hidden = proposed(
            &task,
            1,
            vec![argv(&["sh", "-c", "test -z \"$OHARA_TEST_SECRET_T10\""])],
        );
        // Passes only when PATH was passed through.
        let has_path = proposed(&task, 1, vec![argv(&["sh", "-c", "test -n \"$PATH\""])]);
        store.put_decision(&hidden).await.unwrap();
        store.put_decision(&has_path).await.unwrap();

        let rt = RuntimeName("claude".into());
        let logs = tempfile::tempdir().unwrap();
        let report = settle_round(
            &store,
            &git,
            &task,
            &Settlement {
                round: 1,
                checkpoint_sha: &ckpt,
                scripts: &[],
                runtime: &rt,
                logs: logs.path(),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            report,
            SettleReport {
                settled: 2,
                rejected: 0
            }
        );
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
