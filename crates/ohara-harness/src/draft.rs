//! Contract drafting: ask the contract-role agent to adapt a workflow's
//! template to the repository, then validate whatever it wrote.
//!
//! The agent writes `contract.draft.toml` in a scratch directory under the
//! task directory (never the worktree). Its free-text reply is ignored: only a
//! file that passes [`Contract::parse`] is used, otherwise the template is.

use std::path::Path;

use chrono::Utc;
use ohara_core::task::{Deadline, Role, RuntimeName, StepRecord, StepState, TaskRecord, TaskStore};

use crate::agent::roles::RoleRunner;
use crate::agent::{AgentRequest, AgentStatus};
use crate::contract::{Budget, Contract};
use crate::error::{Error, Result};
use crate::workflow::Workflow;

const PROMPT: &str = include_str!("prompts/contract.md");
const FALLBACK_DETAIL: &str = "drafting fell back to template";
const HINT_FILES: [&str; 3] = ["Cargo.toml", "package.json", "pyproject.toml"];
/// Upper bound on a draft file; a contract is a few KB at most.
const MAX_DRAFT_BYTES: u64 = 256 * 1024;
const DRAFT_TIME: chrono::Duration = chrono::Duration::minutes(10);

/// Fill `{{key}}` placeholders in one pass, so substituted text (which can
/// contain anything, e.g. the user's outcome) is never re-scanned.
fn render(template: &str, values: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let hit = after.find("}}").and_then(|end| {
            let key = &after[..end];
            values
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| (end, *v))
        });
        let Some((end, value)) = hit else {
            out.push_str("{{");
            rest = after;
            continue;
        };
        out.push_str(value);
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    out
}

fn repo_hints(cwd: &Path) -> String {
    let found: Vec<&str> = HINT_FILES
        .iter()
        .copied()
        .filter(|f| cwd.join(f).is_file())
        .collect();
    if found.is_empty() {
        return "none of Cargo.toml, package.json or pyproject.toml found".to_string();
    }
    format!("present at the repository root: {}", found.join(", "))
}

/// The workflow template as a contract for `task`: outcome and budget come
/// from the task, scripts/rubric/protected paths from the workflow. Round
/// trips through [`Contract::parse`] so a bad template fails loudly.
fn template_contract(task: &TaskRecord, wf: &Workflow) -> Result<Contract> {
    let t = &wf.contract_template;
    let c = Contract {
        outcome: task.outcome.0.clone(),
        protected_paths: t.protected_paths.clone(),
        budget: Budget {
            deadline: task.deadline,
            cost_ceiling_cents: task.cost_ceiling.map(|c| c.0),
        },
        scripts: t.scripts.clone(),
        rubrics: t.rubric.clone(),
    };
    Contract::parse(&c.to_toml())
}

fn read_draft(path: &Path) -> Result<Contract> {
    let len = std::fs::metadata(path)
        .map_err(|e| Error::Contract(format!("agent wrote no draft at {}: {e}", path.display())))?
        .len();
    if len > MAX_DRAFT_BYTES {
        return Err(Error::Contract(format!("draft is {len} bytes; too large")));
    }
    let text = std::fs::read_to_string(path)
        .map_err(|e| Error::Contract(format!("draft is unreadable: {e}")))?;
    Contract::parse(&text)
}

/// Draft the contract for `task`, running the agent with `cwd` as its working
/// directory. Task directory is `$OHARA_HOME/tasks/<task-id>`.
///
/// The outcome and budget always come from the task. If the agent fails or
/// its draft does not parse, the template is returned and the task's `detail`
/// is set to "drafting fell back to template".
pub async fn draft_contract(
    runner: &RoleRunner,
    store: &dyn TaskStore,
    task: &TaskRecord,
    wf: &Workflow,
    cwd: &Path,
) -> Result<Contract> {
    let task_dir = ohara_core::paths::ohara_home()?
        .join("tasks")
        .join(task.id.to_string());
    draft_in(runner, store, task, wf, cwd, &task_dir).await
}

