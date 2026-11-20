//! `ohara task "<outcome>"`: record the task, create its worktree, draft the
//! contract and leave the task awaiting approval.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use ohara_core::task::{CostCents, Deadline, Outcome, TaskId, TaskRecord, TaskState, TaskStore};
use ohara_harness::agent::roles::RoleRunner;
use ohara_harness::draft::draft_contract;
use ohara_harness::git::Git;
use ohara_harness::workflow::Workflow;

use super::clock::{encode_for, split_detail, Budget};
use super::{blocking, pending_path, task_dir, worktree_path};

/// Inputs to [`create`].
#[derive(Debug, Clone)]
pub struct CreateOpts {
    /// What the user wants done.
    pub outcome: String,
    /// `--for` or `--until`.
    pub budget: Budget,
    /// Workflow name.
    pub workflow: String,
    /// Repository (or any directory inside it) to work on.
    pub repo: PathBuf,
}

/// The branch a task works on: `ohara/<id, lowercase>`.
fn branch_for(id: &TaskId) -> String {
    format!("ohara/{}", id.to_string().to_lowercase())
}

async fn abort(store: &dyn TaskStore, task: &TaskRecord, why: &str) -> Result<()> {
    let record = TaskRecord {
        state: TaskState::Aborted,
        finished_at: Some(Utc::now()),
        detail: Some(why.to_string()),
        ..task.clone()
    };
    store.update_task(&record).await?;
    Ok(())
}

/// Create a task, its worktree at the repo's `HEAD`, and its drafted
/// contract; returns the task in `AwaitingApproval`.
///
/// A `--for` budget is recorded in `detail` and only starts counting at
/// approval; the provisional deadline just bounds drafting.
pub async fn create(
    store: &dyn TaskStore,
    runner: &RoleRunner,
    opts: CreateOpts,
    now: DateTime<Utc>,
) -> Result<TaskRecord> {
    let (deadline, for_budget) = match opts.budget {
        Budget::For(d) => (now + d, Some(d)),
        Budget::Until(at) if at > now => (at, None),
        Budget::Until(_) => bail!("--until is in the past"),
    };
    let workflow = Workflow::load(&opts.workflow)?;
    let repo = opts.repo.clone();
    let (top, base_sha) = blocking(move || {
        let probe = Git::new(&repo);
        let top = PathBuf::from(probe.run(&["rev-parse", "--show-toplevel"])?.trim());
        let base = Git::new(&top).head()?;
        Ok::<_, ohara_harness::Error>((top, base))
    })
    .await
    .with_context(|| {
        format!(
            "{} is not inside a git repository with a commit",
            opts.repo.display()
        )
    })?;

    let id = TaskId::new();
    let task = TaskRecord {
        id,
        outcome: Outcome(opts.outcome),
        workflow: workflow.name.clone(),
        repo_root: top.clone(),
        base_sha: base_sha.clone(),
        branch: branch_for(&id),
        state: TaskState::Drafting,
        deadline: Deadline(deadline),
        cost_ceiling: None::<CostCents>,
        cost_spent: CostCents(0),
        contract_hash: None,
        heartbeat_at: None,
        round: 0,
        created_at: now,
        finished_at: None,
        detail: for_budget.map(|d| encode_for(d, None)),
    };
    store.create_task(&task).await?;

    let dir = task_dir(&id)?;
    std::fs::create_dir_all(&dir)?;
    let worktree = worktree_path(&id)?;
    let (branch, wt) = (task.branch.clone(), worktree.clone());
    let added = blocking(move || Git::new(&top).worktree_add(&wt, &branch, &base_sha)).await;
    if let Err(e) = added {
        abort(store, &task, &format!("worktree creation failed: {e}")).await?;
        return Err(e.context("creating the task worktree"));
    }

    let contract = match draft_contract(runner, store, &task, &workflow, &worktree).await {
        Ok(c) => c,
        Err(e) => {
            abort(store, &task, &format!("drafting failed: {e}")).await?;
            return Err(anyhow::Error::from(e).context("drafting the contract"));
        }
    };
    std::fs::write(pending_path(&id)?, contract.to_toml())?;

    // Drafting may have replaced `detail` with a fallback note; keep both.
    let latest = store.get_task(&id).await?.unwrap_or_else(|| task.clone());
    let (_, note) = split_detail(latest.detail.as_deref());
    let detail = match for_budget {
        Some(d) => Some(encode_for(d, note.as_deref())),
        None => note,
    };
    let ready = TaskRecord {
        state: TaskState::AwaitingApproval,
        detail,
        ..latest
    };
    store.update_task(&ready).await?;
    Ok(ready)
}
