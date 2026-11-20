//! `ohara task approve <id> [--edit]`: optional edit loop, then freeze.

use std::path::Path;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use ohara_core::task::{ContractHash, Deadline, TaskId, TaskRecord, TaskState, TaskStore};
use ohara_harness::contract::freeze::{freeze, load_frozen};
use ohara_harness::contract::Contract;
use ohara_harness::git::Git;

use super::clock::split_detail;
use super::{blocking, pending_path, shown_path, task_dir, worktree_path};

/// Workflow whose contracts may carry no scripts (rubrics only).
const SCRIPTLESS_WORKFLOW: &str = "research";
/// Printed with the contract, once per approval.
pub const SANDBOX_WARNING: &str = "Phase 1: agents run with your permissions; no sandbox.";
const MAX_EDIT_ATTEMPTS: usize = 5;

/// Opens `path` for the user to edit.
pub type Editor<'a> = dyn FnMut(&Path) -> Result<()> + 'a;

/// Run the user's `$EDITOR` (default `vi`) on `path`.
pub fn run_editor(path: &Path) -> Result<()> {
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(r#"${EDITOR:-vi} "$1""#)
        .arg("sh")
        .arg(path)
        .status()
        .context("starting $EDITOR")?;
    if !status.success() {
        bail!("editor exited with {status}");
    }
    Ok(())
}

fn strip_error_header(text: &str) -> String {
    text.lines()
        .filter(|l| !l.starts_with("# error:"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Edit `initial` in a copy at `path` until it parses as a contract. An
/// invalid save re-opens the editor with the error as a comment; an empty
/// file aborts.
fn edit_loop(initial: &str, path: &Path, editor: &mut Editor<'_>) -> Result<Contract> {
    std::fs::write(path, initial)?;
    for _ in 0..MAX_EDIT_ATTEMPTS {
        editor(path)?;
        let text = std::fs::read_to_string(path)?;
        if text.trim().is_empty() {
            bail!("edit aborted: the contract file was emptied");
        }
        match Contract::parse(&text) {
            Ok(c) => {
                std::fs::remove_file(path)?;
                return Ok(c);
            }
            Err(e) => {
                eprintln!("{e}");
                let note: String = e
                    .to_string()
                    .lines()
                    .map(|l| format!("# error: {l}\n"))
                    .collect();
                std::fs::write(path, format!("{note}{}", strip_error_header(&text)))?;
            }
        }
    }
    bail!(
        "contract still invalid after {MAX_EDIT_ATTEMPTS} edits; fix {}",
        path.display()
    )
}

/// What `approve` shows after freezing: the final contract, its hash and the
/// sandbox warning.
pub fn approval_report(task: &TaskRecord, frozen: &Contract) -> String {
    let hash = task
        .contract_hash
        .map(|h| h.to_string())
        .unwrap_or_else(|| frozen.hash().to_string());
    format!(
        "task {} approved; running until {}\nfrozen contract:\n{}\ncontract hash: {hash}\n{SANDBOX_WARNING}\n",
        task.id,
        task.deadline.0.to_rfc3339(),
        frozen.to_toml(),
    )
}

/// Load (and verify) the contract frozen for an approved `task`.
pub async fn load_frozen_contract(task: &TaskRecord) -> Result<Contract> {
    let hash: ContractHash = task.contract_hash.context("task has no contract hash")?;
    let (id, wt) = (task.id, worktree_path(&task.id)?);
    let (c, _) = blocking(move || load_frozen(&Git::new(&wt), &id, &hash)).await?;
    Ok(c)
}

fn ref_name(id: &TaskId) -> String {
    format!("refs/ohara/tasks/{id}/contract")
}

/// Freeze `contract`. If the side ref already exists from an earlier attempt
/// whose record update failed, adopt it when it matches `contract` apart
/// from the deadline; otherwise name the ref to delete.
fn freeze_or_recover(
    git: &Git,
    id: &TaskId,
    contract: &Contract,
) -> ohara_harness::Result<Contract> {
    let err = match freeze(git, id, contract) {
        Ok(_) => return Ok(contract.clone()),
        Err(e) if e.to_string().contains("already frozen") => e,
        Err(e) => return Err(e),
    };
    let spec = format!("{}:contract.toml", ref_name(id));
    let frozen = Contract::parse(&git.run(&["show", &spec])?)?;
    let mut expected = contract.clone();
    expected.budget.deadline = frozen.budget.deadline;
    if expected == frozen {
        return Ok(frozen);
    }
    Err(ohara_harness::Error::Contract(format!(
        "{err}, and it differs from the pending contract; delete {} and approve again",
        ref_name(id)
    )))
}

/// Approve `id`: apply the final deadline, freeze the contract, store its
/// hash and move the task to `Running` (heartbeat starts at `now`).
///
/// Without `editor`, the pending contract must be the one `create` showed
/// (its hash is recorded then); with one, the edited contract is accepted and
/// becomes the new baseline. `--for` budgets count from `now`; absolute
/// deadlines are kept. The contract's own deadline is always set to the task's
/// final deadline. A freeze left by a failed earlier attempt is adopted.
pub async fn approve(
    store: &dyn TaskStore,
    id: &TaskId,
    now: DateTime<Utc>,
    editor: Option<&mut Editor<'_>>,
) -> Result<TaskRecord> {
    let task = store
        .get_task(id)
        .await?
        .with_context(|| format!("no task {id}"))?;
    if task.state != TaskState::AwaitingApproval {
        bail!(
            "task {id} is {}, not awaiting approval",
            task.state.as_str()
        );
    }
    let pending = pending_path(id)?;
    let text = std::fs::read_to_string(&pending)
        .with_context(|| format!("reading the drafted contract {}", pending.display()))?;
    let base = match editor {
        Some(ed) => {
            let edited = edit_loop(&text, &task_dir(id)?.join("contract.edit.toml"), ed)?;
            std::fs::write(&pending, edited.to_toml())?;
            std::fs::write(shown_path(id)?, edited.hash().to_string())?;
            edited
        }
        None => {
            let parsed = Contract::parse(&text)?;
            let shown = std::fs::read_to_string(shown_path(id)?).unwrap_or_default();
            if shown.trim() != parsed.hash().to_string() {
                bail!(
                    "contract changed since it was shown; re-run with --edit to review: ohara task approve {id} --edit"
                );
            }
            parsed
        }
    };

    let (for_budget, note) = split_detail(task.detail.as_deref());
    let deadline = match for_budget {
        Some(d) => now + d,
        None => task.deadline.0,
    };
    if deadline <= now {
        bail!("task {id}: deadline {deadline} is in the past");
    }
    if base.scripts.is_empty() && task.workflow != SCRIPTLESS_WORKFLOW {
        bail!(
            "the contract has no scripts, so nothing could verify the work for a {:?} task; \
             add at least one [[script]] (ohara task approve {id} --edit)",
            task.workflow
        );
    }
    let mut contract = base;
    contract.budget.deadline = Deadline(deadline);

    let worktree = worktree_path(id)?;
    let task_id = *id;
    let frozen =
        blocking(move || freeze_or_recover(&Git::new(&worktree), &task_id, &contract)).await?;

    let running = TaskRecord {
        state: TaskState::Running,
        deadline: frozen.budget.deadline,
        contract_hash: Some(frozen.hash()),
        heartbeat_at: Some(now),
        detail: note,
        ..task
    };
    store.update_task(&running).await?;
    Ok(running)
}
