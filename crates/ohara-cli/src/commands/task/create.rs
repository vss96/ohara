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
use super::{blocking, pending_path, shown_path, task_dir, worktree_path};

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

/// Single cleanup path for a failed create: mark the task aborted and remove
/// the worktree and branch from the user's repo. Best effort throughout.
async fn cleanup(store: &dyn TaskStore, task: &TaskRecord, top: PathBuf, why: &str) {
    if let Err(e) = abort(store, task, why).await {
        tracing::warn!(task = %task.id, error = %e, "could not mark task aborted");
    }
    let worktree = worktree_path(&task.id).ok();
    let (branch, wt) = (task.branch.clone(), worktree.clone());
    let removed = blocking(move || {
        let git = Git::new(&top);
        if let Some(wt) = &wt {
            if wt.exists() {
                git.worktree_remove(wt)?;
            }
        }
        // The branch only exists once the worktree add succeeded.
        git.branch_delete(&branch)
            .or(Ok::<(), ohara_harness::Error>(()))
    })
    .await;
    if let Err(e) = removed {
        tracing::warn!(task = %task.id, error = %e, "could not remove task worktree");
    }
    if let Some(wt) = worktree {
        let _ = std::fs::remove_dir_all(wt);
    }
}

fn check_home_outside_repo(top: &std::path::Path) -> Result<()> {
    let home = ohara_core::paths::ohara_home()?;
    std::fs::create_dir_all(&home)?;
    let (home, top) = (std::fs::canonicalize(&home)?, std::fs::canonicalize(top)?);
    if home.starts_with(&top) {
        bail!(
            "OHARA_HOME ({}) is inside the repository {}; agents could modify harness state",
            home.display(),
            top.display()
        );
    }
    Ok(())
}

/// Everything after the task row exists; any error triggers [`cleanup`].
async fn build(
    store: &dyn TaskStore,
    runner: &RoleRunner,
    task: &TaskRecord,
    workflow: &Workflow,
    top: PathBuf,
    for_budget: Option<chrono::Duration>,
) -> Result<TaskRecord> {
    let id = task.id;
    std::fs::create_dir_all(task_dir(&id)?)?;
    let worktree = worktree_path(&id)?;
    let (branch, wt, base) = (task.branch.clone(), worktree.clone(), task.base_sha.clone());
    blocking(move || Git::new(&top).worktree_add(&wt, &branch, &base))
        .await
        .context("creating the task worktree")?;

    let contract = draft_contract(runner, store, task, workflow, &worktree)
        .await
        .context("drafting the contract")?;
    std::fs::write(pending_path(&id)?, contract.to_toml())?;
    std::fs::write(shown_path(&id)?, contract.hash().to_string())?;

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

/// Create a task, its worktree at the repo's `HEAD`, and its drafted
/// contract; returns the task in `AwaitingApproval`.
///
/// A `--for` budget is recorded in `detail` and only starts counting at
/// approval; the provisional deadline just bounds drafting. On any failure
/// after the task row exists the task is aborted and its worktree and branch
/// are removed from the user's repo.
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
    let (top, base_sha, dirty) = blocking(move || {
        let probe = Git::new(&repo);
        let top = PathBuf::from(probe.run(&["rev-parse", "--show-toplevel"])?.trim());
        let git = Git::new(&top);
        let base = git.head()?;
        let dirty = !git.status_porcelain()?.trim().is_empty();
        Ok::<_, ohara_harness::Error>((top, base, dirty))
    })
    .await
    .with_context(|| {
        format!(
            "{} is not inside a git repository with a commit",
            opts.repo.display()
        )
    })?;
    check_home_outside_repo(&top)?;
    if dirty {
        eprintln!(
            "warning: {} has uncommitted changes; the task starts from HEAD ({}) and will not see them",
            top.display(),
            &base_sha[..base_sha.len().min(12)]
        );
    }

    let id = TaskId::new();
    let task = TaskRecord {
        id,
        outcome: Outcome(opts.outcome),
        workflow: workflow.name.clone(),
        repo_root: top.clone(),
        base_sha,
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
    match build(store, runner, &task, &workflow, top.clone(), for_budget).await {
        Ok(ready) => Ok(ready),
        Err(e) => {
            cleanup(store, &task, top, &format!("create failed: {e:#}")).await;
            Err(e)
        }
    }
}