/// [`draft_contract`] against an explicit task directory.
pub(crate) async fn draft_in(
    runner: &RoleRunner,
    store: &dyn TaskStore,
    task: &TaskRecord,
    wf: &Workflow,
    cwd: &Path,
    task_dir: &Path,
) -> Result<Contract> {
    return Err(Error::Contract("unimplemented".into()));
    #[allow(unreachable_code)]
    let template = template_contract(task, wf)?;
    let scratch = task_dir.join("draft");
    std::fs::create_dir_all(&scratch)?;
    let draft_path = scratch.join("contract.draft.toml");
    match std::fs::remove_file(&draft_path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(Error::Io(e)),
    }

    let prompt = render(
        PROMPT,
        &[
            ("outcome", &task.outcome.0),
            ("hints", &repo_hints(cwd)),
            ("template", &template.to_toml()),
            ("draft_path", &draft_path.to_string_lossy()),
        ],
    );
    let now = Utc::now();
    let req = AgentRequest {
        cwd: cwd.to_path_buf(),
        prompt,
        mcp_config: None,
        deadline: Deadline((now + DRAFT_TIME).min(task.deadline.0)),
        transcript: scratch.join("contract-draft.log"),
        model: None,
    };
    // Drafting is round 0, so it never collides with build rounds (>= 1).
    let step = StepRecord {
        task_id: task.id,
        round: 0,
        seq: 0,
        role: Role::Contract,
        runtime: RuntimeName(String::new()),
        model: None,
        state: StepState::Running,
        started_at: now,
        finished_at: None,
        head_sha: None,
        cost: None,
        failover_from: None,
        transcript: req.transcript.clone(),
        summary: None,
    };
    // The agent only writes to the scratch dir, so a failover needs no reset.
    let (outcome, _) = runner
        .run(Role::Contract, req, step, store, || Ok(()))
        .await?;

    let drafted = match outcome.status {
        AgentStatus::Completed => read_draft(&draft_path),
        other => Err(Error::Runtime(format!("contract agent ended {other:?}"))),
    };
    match drafted {
        Ok(c) => Ok(Contract {
            outcome: template.outcome,
            budget: template.budget,
            ..c
        }),
        Err(e) => {
            tracing::warn!(error = %e, task = %task.id, "contract draft unusable; using template");
            let detail = TaskRecord {
                detail: Some(FALLBACK_DETAIL.to_string()),
                ..task.clone()
            };
            store.update_task(&detail).await?;
            Ok(template)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::fake::FakeRuntime;
    use crate::agent::AgentOutcome;
    use crate::config::{HarnessConfig, RoleMap, RuntimeKind, RuntimeSpec};
    use ohara_core::task::decision::DecisionRecord;
    use ohara_core::task::{CostCents, Outcome, TaskId, TaskState};
    use ohara_core::Result as CoreResult;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct MemStore {
        updated: Mutex<Vec<TaskRecord>>,
    }

    #[async_trait::async_trait]
    impl TaskStore for MemStore {
        async fn create_task(&self, _t: &TaskRecord) -> CoreResult<()> {
            Ok(())
        }
        async fn update_task(&self, t: &TaskRecord) -> CoreResult<()> {
            self.updated.lock().unwrap().push(t.clone());
            Ok(())
        }
        async fn get_task(&self, _id: &TaskId) -> CoreResult<Option<TaskRecord>> {
            Ok(None)
        }
        async fn list_tasks(&self) -> CoreResult<Vec<TaskRecord>> {
            Ok(vec![])
        }
        async fn heartbeat(&self, _id: &TaskId, _at: chrono::DateTime<Utc>) -> CoreResult<()> {
            Ok(())
        }
        async fn record_step(&self, _s: &StepRecord) -> CoreResult<()> {
            Ok(())
        }
        async fn list_steps(&self, _id: &TaskId) -> CoreResult<Vec<StepRecord>> {
            Ok(vec![])
        }
        async fn put_decision(&self, _d: &DecisionRecord) -> CoreResult<()> {
            Ok(())
        }
        async fn list_decisions(&self, _id: &TaskId) -> CoreResult<Vec<DecisionRecord>> {
            Ok(vec![])
        }
    }

    fn task(outcome: &str) -> TaskRecord {
        TaskRecord {
            id: TaskId::new(),
            outcome: Outcome(outcome.into()),
            workflow: "bug".into(),
            repo_root: "/tmp".into(),
            base_sha: "abc".into(),
            branch: "ohara/x".into(),
            state: TaskState::Drafting,
            deadline: Deadline(Utc::now() + chrono::Duration::hours(1)),
            cost_ceiling: Some(CostCents(500)),
            cost_spent: CostCents(0),
            contract_hash: None,
            heartbeat_at: None,
            round: 0,
            created_at: Utc::now(),
            finished_at: None,
            detail: None,
        }
    }

    fn bug_workflow() -> Workflow {
        let dir = tempfile::tempdir().unwrap();
        Workflow::load_from(dir.path(), "bug").unwrap()
    }

    /// A runner over one fake whose edit hook writes `draft` (if any) to the
    /// scratch path the prompt names.
    fn runner(draft: Option<&'static str>, status: AgentStatus) -> (RoleRunner, Arc<FakeRuntime>) {
        let name = RuntimeName("fake".into());
        let spec = RuntimeSpec {
            kind: RuntimeKind::Fake,
            model: None,
            command: None,
            prompt: None,
            done: None,
        };
        let cfg = HarnessConfig {
            runtimes: BTreeMap::from([(name.clone(), spec)]),
            roles: RoleMap(BTreeMap::from([(Role::Contract, vec![name.clone()])])),
        };
        let outcome = AgentOutcome {
            status,
            final_message: "I wrote it, honest. ```toml garbage".into(),
            cost: None,
            exit_code: None,
        };
        let fake = Arc::new(
            FakeRuntime::new(name, vec![outcome]).with_edit(move |req| {
                let Some(body) = draft else { return };
                let marker = "exactly this file, and nowhere else:";
                let path = req
                    .prompt
                    .split_once(marker)
                    .map(|(_, rest)| rest.trim().lines().next().unwrap_or("").trim().to_string())
                    .unwrap();
                std::fs::write(path, body).unwrap();
            }),
        );
        let by = fake.clone();
        (RoleRunner::new(&cfg, move |_, _| by.clone()).unwrap(), fake)
    }

    const VALID: &str = r#"
outcome = "agent rewrote the outcome"
protected_paths = ["ci/"]
[budget]
deadline = "2001-01-01T00:00:00Z"
[[script]]
name = "py"
run = ["pytest", "-q"]
kind = "test"
[[rubric]]
id = "r"
criterion = "c"
pass_mark = 5
scale_max = 10
"#;

    #[tokio::test]
    async fn draft_uses_agent_output() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("pyproject.toml"), "").unwrap();
        let (runner, fake) = runner(Some(VALID), AgentStatus::Completed);
        let store = MemStore::default();
        let t = task("fix the flaky thing");
        let c = draft_in(&runner, &store, &t, &bug_workflow(), dir.path(), dir.path())
            .await
            .unwrap();

        assert_eq!(c.scripts[0].name, "py");
        assert_eq!(c.protected_paths, ["ci/"]);
        // Outcome and budget are the supervisor's, not the agent's.
        assert_eq!(c.outcome, "fix the flaky thing");
        assert_eq!(c.budget.deadline, t.deadline);
        assert_eq!(c.budget.cost_ceiling_cents, Some(500));
        assert_eq!(Contract::parse(&c.to_toml()).unwrap(), c);
        assert!(store.updated.lock().unwrap().is_empty());

        let calls = fake.calls();
        assert!(calls[0].prompt.contains("fix the flaky thing"));
        assert!(calls[0].prompt.contains("pyproject.toml"));
        assert!(calls[0].prompt.contains("cargo")); // the template
        assert!(!calls[0].prompt.contains("{{"));
        // The draft lives in the scratch dir, not in the repo/cwd.
        assert!(dir.path().join("draft/contract.draft.toml").exists());
    }

    #[tokio::test]
    async fn invalid_draft_falls_back_to_template_with_detail() {
        let bad: [Option<&'static str>; 4] = [
            Some("this is { not toml"),
            Some("outcome = \"x\"\nprotected_paths = [\"./\"]\n[budget]\ndeadline = \"2026-11-20T17:00:00Z\"\n"),
            Some("```toml\noutcome = 1\n```"),
            None, // agent wrote nothing
        ];
        for draft in bad {
            let dir = tempfile::tempdir().unwrap();
            let (runner, _) = runner(draft, AgentStatus::Completed);
            let store = MemStore::default();
            let t = task("fix it");
            let c = draft_in(&runner, &store, &t, &bug_workflow(), dir.path(), dir.path())
                .await
                .unwrap();
            assert_eq!(c.outcome, "fix it");
            assert_eq!(c.scripts, bug_workflow().contract_template.scripts);
            assert_eq!(c.rubrics, bug_workflow().contract_template.rubric);
            let updated = store.updated.lock().unwrap();
            assert_eq!(updated.len(), 1, "{draft:?}");
            assert_eq!(
                updated[0].detail.as_deref(),
                Some("drafting fell back to template")
            );
        }
    }

    #[tokio::test]
    async fn crashed_agent_falls_back_even_if_it_wrote_a_draft() {
        let dir = tempfile::tempdir().unwrap();
        let (runner, _) = runner(Some(VALID), AgentStatus::Crashed);
        let store = MemStore::default();
        let c = draft_in(&runner, &store, &task("t"), &bug_workflow(), dir.path(), dir.path())
            .await
            .unwrap();
        assert_eq!(c.scripts[0].name, "tests");
        assert_eq!(store.updated.lock().unwrap().len(), 1);
    }

    #[test]
    fn render_does_not_rescan_substituted_text() {
        let out = render("a {{x}} b {{y}} {{z}}", &[("x", "{{y}}"), ("y", "Y")]);
        assert_eq!(out, "a {{y}} b Y {{z}}");
    }
}
