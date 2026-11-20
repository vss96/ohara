//! Freezing the contract under a side ref and loading it back.

use ohara_core::task::{ContractHash, TaskId};

use super::Contract;
use crate::error::Result;
use crate::git::Git;

/// File name of the contract inside the frozen tree.
const CONTRACT_FILE: &str = "contract.toml";

fn ref_name(task: &TaskId) -> String {
    format!("refs/ohara/tasks/{task}/contract")
}

/// Freeze `c` as `refs/ohara/tasks/<id>/contract`, returning its hash.
///
/// Writes the canonical TOML as a blob, a one-file tree and a commit using
/// plumbing only, so the working tree and index are never touched.
pub fn freeze(git: &Git, task: &TaskId, c: &Contract) -> Result<ContractHash> {
    let blob = git
        .run_with_stdin(&["hash-object", "-w", "--stdin"], Some(&c.to_toml()))?
        .trim()
        .to_string();
    let entry = format!("100644 blob {blob}\t{CONTRACT_FILE}\n");
    let tree = git
        .run_with_stdin(&["mktree"], Some(&entry))?
        .trim()
        .to_string();
    let commit = git
        .run_as_harness(
            &["commit-tree", &tree, "-m", "ohara: freeze task contract"],
            None,
        )?
        .trim()
        .to_string();
    git.run(&["update-ref", &ref_name(task), &commit])?;
    Ok(c.hash())
}

/// Load the frozen contract for `task` with its hash.
pub fn load_frozen(
    git: &Git,
    task: &TaskId,
    _expected: &ContractHash,
) -> Result<(Contract, ContractHash)> {
    let spec = format!("{}:{CONTRACT_FILE}", ref_name(task));
    let text = git.run(&["show", &spec])?;
    let contract = Contract::parse(&text)?;
    let hash = contract.hash();
    Ok((contract, hash))
}
